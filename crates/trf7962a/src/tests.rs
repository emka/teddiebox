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

/// SLOS757C: reading the interrupt status register over SPI needs the
/// continuous-address bit set and a dummy read of the next register, "because
/// the reader's IRQ Status register needs an additional clock cycle to clear
/// the register". A plain single-byte read leaves it uncleared, and the
/// address byte is 0x6C rather than 0x4C.
///
/// Literal bytes, from the datasheet's own step-by-step procedure. The driver
/// read this register the ordinary way for its whole life, and at the bench
/// that reads as a reader which never raises an interrupt at all.
#[test]
fn the_interrupt_status_is_read_with_a_dummy_byte() {
    let mut trf = reader(&[
        Transaction::transaction_start(),
        Transaction::transfer(vec![0x6C, 0x00, 0x00], vec![0x00, 0x80, 0x00]),
        Transaction::transaction_end(),
    ]);
    assert_eq!(trf.read_irq_status(), Ok(0x80));
    check(trf);
}

/// The interrupt status read, which is not an ordinary register read: the
/// continuous-address bit is set and a dummy byte follows. See
/// `the_interrupt_status_is_read_with_a_dummy_byte`.
fn spi_read_irq(value: u8) -> Vec<Transaction<u8>> {
    vec![
        Transaction::transaction_start(),
        Transaction::transfer(vec![0x6C, 0x00, 0x00], vec![0x00, value, 0x00]),
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
    // Two interrupts, not one: the transmit finishing (0x80), then the tag's
    // answer (0x40), with the FIFO reset between them. Asserted byte for byte
    // by `the_transmit_interrupt_is_not_mistaken_for_the_tags_reply`.
    t.extend(spi_read_irq(0x80));
    t.extend(spi_write(vec![0x8F]));
    t.extend(spi_read_irq(0x40));
    t.extend(spi_read(0x5C, fifo_status)); // FIFO status, read bit set
    t.extend(fifo_burst(response));
    t
}

/// The continuous read that empties the FIFO — SLOS757G Figure 6-23.
///
/// Asserted byte for byte by `the_fifo_is_read_as_one_continuous_burst`; this
/// builds the same burst for the tests that care about what it returns.
fn fifo_burst(response: &[u8]) -> Vec<Transaction<u8>> {
    let mut out = vec![0x7Fu8];
    out.extend(core::iter::repeat_n(0x00, response.len()));
    let mut back = vec![0x00u8];
    back.extend_from_slice(response);
    vec![
        Transaction::transaction_start(),
        Transaction::transfer(out, back),
        Transaction::transaction_end(),
    ]
}

/// The interrupt line through one exchange that a tag answers: the transmit's
/// own interrupt, then the tag's.
fn irq_exchange() -> Vec<PinTransaction> {
    let mut t = irq_after(0);
    t.extend(irq_after(0));
    t
}

/// Everything up to and including the transmit command — all that happens
/// when nothing answers.
/// The transmit sequence, as one slave-select window — SLOS757G Figure 6-20.
///
/// The shape is asserted byte for byte by
/// `a_transmit_is_one_burst_exactly_as_the_datasheet_shows_it`; this only
/// builds the same burst for the tests that care about what comes after it.
fn transmit_transactions(request: &[u8], tx_length: [u8; 2]) -> Vec<Transaction<u8>> {
    let mut burst = vec![
        0x8F, // command: reset FIFO
        0x91, // command: transmit with CRC
        0x3D, // continuous write, starting at the TX length register 0x1D
        tx_length[0],
        tx_length[1],
    ];
    burst.extend_from_slice(request);
    spi_write(burst)
}

/// GET RANDOM NUMBER, spelled out rather than taken from `slix`.
const GET_RANDOM_NUMBER: [u8; 3] = [0x02, 0xB2, 0x04];
/// SET PASSWORD for privacy, password 0 masked with random number 0xABCD.
const SET_PASSWORD_0: [u8; 8] = [0x02, 0xB3, 0x04, 0x04, 0xCD, 0xAB, 0xCD, 0xAB];
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

    // Two polls come back low before the tag's answer arrives; the transmit's
    // own interrupt is already there when it is first looked for.
    let mut irq = irq_after(0);
    irq.extend(irq_after(2));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(2)),
        PinMock::new(&irq),
    );

    let uid = r.inventory().unwrap().expect("a tag answered");
    // Reported most-significant byte first, the order printed on a figure.
    assert_eq!(uid, [1, 2, 3, 4, 5, 6, 7, 8]);
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// The FIFO holds twelve bytes and this driver loads a request in one go, so
/// a longer one cannot be sent. Silently, it both overran the FIFO and, past
/// 4096 bytes, wrapped the 12-bit length field into a plausible small number.
#[test]
fn a_request_too_large_for_the_fifo_is_refused_before_any_bus_traffic() {
    let mut r = reader(&[]);
    let mut response = [0u8; 16];
    assert_eq!(
        r.transceive(&[0xAA; 13], &mut response),
        Err(Error::RequestTooLong)
    );
    check(r);
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
    spi.extend(spi_read_irq(0x80));
    spi.extend(spi_write(vec![0x8F]));
    spi.extend(spi_read_irq(0x40));
    spi.extend(spi_read(0x5C, 0x1A));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq_exchange()),
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
        PinMock::new(&irq_exchange()),
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

    let mut irq = irq_exchange();
    irq.extend(irq_exchange());

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

    let mut irq = irq_exchange();
    irq.extend(irq_exchange());

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
        PinMock::new(&irq_exchange()),
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

/// The interrupt line is readable without an exchange in flight.
///
/// At a bench the register and the pin disagree in the one case that matters:
/// the reader latched an interrupt the wiring never delivered. Telling those
/// apart needs the level on its own, outside a transceive.
#[test]
fn the_interrupt_line_can_be_read_on_its_own() {
    let mut r = Trf7962a::new(
        SpiMock::new(&[]),
        NoopDelay,
        PinMock::new(&[PinTransaction::get(PinState::High)]),
    );
    assert_eq!(r.irq_asserted(), Ok(true));
    check(r);
}

/// SLOS757G Figure 6-20, byte for byte: the datasheet's own single-slot
/// inventory, in one slave-select window.
///
/// Loading the FIFO one transaction per byte does not reach the FIFO at all.
/// Measured on the reader: after a reset and three single-address writes of
/// 0x26, 0x01, 0x00 the FIFO byte counter still reads 0x00, while a write to
/// the length register 0x1E in the same style reads back correctly. Since
/// "transmission starts automatically after the first byte is written into
/// the FIFO" (§6.12.5), a FIFO that never takes a byte is a transmitter that
/// never starts — which at a bench is a reader that raises no interrupt and
/// looks like an empty plate.
///
/// The bytes are copied from the figure rather than assembled here: 0x8F
/// reset FIFO, 0x91 transmit with CRC, 0x3D continuous write from 0x1D, the
/// two length bytes, then the request.
#[test]
fn a_transmit_is_one_burst_exactly_as_the_datasheet_shows_it() {
    let mut r = Trf7962a::new(
        SpiMock::new(&spi_write(vec![
            0x8F, 0x91, 0x3D, 0x00, 0x30, 0x26, 0x01, 0x00,
        ])),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );
    let mut response = [0u8; MAX_RESPONSE];
    assert_eq!(r.transceive(&INVENTORY, &mut response), Ok(0));
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// SLOS757G §6.12.5 and Figure 6-21: a transmit raises an interrupt of its
/// own, before the tag has said anything.
///
/// "The flag is set at the start of TX but the interrupt request is sent when
/// TX is finished" (Table 6-29, B7). The tag's answer is a *second* interrupt,
/// about 4 ms later, and between the two the datasheet resets the FIFO.
///
/// Measured on the reader with the transmit burst working: the first
/// interrupt arrives, the FIFO status reads 0x00 because the FIFO is empty,
/// and the driver's N-1 rule turns that into a one-byte reply of 0x00. A
/// reader transmitting perfectly well looked exactly like a tag answering
/// with nonsense.
///
/// Bytes spelled out: 0x6C the interrupt status read, 0x80 TX complete, 0x8F
/// reset FIFO, 0x40 RX started, 0x5C the FIFO status read.
#[test]
fn the_transmit_interrupt_is_not_mistaken_for_the_tags_reply() {
    let reply = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    let mut spi = spi_write(vec![0x8F, 0x91, 0x3D, 0x00, 0x30, 0x26, 0x01, 0x00]);
    spi.extend(spi_read_irq(0x80)); // TX finished; the tag has not answered yet
    spi.extend(spi_write(vec![0x8F])); // reset the FIFO before the reception
    spi.extend(spi_read_irq(0x40)); // RX started, and now finished
    spi.extend(spi_read(0x5C, 9)); // ten bytes, counted as N-1
    spi.extend(fifo_burst(&reply));

    let mut irq = irq_after(0);
    irq.extend(irq_after(0));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq),
    );
    assert_eq!(
        r.inventory().unwrap(),
        Some([1, 2, 3, 4, 5, 6, 7, 8]),
        "the reply follows the transmit interrupt, not the transmit itself"
    );
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// A reader that transmits and hears nothing must say so, not read the FIFO.
///
/// The transmit interrupt always arrives, so "an interrupt happened" is not
/// evidence a tag answered — which is the whole reason an empty plate used to
/// come back as a one-byte reply.
#[test]
fn a_transmit_with_no_answer_reports_no_tag() {
    let mut spi = spi_write(vec![0x8F, 0x91, 0x3D, 0x00, 0x30, 0x26, 0x01, 0x00]);
    spi.extend(spi_read_irq(0x80));
    spi.extend(spi_write(vec![0x8F]));

    let mut irq = irq_after(0);
    irq.extend(irq_never());

    let mut delay = polls(0);
    delay.extend(polls(IRQ_POLL_ATTEMPTS as usize));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );
    assert_eq!(r.inventory().unwrap(), None);
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// SLOS757G Figure 6-23: the FIFO is read as one continuous burst.
///
/// The address byte is 0x7F — read bit, continuous bit, FIFO address — sent
/// once, followed by a filler byte per byte wanted, all inside one slave
/// select. The reply arrives one byte behind, as it does for every read.
///
/// Read a byte per transaction instead, the count is right and every byte
/// comes back 0x00. Measured on the board: a GET RANDOM NUMBER answered with
/// exactly three bytes — the right shape, so the N-1 count is sound — and all
/// three were zero, giving a random number of 0x0000 on every run. The FIFO
/// refuses single-address reads the same way it refuses single-address
/// writes.
#[test]
fn the_fifo_is_read_as_one_continuous_burst() {
    let mut spi = spi_write(vec![0x8F, 0x91, 0x3D, 0x00, 0x30, 0x02, 0xB2, 0x04]);
    spi.extend(spi_read_irq(0x80));
    spi.extend(spi_write(vec![0x8F]));
    spi.extend(spi_read_irq(0x40));
    spi.extend(spi_read(0x5C, 0x02)); // three bytes, counted as N-1
    spi.extend(vec![
        Transaction::transaction_start(),
        Transaction::transfer(vec![0x7F, 0x00, 0x00, 0x00], vec![0x00, 0x00, 0xCD, 0xAB]),
        Transaction::transaction_end(),
    ]);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq_exchange()),
    );
    assert_eq!(
        r.get_random_number(),
        Ok(0xABCD),
        "a random number of zero on every run is a FIFO that was never read"
    );
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// A reception the reader could not decode says why.
///
/// SLOS757G Table 6-29 gives four separate reasons — CRC (B4), parity (B3),
/// byte framing or EOF (B2), and collision (B1) — and at a bench they point
/// at quite different things: a collision or a framing error is a reader
/// mistuned for the reply it is getting, while a CRC error is a reply that
/// arrived and was corrupted. Folding them into one "bad response" throws
/// away the only evidence that separates them.
#[test]
fn a_reception_error_carries_the_reader_s_own_reason() {
    let mut spi = spi_write(vec![0x8F, 0x91, 0x3D, 0x00, 0x30, 0x26, 0x01, 0x00]);
    spi.extend(spi_read_irq(0x80));
    spi.extend(spi_write(vec![0x8F]));
    // Reception started, and the reader flagged a collision on it.
    spi.extend(spi_read_irq(0x42));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq_exchange()),
    );
    let mut response = [0u8; MAX_RESPONSE];
    assert_eq!(
        r.transceive(&INVENTORY, &mut response),
        Err(Error::ReceiveError(0x02)),
        "the reason is the reader's own flags, not a verdict on the reply"
    );
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// A frame of five bytes or more interrupts part-way through its own
/// transmission, and that is not the end of it.
///
/// SLOS757G §6.12.5: "if the number of bytes to be transmitted is higher or
/// equal to 5, then the interrupt is generated. This occurs also when the
/// number of bytes in the FIFO reaches 3", so the MCU can load more. This
/// driver preloads the whole request, so there is never more to load and the
/// interrupt is simply not the end of the transmit.
///
/// Measured on the board for the eight-byte SET PASSWORD: 0xA0 at 1.4 ms —
/// transmit in progress, FIFO running low — then 0x80 at 1.6 ms when it
/// actually finished. Taking the first for the end resets the FIFO in the
/// middle of the frame, so the tag receives a truncated request and says
/// nothing at all. Three-byte requests are below the threshold and worked
/// throughout, which is what made this look like a password being refused.
#[test]
fn a_fifo_interrupt_during_a_transmit_is_not_the_end_of_it() {
    let reply = [0x00u8, 0xCD, 0xAB];
    // Eight bytes: the length field splits as 0x00 / 0x80.
    let mut spi = spi_write(vec![
        0x8F, 0x91, 0x3D, 0x00, 0x80, 0x02, 0xB3, 0x04, 0x04, 0xCD, 0xAB, 0xCD, 0xAB,
    ]);
    spi.extend(spi_read_irq(0xA0)); // transmitting still, FIFO down to three
    spi.extend(spi_read_irq(0x80)); // now the transmit is finished
    spi.extend(spi_write(vec![0x8F]));
    spi.extend(spi_read_irq(0x40));
    spi.extend(spi_read(0x5C, 0x02));
    spi.extend(fifo_burst(&reply));

    let mut irq = irq_after(0);
    irq.extend(irq_exchange());

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq),
    );
    let mut response = [0u8; MAX_RESPONSE];
    assert_eq!(
        r.transceive(&SET_PASSWORD_0, &mut response),
        Ok(3),
        "the frame is preloaded, so a request for more data is only noise"
    );
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}

/// A reply longer than eight bytes arrives in two parts.
///
/// SLOS757G §6.12.4: "if the received packet is longer than 8 bytes, the
/// interrupt is sent before the end of the receive operation when the ninth
/// byte is loaded into the FIFO... In the case of an IRQ_FIFO, the MCU should
/// expect either another IRQ_FIFO or RX complete interrupt. This is repeated
/// until an RX complete interrupt is generated." The datasheet's own example
/// reads nine bytes and then collects the tenth, the UID's most significant
/// byte, from a second interrupt 160 µs later (Figures 6-23 and 6-24).
///
/// An inventory reply is exactly ten bytes, so this is every tag read there
/// will ever be. Stopping at the first interrupt returns nine, one short, and
/// the caller rejects a perfectly good tag as a malformed response.
#[test]
fn a_reply_longer_than_the_fifo_warning_is_collected_in_full() {
    let first = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02];
    let last = [0x01u8];

    let mut spi = spi_write(vec![0x8F, 0x91, 0x3D, 0x00, 0x30, 0x26, 0x01, 0x00]);
    spi.extend(spi_read_irq(0x80));
    spi.extend(spi_write(vec![0x8F]));
    // Reception under way, and the FIFO already carrying nine bytes.
    spi.extend(spi_read_irq(0x60));
    spi.extend(spi_read(0x5C, 0x08));
    spi.extend(fifo_burst(&first));
    // Reception complete, with the tenth byte still to collect.
    spi.extend(spi_read_irq(0x40));
    spi.extend(spi_read(0x5C, 0x00));
    spi.extend(fifo_burst(&last));

    let mut irq = irq_after(0);
    irq.extend(irq_exchange());

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(0)),
        PinMock::new(&irq),
    );
    assert_eq!(
        r.inventory().unwrap(),
        Some([1, 2, 3, 4, 5, 6, 7, 8]),
        "the last byte of the UID arrives on its own interrupt"
    );
    let (mut spi, mut delay, mut irq) = r.release();
    spi.done();
    delay.done();
    irq.done();
}
