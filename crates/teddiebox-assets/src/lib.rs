#![no_std]

//! Files out of the stock `assets` partition.
//!
//! Stock keeps its sounds and the box's certificate and key in a FAT12 volume
//! under ESP-IDF's wear levelling. Nothing here writes: a raw write would
//! corrupt the volume, because wear levelling moves sectors around.
//!
//! What it reads is what stock's volume is: 4096-byte sectors, 12-bit FAT
//! entries, 8.3 names matched without regard to case, and each directory held
//! in one cluster. A volume that is anything else is refused or not found, not
//! read wrongly, with one exception: the wear-levelling state is not checked
//! against its checksum.

#[cfg(test)]
extern crate std;

/// Where the bytes come from: a flash region, read at byte offsets.
pub trait ReadAt {
    type Error;

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<(), Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// The source could not be read.
    Source(E),
    /// No file at that path.
    NotFound,
    /// The file does not fit in the buffer given.
    TooSmall,
    /// The partition does not hold the volume this crate reads.
    NotFormatted,
    /// The volume is laid out in a way this crate has not been checked against.
    Unsupported,
}

/// Bytes in a directory entry.
const ENTRY: u32 = 32;

/// An open `assets` volume.
pub struct Assets<R> {
    source: R,
    /// The sector the wear levelling is currently keeping empty.
    dummy: u32,
    sector: u32,
    sectors_per_cluster: u32,
    fat_at: u32,
    root_at: u32,
    root_entries: u32,
    data_at: u32,
}

impl<R: ReadAt> Assets<R> {
    /// Opens the volume held in the `len` bytes `source` reads, which are the
    /// whole `assets` partition.
    pub fn open(mut source: R, len: u32) -> Result<Self, Error<R::Error>> {
        let dummy = dummy_sector(&mut source, len)?;
        let mut boot = [0u8; 24];
        read_logical(&mut source, dummy, 0, &mut boot).map_err(Error::Source)?;
        let sector = u32::from(u16::from_le_bytes([boot[11], boot[12]]));
        let sectors_per_cluster = u32::from(boot[13]);
        let reserved = u32::from(u16::from_le_bytes([boot[14], boot[15]]));
        let fats = u32::from(boot[16]);
        let root_entries = u32::from(u16::from_le_bytes([boot[17], boot[18]]));
        let fat_sectors = u32::from(u16::from_le_bytes([boot[22], boot[23]]));
        if sector != SECTOR || sectors_per_cluster == 0 || fats == 0 || root_entries == 0 {
            return Err(Error::NotFormatted);
        }
        let root_sectors = (root_entries * ENTRY).div_ceil(sector);
        let data_sector = u64::from(reserved + fats * fat_sectors + root_sectors);
        if data_sector * u64::from(sector) >= u64::from(len) {
            return Err(Error::NotFormatted);
        }
        let fat_at = reserved * sector;
        let root_at = (reserved + fats * fat_sectors) * sector;
        let data_at = data_sector as u32 * sector;
        Ok(Self {
            source,
            dummy,
            sector,
            sectors_per_cluster,
            fat_at,
            root_at,
            root_entries,
            data_at,
        })
    }

    /// Reads the file at `path`, directories separated by `/`, into `out`.
    pub fn read_file(&mut self, path: &str, out: &mut [u8]) -> Result<usize, Error<R::Error>> {
        let mut dir = Dir::Root;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let (cluster, size) = self.find(dir, part)?;
            if parts.peek().is_some() {
                dir = Dir::Cluster(cluster);
            } else {
                return self.read_chain(cluster, size, out);
            }
        }
        Err(Error::NotFound)
    }

    fn read_chain(
        &mut self,
        mut cluster: u32,
        size: u32,
        out: &mut [u8],
    ) -> Result<usize, Error<R::Error>> {
        let len = size as usize;
        if len > out.len() {
            return Err(Error::TooSmall);
        }
        let cluster_bytes = (self.sectors_per_cluster * self.sector) as usize;
        let mut done = 0;
        while done < len {
            let chunk = cluster_bytes.min(len - done);
            let at = self.cluster_at(cluster);
            read_logical(
                &mut self.source,
                self.dummy,
                at,
                &mut out[done..done + chunk],
            )
            .map_err(Error::Source)?;
            done += chunk;
            cluster = self.next_cluster(cluster)?;
        }
        Ok(len)
    }

    /// The cluster after `cluster`, from the FAT's 12-bit entries.
    fn next_cluster(&mut self, cluster: u32) -> Result<u32, Error<R::Error>> {
        let mut pair = [0u8; 2];
        let at = self.fat_at + cluster * 3 / 2;
        read_logical(&mut self.source, self.dummy, at, &mut pair).map_err(Error::Source)?;
        let entry = u32::from(u16::from_le_bytes(pair));
        Ok(if cluster.is_multiple_of(2) {
            entry & 0xFFF
        } else {
            entry >> 4
        })
    }

    fn cluster_at(&self, cluster: u32) -> u32 {
        self.data_at + (cluster - 2) * self.sectors_per_cluster * self.sector
    }

    fn find(&mut self, dir: Dir, name: &str) -> Result<(u32, u32), Error<R::Error>> {
        let (at, entries) = match dir {
            Dir::Root => (self.root_at, self.root_entries),
            Dir::Cluster(cluster) => (self.cluster_at(cluster), self.sector / ENTRY),
        };
        for entry in 0..entries {
            let mut raw = [0u8; ENTRY as usize];
            read_logical(&mut self.source, self.dummy, at + entry * ENTRY, &mut raw)
                .map_err(Error::Source)?;
            if short_name(name).eq_ignore_ascii_case(&raw[..11]) {
                let cluster = u32::from(u16::from_le_bytes([raw[26], raw[27]]));
                let size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
                return Ok((cluster, size));
            }
        }
        Err(Error::NotFound)
    }
}

/// Bytes in a wear-levelling sector.
const SECTOR: u32 = 4096;
/// Bytes in the header of a wear-levelling state, and in each move record after it.
const STATE_HEADER: u32 = 64;
const RECORD: u32 = 16;

fn read_u32<R: ReadAt>(source: &mut R, at: u32) -> Result<u32, Error<R::Error>> {
    let mut word = [0u8; 4];
    source.read_at(at, &mut word).map_err(Error::Source)?;
    Ok(u32::from_le_bytes(word))
}

/// Where the dummy sector is: the header's position, plus one for each move
/// recorded since the header was written.
///
/// A state whose dummy sector has been all the way round and moved the volume
/// on by a sector (`move_count` above zero) is refused: the mapping for that is
/// not checked against any real flash.
///
/// The state sits after the volume, in two copies of as many sectors as
/// header and records need, followed by one configuration sector.
fn dummy_sector<R: ReadAt>(source: &mut R, len: u32) -> Result<u32, Error<R::Error>> {
    let sectors = len / SECTOR;
    let state_sectors = (STATE_HEADER + RECORD * sectors).div_ceil(SECTOR);
    let state_at = (sectors - 1 - 2 * state_sectors) * SECTOR;

    let position = read_u32(source, state_at)?;
    let moves = read_u32(source, state_at + 8)?;
    if position == u32::MAX {
        return Err(Error::NotFormatted);
    }
    if moves != 0 {
        return Err(Error::Unsupported);
    }
    let mut dummy = position;

    let mut record = [0u8; RECORD as usize];
    for index in 0..sectors {
        source
            .read_at(state_at + STATE_HEADER + index * RECORD, &mut record)
            .map_err(Error::Source)?;
        if record.iter().all(|&byte| byte == 0xFF) {
            break;
        }
        dummy += 1;
    }
    Ok(dummy)
}

/// Reads `out.len()` bytes at `offset` in the volume, which wear levelling
/// scatters across the partition a sector at a time.
fn read_logical<R: ReadAt>(
    source: &mut R,
    dummy: u32,
    offset: u32,
    out: &mut [u8],
) -> Result<(), R::Error> {
    let mut done = 0;
    while done < out.len() {
        let at = offset + done as u32;
        let within = at % SECTOR;
        let chunk = (SECTOR - within).min((out.len() - done) as u32) as usize;
        let logical = at / SECTOR;
        let physical = if logical < dummy {
            logical
        } else {
            logical + 1
        };
        source.read_at(physical * SECTOR + within, &mut out[done..done + chunk])?;
        done += chunk;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Dir {
    Root,
    Cluster(u32),
}

/// `name` as the 11 bytes a directory entry holds: eight of name and three of
/// extension, space padded.
fn short_name(name: &str) -> [u8; 11] {
    let (base, extension) = name.rsplit_once('.').unwrap_or((name, ""));
    let mut short = [b' '; 11];
    short[..base.len()].copy_from_slice(base.as_bytes());
    short[8..8 + extension.len()].copy_from_slice(extension.as_bytes());
    short
}

#[cfg(test)]
mod fixture;

#[cfg(test)]
mod tests {
    use crate::fixture::{
        fragmented, image, image_formatted, image_moved_to, Format, Image, STATE_AT,
    };
    use crate::{Assets, Error};

    #[test]
    fn a_file_in_a_subdirectory_is_read() {
        // Given
        let flash = image(&[("CERT/client.der", b"certificate bytes")], 0);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 32];

        // When
        let len = assets.read_file("CERT/client.der", &mut out).unwrap();

        // Then
        assert_eq!(&out[..len], b"certificate bytes");
    }

    #[test]
    fn sectors_before_the_dummy_sector_are_read_where_they_are() {
        // Given
        let flash = image(&[("CERT/client.der", b"certificate bytes")], 16);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 32];

        // When
        let len = assets.read_file("CERT/client.der", &mut out).unwrap();

        // Then
        assert_eq!(&out[..len], b"certificate bytes");
    }

    #[test]
    fn a_file_whose_clusters_are_apart_is_read_across_the_dummy_sector() {
        // Given
        let contents: std::vec::Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        let flash = fragmented("DATA.BIN", &contents, 8);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 9000];

        // When
        let len = assets.read_file("DATA.BIN", &mut out).unwrap();

        // Then
        assert!(out[..len] == contents[..], "the file read back differs");
    }

    #[test]
    fn a_file_that_is_not_there_is_not_found() {
        // Given
        let flash = image(&[("CERT/client.der", b"certificate bytes")], 16);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 32];

        // When
        let result = assets.read_file("CERT/private.der", &mut out);

        // Then
        assert_eq!(result, Err(Error::NotFound));
    }

    #[test]
    fn a_buffer_smaller_than_the_file_is_refused_rather_than_filled_short() {
        // Given
        let flash = image(&[("CERT/client.der", b"certificate bytes")], 16);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 8];

        // When
        let result = assets.read_file("CERT/client.der", &mut out);

        // Then
        assert_eq!(result, Err(Error::TooSmall));
    }

    #[test]
    fn a_blank_partition_is_not_a_volume() {
        // Given
        let flash = std::vec![0xFF; 352 * 4096];

        // When
        let result = Assets::open(Image(&flash), flash.len() as u32);

        // Then
        assert_eq!(result.err(), Some(Error::NotFormatted));
    }

    #[test]
    fn a_state_that_has_rotated_is_refused_rather_than_read_with_a_guess() {
        // Given
        let mut flash = image(&[("CERT/client.der", b"certificate bytes")], 16);
        flash[STATE_AT + 8..STATE_AT + 12].copy_from_slice(&1u32.to_le_bytes());

        // When
        let result = Assets::open(Image(&flash), flash.len() as u32);

        // Then
        assert_eq!(result.err(), Some(Error::Unsupported));
    }

    #[test]
    fn names_match_whatever_their_case() {
        // Given
        let flash = image(&[("CERT/client.der", b"certificate bytes")], 16);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 32];

        // When
        let len = assets.read_file("cert/CLIENT.DER", &mut out).unwrap();

        // Then
        assert_eq!(&out[..len], b"certificate bytes");
    }

    #[test]
    fn a_chain_that_runs_through_odd_and_even_clusters_is_followed() {
        // Given
        let contents: std::vec::Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        let flash = image(&[("A.BIN", b"x"), ("DATA.BIN", &contents)], 16);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 9000];

        // When
        let len = assets.read_file("DATA.BIN", &mut out).unwrap();

        // Then
        assert!(out[..len] == contents[..], "the file read back differs");
    }

    #[test]
    fn a_volume_with_two_fats_and_two_sector_clusters_is_read_even_where_the_dummy_sector_splits_a_cluster(
    ) {
        // Given
        let contents: std::vec::Vec<u8> = (0..22000u32).map(|i| (i % 251) as u8).collect();
        let format = Format {
            fats: 2,
            sectors_per_cluster: 2,
        };
        let flash = image_formatted(&[("DIR/DATA.BIN", &contents)], 8, format);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 22000];

        // When
        let len = assets.read_file("DIR/DATA.BIN", &mut out).unwrap();

        // Then
        assert!(out[..len] == contents[..], "the file read back differs");
    }

    #[test]
    fn a_boot_sector_that_does_not_describe_a_volume_is_refused() {
        // Given: each field the volume cannot do without, zeroed in turn
        let good = image(&[("A.BIN", b"x")], 16);
        // With sixteen moves recorded the boot sector is at the start.
        for (name, at, len) in [
            ("bytes per sector", 11, 2),
            ("sectors per cluster", 13, 1),
            ("fats", 16, 1),
            ("root entries", 17, 2),
        ] {
            let mut flash = good.clone();
            flash[at..at + len].fill(0);

            // When
            let result = Assets::open(Image(&flash), flash.len() as u32);

            // Then
            assert_eq!(result.err(), Some(Error::NotFormatted), "{name} zeroed");
        }
    }

    #[test]
    fn the_headers_position_counts_as_well_as_the_records_after_it() {
        // Given
        let flash = image_moved_to(&[("CERT/client.der", b"certificate bytes")], 10, 6);
        let mut assets = Assets::open(Image(&flash), flash.len() as u32).unwrap();
        let mut out = [0u8; 32];

        // When
        let len = assets.read_file("CERT/client.der", &mut out).unwrap();

        // Then
        assert_eq!(&out[..len], b"certificate bytes");
    }

    #[test]
    fn a_boot_sector_whose_tables_run_past_the_partition_is_refused() {
        // Given: a FAT claiming 65535 sectors
        let mut flash = image(&[("A.BIN", b"x")], 16);
        flash[22..24].copy_from_slice(&0xFFFFu16.to_le_bytes());

        // When
        let result = Assets::open(Image(&flash), flash.len() as u32);

        // Then
        assert_eq!(result.err(), Some(Error::NotFormatted));
    }

    #[test]
    fn a_read_that_starts_mid_sector_follows_the_sectors_across_the_dummy_sector() {
        // Given: every physical sector is filled with its own number
        let mut flash = std::vec![0u8; 8 * 4096];
        for (number, sector) in flash.chunks_mut(4096).enumerate() {
            sector.fill(number as u8);
        }
        let mut out = [0u8; 8];

        // When: logical sectors 4 and 5 sit either side of the dummy sector
        super::read_logical(&mut Image(&flash), 5, 5 * 4096 - 4, &mut out).unwrap();

        // Then
        assert_eq!(out, [4, 4, 4, 4, 6, 6, 6, 6]);
    }
}
