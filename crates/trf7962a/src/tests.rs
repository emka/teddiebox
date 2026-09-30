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

/// Checks that a reader with a clock used exactly the scripted bus traffic,
/// waits and interrupt polls.
fn finish(reader: Trf7962a<SpiMock<u8>, CheckedDelay, PinMock>) {
    let (mut spi, mut delay, mut irq) = reader.release();
    spi.done();
    delay.done();
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
/// continuous-address bit and a dummy read of the next register, "because the
/// reader's IRQ Status register needs an additional clock cycle to clear the
/// register". So the address byte is 0x6C, not 0x4C.
///
/// Literal bytes from the datasheet's procedure.
#[test]
fn the_interrupt_status_is_read_with_a_dummy_byte() {
    // Given
    let mut r = reader(&[
        Transaction::transaction_start(),
        Transaction::transfer(vec![0x6C, 0x00, 0x00], vec![0x00, 0x80, 0x00]),
        Transaction::transaction_end(),
    ]);

    // When
    let status = r.read_irq_status();

    // Then
    assert_eq!(status, Ok(0x80));
    check(r);
}

/// The interrupt status read: continuous-address bit set, and a dummy byte
/// after. See `the_interrupt_status_is_read_with_a_dummy_byte`.
fn spi_read_irq(value: u8) -> Vec<Transaction<u8>> {
    vec![
        Transaction::transaction_start(),
        Transaction::transfer(vec![0x6C, 0x00, 0x00], vec![0x00, value, 0x00]),
        Transaction::transaction_end(),
    ]
}

/// One SPI transaction reading a register.
///
/// Two bytes are clocked: the value comes back during the second.
fn spi_read(address: u8, value: u8) -> Vec<Transaction<u8>> {
    vec![
        Transaction::transaction_start(),
        Transaction::transfer(vec![address, 0x00], vec![0x00, value]),
        Transaction::transaction_end(),
    ]
}

/// The IRQ line going high on the `n`th poll.
///
/// The poll count is a tuning choice, not a datasheet value, so these helpers
/// may use the driver's constants.
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

/// The delays in one exchange a tag answers: `n` interrupt polls, then the t2
/// quiet time. An unanswered exchange has no t2; see
/// `the_air_is_left_quiet_for_t2_after_a_tag_has_answered`.
fn answered(n: usize) -> Vec<DelayTransaction> {
    let mut t = polls(n);
    t.push(DelayTransaction::delay_us(T2_QUIET_US));
    t
}

/// Builds the SPI transactions for one transceive that a tag answers.
///
/// `tx_length` is the two-register length field, given literally by the
/// caller so the length encoding is really tested. `fifo_status` is the raw
/// status byte, so a caller can set the overflow flag separately. Per
/// SLOS757C Table 6-21 the count is in B3-B0 and reads as N-1, so a ten-byte
/// reply is 9.
fn transceive_transactions(
    request: &[u8],
    tx_length: [u8; 2],
    fifo_status: u8,
    response: &[u8],
) -> Vec<Transaction<u8>> {
    let mut t = transmit_transactions(request, tx_length);
    // Two interrupts: the transmit finishing (0x80), then the tag's answer
    // (0x40), with a FIFO reset between them. See
    // `the_transmit_interrupt_is_not_mistaken_for_the_tags_reply`.
    t.extend(spi_read_irq(0x80));
    t.extend(spi_write(vec![0x8F]));
    t.extend(spi_read_irq(0x40));
    t.extend(spi_read(0x5C, fifo_status)); // FIFO status, read bit set
    t.extend(fifo_burst(response));
    t
}

/// The continuous read that empties the FIFO (SLOS757G Figure 6-23).
///
/// Checked byte for byte by `the_fifo_is_read_as_one_continuous_burst`; this
/// helper builds it for other tests.
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

/// The transmit sequence, as one SPI transaction (SLOS757G Figure 6-20). This
/// is all that happens when nothing answers.
///
/// Checked byte for byte by
/// `a_transmit_is_one_burst_exactly_as_the_datasheet_shows_it`; this helper
/// builds it for other tests.
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
/// With a zero password, byte order does not matter.
const SET_PASSWORD_0: [u8; 8] = [0x02, 0xB3, 0x04, 0x04, 0xCD, 0xAB, 0xCD, 0xAB];
/// Single-slot inventory: high data rate, inventory, one slot.
const INVENTORY: [u8; 3] = [0x26, 0x01, 0x00];

#[test]
fn a_register_write_sends_the_bare_address() {
    // Given
    let mut r = reader(&spi_write(vec![0x01, 0x02]));

    // When
    r.write_register(regs::ISO_CONTROL, 0x02).unwrap();

    // Then: the reader did exactly what was scripted
    check(r);
}

#[test]
fn a_register_read_sets_the_read_bit() {
    // Given
    // Address 0x01 with the read bit set, then a second byte for the value.
    let mut r = reader(&spi_read(0x41, 0x02));

    // When
    let result = r.read_register(regs::ISO_CONTROL).unwrap();

    // Then
    assert_eq!(result, 0x02);
    check(r);
}

#[test]
fn a_direct_command_sets_the_command_bit() {
    // Given
    let mut r = reader(&spi_write(vec![0x83]));

    // When
    r.send_command(regs::cmd::SOFT_INIT).unwrap();

    // Then: the reader did exactly what was scripted
    check(r);
}

/// Every byte `init_iso15693` puts on the wire, written out by hand.
///
/// Not derived from `INIT_SEQUENCE`, so the test can disagree with the code
/// and be checked against the datasheet.
#[test]
fn initialisation_puts_exactly_this_sequence_on_the_bus() {
    // Given
    let mut spi = Vec::new();
    spi.extend(spi_write(vec![0x83])); // command: soft init
    spi.extend(spi_write(vec![0x80])); // command: idle
    spi.extend(spi_write(vec![0x01, 0x02])); // ISO control: 15693 high rate
                                             // Chip status, written last. B0 (`vrs5_3`) clear selects 3-V operation
                                             // (Table 6-16), which §6.4 says to use below 4.3 V. The reader runs at
                                             // 3.3 V.
    spi.extend(spi_write(vec![0x00, 0x20]));

    // The reader needs time to settle after a soft init, so both commands
    // are followed by a wait. The third wait is for the field; see
    // `FIELD_SETTLE_MS`.
    let delay = [
        DelayTransaction::delay_ms(SOFT_INIT_SETTLE_MS),
        DelayTransaction::delay_ms(SOFT_INIT_SETTLE_MS),
        DelayTransaction::delay_ms(FIELD_SETTLE_MS),
    ];

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&[]),
    );

    // When
    r.init_iso15693().unwrap();

    // Then: the reader did exactly what was scripted
    finish(r);
}

#[test]
fn inventory_waits_for_the_reader_before_reading_the_fifo() {
    // Given: the response is flags, DSFID, then the UID least-significant
    // byte first; three request bytes split the 12-bit length field as 0x00 /
    // 0x30. Two polls are low before the tag's answer arrives; the transmit's
    // interrupt is already there at the first poll.
    let response = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    let spi = transceive_transactions(&INVENTORY, [0x00, 0x30], 9, &response);
    let mut irq = irq_after(0);
    irq.extend(irq_after(2));
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(2)),
        PinMock::new(&irq),
    );

    // When
    let uid = r.inventory().unwrap().expect("a tag answered");

    // Then: reported most-significant byte first, the order printed on a
    // figure
    assert_eq!(uid, [1, 2, 3, 4, 5, 6, 7, 8]);
    finish(r);
}

/// ISO 15693-3 §9.1: after a tag answers, the reader must wait t2 before the
/// next request, because the tag is not listening until then.
///
/// Measured on the board without this wait: in twelve privacy unlocks, GET
/// RANDOM NUMBER was always answered, but the SET PASSWORD sent straight
/// after it got no answer 6 times out of 12.
#[test]
fn the_air_is_left_quiet_for_t2_after_a_tag_has_answered() {
    // Given
    let response = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    let spi = transceive_transactions(&INVENTORY, [0x00, 0x30], 9, &response);

    let mut irq = irq_after(0);
    irq.extend(irq_after(2));

    // The two IRQ polls, then the t2 wait after the reply.
    let mut delay = polls(2);
    delay.push(DelayTransaction::delay_us(T2_QUIET_US));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );

    let mut out = [0u8; MAX_RESPONSE];

    // When
    let result = r.transceive(&INVENTORY, &mut out);

    // Then
    assert_eq!(result, Ok(10));
    finish(r);
}

/// A tag that refused a password stops answering until it loses power
/// (SL2S2602 §9.5.3.2: "if the IC receives an invalid password, it will not
/// execute any following command until a Power-On Reset (POR) (RF reset) is
/// executed"). The tag is powered by the reader's field, so the reader turns
/// the field off, waits, and turns it on again.
#[test]
fn a_tag_is_reset_by_taking_its_field_away_and_giving_it_back() {
    // Given
    let mut spi = Vec::new();
    // Field off: the same word with the transmitter bit cleared.
    spi.extend(spi_write(vec![0x00, 0x00]));
    spi.extend(spi_write(vec![0x00, 0x20]));

    let delay = [
        DelayTransaction::delay_ms(FIELD_OFF_MS),
        DelayTransaction::delay_ms(FIELD_SETTLE_MS),
    ];

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&[]),
    );

    // When
    r.reset_tags().unwrap();

    // Then: the reader did exactly what was scripted
    finish(r);
}

/// SET PASSWORD for privacy, the NXP vendor default 0x0F0F0F0F masked with a
/// random number of zero, so the password shows through unchanged.
const SET_PASSWORD_VENDOR: [u8; 8] = [0x02, 0xB3, 0x04, 0x04, 0x0F, 0x0F, 0x0F, 0x0F];

/// A wrong password gets no answer, and the tag then ignores everything
/// until its field is turned off (SL2S2602 §9.5.3.2). So the field must be
/// reset before trying the next password.
#[test]
fn a_refused_password_is_followed_by_a_field_reset_before_the_next_one() {
    // Given
    let uid_response = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];

    let mut spi = Vec::new();
    // A tag in privacy mode ignores inventory entirely.
    spi.extend(transmit_transactions(&INVENTORY, [0x00, 0x30]));
    // It does answer GET RANDOM NUMBER, then says nothing to a wrong
    // password.
    spi.extend(transceive_transactions(
        &GET_RANDOM_NUMBER,
        [0x00, 0x30],
        2,
        &[0x00, 0xCD, 0xAB],
    ));
    spi.extend(transmit_transactions(&SET_PASSWORD_0, [0x00, 0x80]));
    // Field off and on again: the tag's power-on reset.
    spi.extend(spi_write(vec![0x00, 0x00]));
    spi.extend(spi_write(vec![0x00, 0x20]));
    // The tag listens again, so the second password can be tried.
    spi.extend(transceive_transactions(
        &GET_RANDOM_NUMBER,
        [0x00, 0x30],
        2,
        &[0x00, 0x00, 0x00],
    ));
    spi.extend(transceive_transactions(
        &SET_PASSWORD_VENDOR,
        [0x00, 0x80],
        0,
        &[0x00],
    ));
    spi.extend(transceive_transactions(
        &INVENTORY,
        [0x00, 0x30],
        9,
        &uid_response,
    ));

    let mut irq = irq_never();
    irq.extend(irq_exchange());
    irq.extend(irq_never());
    irq.extend(irq_exchange());
    irq.extend(irq_exchange());
    irq.extend(irq_exchange());

    let mut delay = polls(IRQ_POLL_ATTEMPTS as usize);
    delay.extend(answered(0));
    delay.extend(polls(IRQ_POLL_ATTEMPTS as usize));
    delay.push(DelayTransaction::delay_ms(FIELD_OFF_MS));
    delay.push(DelayTransaction::delay_ms(FIELD_SETTLE_MS));
    delay.extend(answered(0));
    delay.extend(answered(0));
    delay.extend(answered(0));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );

    // When
    let result = r.inventory_unlocked(&[0x0000_0000, 0x0F0F_0F0F]);

    // Then
    assert_eq!(
        result,
        Ok(Some([1, 2, 3, 4, 5, 6, 7, 8])),
        "the second password must reach a tag that is listening"
    );
    finish(r);
}

/// The FIFO holds twelve bytes and the whole request is loaded at once, so a
/// longer request is refused.
#[test]
fn a_request_too_large_for_the_fifo_is_refused_before_any_bus_traffic() {
    // Given
    let mut r = reader(&[]);
    let mut response = [0u8; 16];

    // When
    let result = r.transceive(&[0xAA; 13], &mut response);

    // Then
    assert_eq!(result, Err(Error::RequestTooLong));
    check(r);
}

#[test]
fn an_empty_plate_reports_no_tag_without_reading_the_fifo() {
    // Given
    // Nothing answers, so there is no interrupt and the FIFO is not read.
    let spi = transmit_transactions(&INVENTORY, [0x00, 0x30]);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );

    // When
    let result = r.inventory().unwrap();

    // Then
    assert_eq!(
        result, None,
        "an unanswered exchange means no tag, or a tag still in privacy mode"
    );
    finish(r);
}

#[test]
fn an_overflowed_fifo_is_an_error_rather_than_a_byte_count() {
    // Given
    // B4 is the overflow flag; the count is B3-B0. 0x1A has the overflow
    // flag set.
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

    // When
    let result = r.inventory();

    // Then
    assert_eq!(result, Err(Error::FifoOverflow));
    finish(r);
}

/// B6 and B5 are the FIFO level flags, not part of the count. Counting them
/// would turn a ten-byte reply into 105 bytes.
#[test]
fn the_fifo_level_flags_are_not_counted_as_received_bytes() {
    // Given: 0x69 is level-high, level-low, and a count nibble of 9 meaning
    // ten bytes
    let response = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    let spi = transceive_transactions(&INVENTORY, [0x00, 0x30], 0x69, &response);
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq_exchange()),
    );

    // When
    let uid = r.inventory().unwrap();

    // Then
    assert!(uid.is_some());
    finish(r);
}

#[test]
fn unlocking_fetches_a_random_number_then_sends_the_masked_password() {
    // Given
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

    // Two exchanges, so two t2 waits. Without the first, the tag ignores a
    // SET PASSWORD that follows GET RANDOM NUMBER too soon.
    let mut delay = answered(0);
    delay.extend(answered(0));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );

    // When
    r.unlock_privacy(0).unwrap();

    // Then: the reader did exactly what was scripted
    finish(r);
}

/// ENABLE PRIVACY with password 0, masked with random number 0xABCD, spelled
/// out rather than taken from `slix`. Seven bytes, with no password
/// identifier.
const ENABLE_PRIVACY_0: [u8; 7] = [0x02, 0xBA, 0x04, 0xCD, 0xAB, 0xCD, 0xAB];

#[test]
fn relocking_fetches_a_random_number_then_sends_the_masked_password() {
    // Given: seven bytes split the length field as 0x00 / 0x70
    let mut spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 2, &[0x00, 0xCD, 0xAB]);
    spi.extend(transceive_transactions(
        &ENABLE_PRIVACY_0,
        [0x00, 0x70],
        0,
        &[0x00],
    ));
    let mut irq = irq_exchange();
    irq.extend(irq_exchange());
    let mut delay = answered(0);
    delay.extend(answered(0));
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );

    // When
    let relocked = r.enable_privacy(0);

    // Then
    assert_eq!(relocked, Ok(()));
    finish(r);
}

/// A tag that did not answer did not confirm it is locked again.
#[test]
fn relocking_a_tag_that_does_not_answer_is_a_timeout() {
    // Given
    let mut spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 2, &[0x00, 0xCD, 0xAB]);
    spi.extend(transmit_transactions(&ENABLE_PRIVACY_0, [0x00, 0x70]));
    let mut irq = irq_exchange();
    irq.extend(irq_never());
    let mut delay = answered(0);
    delay.extend(polls(IRQ_POLL_ATTEMPTS as usize));
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );

    // When
    let relocked = r.enable_privacy(0);

    // Then
    assert_eq!(relocked, Err(Error::Timeout));
    finish(r);
}

#[test]
fn a_rejected_password_is_reported_rather_than_read_as_success() {
    // Given
    // ISO 15693-3 §7.4: bit 0 of the response flags means the payload is an
    // error code. A SLIX refusing the password answers [0x01, 0x0F], which a
    // length check alone would accept.
    let mut spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 2, &[0x00, 0xCD, 0xAB]);
    spi.extend(transceive_transactions(
        &SET_PASSWORD_0,
        [0x00, 0x80],
        1,
        &[0x01, 0x0F],
    ));

    let mut irq = irq_exchange();
    irq.extend(irq_exchange());

    // Two exchanges, so two t2 waits.
    let mut delay = answered(0);
    delay.extend(answered(0));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );

    // When
    let result = r.unlock_privacy(0);

    // Then
    assert_eq!(
        result,
        Err(Error::TagError(0x0F)),
        "a wrong password must not look like an unlocked tag"
    );
    finish(r);
}

#[test]
fn an_error_response_is_not_mistaken_for_a_random_number() {
    // Given
    let spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 1, &[0x01, 0x03]);
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq_exchange()),
    );

    // When
    let result = r.get_random_number();

    // Then
    assert_eq!(result, Err(Error::TagError(0x03)));
    finish(r);
}

#[test]
fn unlocking_fails_cleanly_when_no_tag_answers() {
    // Given
    let spi = transmit_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30]);
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );

    // When
    let result = r.unlock_privacy(0);

    // Then
    assert_eq!(result, Err(Error::Timeout));
    finish(r);
}

#[test]
fn the_field_is_turned_on_last() {
    // Given
    let sequence = INIT_SEQUENCE;

    // When
    let last = sequence.last().unwrap();

    // Then
    assert_eq!(
        last.0,
        regs::CHIP_STATUS_CONTROL,
        "enabling the field before the protocol is configured radiates noise"
    );
}

/// The interrupt line is readable without an exchange in flight.
///
/// For debugging: shows whether the wiring delivers an interrupt that the
/// status register reports.
#[test]
fn the_interrupt_line_can_be_read_on_its_own() {
    // Given
    let mut r = Trf7962a::new(
        SpiMock::new(&[]),
        NoopDelay,
        PinMock::new(&[PinTransaction::get(PinState::High)]),
    );

    // When
    let result = r.irq_asserted();

    // Then
    assert_eq!(result, Ok(true));
    check(r);
}

/// SLOS757G Figure 6-20, byte for byte: the datasheet's single-slot
/// inventory, in one SPI transaction.
///
/// Written one byte per transaction, the FIFO stays empty (measured), and
/// since "transmission starts automatically after the first byte is written
/// into the FIFO" (§6.12.5), nothing is sent.
///
/// The bytes are copied from the figure: 0x8F reset FIFO, 0x91 transmit with
/// CRC, 0x3D continuous write from 0x1D, the two length bytes, then the
/// request.
#[test]
fn a_transmit_is_one_burst_exactly_as_the_datasheet_shows_it() {
    // Given
    let mut r = Trf7962a::new(
        SpiMock::new(&spi_write(vec![
            0x8F, 0x91, 0x3D, 0x00, 0x30, 0x26, 0x01, 0x00,
        ])),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );
    let mut response = [0u8; MAX_RESPONSE];

    // When
    let result = r.transceive(&INVENTORY, &mut response);

    // Then
    assert_eq!(result, Ok(0));
    finish(r);
}

/// SLOS757G §6.12.5 and Figure 6-21: a transmit raises its own interrupt
/// before the tag answers.
///
/// "The flag is set at the start of TX but the interrupt request is sent when
/// TX is finished" (Table 6-29, B7). The tag's answer is a *second*
/// interrupt, about 4 ms later, and the FIFO is reset between the two.
/// Reading the FIFO at the first interrupt would give an empty FIFO, which
/// the N-1 count reports as one byte.
///
/// Bytes: 0x6C interrupt status read, 0x80 TX complete, 0x8F reset FIFO,
/// 0x40 RX started, 0x5C FIFO status read.
#[test]
fn the_transmit_interrupt_is_not_mistaken_for_the_tags_reply() {
    // Given
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
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq),
    );

    // When
    let result = r.inventory().unwrap();

    // Then
    assert_eq!(
        result,
        Some([1, 2, 3, 4, 5, 6, 7, 8]),
        "the reply follows the transmit interrupt, not the transmit itself"
    );
    finish(r);
}

/// A reader that transmits and hears nothing must say so, not read the FIFO.
///
/// The transmit interrupt always arrives, so an interrupt alone does not
/// mean a tag answered.
#[test]
fn a_transmit_with_no_answer_reports_no_tag() {
    // Given
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

    // When
    let result = r.inventory().unwrap();

    // Then
    assert_eq!(result, None);
    finish(r);
}

/// SLOS757G Figure 6-23: the FIFO is read as one continuous burst.
///
/// The address byte 0x7F (read bit, continuous bit, FIFO address) is sent
/// once, then one filler byte per byte wanted, all in one SPI transaction.
/// The reply arrives one byte behind.
///
/// Read one byte per transaction, every byte comes back 0x00 (measured).
#[test]
fn the_fifo_is_read_as_one_continuous_burst() {
    // Given
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
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq_exchange()),
    );

    // When
    let result = r.get_random_number();

    // Then
    assert_eq!(
        result,
        Ok(0xABCD),
        "a random number of zero on every run is a FIFO that was never read"
    );
    finish(r);
}

/// A reception the reader could not decode says why.
///
/// SLOS757G Table 6-29 gives four reasons: CRC (B4), parity (B3), byte
/// framing or EOF (B2), and collision (B1). A collision or framing error
/// suggests a mistuned reader; a CRC error suggests a corrupted reply. So the
/// reason is kept.
#[test]
fn a_reception_error_carries_the_reader_s_own_reason() {
    // Given
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

    // When
    let result = r.transceive(&INVENTORY, &mut response);

    // Then
    assert_eq!(
        result,
        Err(Error::ReceiveError(0x02)),
        "the reason is the reader's own flags, not a verdict on the reply"
    );
    finish(r);
}

/// A frame of five bytes or more interrupts part-way through its own
/// transmission, and that is not the end of it.
///
/// SLOS757G §6.12.5: "if the number of bytes to be transmitted is higher or
/// equal to 5, then the interrupt is generated. This occurs also when the
/// number of bytes in the FIFO reaches 3", so more data can be loaded. The
/// whole request is already loaded, so this interrupt is ignored.
///
/// Measured for the eight-byte SET PASSWORD: 0xA0 at 1.4 ms (transmitting,
/// FIFO low), then 0x80 at 1.6 ms (finished). Treating the first as the end
/// would reset the FIFO mid-frame, and the tag would not answer.
#[test]
fn a_fifo_interrupt_during_a_transmit_is_not_the_end_of_it() {
    // Given
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
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq),
    );
    let mut response = [0u8; MAX_RESPONSE];

    // When
    let result = r.transceive(&SET_PASSWORD_0, &mut response);

    // Then
    assert_eq!(
        result,
        Ok(3),
        "the frame is preloaded, so a request for more data is only noise"
    );
    finish(r);
}

/// READ SINGLE BLOCK for block 5, spelled out rather than built by the driver.
const READ_BLOCK_5: [u8; 3] = [0x02, 0x20, 0x05];

#[test]
fn a_block_read_returns_the_four_data_bytes_in_wire_order() {
    // Given
    // flags, then four data bytes in an order that would catch a reversal.
    let response = [0x00u8, 0x11, 0x22, 0x33, 0x44];
    let spi = transceive_transactions(&READ_BLOCK_5, [0x00, 0x30], 4, &response);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq_exchange()),
    );

    // When
    let result = r.read_block(5);

    // Then
    assert_eq!(
        result,
        Ok([0x11, 0x22, 0x33, 0x44]),
        "block data has no byte-order convention and must come back as sent"
    );
    finish(r);
}

#[test]
fn a_tag_error_on_a_block_read_is_reported_rather_than_returned_as_data() {
    // Given
    // ISO 15693-3 §7.4: bit 0 of the flags marks an error response, whose
    // payload is a one-byte error code rather than four bytes of data.
    let spi = transceive_transactions(&READ_BLOCK_5, [0x00, 0x30], 1, &[0x01, 0x0F]);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq_exchange()),
    );

    // When
    let result = r.read_block(5);

    // Then
    assert_eq!(
        result,
        Err(Error::TagError(0x0F)),
        "a refused read must not look like a successful one"
    );
    finish(r);
}

#[test]
fn a_short_block_response_is_rejected_rather_than_read_past() {
    // Given
    // Two bytes: flags and one data byte instead of four. Reading past them
    // would return stale bytes as if they were tag memory.
    let response = [0x00u8, 0x11];
    let spi = transceive_transactions(&READ_BLOCK_5, [0x00, 0x30], 1, &response);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq_exchange()),
    );

    // When
    let result = r.read_block(5);

    // Then
    assert_eq!(result, Err(Error::BadResponse));
    finish(r);
}

#[test]
fn an_absent_block_response_is_a_timeout_not_a_read() {
    // Given
    let spi = transmit_transactions(&READ_BLOCK_5, [0x00, 0x30]);

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );

    // When
    let result = r.read_block(5);

    // Then
    assert_eq!(result, Err(Error::Timeout));
    finish(r);
}

#[test]
fn reading_memory_issues_one_exchange_per_block_and_concatenates_in_order() {
    // Given
    let mut spi = transceive_transactions(
        &[0x02, 0x20, 0x05],
        [0x00, 0x30],
        4,
        &[0x00, 0x11, 0x22, 0x33, 0x44],
    );
    spi.extend(transceive_transactions(
        &[0x02, 0x20, 0x06],
        [0x00, 0x30],
        4,
        &[0x00, 0x55, 0x66, 0x77, 0x88],
    ));

    let mut irq = irq_exchange();
    irq.extend(irq_exchange());
    let mut delay = answered(0);
    delay.extend(answered(0));

    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&delay),
        PinMock::new(&irq),
    );
    let mut out = [0u8; 8];

    // When
    r.read_memory(5, &mut out).unwrap();

    // Then
    assert_eq!(
        out,
        [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        "the second block's bytes must follow the first's, not precede them"
    );
    finish(r);
}

#[test]
fn reading_memory_with_a_length_not_a_multiple_of_four_is_refused_before_any_bus_traffic() {
    // Given
    let mut r = reader(&[]);
    let mut out = [0u8; 5];

    // When
    let result = r.read_memory(0, &mut out);

    // Then
    assert_eq!(result, Err(Error::BadLength));
    check(r);
}

/// A reply longer than eight bytes arrives in two parts.
///
/// SLOS757G §6.12.4: "if the received packet is longer than 8 bytes, the
/// interrupt is sent before the end of the receive operation when the ninth
/// byte is loaded into the FIFO... In the case of an IRQ_FIFO, the MCU should
/// expect either another IRQ_FIFO or RX complete interrupt. This is repeated
/// until an RX complete interrupt is generated." The datasheet's example
/// reads nine bytes, then the tenth from a second interrupt 160 µs later
/// (Figures 6-23 and 6-24).
///
/// An inventory reply is exactly ten bytes, so this always happens.
#[test]
fn a_reply_longer_than_the_fifo_warning_is_collected_in_full() {
    // Given
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
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq),
    );

    // When
    let result = r.inventory().unwrap();

    // Then
    assert_eq!(
        result,
        Some([1, 2, 3, 4, 5, 6, 7, 8]),
        "the last byte of the UID arrives on its own interrupt"
    );
    finish(r);
}

/// The plate's presence check. SL2S5002 §1.3: in privacy mode the label "will
/// not respond to any command except the command GET RANDOM NUMBER, until it
/// next receives the correct Privacy password". So this command shows whether
/// a Tonie is on the plate without unlocking it. `spi.done()` checks that no
/// SET PASSWORD follows.
#[test]
fn a_tag_is_noticed_without_being_unlocked() {
    // Given
    let random_response = [0x00u8, 0xCD, 0xAB]; // flags, then RN low, high
    let spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 2, &random_response);
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(0)),
        PinMock::new(&irq_exchange()),
    );

    // When
    let result = r.tag_present();

    // Then
    assert_eq!(result, Ok(true));
    finish(r);
}

/// An empty plate costs exactly one unanswered exchange. The box is in this
/// state most of the time.
#[test]
fn an_empty_plate_costs_one_unanswered_exchange() {
    // Given
    let spi = transmit_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30]);
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&polls(IRQ_POLL_ATTEMPTS as usize)),
        PinMock::new(&irq_never()),
    );

    // When
    let result = r.tag_present();

    // Then
    assert_eq!(result, Ok(false));
    finish(r);
}

/// The driver remembers the slowest reply it has seen, to size
/// `IRQ_POLL_ATTEMPTS` from real measurements.
#[test]
fn the_slowest_reply_is_remembered_for_the_bench_to_read() {
    // Given: the transmit interrupt answers at once; the tag's reply takes
    // two polls
    let random_response = [0x00u8, 0xCD, 0xAB];
    let spi = transceive_transactions(&GET_RANDOM_NUMBER, [0x00, 0x30], 2, &random_response);
    let mut irq = irq_after(0);
    irq.extend(irq_after(2));
    let mut r = Trf7962a::new(
        SpiMock::new(&spi),
        CheckedDelay::new(&answered(2)),
        PinMock::new(&irq),
    );
    assert_eq!(r.slowest_reply_polls(), 0, "nothing has been answered yet");

    // When
    let random = r.get_random_number();

    // Then
    assert_eq!(random, Ok(0xABCD));
    assert_eq!(r.slowest_reply_polls(), 2);
    finish(r);
}
