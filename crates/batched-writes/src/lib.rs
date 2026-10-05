#![no_std]

//! Batches consecutive block writes into one write to the device.
//!
//! A filesystem writes one block per call. An SD card programs a run of
//! consecutive blocks sent as one multi-block write far faster than the same
//! blocks sent one at a time, so [`BatchedWrites`] holds consecutive writes
//! between [`BatchedWrites::start_batch`] and [`BatchedWrites::finish_batch`]
//! and sends them together.
//!
//! **Nothing is held once a batch finishes.** A caller that finishes the
//! batch before recording a file's new length can lose power at any moment
//! without the length ever covering blocks that are not on the device.
//!
//! The caller provides the buffer the run is held in, so wrapping a device
//! never builds a large value on the stack.

use core::cell::{Cell, RefCell};

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

/// A block device that holds consecutive writes during a batch and passes
/// everything else straight through to the device it wraps.
///
/// Inside a batch, a write that continues the held run is appended, a write
/// to a block already held replaces it, and any other write first sends the
/// held run. A read that overlaps the held run sends it first, so reads see
/// what was written. A run that fills the buffer is sent at once.
pub struct BatchedWrites<'b, D: BlockDevice> {
    device: D,
    held: RefCell<&'b mut [Block]>,
    held_len: Cell<usize>,
    held_start: Cell<u32>,
    batching: Cell<bool>,
}

impl<'b, D: BlockDevice> BatchedWrites<'b, D> {
    /// Wraps `device`, holding at most `buffer.len()` blocks in a run.
    ///
    /// `buffer` must hold at least one block.
    pub fn new(device: D, buffer: &'b mut [Block]) -> Self {
        Self {
            device,
            held: RefCell::new(buffer),
            held_len: Cell::new(0),
            held_start: Cell::new(0),
            batching: Cell::new(false),
        }
    }

    /// Starts holding consecutive writes.
    pub fn start_batch(&self) {
        self.batching.set(true);
    }

    /// Sends whatever is held and stops holding writes.
    ///
    /// On an error the held blocks are dropped: the caller decides what to
    /// write again.
    pub fn finish_batch(&self) -> Result<(), D::Error> {
        self.batching.set(false);
        self.write_held(&self.held.borrow())
    }

    /// The index just past the held run.
    fn held_end(&self) -> u32 {
        self.held_start.get() + self.held_len.get() as u32
    }

    fn write_held(&self, held: &[Block]) -> Result<(), D::Error> {
        if self.held_len.get() == 0 {
            return Ok(());
        }
        let result = self.device.write(
            &held[..self.held_len.get()],
            BlockIdx(self.held_start.get()),
        );
        self.held_len.set(0);
        result
    }
}

impl<D: BlockDevice> BlockDevice for BatchedWrites<'_, D> {
    type Error = D::Error;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        let read_end = start.0 + blocks.len() as u32;
        if start.0 < self.held_end() && self.held_start.get() < read_end {
            self.write_held(&self.held.borrow())?;
        }
        self.device.read(blocks, start)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
        if !self.batching.get() {
            return self.device.write(blocks, start);
        }
        let mut held = self.held.borrow_mut();
        for (index, block) in (start.0..).zip(blocks) {
            if (self.held_start.get()..self.held_end()).contains(&index) {
                held[(index - self.held_start.get()) as usize] = block.clone();
                continue;
            }
            if index != self.held_end() {
                self.write_held(&held)?;
            }
            let len = self.held_len.get();
            if len == 0 {
                self.held_start.set(index);
            }
            held[len] = block.clone();
            self.held_len.set(len + 1);
            if len + 1 == held.len() {
                self.write_held(&held)?;
            }
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.device.num_blocks()
    }
}

#[cfg(test)]
mod tests;
