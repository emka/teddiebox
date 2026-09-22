//! The setup portal: one page, one form, one lease.
//!
//! Reached by holding both ears through a power-on. While this runs, the box
//! is not a teddy bear — no decoder, no codec, no NFC, no media loop.
//!
//! That absence buys *stack*, and the portal's problem is not stack. See
//! [`REQUEST`] for what it actually costs and what has been measured.
//!
//! The wire formats live in `teddiebox_portal` and are tested on the host.
//! What is here is the part that cannot be: the access point, the two
//! sockets, the card, and the order the save path does things in.
//!
//! Nothing here has been run on hardware. It compiles and links, which is the
//! whole of what is known about it.

use core::future::Future;
use core::pin::Pin;

use embassy_futures::select::{select, select4, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ipv4Address, Stack};
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::{GPIO44, UART0, WIFI};
use esp_hal::uart::{Config as UartConfig, ConfigError, UartRx};
use teddiebox_console::{Command, CommandWatch};
use teddiebox_core::LedState;
use teddiebox_portal::submission::{examine, Submission};
use teddiebox_portal::{dhcp, http, page, MAX_BODY, MAX_CONFIG};

use crate::net;
use crate::stack;
use crate::storage::Mounted;

/// How long the box will sit with its radio up before restarting itself.
///
/// The pack on this box goes flat without warning, and an access point left
/// beaconing overnight is a box that is dead in the morning for no reason
/// anybody will connect to this.
///
/// **Measured from entry, not from the last connection.** An idle timeout is
/// the obvious shape and it does not work here: a phone that has joined the
/// network runs captive-portal probes at it for as long as it stays joined,
/// so every restart-on-activity keeps restarting and the window never closes.
/// Ten minutes from the moment both ears were held is a bound that a phone
/// cannot argue with, and it is far longer than editing one text file takes.
const SETUP_WINDOW: Duration = Duration::from_secs(600);

/// How long a single connection may sit without saying anything.
///
/// The listener takes one connection at a time, and a phone that opens one and
/// wanders off — captive-portal probes do this constantly — would otherwise
/// hold the whole portal shut. Long enough that a slow phone is not cut off,
/// short enough that a dead one is.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the answer gets to reach the phone before the box reboots.
///
/// [`send`] already waits for the bytes to be acknowledged, so this is margin
/// and not the mechanism.
const SETTLE: Duration = Duration::from_millis(500);

/// Room for the head above the body in the request buffer.
///
/// A phone's POST head is the part nothing here controls: a request line, a
/// `Host`, the two content headers, and then whatever the browser adds —
/// `User-Agent`, `Accept`, `Accept-Language`, `Origin`, `Referer`, the five
/// `Sec-Fetch-*` lines. On a current mobile Chrome that set runs six to nine
/// hundred bytes, so 512 would leave a full-size config no room to land and
/// the refusal would blame the body.
///
/// **Not measured on the box.** It is sized from what browsers are known to
/// send, and a bench session with a real phone is what would settle it.
const HEAD_ROOM: usize = 1024;

/// The request buffer: the largest body, plus room for the head above it.
///
/// `MAX_BODY` (3088) + `HEAD_ROOM` (1024) = 4112 bytes.
///
/// **Not on the stack, and an earlier version of this comment said it was.**
/// This buffer is held across `await` points, so it sizes `main`'s embassy
/// task future, which is a `static` in `.bss`; `esp-hal` then gives the stack
/// whatever DRAM `.bss` leaves over. Not starting the decoder frees stack —
/// the decoder's 15.8 KB is transient depth inside another task's poll — and
/// frees no `.bss` whatsoever, so it pays for none of this.
///
/// What it costs, from the linker's own symbols:
///
/// | | `_stack_start - _stack_end` | `___embassy_main4POOL` |
/// |---|---|---|
/// | before the portal was wired in | 50,956 | 384 |
/// | wired in, page held in a buffer | 23,308 | 27,120 |
/// | wired in, page streamed | 32,140 | 18,288 |
///
/// The deepest the stack has ever been measured on this box is 49,024 bytes,
/// so 32,140 was not a smaller margin — it was an ordinary boot running out
/// of stack.
///
/// Two measurements bounded what was left to win without changing *where*
/// the portal ran. Shrinking every remaining buffer to nothing left a
/// 6,656-byte future and 43,788 bytes of stack region; 3,472 of that 6,656
/// was the `StackResources<5>` inside [`net::Radio`]. So even moving every
/// byte of buffer *and* the radio's resources into storage that costs
/// nothing — storage some other mode already owns and setup mode never
/// touches — stopped at 47,260, still under the 49,024 already seen used.
///
/// The rest was the future itself, and the only place it could go was a task
/// pool that already exists and sits idle in setup mode: the decoder's,
/// [`crate::audio::SCRATCH`] — 51,712 bytes setup mode never touches because
/// setup mode starts no decoder. [`place`] is the move: it puts the whole
/// future returned by [`run`] — this buffer, the DHCP buffers below, and the
/// radio's `StackResources` included — into that scratch instead of `main`'s
/// own future, gated by [`crate::audio::take_scratch_bytes`] so the two
/// claims on the scratch can never both succeed.
///
/// | | `_stack_start - _stack_end` | `___embassy_main4POOL` |
/// |---|---|---|
/// | before the portal was wired in | 50,956 | 384 |
/// | wired in, page held in a buffer | 23,308 | 27,120 |
/// | wired in, page streamed | 32,140 | 18,288 |
/// | wired in, placed in the decode scratch | 50,060 | 392 |
const REQUEST: usize = MAX_BODY + HEAD_ROOM;

/// Raises the access point and serves until it is done with.
///
/// Never returns. It resets the box itself once a config has been saved, or
/// once [`SETUP_WINDOW`] is up; if the radio will not start at all it stops
/// where it stands instead.
///
/// `card` is an `Option` because the access point is worth raising without
/// one: a box whose card is loose is exactly the box somebody is holding both
/// ears on. With no card the page says so and refuses only the save.
///
/// `paint` is how this reaches the LED. `main` owns the LEDC controller and
/// never hands it away — its own loop is what paints for every other mode,
/// and this function never reaches that loop — so it hands down a closure
/// over the controller instead, built from the same `Gates` and `Rgb` the
/// rest of the boot uses. Nothing here knows how a colour becomes light; it
/// only knows which colour a state is.
pub async fn run(
    wifi: WIFI<'static>,
    uart: UART0<'static>,
    rx: GPIO44<'static>,
    card: Option<Mounted>,
    seed: u64,
    paint: impl Fn(LedState),
) -> ! {
    // Painted before the access point is even asked for. This is the only
    // feedback setup mode has — no screen, and by the time this runs, no
    // decoder or codec either — so a dark LED here would read exactly like a
    // box that failed to start rather than one waiting to be talked to.
    paint(LedState::Setup);

    // Built here rather than inside `serve_console`, and this is not tidiness.
    // `UartRx::new` reconfigures UART0, whose transmit half `esp-println` is
    // using — so constructing it lazily on the first poll of the `select4`
    // landed in the middle of the "portal up" line and shredded it. Draining
    // first and taking the receiver before any of this function's own output
    // keeps the reconfiguration away from a print in flight.
    crate::drain_console();
    let console = UartRx::new(uart, UartConfig::default().with_baudrate(115200))
        .map(|console| console.with_rx(rx));

    // Setup mode never reaches the console loop, so the `stack` command that
    // would answer this question is not there to be typed. Printed on the way
    // in and again on the way out instead: the pair is what turns the portal's
    // own depth into a measurement rather than another estimate.
    stack::report();

    // Read before the access point exists, because it decides what the access
    // point's passphrase is. Anything that goes wrong here — no card, an
    // unreadable file, a file that does not parse — falls back to the
    // published passphrase, which is the property this mode rests on: the way
    // back into a box cannot depend on the file somebody is here to repair.
    let setup_password = card
        .as_ref()
        .and_then(|card| {
            let mut bytes = [0u8; MAX_CONFIG];
            let filled = card.read_config_bytes(&mut bytes).ok()?;
            let text = core::str::from_utf8(&bytes[..filled]).ok()?;
            teddiebox_config::Config::parse(text).ok()?.setup_password
        })
        .unwrap_or_else(|| {
            heapless::String::try_from(net::SETUP_PASSWORD)
                .expect("the compiled-in passphrase fits a WPA2 passphrase")
        });

    // Built here rather than handed in, so that the radio's `StackResources`
    // — 3,472 bytes of it — is part of *this* future and therefore lands in
    // the scratch with everything else. A `&mut` from the caller would leave
    // it in the caller's future, which is the `.bss` this whole arrangement
    // exists to keep empty.
    let mut radio = net::Radio::new(wifi);
    let (session, mut link) = match radio.serve(seed, &setup_password) {
        Ok(pair) => pair,
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: portal could not raise the access point — {trouble:?}"
            );
            // Stop here rather than reset. The ears are still held — that is
            // what got the box into this mode — so a reset would re-enter
            // setup, fail again, and keep doing it for as long as somebody
            // holds on, which from the outside is indistinguishable from a
            // brick. A box sitting still with a red LED says what happened
            // and leaves switching it off to the person holding it.
            paint(LedState::Error);
            park().await
        }
    };

    esp_println::println!(
        "teddiebox: portal up — join `{}` ({}) and open http://192.168.4.1/",
        net::SETUP_SSID,
        if setup_password == net::SETUP_PASSWORD {
            "the published passphrase"
        } else {
            "the card's own passphrase"
        }
    );

    // The runner has to be polled throughout rather than awaited first: it
    // never returns, and neither server makes any progress without it. The
    // three share one stack frame instead of three tasks because the session
    // and the link are borrowed out of the radio and cannot be handed away.
    let stack = session.stack();
    let serving = select4(
        link.run(),
        serve_http(stack, card.as_ref()),
        serve_dhcp(stack),
        serve_console(console, card.as_ref()),
    );
    match select(Timer::after(SETUP_WINDOW), serving).await {
        Either::First(()) => esp_println::println!(
            "teddiebox: portal's {} minutes are up — restarting",
            SETUP_WINDOW.as_secs() / 60
        ),
        Either::Second(_) => unreachable!("none of the three ever returns"),
    }

    stack::report();
    crate::drain_console();
    esp_hal::system::software_reset();
}

/// Moves a future into memory somebody else owns, and pins it there.
///
/// This is the whole mechanism by which setup mode costs the stack region
/// nothing. An `async fn`'s locals that are live across an `await` are fields
/// of its future, and a future awaited inside an embassy task is part of
/// *that* task's future, which is a `static` in `.bss`. So awaiting [`run`]
/// directly from `main` put every buffer in this file, plus the radio's
/// `StackResources`, into `___embassy_main4POOL` — on every boot, ears held
/// or not. Handing the future somewhere else to live leaves `main` holding a
/// pointer.
///
/// `room` comes from [`crate::audio::take_scratch_bytes`], which is the
/// decoder's scratch: the one large buffer this box owns that setup mode
/// provably never touches.
///
/// Returns `None` if `room` cannot hold `future` aligned. That is a check and
/// not an assertion because it is the only thing standing between a future
/// that has grown and a write past the end of the scratch; the size is
/// printed either way, so a bench session can see the margin rather than
/// infer it.
///
/// The borrow is `room`'s own elided lifetime rather than `'static` on
/// purpose: the future captures the LED closure, which borrows the LEDC
/// controller `main` holds, and `main` never returns. Demanding `'static`
/// would only force that closure to be rebuilt around owned state for no
/// gain.
pub fn place<F: Future>(room: &mut [u8], future: F) -> Option<Pin<&mut F>> {
    let size = core::mem::size_of::<F>();
    esp_println::println!(
        "teddiebox: portal future is {size} bytes of {} in the decode scratch",
        room.len()
    );

    let offset = room.as_ptr().align_offset(core::mem::align_of::<F>());
    if room
        .len()
        .checked_sub(offset)
        .is_none_or(|left| left < size)
    {
        esp_println::println!("teddiebox: portal future does not fit the decode scratch");
        return None;
    }

    // SAFETY: `offset + size` is within `room` — the check above says so —
    // and `offset` is what `align_offset` asked for, so `slot` is a properly
    // aligned, in-bounds pointer to `size` writable bytes. `room` is a unique
    // borrow that this consumes and never touches again, and the returned
    // `Pin` borrows it for the rest of its lifetime, so nothing else can read
    // or write those bytes while the future is alive. The bytes being
    // overwritten are the decoder's scratch: two plain arrays with no
    // destructor to skip. `Pin::new_unchecked` is honest because the future
    // is reachable only through the pointer returned here, so it can never be
    // moved again; it is never dropped, and the storage it pins is a `static`
    // that outlives the borrow and is never handed back — so no drop is
    // skipped over memory that is later reused.
    unsafe {
        let slot = room.as_mut_ptr().add(offset).cast::<F>();
        slot.write(future);
        Some(Pin::new_unchecked(&mut *slot))
    }
}

/// Stops the box where it stands.
///
/// Never returns and never wakes again. For the failures that leave nothing to
/// retry and nothing to serve: the alternative is a reset, and a reset with
/// both ears still held comes straight back here.
///
/// Callers paint before calling this — see [`run`] — because what is worth
/// showing differs by failure, and this has no opinion of its own left to
/// add once the console is drained.
pub async fn park() -> ! {
    crate::drain_console();
    stay_put().await
}

/// Never completes, and costs nothing while not completing.
async fn stay_put() -> ! {
    core::future::pending::<()>().await;
    unreachable!("pending never completes")
}

/// Answers one connection at a time on port 80, for as long as it is polled.
///
/// Never returns of its own accord. The window that ends the portal is
/// [`run`]'s, and the save path does not come back through here either — it
/// resets the box from inside the handler.
async fn serve_http(stack: Stack<'_>, card: Option<&Mounted>) {
    // The portal's buffer claim, named in one place rather than spread
    // through the call tree: 1536 + 1536 + 4112 here, `CHUNK` in `send_page`
    // below, and 1024 + 1024 + 590 in `serve_dhcp` beside it. None of it is
    // stack — see [`REQUEST`].
    let mut rx = [0u8; 1536];
    let mut tx = [0u8; 1536];
    let mut buffer = [0u8; REQUEST];

    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(CONNECTION_TIMEOUT));

    loop {
        match socket.accept(80).await {
            Ok(()) => handle(&mut socket, card, &mut buffer).await,
            Err(trouble) => {
                esp_println::println!("teddiebox: portal could not accept — {trouble:?}");
            }
        }
        // One request per accept, so the socket goes back to the listener
        // rather than being kept for a phone that has already been answered.
        // `send` has waited for the response to be acknowledged by this point,
        // so the reset this puts on the wire cannot lose it.
        socket.abort();
        let _ = socket.flush().await;
    }
}

/// Reads one request and answers it.
async fn handle(socket: &mut TcpSocket<'_>, card: Option<&Mounted>, buffer: &mut [u8]) {
    let filled = match receive(socket, buffer).await {
        Received::Request(filled) => filled,
        Received::Refused(status, why) => return send_page(socket, status, b"", Some(why)).await,
        Received::Gone => return,
    };

    // Parsed a second time: the first parse happens inside `receive`, where
    // holding on to the `Request` would borrow the buffer that the next read
    // has to fill. A head is a couple of hundred bytes, and this way the
    // borrow checker has nothing to argue with.
    let Ok(request) = http::parse(&buffer[..filled]) else {
        esp_println::println!("teddiebox: portal unparseable request, {filled} bytes of {REQUEST}");
        return send_page(
            socket,
            http::Status::BadRequest,
            b"",
            Some("that request did not make sense to the box"),
        )
        .await;
    };

    // `HEAD_ROOM` was sized from what browsers are *known* to send, never from
    // one. This is the measurement: the head is the part nothing here
    // controls, so what matters is how close a real phone gets to the 1024
    // this buffer reserves above the body.
    esp_println::println!(
        "teddiebox: portal head {} of {} head room, body {} of {}, total {} of {}",
        request.header_len,
        HEAD_ROOM,
        request.content_length,
        MAX_BODY,
        filled,
        REQUEST
    );

    match (request.method, request.path) {
        (http::Method::Get, "/") => show(socket, card).await,
        (http::Method::Post, "/save") => {
            let body = &buffer[request.header_len..request.header_len + request.content_length];
            save(socket, card, body).await
        }
        // Everything else, with no body. A phone probing `/generate_204` or
        // `/hotspot-detect.html` gets a plain 404 and correctly reports that
        // this network has no internet, which is true.
        _ => send(socket, http::Status::NotFound, b"").await,
    }
}

/// What came off the socket.
enum Received {
    /// A whole request, this many bytes of it.
    Request(usize),
    /// Not one this box will answer, with the reason to show.
    Refused(http::Status, &'static str),
    /// The peer closed, or the socket did. There is nobody to answer.
    Gone,
}

/// Reads until the head parses and the whole body has arrived.
async fn receive(socket: &mut TcpSocket<'_>, buffer: &mut [u8]) -> Received {
    let mut filled = 0;
    loop {
        match complete(&buffer[..filled]) {
            Ok(true) => return Received::Request(filled),
            Ok(false) => {}
            Err(http::RequestError::TooLarge) => {
                return Received::Refused(
                    http::Status::TooLarge,
                    "that was far more than a config file — the box stopped reading it",
                )
            }
            Err(http::RequestError::Malformed) => {
                return Received::Refused(
                    http::Status::BadRequest,
                    "that request did not make sense to the box",
                )
            }
            // `complete` turns this one into `Ok(false)`; it means read more.
            Err(http::RequestError::Incomplete) => unreachable!("incomplete asks for another read"),
        }

        if filled == buffer.len() {
            // A head that never ended. `TooLarge` refuses the body by its
            // declared length, so what fills this buffer without parsing is
            // headers, and there is no length to refuse it by.
            return Received::Refused(
                http::Status::TooLarge,
                "that request had more headers than the box will read",
            );
        }

        match socket.read(&mut buffer[filled..]).await {
            Ok(0) => return Received::Gone,
            Ok(n) => filled += n,
            Err(trouble) => {
                esp_println::println!("teddiebox: portal read failed — {trouble:?}");
                return Received::Gone;
            }
        }
    }
}

/// Answers the console, so a box serving the portal does not look like a dead one.
///
/// Setup mode diverges before `main` ever builds its console loop, so until
/// this existed nothing in the box read UART0 while the portal was up. The box
/// went on *printing* — the heartbeat and the pack watch run in both modes —
/// so from outside it was talking and ignoring every command, which is exactly
/// what a wedged console looks like. On 2026-09-16 that cost two failed flash
/// attempts and a diagnosis of a box that was working perfectly.
///
/// `rb` is enough to fix it and is all this offers. It leaves setup mode for an
/// ordinary boot, and `dl` works normally from there — so the way to flash a
/// box that is serving the portal is two commands rather than a ten-minute
/// wait. `dl` itself is deliberately not handled here: `reboot_to_download`
/// hands the USB pads back and releases the rails through `Gates`, and setup
/// mode has neither, so it would be a second implementation of the one path in
/// this firmware that must not be got wrong.
///
/// Everything else gets a sentence saying where it is. That is the part that
/// actually removes the trap: any command at all, including the `dl` that
/// `scripts/flash.sh` sends first, now produces a line naming setup mode.
/// Writes `CONFIG.TXT` back with `setup_password` set, or taken out.
///
/// Returns whether the card now holds it, so the caller only resets on a write
/// that happened.
///
/// Validated by re-parsing the whole file rather than by checking the value,
/// so `teddiebox_config` stays the one authority on what this key may be — and
/// so a file that was *already* wrong is refused here rather than saved into a
/// box that will not boot. Same order as `save`: validate, then write, never
/// the other way round.
fn rewrite_setup_password(card: Option<&Mounted>, value: Option<&str>) -> bool {
    let Some(card) = card else {
        esp_println::println!("teddiebox: setup no card to write to");
        return false;
    };

    let mut bytes = [0u8; MAX_CONFIG];
    let filled = match card.read_config_bytes(&mut bytes) {
        Ok(filled) => filled,
        Err(why) => {
            esp_println::println!("teddiebox: setup cannot read the config — {why}");
            return false;
        }
    };
    let Ok(text) = core::str::from_utf8(&bytes[..filled]) else {
        esp_println::println!("teddiebox: setup the config on the card is not text");
        return false;
    };

    let mut rewritten = heapless::String::<MAX_CONFIG>::new();
    if let Err(trouble) = teddiebox_config::set_key(text, "setup_password", value, &mut rewritten) {
        esp_println::println!("teddiebox: setup the config would not take it — {trouble:?}");
        return false;
    }
    if let Err(trouble) = teddiebox_config::Config::parse(&rewritten) {
        esp_println::println!("teddiebox: setup that leaves an invalid config — {trouble:?}");
        return false;
    }

    match card.write_config(rewritten.as_bytes()) {
        Ok(()) => true,
        Err(trouble) => {
            esp_println::println!("teddiebox: setup could not write the card — {trouble}");
            false
        }
    }
}

async fn serve_console(
    console: Result<UartRx<'static, esp_hal::Blocking>, ConfigError>,
    card: Option<&Mounted>,
) -> ! {
    let mut console = match console {
        Ok(console) => console,
        Err(trouble) => {
            // Not fatal to the portal: the access point and the page are the
            // point, and this only ever made flashing easier.
            esp_println::println!("teddiebox: portal could not open the console — {trouble:?}");
            stay_put().await;
        }
    };

    let mut watch = CommandWatch::new();
    loop {
        let mut buf = [0u8; 16];
        if let Ok(n) = console.read_buffered(&mut buf) {
            if let Some(command) = buf[..n].iter().find_map(|&b| watch.feed(b)) {
                match command {
                    Command::Reboot => {
                        esp_println::println!("teddiebox: leaving the configuration portal");
                        crate::drain_console();
                        esp_hal::system::software_reset()
                    }
                    // The one command that is *more* use here than in the
                    // ordinary console: it changes the passphrase of the
                    // access point standing between somebody and this page.
                    Command::SetupPassword(value) => {
                        if rewrite_setup_password(card, value.as_deref()) {
                            esp_println::println!("teddiebox: setup passphrase saved — restarting");
                            crate::drain_console();
                            esp_hal::system::software_reset()
                        }
                    }
                    _ => esp_println::println!(
                        "teddiebox: the configuration portal is running — join `{}` and open \
                         http://192.168.4.1/, `setup pw off` to put its passphrase back to \
                         the published one, or `rb` to leave",
                        net::SETUP_SSID
                    ),
                }
            }
        }
        // Polled rather than awaited. `read_buffered` does not block, and the
        // portal's other three futures must keep being polled — a console that
        // parked here waiting for a byte would stop the access point.
        Timer::after(Duration::from_millis(50)).await;
    }
}

/// Whether what has arrived so far is a whole request.
///
/// Deliberately answers a `bool` rather than the `Request` it parsed: the
/// caller needs the buffer back, mutably, the moment the answer is `false`.
fn complete(buffer: &[u8]) -> Result<bool, http::RequestError> {
    match http::parse(buffer) {
        Ok(request) => Ok(buffer.len() - request.header_len >= request.content_length),
        Err(http::RequestError::Incomplete) => Ok(false),
        Err(other) => Err(other),
    }
}

/// What the page says when the box has no card to read or write.
///
/// The access point comes up either way — a loose or dead card is exactly the
/// box somebody is holding both ears on — so this is a thing to report on the
/// page rather than a reason not to serve one.
const NO_CARD: &str = "the box could not read its card — check it is pushed in, \
                       then reload this page";

/// `GET /` — the card's file, in the box, as it is.
async fn show(socket: &mut TcpSocket<'_>, card: Option<&Mounted>) {
    let Some(card) = card else {
        return respond_page(socket, b"", Some(NO_CARD)).await;
    };

    let mut config = [0u8; MAX_CONFIG];
    match card.read_config_bytes(&mut config) {
        // `Ok(0)` is a card with no config on it — a box being set up for the
        // first time, which is what this whole path exists for. An empty
        // textarea is the right answer to it, not an error.
        Ok(filled) => respond_page(socket, &config[..filled], None).await,
        Err(why) => respond_page(socket, b"", Some(why)).await,
    }
}

/// `POST /save` — decode, validate, write, reset.
///
/// What the submission *means* is [`examine`]'s, on the host, where the
/// validate-before-write rule can be tested. What is left here is the card and
/// the socket.
async fn save(socket: &mut TcpSocket<'_>, card: Option<&Mounted>, body: &[u8]) {
    let submitted = match examine(body) {
        Submission::Refuse(why) => return respond_error(socket, why).await,
        Submission::HandBack(bytes, why) => return respond_page(socket, &bytes, Some(why)).await,
        Submission::Write(bytes) => bytes,
    };

    // Checked here rather than on the way in, so that a config typed against
    // a card that is not there is still decoded, still validated, and still
    // handed back in the textarea. Losing what somebody typed is a second
    // problem on top of the card.
    let Some(card) = card else {
        return respond_page(socket, &submitted, Some(NO_CARD)).await;
    };

    match card.write_config(&submitted) {
        // Only a confirmed write resets. A failed one leaves the box here so
        // it can be tried again, rather than rebooting into whatever is on the
        // card now.
        Ok(()) => {
            esp_println::println!(
                "teddiebox: portal wrote {} bytes — restarting",
                submitted.len()
            );
            respond_saved(socket).await;
            crate::drain_console();
            Timer::after(SETTLE).await;
            esp_hal::system::software_reset();
        }
        Err(why) => respond_page(socket, &submitted, Some(why)).await,
    }
}

/// The page, with whatever should be in the textarea and whatever went wrong.
async fn respond_page(socket: &mut TcpSocket<'_>, config: &[u8], error: Option<&str>) {
    send_page(socket, http::Status::Ok, config, error).await
}

/// The page again, but as a refusal, with nothing in the textarea.
///
/// For the two cases where there are no bytes worth handing back: a form that
/// did not decode, and one that is not text. Both are things a browser does
/// not do, so nothing a person typed is lost by not echoing them.
async fn respond_error(socket: &mut TcpSocket<'_>, message: &str) {
    send_page(socket, http::Status::BadRequest, b"", Some(message)).await
}

/// The last thing the phone sees before the network disappears.
///
/// Its own page rather than [`page::pieces`], which always carries the form:
/// offering a Save button to a box that is already rebooting would only invite
/// a submission nothing is listening for.
async fn respond_saved(socket: &mut TcpSocket<'_>) {
    send(socket, http::Status::Ok, SAVED.as_bytes()).await
}

/// Self-contained, for the same reason `page.rs` is: on this access point the
/// phone has a route to nothing, so anything referenced would not load.
const SAVED: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>teddiebox setup</title><style>\
body{font:16px system-ui;margin:0;padding:1rem;background:#f6f5f3;color:#1a1a1a}\
</style></head><body><h1>saved</h1>\
<p>The box is restarting. This network will disappear on its own.</p>\
</body></html>";

/// How much of an escaped run is held at a time.
///
/// The only thing standing in for a whole page. It has to be at least as long
/// as the longest escape — `&quot;`, six bytes — or [`page::escape_chunk`]
/// cannot make progress; past that the size only decides how many writes a
/// page takes, and the socket's own transmit buffer is what actually goes on
/// the wire.
const CHUNK: usize = 64;

/// Sends the page under the given status, a piece at a time.
///
/// Nothing here holds a page: [`page::length`] promises the size in the
/// response head, and the pieces follow it. That is what keeps a full-size
/// `CONFIG.TXT` — 1024 bytes that escaping can take to 6144 — showable
/// without 7.6 KB of buffer sitting in `.bss` for the life of the firmware.
async fn send_page(
    socket: &mut TcpSocket<'_>,
    status: http::Status,
    config: &[u8],
    error: Option<&str>,
) {
    let head = http::head(status, page::length(config, error));
    if write_all(socket, &head).await.is_err() {
        return;
    }
    for piece in page::pieces(config, error) {
        let sent = match piece {
            page::Piece::Literal(text) => write_all(socket, text.as_bytes()).await,
            page::Piece::Escaped(bytes) => write_escaped(socket, bytes).await,
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = socket.flush().await;
}

/// Writes `bytes` HTML-escaped, [`CHUNK`] at a time.
async fn write_escaped(socket: &mut TcpSocket<'_>, bytes: &[u8]) -> Result<(), ()> {
    let mut chunk = [0u8; CHUNK];
    let mut at = 0;
    while at < bytes.len() {
        let (consumed, written) = page::escape_chunk(&bytes[at..], &mut chunk);
        if consumed == 0 {
            // `CHUNK` is larger than the longest escape, so this cannot
            // happen — and if it ever did, looping for ever with the radio up
            // is the one outcome worth refusing outright.
            esp_println::println!("teddiebox: portal made no progress escaping the page");
            return Err(());
        }
        at += consumed;
        write_all(socket, &chunk[..written]).await?;
    }
    Ok(())
}

/// Head, body, and a flush that waits for the phone to have it.
///
/// The flush is what makes it safe for the caller to abort the connection — or
/// to reset the box — immediately afterwards.
async fn send(socket: &mut TcpSocket<'_>, status: http::Status, body: &[u8]) {
    let head = http::head(status, body.len());
    if write_all(socket, &head).await.is_err() {
        return;
    }
    if write_all(socket, body).await.is_err() {
        return;
    }
    let _ = socket.flush().await;
}

/// Writes the whole slice.
///
/// `TcpSocket::write` returns how much it took, which is not always all of it;
/// a page sent with one call silently arrives cut in half.
async fn write_all(socket: &mut TcpSocket<'_>, bytes: &[u8]) -> Result<(), ()> {
    let mut rest = bytes;
    while !rest.is_empty() {
        match socket.write(rest).await {
            Ok(0) => return Err(()),
            Ok(n) => rest = &rest[n..],
            Err(trouble) => {
                esp_println::println!("teddiebox: portal write failed — {trouble:?}");
                return Err(());
            }
        }
    }
    Ok(())
}

/// Hands out the one lease, to whoever asks, for as long as the portal runs.
///
/// Never returns: a phone that renews mid-session has to be answered, so this
/// keeps listening even after somebody has been given an address.
async fn serve_dhcp(stack: Stack<'_>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx = [0u8; 1024];
    let mut buffer = [0u8; dhcp::MAX_DATAGRAM];

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    if let Err(trouble) = socket.bind(67) {
        esp_println::println!("teddiebox: portal could not bind dhcp — {trouble:?}");
        // Only this half is lost: the HTTP side can still be reached by a
        // phone given a static address by hand. So park this branch rather
        // than return from it — returning completes the `select4` and takes
        // the whole portal down with it.
        stay_put().await;
    }

    loop {
        let (n, _from) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            Err(trouble) => {
                esp_println::println!("teddiebox: portal dhcp recv failed — {trouble:?}");
                continue;
            }
        };

        let Some(incoming) = dhcp::parse(&buffer[..n]) else {
            continue;
        };
        let answer = match incoming.kind {
            dhcp::Kind::Discover => dhcp::Reply::Offer,
            dhcp::Kind::Request => dhcp::Reply::Ack,
            dhcp::Kind::Other => continue,
        };

        // Broadcast, not back to where it came from. A client asking for an
        // address does not have one yet — that is the whole point of DHCP — so
        // its source address is 0.0.0.0 and a unicast reply goes nowhere.
        let datagram = dhcp::reply(&incoming, answer);
        if let Err(trouble) = socket
            .send_to(&datagram, (Ipv4Address::BROADCAST, 68))
            .await
        {
            esp_println::println!("teddiebox: portal dhcp send failed — {trouble:?}");
        }

        // Printed after the reply is away, so a client with no address never
        // waits on a diagnostic. Only failures used to print here, which meant
        // a phone that leased perfectly and a DHCP server that never ran
        // looked identical on the console.
        //
        // The full datagram was dumped here on 2026-09-16 to capture a real
        // handset's bytes; they are pinned in `dhcp.rs`'s tests now, so the
        // hex is gone and the summary stays.
        esp_println::println!("teddiebox: portal dhcp {:?}, {n} bytes", incoming.kind);
    }
}
