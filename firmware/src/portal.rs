//! The setup portal: one page, one form, one lease.
//!
//! Reached by holding both ears through a power-on. While this runs, the box
//! is not a teddy bear — no decoder, no codec, no NFC — which is what makes
//! the stack for a TCP server and a DHCP server affordable at all.
//!
//! The wire formats live in `teddiebox_portal` and are tested on the host.
//! What is here is the part that cannot be: the access point, the two
//! sockets, the card, and the order the save path does things in.
//!
//! Nothing here has been run on hardware. It compiles and links, which is the
//! whole of what is known about it.

use embassy_futures::select::{select, select3, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ipv4Address, Stack};
use embassy_time::{Duration, Timer};
use teddiebox_core::LedState;
use teddiebox_portal::{dhcp, form, http, page, MAX_BODY, MAX_CONFIG};

use crate::net;
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
/// `MAX_BODY` (3088) + `HEAD_ROOM` (1024) = 4112 bytes, on the stack of
/// [`serve_http`]. Affordable only because this mode starts no decoder — that
/// alone wants 15.8 KB — and no codec, no NFC and no media loop either.
const REQUEST: usize = MAX_BODY + HEAD_ROOM;

/// Raises the access point and serves until it is done with.
///
/// Never returns. It resets the box itself once a config has been saved, or
/// once [`SETUP_WINDOW`] is up; if the radio will not start at all it stops
/// where it stands instead.
///
/// `paint` is how this reaches the LED. `main` owns the LEDC controller and
/// never hands it away — its own loop is what paints for every other mode,
/// and this function never reaches that loop — so it hands down a closure
/// over the controller instead, built from the same `Gates` and `Rgb` the
/// rest of the boot uses. Nothing here knows how a colour becomes light; it
/// only knows which colour a state is.
pub async fn run(
    radio: &mut net::Radio<'_>,
    card: &Mounted,
    seed: u64,
    paint: impl Fn(LedState),
) -> ! {
    // Painted before the access point is even asked for. This is the only
    // feedback setup mode has — no screen, and by the time this runs, no
    // decoder or codec either — so a dark LED here would read exactly like a
    // box that failed to start rather than one waiting to be talked to.
    paint(LedState::Setup);

    let (session, mut link) = match radio.serve(seed) {
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
        "teddiebox: portal up — join `{}` and open http://192.168.4.1/",
        net::SETUP_SSID
    );

    // The runner has to be polled throughout rather than awaited first: it
    // never returns, and neither server makes any progress without it. The
    // three share one stack frame instead of three tasks because the session
    // and the link are borrowed out of the radio and cannot be handed away.
    let stack = session.stack();
    let serving = select3(link.run(), serve_http(stack, card), serve_dhcp(stack));
    match select(Timer::after(SETUP_WINDOW), serving).await {
        Either::First(()) => esp_println::println!(
            "teddiebox: portal's {} minutes are up — restarting",
            SETUP_WINDOW.as_secs() / 60
        ),
        Either::Second(_) => unreachable!("none of the three ever returns"),
    }

    crate::drain_console();
    esp_hal::system::software_reset();
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
async fn park() -> ! {
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
async fn serve_http(stack: Stack<'_>, card: &Mounted) {
    // The portal's buffer claim, named in one place rather than spread
    // through the call tree: 1536 + 1536 + 4112 here, `CHUNK` in `send_page`
    // below, and 1024 + 1024 + 590 in `serve_dhcp` beside it.
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
async fn handle(socket: &mut TcpSocket<'_>, card: &Mounted, buffer: &mut [u8]) {
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
        return send_page(
            socket,
            http::Status::BadRequest,
            b"",
            Some("that request did not make sense to the box"),
        )
        .await;
    };

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

/// `GET /` — the card's file, in the box, as it is.
async fn show(socket: &mut TcpSocket<'_>, card: &Mounted) {
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
async fn save(socket: &mut TcpSocket<'_>, card: &Mounted, body: &[u8]) {
    // One arm per variant here too: `TooLong` is the only one of the three
    // that is about the *file* rather than about the request carrying it, and
    // it is the one somebody can do something about. Saying "did not arrive
    // intact" to a config that is simply too big sends them looking at their
    // phone instead of at their file.
    let submitted: heapless::Vec<u8, MAX_CONFIG> = match form::field(body, "config") {
        Ok(bytes) => bytes,
        Err(form::FormError::TooLong) => {
            return respond_error(socket, "that config is longer than the box will hold").await
        }
        Err(form::FormError::NotFound) | Err(form::FormError::BadEscape) => {
            return respond_error(socket, "that form did not arrive intact").await
        }
    };

    // Validate before writing, never after. A file the box will refuse at its
    // next boot must not reach the card: the person who would find out is
    // whoever picks up a box that no longer works, with no clue why.
    let text = match core::str::from_utf8(&submitted) {
        Ok(text) => text,
        Err(_) => return respond_error(socket, "that is not text").await,
    };
    if let Err(trouble) = teddiebox_config::Config::parse(text) {
        // The submitted bytes go back into the textarea, so a typo costs a
        // correction rather than a retype.
        return respond_page(socket, &submitted, Some(describe(trouble))).await;
    }

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

/// Says what went wrong in words somebody can act on.
///
/// One arm per variant and no catch-all, so a new [`teddiebox_config::ConfigError`]
/// makes this fail to compile rather than quietly telling everybody "invalid".
fn describe(trouble: teddiebox_config::ConfigError) -> &'static str {
    use teddiebox_config::ConfigError::*;
    match trouble {
        MissingSsid => "no ssid line — the box needs a network name",
        MissingServer => "no server line — the box needs somewhere to fetch from",
        ValueTooLong => "one of those values is too long for the box to hold",
        MalformedLine => "a line without an = on it",
        MalformedValue => "a key was given a value it does not accept",
        Truncated => "that config is longer than the box will read",
        NotText => "that is not text",
        EmptyUpdateUrl => "update_url is there but empty — give it a URL or remove it",
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
        // than return from it — returning completes the `select3` and takes
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
    }
}
