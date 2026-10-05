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

fn buffer(len: usize) -> Vec<Block> {
    vec![Block::new(); len]
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
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);

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
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    let mut read = [Block::new()];

    // When
    batched.read(&mut read, BlockIdx(7)).unwrap();

    // Then
    assert_eq!(read[0].contents[0], 0x07);
}

#[test]
fn the_device_size_is_passed_through() {
    // Given
    let device = Recorder::with_blocks(1000);
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);

    // When
    let size = batched.num_blocks().unwrap();

    // Then
    assert_eq!(size, BlockCount(1000));
}

#[test]
fn consecutive_writes_in_a_batch_reach_the_device_as_one_write() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();

    // When
    batched.write(&[tagged(1)], BlockIdx(20)).unwrap();
    batched.write(&[tagged(2)], BlockIdx(21)).unwrap();
    batched.write(&[tagged(3)], BlockIdx(22)).unwrap();
    batched.finish_batch().unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 20,
            tags: vec![1, 2, 3]
        }]
    );
}

#[test]
fn a_full_run_is_written_when_it_fills() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(2);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();

    // When
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    batched.write(&[tagged(2)], BlockIdx(6)).unwrap();
    batched.write(&[tagged(3)], BlockIdx(7)).unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 5,
            tags: vec![1, 2]
        }]
    );
}

#[test]
fn a_write_that_does_not_follow_the_run_writes_the_run_first() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    batched.write(&[tagged(2)], BlockIdx(6)).unwrap();

    // When
    batched.write(&[tagged(3)], BlockIdx(40)).unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 5,
            tags: vec![1, 2]
        }]
    );
}

#[test]
fn a_rewritten_block_replaces_the_held_one() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    batched.write(&[tagged(2)], BlockIdx(6)).unwrap();

    // When
    batched.write(&[tagged(9)], BlockIdx(5)).unwrap();
    batched.finish_batch().unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 5,
            tags: vec![9, 2]
        }]
    );
}

#[test]
fn a_read_overlapping_the_run_sees_the_held_bytes() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    batched.write(&[tagged(2)], BlockIdx(6)).unwrap();
    let mut read = [Block::new()];

    // When
    batched.read(&mut read, BlockIdx(6)).unwrap();

    // Then
    assert_eq!(read[0].contents[0], 2);
}

#[test]
fn a_read_elsewhere_leaves_the_run_held() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    batched.write(&[tagged(2)], BlockIdx(6)).unwrap();
    let mut read = [Block::new()];

    // When
    batched.read(&mut read, BlockIdx(100)).unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Read {
            start: 100,
            count: 1
        }]
    );
}

#[test]
fn finishing_with_nothing_held_does_not_touch_the_device() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();

    // When
    let finished = batched.finish_batch();

    // Then
    assert_eq!((finished, device.calls.borrow().len()), (Ok(()), 0));
}

#[test]
fn a_failed_run_write_is_reported() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    device.fail_writes.set(true);

    // When
    let finished = batched.finish_batch();

    // Then
    assert_eq!(finished, Err(Fault));
}

#[test]
fn a_failed_run_is_not_sent_again() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    device.fail_writes.set(true);
    let _ = batched.finish_batch();
    device.fail_writes.set(false);

    // When
    batched.finish_batch().unwrap();

    // Then
    assert!(device.calls.borrow().is_empty());
}

#[test]
fn writes_after_a_finished_batch_go_straight_through() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.finish_batch().unwrap();

    // When
    batched.write(&[tagged(7)], BlockIdx(50)).unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 50,
            tags: vec![7]
        }]
    );
}

#[test]
fn a_multi_block_write_in_a_batch_is_held_in_order() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();

    // When
    batched
        .write(&[tagged(1), tagged(2), tagged(3)], BlockIdx(30))
        .unwrap();
    batched.finish_batch().unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 30,
            tags: vec![1, 2, 3]
        }]
    );
}

#[test]
fn a_write_overlapping_the_run_end_replaces_and_extends_it() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched
        .write(&[tagged(1), tagged(2)], BlockIdx(30))
        .unwrap();

    // When
    batched
        .write(&[tagged(8), tagged(9)], BlockIdx(31))
        .unwrap();
    batched.finish_batch().unwrap();

    // Then
    assert_eq!(
        *device.calls.borrow(),
        [Call::Write {
            start: 30,
            tags: vec![1, 8, 9]
        }]
    );
}

#[test]
fn a_read_that_starts_before_the_run_sees_the_held_bytes() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(5)], BlockIdx(31)).unwrap();
    let mut read = [Block::new(), Block::new()];

    // When
    batched.read(&mut read, BlockIdx(30)).unwrap();

    // Then
    assert_eq!(read[1].contents[0], 5);
}

#[test]
fn a_read_that_needs_a_failed_run_write_returns_the_error_without_reading() {
    // Given
    let device = Recorder::default();
    let mut run = buffer(4);
    let batched = BatchedWrites::new(&device, &mut run);
    batched.start_batch();
    batched.write(&[tagged(1)], BlockIdx(5)).unwrap();
    device.fail_writes.set(true);
    let mut read = [Block::new()];

    // When
    let result = batched.read(&mut read, BlockIdx(5));

    // Then
    assert_eq!((result, device.calls.borrow().len()), (Err(Fault), 0));
}
