//! The setup portal: one page, one form, one lease.
//!
//! Started by holding both ears while powering on. While it runs, the box
//! plays nothing: no decoder, codec, NFC or media loop.
//!
//! See [`REQUEST`] and [`place`] for where its memory comes from.
//!
//! The wire formats are in `teddiebox_portal` and tested on the host. This
//! file has the access point, the sockets, the card, and the save order.

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
/// An access point left on overnight would empty the battery.
///
/// **Counted from the start, not from the last activity.** A joined phone
/// keeps sending captive-portal probes, so an idle timeout would never
/// expire. Ten minutes is plenty to edit one text file.
const SETUP_WINDOW: Duration = Duration::from_secs(600);

/// How long a single connection may sit without saying anything.
///
/// The listener takes one connection at a time, and a phone that opens one
/// and goes quiet (captive-portal probes often do) would block the portal.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the answer gets to reach the phone before the box reboots.
///
/// [`send`] already waits for the bytes to be acknowledged; this is extra
/// margin.
const SETTLE: Duration = Duration::from_millis(500);

/// Room for the head above the body in the request buffer.
///
/// The browser decides the POST headers: besides the request line, `Host`
/// and the content headers it adds `User-Agent`, `Accept`,
/// `Accept-Language`, `Origin`, `Referer` and five `Sec-Fetch-*` lines. On
/// mobile Chrome these are six to nine hundred bytes.
///
/// Sized from what browsers are known to send; [`handle`] prints the real
/// size.
const HEAD_ROOM: usize = 1024;

/// The request buffer: the largest body, plus room for the head above it.
///
/// `MAX_BODY` (3088) + `HEAD_ROOM` (1024) = 4112 bytes.
///
/// **Not on the stack.** This buffer is held across `await` points, so it is
/// part of the portal's future. If that future were part of `main`'s embassy
/// task, it would be in `.bss`, and `esp-hal` gives the stack only the DRAM
/// that `.bss` leaves. That would shrink the stack below what a normal boot
/// needs (49,024 bytes measured).
///
/// So [`place`] puts the whole future of [`run`] (this buffer, the DHCP
/// buffers and the radio's `StackResources`) into the decoder's
/// [`crate::audio::SCRATCH`], which setup mode never uses.
/// [`crate::audio::take_scratch_bytes`] ensures only one of the two can
/// claim it. With this, the stack region is 50,060 bytes.
const REQUEST: usize = MAX_BODY + HEAD_ROOM;

/// Raises the access point and serves until it is done with.
///
/// Never returns. It resets the box itself once a config has been saved, or
/// once [`SETUP_WINDOW`] is up; if the radio will not start at all it stops
/// where it stands instead.
///
/// `card` is an `Option` because the access point is still useful without a
/// card (a loose card is a common reason to use setup mode). Without a card
/// the page says so and only saving is refused.
///
/// `paint` sets the LED. `main` owns the LED controller and passes a closure
/// instead, since this never reaches `main`'s loop.
pub async fn run(
    wifi: WIFI<'static>,
    uart: UART0<'static>,
    rx: GPIO44<'static>,
    card: Option<Mounted>,
    seed: u64,
    paint: impl Fn(LedState),
) -> ! {
    // Set the LED first: it is the only sign that setup mode is running.
    paint(LedState::Setup);

    // Created here, not inside `serve_console`: `UartRx::new` reconfigures
    // UART0, which `esp-println` uses for output, and doing that during a
    // print garbles it. So drain the output first, then create the receiver
    // before printing anything.
    crate::drain_console();
    let console = UartRx::new(uart, UartConfig::default().with_baudrate(115200))
        .map(|console| console.with_rx(rx));

    // Setup mode has no `stack` command, so the stack use is printed on entry
    // and on exit.
    stack::report();

    // Read first, because it sets the access point's passphrase. If anything
    // goes wrong (no card, unreadable or invalid file), use the published
    // passphrase, so a broken file can always be fixed.
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

    // Created here rather than passed in, so the radio's `StackResources`
    // (3,472 bytes) is part of this future and ends up in the scratch.
    let mut radio = net::Radio::new(wifi);
    let (session, mut link) = match radio.serve(seed, &setup_password) {
        Ok(pair) => pair,
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: portal could not raise the access point — {trouble:?}"
            );
            // Stop rather than reset: the ears are probably still held, so a
            // reset would enter setup mode and fail again, over and over. A
            // red LED shows what happened.
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

    // The runner must be polled all the time (it never returns), alongside
    // the servers. They share one future rather than separate tasks, because
    // the session and link borrow from the radio.
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
/// This keeps setup mode's memory out of the stack region. An `async fn`'s
/// locals that live across an `await` are part of its future, and a future
/// awaited in an embassy task is part of that task's `static` future in
/// `.bss`. Awaiting [`run`] directly from `main` would put all its buffers
/// into `___embassy_main4POOL` on every boot. Placed here, `main` only holds
/// a pointer.
///
/// `room` comes from [`crate::audio::take_scratch_bytes`]: the decoder's
/// scratch, which setup mode never uses.
///
/// Returns `None` if `room` is too small for `future` (after alignment),
/// instead of writing past it. The size is printed either way.
///
/// The borrow uses `room`'s lifetime, not `'static`, because the future
/// captures the LED closure, which borrows `main`'s LED controller.
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

    // SAFETY: `offset + size` is within `room` (checked above) and `offset`
    // comes from `align_offset`, so `slot` is aligned, in bounds and points
    // to `size` writable bytes. `room` is a unique borrow used only here, and
    // the returned `Pin` borrows it, so nothing else can access those bytes
    // while the future lives. The overwritten bytes are the decoder's scratch
    // (plain arrays with no destructor). `Pin::new_unchecked` is sound because
    // the future is only reachable through this pointer, so it cannot move;
    // it is never dropped, and its storage is a `static` that is never reused.
    unsafe {
        let slot = room.as_mut_ptr().add(offset).cast::<F>();
        slot.write(future);
        Some(Pin::new_unchecked(&mut *slot))
    }
}

/// Stops the box where it stands.
///
/// Never returns. For failures where retrying cannot help: a reset with both
/// ears still held would come straight back here.
///
/// Callers set the LED first, since the colour depends on the failure.
pub async fn park() -> ! {
    crate::drain_console();
    stay_put().await
}

/// Never completes, and uses no CPU while waiting.
async fn stay_put() -> ! {
    core::future::pending::<()>().await;
    unreachable!("pending never completes")
}

/// Answers one connection at a time on port 80, for as long as it is polled.
///
/// Never returns. [`run`] ends the portal after its time window, and a save
/// resets the box from inside the handler.
async fn serve_http(stack: Stack<'_>, card: Option<&Mounted>) {
    // The portal's buffers: 1536 + 1536 + 4112 here, `CHUNK` in `send_page`,
    // and 1024 + 1024 + 590 in `serve_dhcp`. None of it is on the stack; see
    // [`REQUEST`].
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
        // One request per connection, then back to listening. `send` has
        // already waited for the response to be acknowledged, so aborting
        // cannot lose it.
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

    // Parsed a second time, because keeping the `Request` from `receive`
    // would borrow the buffer that later reads must fill. Parsing a head is
    // cheap.
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

    // Print the real sizes, to check `HEAD_ROOM` against a real phone.
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
        // Everything else gets an empty 404. A phone probing `/generate_204`
        // or `/hotspot-detect.html` then correctly reports no internet.
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
            // `complete` turns this into `Ok(false)`, meaning read more.
            Err(http::RequestError::Incomplete) => unreachable!("incomplete asks for another read"),
        }

        if filled == buffer.len() {
            // The buffer filled up without the headers ending (a too-long
            // body is caught above by its declared length).
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

/// Writes `CONFIG.TXT` back with `setup_password` set or removed.
///
/// Returns whether the card was written, so the caller only resets after a
/// real write.
///
/// The whole file is parsed before writing, so `teddiebox_config` decides
/// what is valid, and a file that was already broken is not saved. As in
/// `save`: check first, then write.
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

/// Answers the console, so a box in setup mode does not look frozen.
///
/// Setup mode never reaches `main`'s console loop, but the box keeps
/// printing, so without this it would seem to ignore every command.
///
/// Only `rb` (reboot into a normal boot, where `dl` works) and `setup pw` are
/// handled. `dl` itself is not: `reboot_to_download` needs the USB pads and
/// the `Gates`, which setup mode does not have.
///
/// Any other command, including the `dl` that `scripts/flash.sh` sends first,
/// prints a line saying setup mode is running.
async fn serve_console(
    console: Result<UartRx<'static, esp_hal::Blocking>, ConfigError>,
    card: Option<&Mounted>,
) -> ! {
    let mut console = match console {
        Ok(console) => console,
        Err(trouble) => {
            // Not fatal: the access point and page still work.
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
                    // Changes the passphrase of this access point.
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
        // Polled, because `read_buffered` does not block and the other three
        // futures must keep running.
        Timer::after(Duration::from_millis(50)).await;
    }
}

/// Whether what has arrived so far is a whole request.
///
/// Returns a `bool` rather than the `Request`, because the caller needs the
/// buffer back mutably when the answer is `false`.
fn complete(buffer: &[u8]) -> Result<bool, http::RequestError> {
    match http::parse(buffer) {
        Ok(request) => Ok(buffer.len() - request.header_len >= request.content_length),
        Err(http::RequestError::Incomplete) => Ok(false),
        Err(other) => Err(other),
    }
}

/// What the page says when the box has no card to read or write.
///
/// The access point starts even without a card, and the page shows this.
const NO_CARD: &str = "the box could not read its card — check it is pushed in, \
                       then reload this page";

/// `GET /` — the card's file, in the box, as it is.
async fn show(socket: &mut TcpSocket<'_>, card: Option<&Mounted>) {
    let Some(card) = card else {
        return respond_page(socket, b"", Some(NO_CARD)).await;
    };

    let mut config = [0u8; MAX_CONFIG];
    match card.read_config_bytes(&mut config) {
        // `Ok(0)` is a card with no config yet, as on a new box: show an empty
        // textarea, not an error.
        Ok(filled) => respond_page(socket, &config[..filled], None).await,
        Err(why) => respond_page(socket, b"", Some(why)).await,
    }
}

/// `POST /save` — decode, validate, write, reset.
///
/// [`examine`] decides what the submission means (tested on the host); this
/// handles the card and the socket.
async fn save(socket: &mut TcpSocket<'_>, card: Option<&Mounted>, body: &[u8]) {
    let submitted = match examine(body) {
        Submission::Refuse(why) => return respond_error(socket, why).await,
        Submission::HandBack(bytes, why) => return respond_page(socket, &bytes, Some(why)).await,
        Submission::Write(bytes) => bytes,
    };

    // Checked after decoding, so a config typed without a card is still
    // checked and shown back, not lost.
    let Some(card) = card else {
        return respond_page(socket, &submitted, Some(NO_CARD)).await;
    };

    match card.write_config(&submitted) {
        // Only reset after a successful write. After a failure the box stays
        // here so it can be tried again.
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
/// For a form that did not decode or is not text. Browsers do not send
/// either, so nothing typed is lost.
async fn respond_error(socket: &mut TcpSocket<'_>, message: &str) {
    send_page(socket, http::Status::BadRequest, b"", Some(message)).await
}

/// The last thing the phone sees before the network disappears.
///
/// Its own page rather than [`page::pieces`], which always has the form: a
/// Save button would be pointless while the box reboots.
async fn respond_saved(socket: &mut TcpSocket<'_>) {
    send(socket, http::Status::Ok, SAVED.as_bytes()).await
}

/// Self-contained, like `page.rs`: the phone cannot load anything from the
/// internet on this network.
const SAVED: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>teddiebox setup</title><style>\
body{font:16px system-ui;margin:0;padding:1rem;background:#f6f5f3;color:#1a1a1a}\
</style></head><body><h1>saved</h1>\
<p>The box is restarting. This network will disappear on its own.</p>\
</body></html>";

/// How much of an escaped run is held at a time.
///
/// Must be at least as long as the longest escape (`&quot;`, six bytes), or
/// [`page::escape_chunk`] cannot make progress. Beyond that, it only sets how
/// many writes a page takes.
const CHUNK: usize = 64;

/// Sends the page under the given status, a piece at a time.
///
/// The page is never held in memory: [`page::length`] gives the size for the
/// header, and the pieces follow. A full `CONFIG.TXT` (1024 bytes, up to
/// 6144 escaped) would otherwise need a 7.6 KB buffer.
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
            // Cannot happen, since `CHUNK` is larger than the longest escape;
            // but never loop forever.
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
/// The flush makes it safe to close the connection, or reset the box, right
/// afterwards.
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
/// `TcpSocket::write` may take only part of the slice, so this loops.
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
/// Never returns: a phone may renew its lease, so this keeps listening.
async fn serve_dhcp(stack: Stack<'_>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx = [0u8; 1024];
    let mut buffer = [0u8; dhcp::MAX_DATAGRAM];

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    if let Err(trouble) = socket.bind(67) {
        esp_println::println!("teddiebox: portal could not bind dhcp — {trouble:?}");
        // HTTP still works for a phone with a manual static address. Wait
        // here instead of returning, which would end the whole `select4`.
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

        // Broadcast: the client has no address yet (its source is 0.0.0.0),
        // so a unicast reply would go nowhere.
        let datagram = dhcp::reply(&incoming, answer);
        if let Err(trouble) = socket
            .send_to(&datagram, (Ipv4Address::BROADCAST, 68))
            .await
        {
            esp_println::println!("teddiebox: portal dhcp send failed — {trouble:?}");
        }

        // Printed after the reply is sent, so the client is not delayed. Shows
        // on the console that DHCP is working.
        esp_println::println!("teddiebox: portal dhcp {:?}, {n} bytes", incoming.kind);
    }
}
