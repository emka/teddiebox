//! A flash image of an `assets` partition, built the way ESP-IDF lays one out.

use std::io::{Cursor, Write};
use std::vec;
use std::vec::Vec;

use crate::ReadAt;

const SECTOR: usize = 4096;
/// The partition's size in sectors, as in a stock box.
const TOTAL: usize = 352;
/// State sectors for each of the two copies, as in a stock box.
const STATE_SECTORS: usize = 2;
/// Sectors the volume itself spans: everything but the dummy sector, the two
/// state copies and the configuration sector.
const VOLUME_SECTORS: usize = TOTAL - 1 - 2 * STATE_SECTORS - 1;

/// Where the first copy of the wear-levelling state starts.
pub const STATE_AT: usize = (TOTAL - 1 - 2 * STATE_SECTORS) * SECTOR;

/// How the volume is formatted. Stock uses one FAT and one-sector clusters;
/// anything else is here to show the reader follows the volume's own numbers.
#[derive(Clone, Copy)]
pub struct Format {
    pub fats: u8,
    pub sectors_per_cluster: u32,
}

pub const STOCK: Format = Format {
    fats: 1,
    sectors_per_cluster: 1,
};

/// `files` are `(path, contents)`; `records` is how many wear-levelling moves
/// the state has recorded, which is where the dummy sector sits.
pub fn image(files: &[(&str, &[u8])], records: usize) -> Vec<u8> {
    image_formatted(files, records, STOCK)
}

pub fn image_formatted(files: &[(&str, &[u8])], records: usize, format: Format) -> Vec<u8> {
    lay_out(volume(files, format), 0, records)
}

/// As [`image`], with the state's header already saying the dummy sector has
/// moved to `position`, before `records` further moves.
pub fn image_moved_to(files: &[(&str, &[u8])], position: usize, records: usize) -> Vec<u8> {
    lay_out(volume(files, STOCK), position, records)
}

/// One file whose clusters are not next to each other: a second file is
/// created after its first cluster, then the rest of it is appended.
pub fn fragmented(path: &str, contents: &[u8], records: usize) -> Vec<u8> {
    let mut volume = vec![0u8; VOLUME_SECTORS * SECTOR];
    format(&mut volume, STOCK);
    {
        let fs =
            fatfs::FileSystem::new(Cursor::new(&mut volume[..]), fatfs::FsOptions::new()).unwrap();
        let root = fs.root_dir();
        let mut file = root.create_file(path).unwrap();
        file.write_all(&contents[..SECTOR]).unwrap();
        file.flush().unwrap();
        root.create_file("SPACER.BIN")
            .unwrap()
            .write_all(&[0xEE; SECTOR])
            .unwrap();
        file.write_all(&contents[SECTOR..]).unwrap();
    }
    lay_out(volume, 0, records)
}

fn lay_out(volume: Vec<u8>, position: usize, records: usize) -> Vec<u8> {
    let dummy = position + records;
    let mut flash = vec![0xFF; TOTAL * SECTOR];

    for (logical, sector) in volume.chunks(SECTOR).enumerate() {
        let physical = if logical < dummy {
            logical
        } else {
            logical + 1
        };
        flash[physical * SECTOR..][..SECTOR].copy_from_slice(sector);
    }

    for copy in 0..2 {
        let at = STATE_AT + copy * STATE_SECTORS * SECTOR;
        // pos, max_pos, move_count, access_count, max_count, block_size, version
        let header: [u32; 7] = [
            position as u32,
            (VOLUME_SECTORS + 1) as u32,
            0,
            0,
            16,
            SECTOR as u32,
            2,
        ];
        for (i, word) in header.iter().enumerate() {
            flash[at + i * 4..][..4].copy_from_slice(&word.to_le_bytes());
        }
        for record in 0..records {
            flash[at + 64 + record * 16..][..16].fill(0);
        }
    }
    flash
}

fn format(volume: &mut [u8], format: Format) {
    fatfs::format_volume(
        Cursor::new(volume),
        fatfs::FormatVolumeOptions::new()
            .bytes_per_sector(SECTOR as u16)
            .bytes_per_cluster(SECTOR as u32 * format.sectors_per_cluster)
            .fat_type(fatfs::FatType::Fat12)
            .fats(format.fats),
    )
    .unwrap();
}

fn volume(files: &[(&str, &[u8])], format_: Format) -> Vec<u8> {
    let mut volume = vec![0u8; VOLUME_SECTORS * SECTOR];
    format(&mut volume, format_);
    {
        let fs =
            fatfs::FileSystem::new(Cursor::new(&mut volume[..]), fatfs::FsOptions::new()).unwrap();
        for (path, contents) in files {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            let mut at = fs.root_dir();
            for part in dir.split('/').filter(|part| !part.is_empty()) {
                at = at.create_dir(part).unwrap();
            }
            at.create_file(name).unwrap().write_all(contents).unwrap();
        }
    }
    volume
}

/// A flash image as the byte source the reader reads.
pub struct Image<'a>(pub &'a [u8]);

impl ReadAt for Image<'_> {
    type Error = ();

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<(), ()> {
        let from = offset as usize;
        let bytes = self.0.get(from..from + out.len()).ok_or(())?;
        out.copy_from_slice(bytes);
        Ok(())
    }
}
