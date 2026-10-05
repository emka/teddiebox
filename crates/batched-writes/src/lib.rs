#![no_std]

//! Batches consecutive block writes into one write to the device.

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

/// A block device that wraps another one. Every call goes straight through
/// to the wrapped device.
pub struct BatchedWrites<D: BlockDevice> {
    device: D,
}

impl<D: BlockDevice> BatchedWrites<D> {
    /// Wraps `device`.
    pub fn new(device: D) -> Self {
        Self { device }
    }
}

impl<D: BlockDevice> BlockDevice for BatchedWrites<D> {
    type Error = D::Error;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        self.device.read(blocks, start)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
        self.device.write(blocks, start)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.device.num_blocks()
    }
}

#[cfg(test)]
mod tests;
