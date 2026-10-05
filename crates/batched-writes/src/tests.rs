extern crate std;

use core::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::vec;
use std::vec::Vec;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

use crate::BatchedWrites;

/// What the device was asked to do. A write records the tag of each block.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Write { start: u32, tags: Vec<u8> },
    Read { start: u32, count: usize },
}

#[derive(Debug, PartialEq, Eq)]
struct Fault;

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the device failed")
    }
}

impl core::error::Error for Fault {}

/// A device that records every call and remembers each block's tag.
#[derive(Default)]
struct Recorder {
    calls: RefCell<Vec<Call>>,
    tags: RefCell<BTreeMap<u32, u8>>,
    blocks: u32,
    fail_writes: Cell<bool>,
}

impl Recorder {
    fn with_blocks(blocks: u32) -> Self {
        Self {
            blocks,
            ..Self::default()
        }
    }

    fn holding(block: u32, tag: u8) -> Self {
        let recorder = Self::default();
        recorder.tags.borrow_mut().insert(block, tag);
        recorder
    }
}

impl BlockDevice for &Recorder {
    type Error = Fault;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Fault> {
        self.calls.borrow_mut().push(Call::Read {
            start: start.0,
            count: blocks.len(),
        });
        let tags = self.tags.borrow();
        for (offset, block) in (0u32..).zip(blocks.iter_mut()) {
            let tag = tags.get(&(start.0 + offset)).copied().unwrap_or(0);
            block.contents.fill(tag);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Fault> {
        if self.fail_writes.get() {
            return Err(Fault);
        }
        self.calls.borrow_mut().push(Call::Write {
            start: start.0,
            tags: blocks.iter().map(|b| b.contents[0]).collect(),
        });
        let mut tags = self.tags.borrow_mut();
        for (offset, block) in (0u32..).zip(blocks) {
            tags.insert(start.0 + offset, block.contents[0]);
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Fault> {
        Ok(BlockCount(self.blocks))
    }
}

/// A block whose every byte is `tag`.
fn tagged(tag: u8) -> Block {
    let mut block = Block::new();
    block.contents.fill(tag);
    block
}

#[test]
fn a_write_outside_a_batch_reaches_the_device_at_once() {
    // Given
    let device = Recorder::default();
    let batched = BatchedWrites::new(&device);

    // When
    batched.write(&[tagged(0xA1)], BlockIdx(10)).unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 10,
            tags: vec![0xA1]
        }]
    );
}

#[test]
fn a_read_outside_a_batch_reads_the_device() {
    // Given
    let device = Recorder::holding(7, 0x07);
    let batched = BatchedWrites::new(&device);
    let mut read = [Block::new()];

    // When
    batched.read(&mut read, BlockIdx(7)).unwrap();

    // Then
    assert_eq!(read[0].contents[0], 0x07);
    assert_eq!(*device.calls.borrow(), [Call::Read { start: 7, count: 1 }]);
}

#[test]
fn the_device_size_is_passed_through() {
    // Given
    let device = Recorder::with_blocks(1000);
    let batched = BatchedWrites::new(&device);

    // When
    let size = batched.num_blocks().unwrap();

    // Then
    assert_eq!(size, BlockCount(1000));
}
