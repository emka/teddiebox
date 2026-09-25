//! An SD card in memory, to test the project's assumptions about
//! `embedded-sdmmc` without a real card.
//!
//! Host-only, test support only. Nothing here ships.

use core::cell::RefCell;
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

#[derive(Debug)]
pub struct RamDiskError;

impl core::fmt::Display for RamDiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("read or write outside the disk")
    }
}

impl core::error::Error for RamDiskError {}

/// `BlockDevice` takes `&self`, so the bytes live behind a `RefCell`.
pub struct RamDisk {
    bytes: RefCell<Vec<u8>>,
}

impl RamDisk {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: RefCell::new(bytes),
        }
    }

    pub fn of_size(megabytes: usize) -> Self {
        Self::new(vec![0u8; megabytes * 1024 * 1024])
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes.into_inner()
    }
}

impl BlockDevice for RamDisk {
    type Error = RamDiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        let bytes = self.bytes.borrow();
        for (n, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + n) * Block::LEN;
            let end = at + Block::LEN;
            if end > bytes.len() {
                return Err(RamDiskError);
            }
            block.contents.copy_from_slice(&bytes[at..end]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
        let mut bytes = self.bytes.borrow_mut();
        for (n, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + n) * Block::LEN;
            let end = at + Block::LEN;
            if end > bytes.len() {
                return Err(RamDiskError);
            }
            bytes[at..end].copy_from_slice(&block.contents);
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        Ok(BlockCount((self.bytes.borrow().len() / Block::LEN) as u32))
    }
}
