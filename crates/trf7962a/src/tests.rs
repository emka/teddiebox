extern crate std;
use std::{vec, vec::Vec};

use super::*;
use embedded_hal_mock::eh1::delay::{CheckedDelay, NoopDelay, Transaction as DelayTransaction};
use embedded_hal_mock::eh1::digital::{
    Mock as PinMock, State as PinState, Transaction as PinTransaction,
};
use embedded_hal_mock::eh1::spi::{Mock as SpiMock, Transaction};

/// A reader with no interrupt line and no clock, for the register-level tests
/// that never wait for a tag.
fn reader(spi: &[Transaction<u8>]) -> Trf7962a<SpiMock<u8>, NoopDelay, PinMock> {
    Trf7962a::new(SpiMock::new(spi), NoopDelay, PinMock::new(&[]))
}

fn check(reader: Trf7962a<SpiMock<u8>, NoopDelay, PinMock>) {
    let (mut spi, _, mut irq) = reader.release();
    spi.done();
    irq.done();
}

/// One SPI transaction carrying `bytes`.
fn spi_write(bytes: Vec<u8>) -> Vec<Transaction<u8>> {
    vec![
        Transaction::transaction_start(),
        Transaction::write_vec(bytes),
        Transaction::transaction_end(),
    ]
}

/// One SPI transaction reading a register.
///
/// Two bytes are clocked: the reader cannot answer during the address
/// byte, so the value only appears on the second.
fn spi_read(address: u8, value: u8) -> Vec<Transaction<u8>> {
    vec![
        Transaction::transaction_start(),
        Transaction::transfer(vec![address, 0x00], vec![0x00, value]),
        Transaction::transaction_end(),
    ]
}

/// The IRQ line going high on the `n`th poll.
///
/// The poll count is a tuning policy rather than a bus encoding, so sharing
/// the constant with the driver is safe here — retuning the window should not
/// mean editing a test, and no datasheet transcription error can hide in it.
fn irq_after(n: usize) -> Vec<PinTransaction> {
    let mut t = vec![PinTransaction::get(PinState::Low); n];
    t.push(PinTransaction::get(PinState::High));
    t
}

fn irq_never() -> Vec<PinTransaction> {
    vec![PinTransaction::get(PinState::Low); IRQ_POLL_ATTEMPTS as usize]
}

fn polls(n: usize) -> Vec<DelayTransaction> {
    vec![DelayTransaction::delay_us(IRQ_POLL_INTERVAL_US); n]
}

/// Builds the SPI transactions for one transceive that a tag answers.
///
/// `tx_length` is the two-register length field, stated literally by the
/// caller rather than recomputed here. Recomputing it would mean a wrong
/// length encoding agreed with itself and passed — the encoding is the
/// thing under test, so the test has to spell it out.
/// `fifo_status` is likewise the raw status byte the reader returns, so a
/// caller can set the overflow flag independently of the byte count. Per
/// SLOS757C Table 6-21 the count sits in B3-B0 and reads as N-1, so a
/// ten-byte reply is the literal 9.
fn transceive_transactions(
    request: &[u8],
    tx_length: [u8; 2],
    fifo_status: u8,
    response: &[u8],
) -> Vec<Transaction<u8>> {
    let mut t = transmit_transactions(request, tx_length);
    t.extend(spi_read(0x4C, 0x00)); // IRQ status, read to clear
    t.extend(spi_read(0x5C, fifo_status)); // FIFO status, read bit set
    for &b in response {
        t.extend(spi_read(0x5F, b)); // FIFO, read bit set
    }
    t
}

/// Everything up to and including the transmit command — all that happens
/// when nothing answers.
fn transmit_transactions(request: &[u8], tx_length: [u8; 2]) -> Vec<Transaction<u8>> {
    let mut t = Vec::new();
    t.extend(spi_write(vec![0x8F])); // command: reset FIFO
    t.extend(spi_write(vec![0x1D, tx_length[0]])); // TX length, high nibbles
    t.extend(spi_write(vec![0x1E, tx_length[1]])); // TX length, low nibble
    for &b in request {
        t.extend(spi_write(vec![0x1F, b])); // byte into the FIFO
    }
    t.extend(spi_write(vec![0x91])); // command: transmit with CRC
    t
}

/// GET RANDOM NUMBER, spelled out rather than taken from `slix`.
const GET_RANDOM_NUMBER: [u8; 3] = [0x22, 0xB2, 0x04];
/// SET PASSWORD for privacy, password 0 masked with random number 0xABCD.
const SET_PASSWORD_0: [u8; 8] = [0x22, 0xB3, 0x04, 0x04, 0xCD, 0xAB, 0xCD, 0xAB];
/// Single-slot inventory: high data rate, inventory, one slot.
const INVENTORY: [u8; 3] = [0x26, 0x01, 0x00];

#[test]
fn a_register_write_sends_the_bare_address() {
    let mut r = reader(&spi_write(vec![0x01, 0x02]));
    r.write_register(regs::ISO_CONTROL, 0x02).unwrap();
    check(r);
}

#[test]
fn a_register_read_sets_the_read_bit() {
    // Address 0x01 with the read bit set, then a second byte clocked out so
    // the reader has somewhere to put the value.
    let mut r = reader(&spi_read(0x41, 0x02));
    assert_eq!(r.read_register(regs::ISO_CONTROL).unwrap(), 0x02);
    check(r);
}

#[test]
fn a_direct_command_sets_the_command_bit() {
    let mut r = reader(&spi_write(vec![0x83]));
    r.send_command(regs::cmd::SOFT_INIT).unwrap();
    check(r);
}

/// Every byte `init_iso15693` puts on the wire, written out by hand.
///
/// Deliberately not derived from `INIT_SEQUENCE`: a test that loops over
/// the table under test asserts only that the driver iterates a slice, and
/// cannot tell a correct register map from a shifted one. These literals
/// are what a datasheet gets diffed against.
#[test]
fn initialisation_puts_exactly_this_sequence_on_the_bus() {
    let mut spi = Vec::new();
    spi.extend(spi_write(vec![0x83])); // command: soft init
    spi.extend(spi_write(vec![0x80])); // command: idle
    spi.extend(spi_write(vec![0x01, 0x02])); // ISO control: 15693 high rate
    spi.extend(spi_write(vec![0x00, 0x21])); // chip status: RF on, last

    // The reader needs to settle after a soft init before it takes
    // configuration, so both commands are followed by a wait.
    let delay = [
        DelayTransaction::delay_ms(SOFT_INIT_SETTLE_MS),
        DelayTransaction::delay_ms(SOFT_INIT_SETTLE_MS),
    ];

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&[]),
    );
    r.init_iso15693().unwrap();
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn inventory_waits_for_the_reader_before_reading_the_fifo() {
    // Response: flags, DSFID, then the UID least-significant byte first.
    let response = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    // Three bytes: the 12-bit length field splits as 0x00 / 0x30.
    let spi = transceive_transactions(&INVENTORY, [0x00, 0x30], 9, &response);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        // Two polls come back low before the exchange completes.
        CheckedDelay::new(&polls(2)),
        PinMock::new(&irq_after(2)),
    );

    let uid = r.inventory().unwrap().expect("a tag answered");
    // Reported most-significant byte first, the order printed on a figure.
    assert_eq!(uid, [1, 2, 3, 4, 5, 6, 7, 8]);
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn an_empty_plate_reports_no_tag_without_reading_the_fifo() {
    // Nothing answers, so the interrupt never comes. Reading the FIFO anyway
    // is what made a figure on the plate look like an empty one.
    let spi = transmit_transactions(&INVENTORY, [0x00, 0x30]);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );

    assert_eq!(
        r.inventory().unwrap(),
        None,
        "an unanswered exchange means no tag, or a tag still in privacy mode"
    );
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn an_overflowed_fifo_is_an_error_rather_than_a_byte_count() {
    // Bit 7 is the overflow flag; the count is bits 6:0. Taking the raw
    // byte as a count reads 0x8A as 138 bytes available and fabricates a
    // UID out of whatever the FIFO returns.
    let mut spi = transmit_transactions(&INVENTORY, [0x00, 0x30]);
    spi.extend(spi_read(0x4C, 0x00));
    spi.extend(spi_read(0x5C, 0x1A));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq_after(0)),
    );
    assert_eq!(r.inventory(), Err(Error::FifoOverflow));
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// B6 and B5 are the FIFO level flags, not part of the count. Masking them
/// in turns a ten-byte reply into a claim of 105 bytes.
#[test]
fn the_fifo_level_flags_are_not_counted_as_received_bytes() {
    let response = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    // 0x69: level-high, level-low, and a count nibble of 9 meaning ten bytes.
    let spi = transceive_transactions(&INVENTORY, [0x00, 0x30], 0x69, &response);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq_after(0)),
    );
    assert!(r.inventory().unwrap().is_some());
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn unlocking_fetches_a_random_number_then_sends_the_masked_password() {
    let random_response = [0x00u8, 0xCD, 0xAB]; // flags, then RN low, high
    let mut spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 2, &random_response);
    // Eight bytes: the length field splits as 0x00 / 0x80.
    spi.extend(transceive_transactions(
        &SET_PASSWORD_0,
        [0x00, 0x80],
        0,
        &[0x00],
    ));

    let mut irq = irq_after(0);
    irq.extend(irq_after(0));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq),
    );
    r.unlock_privacy(0).unwrap();
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn a_rejected_password_is_reported_rather_than_read_as_success() {
    // ISO 15693-3 §7.4: bit 0 of the response flags means the payload is
    // an error code. A SLIX refusing the privacy password answers
    // [0x01, 0x0F] — two bytes, so a bare length check calls it a success.
    let mut spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 2, &[0x00, 0xCD, 0xAB]);
    spi.extend(transceive_transactions(
        &SET_PASSWORD_0,
        [0x00, 0x80],
        1,
        &[0x01, 0x0F],
    ));

    let mut irq = irq_after(0);
    irq.extend(irq_after(0));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq),
    );
    assert_eq!(
        r.unlock_privacy(0),
        Err(Error::TagError(0x0F)),
        "a wrong password must not look like an unlocked tag"
    );
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn an_error_response_is_not_mistaken_for_a_random_number() {
    let spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 1, &[0x01, 0x03]);
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq_after(0)),
    );
    assert_eq!(r.get_random_number(), Err(Error::TagError(0x03)));
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn unlocking_fails_cleanly_when_no_tag_answers() {
    let spi = transmit_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30]);
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );
    assert_eq!(r.unlock_privacy(0), Err(Error::Timeout));
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

#[test]
fn the_field_is_turned_on_last() {
    let last = INIT_SEQUENCE.last().unwrap();
    assert_eq!(
        last.0,
        regs::CHIP_STATUS_CONTROL,
        "enabling the field before the protocol is configured radiates noise"
    );
}
