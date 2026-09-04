//! TLS, and only the one handshake this LAN's teddyCloud offers.
//!
//! The server is teddyCloud built on CycloneSSL with `TLS_MIN_VERSION` and
//! `TLS_MAX_VERSION` both pinned to 1.2, so **TLS 1.3 is not on the table** and
//! `embedded-tls`, which speaks 1.3 and nothing else, cannot reach it. This
//! module wraps `mbedtls-rs` instead, cut down in `Cargo.toml` to
//! ECDHE-RSA-AES256-GCM-SHA384 over secp256r1.
//!
//! It sits between two things that already agree about their interface:
//! `embassy_net::tcp::TcpSocket` is `embedded-io-async` 0.7, and so is
//! [`Session`], and so is `teddiebox-cloud`. Nothing here adapts anything; the
//! session is a socket-shaped thing that happens to encrypt.
//!
//! **Certificates are not checked yet.** See [`client_config`].

use core::ffi::CStr;

use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::{Duration, Instant};
use mbedtls_rs::{
    AuthMode, ClientSessionConfig, Session, SessionConfig, SessionError, Tls, TlsReference,
    TlsVersion,
};
use static_cell::StaticCell;
use teddiebox_cloud::stream::{self, Begun, Body};
use teddiebox_cloud::{CloudError, ContentRequest, Route};
use teddiebox_core::checksum::Crc32;

/// How long to wait for the TCP connect and the handshake.
///
/// Generous, because the handshake is RSA and ECDHE in software: this build
/// leaves out the `esp32s3` feature that routes those onto the chip's crypto
/// accelerators, because that feature drags in `esp-hal ~1.1.0` and this
/// firmware is on `1.2.0-rc.0`. How long it actually takes is a bench
/// measurement nobody has taken.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Socket buffers for the TLS transport.
///
/// Separate from mbedtls's own record buffers, which are sized by the
/// `ssl-in-content-len-*` features and come out of the heap.
const TCP_BUFFER: usize = 2048;

/// Longest `host:port` this will take, plus room for the NUL.
///
/// `mbedtls` wants the server name as a C string, and there is no allocator to
/// build one in.
const MAX_NAME: usize = 80;

/// Why a TLS connection could not be made.
///
/// The `SessionError` payloads look unread to rustc because nothing destructures
/// them — they reach a human through `Debug`, on the console, which dead-code
/// analysis does not count. They earn their place: `CIPHER_ALLOC_FAILED` and
/// `CA_CHAIN_REQUIRED` are both "the handshake failed" without them, and one is
/// a heap that needs growing while the other is a setting.
#[derive(Debug)]
#[allow(dead_code)]
pub enum Error {
    /// `server` in the config was not `host:port`.
    MalformedServer,
    /// The host name did not resolve.
    NoSuchHost,
    /// The TCP connection could not be made.
    Connect,
    /// The TLS handshake failed.
    ///
    /// Carries mbedtls's own code, because "the handshake failed" is not a
    /// diagnosis: a missing cipher suite, a record buffer too small for the
    /// certificate chain and a hash algorithm left out of the build all look
    /// identical without it.
    Handshake(SessionError),
    /// Reading or writing the encrypted stream failed.
    ///
    /// `mbedtls-rs` runs the handshake lazily on first use, so this is where a
    /// handshake failure actually surfaces.
    Stream(SessionError),
    /// The server answered, and the answer was not a body.
    Cloud(CloudError),
    /// The server has nothing filed under that identifier.
    NoContent,
}

/// What a fetch found. Nothing is written to the card.
pub struct Fetched {
    pub bytes: u32,
    pub crc32: u32,
    pub seconds: u32,
}

/// The one-time mbedtls context, and the RNG it holds.
///
/// `Tls::new` wants a `&'static mut`, so both live in `StaticCell`s: this is
/// initialised once and lives as long as the firmware.
static RNG: StaticCell<HardwareRng> = StaticCell::new();
static TLS: StaticCell<Tls<'static>> = StaticCell::new();

/// The ESP32's random number generator, in the shape `rand_core` 0.10 asks for.
///
/// `esp-hal` implements `rand_core` 0.6 and 0.9; `mbedtls-rs` wants 0.10, which
/// renamed `RngCore` to `Rng` and made `CryptoRng` an alias for an infallible
/// `TryCryptoRng`. So this is a shim over `random()` rather than a generator.
///
/// **It is only sound while the radio is on.** The ESP32's RNG is documented as
/// cryptographically secure only when the RF subsystem is running, and it is
/// exactly then that this gets used — a TLS session exists only inside a
/// [`crate::net::Session`], which exists only while the modem is powered.
pub struct HardwareRng(esp_hal::rng::Rng);

impl rand_core::TryRng for HardwareRng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(self.0.random())
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok((u64::from(self.0.random()) << 32) | u64::from(self.0.random()))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        // A word at a time, and the tail takes only the bytes it needs. The
        // last chunk is short whenever the length is not a multiple of four,
        // and `copy_from_slice` would panic on a length mismatch rather than
        // quietly leave the end unfilled.
        for chunk in dst.chunks_mut(4) {
            let word = self.0.random().to_ne_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }
}

// `rand_core` 0.10 blanket-implements `Rng` for any infallible `TryRng`, and
// `CryptoRng` for any `TryCryptoRng<Error = Infallible>`, so those two are all
// that has to be written by hand.

/// Asserts the generator is fit for keys, which the paragraph on
/// [`HardwareRng`] is the argument for.
impl rand_core::TryCryptoRng for HardwareRng {}

/// Builds the mbedtls context. Call once.
///
/// Returns `None` if it has already been called, because the statics behind it
/// can only be filled once and a second caller would otherwise panic.
pub fn init() -> Option<TlsReference<'static>> {
    let rng = RNG.try_init(HardwareRng(esp_hal::rng::Rng::new()))?;
    let tls = Tls::new(rng).ok()?;
    Some(TLS.try_init(tls)?.reference())
}

/// The client configuration for one connection.
///
/// **`insecure` turns certificate checking off entirely**, and until the box
/// knows what time it is that is the only setting that works. teddyCloud signs
/// with its own root and sends that root in the chain, so trusting it is a
/// solved problem — but a certificate's validity dates cannot be judged without
/// a clock, the box's time starts at zero every boot, and this certificate's
/// window opens in 2004. A box that believes it is 1970 refuses a good
/// certificate.
///
/// So the honest description of `insecure = yes` is *encrypted but not
/// authenticated*, which is a defensible thing to be on a LAN and an
/// indefensible one anywhere else. Turning it off is what SNTP plus an embedded
/// `CN=TeddyCloud CA Root Certificate` buys.
///
/// `min_version` is pinned to 1.2 rather than left at the default because this
/// server offers nothing else; a build that quietly negotiated something else
/// would be talking to a server this project has not measured.
pub fn client_config<'a>(insecure: bool, server_name: &'a CStr) -> ClientSessionConfig<'a> {
    ClientSessionConfig {
        server_name: Some(server_name),
        auth_mode: if insecure {
            AuthMode::None
        } else {
            AuthMode::Required
        },
        min_version: TlsVersion::Tls1_2,
        ..ClientSessionConfig::new()
    }
}

/// Splits `host:port`, the way it is written in `CONFIG.TXT`.
///
/// Returns the host with a NUL appended, because that is the only form
/// `mbedtls` will take it in.
fn split_server(server: &str, name: &mut [u8; MAX_NAME]) -> Result<(usize, u16), Error> {
    let (host, port) = server.rsplit_once(':').ok_or(Error::MalformedServer)?;
    let port: u16 = port.parse().map_err(|_| Error::MalformedServer)?;
    let bytes = host.as_bytes();
    // One byte over is not a truncation to paper over: a name cut short would
    // resolve to something else or to nothing, and say neither.
    if bytes.is_empty() || bytes.len() + 1 > name.len() {
        return Err(Error::MalformedServer);
    }
    name[..bytes.len()].copy_from_slice(bytes);
    name[bytes.len()] = 0;
    Ok((bytes.len() + 1, port))
}

/// Opens a TLS connection and asks the server one question.
///
/// A `HEAD` for `/`, which every teddyCloud answers and which costs one record
/// in each direction — enough to prove the handshake, the cipher suite and the
/// record layer without pulling a body through a buffer sized for headers.
///
/// **Not yet run against hardware.** It compiles and links, which is the whole
/// of what is known about it.
pub async fn probe(
    tls: TlsReference<'_>,
    stack: &Stack<'_>,
    server: &str,
    insecure: bool,
) -> Result<(), Error> {
    let mut name = [0u8; MAX_NAME];
    let (name_len, port) = split_server(server, &mut name)?;
    let host = core::str::from_utf8(&name[..name_len - 1]).map_err(|_| Error::MalformedServer)?;

    let addresses = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|_| Error::NoSuchHost)?;
    let address = *addresses.first().ok_or(Error::NoSuchHost)?;
    esp_println::println!(
        "teddiebox: tls {host}:{port} is {address}, certificates {}",
        if insecure { "NOT checked" } else { "checked" }
    );

    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let mut socket = TcpSocket::new(*stack, &mut rx, &mut tx);
    socket.set_timeout(Some(CONNECT_TIMEOUT));
    socket
        .connect((address, port))
        .await
        .map_err(|_| Error::Connect)?;
    esp_println::println!("teddiebox: tls connected, starting the handshake");

    // The heap is the Wi-Fi driver's, and mbedtls is a guest on it. A session
    // allocates its record buffers and cipher contexts here, and when it does
    // not fit the failure is `MBEDTLS_ERR_CIPHER_ALLOC_FAILED` — or, worse, a
    // fatal alert from the server, which reads like a protocol fault rather
    // than an out-of-memory. So the number is printed either way.
    esp_println::println!(
        "teddiebox: tls heap free {} bytes before the session",
        esp_alloc::HEAP.free()
    );

    let server_name =
        CStr::from_bytes_with_nul(&name[..name_len]).map_err(|_| Error::MalformedServer)?;
    let mut session = Session::new(
        tls,
        socket,
        &SessionConfig::Client(client_config(insecure, server_name)),
    )
    .map_err(Error::Handshake)?;

    use mbedtls_rs::io::Write;
    session
        .write_all(b"HEAD / HTTP/1.1\r\nHost: ")
        .await
        .map_err(Error::Stream)?;
    session
        .write_all(host.as_bytes())
        .await
        .map_err(Error::Stream)?;
    session
        .write_all(b"\r\nConnection: close\r\n\r\n")
        .await
        .map_err(Error::Stream)?;

    let mut buffer = [0u8; 128];
    let read = session.read(&mut buffer).await.map_err(Error::Stream)?;
    // The status line and nothing more. Enough to say the record layer works
    // in both directions, which is all this is for.
    let line = core::str::from_utf8(&buffer[..read])
        .unwrap_or("<not utf-8>")
        .lines()
        .next()
        .unwrap_or("<empty>");
    esp_println::println!(
        "teddiebox: tls server said {line} — heap free {} bytes with the session open",
        esp_alloc::HEAP.free()
    );

    let _ = session.close().await;
    Ok(())
}

/// Downloads one content file and checksums it, **writing nothing**.
///
/// This is the transport and protocol proven end to end, on its own: TLS, the
/// request shape, the range rule and the body stream, against a file whose
/// length and checksum the host already knows. Keeping the card out of it means
/// a first failure is in one of those and not in the filesystem — and it side-
/// steps, for now, the open question of how a download and the media task share
/// a card that `embedded-sdmmc` will not open twice.
///
/// `ruid` is the identifier as it appears in the URL. `ContentRequest` reverses
/// what it is given, so it is handed over backwards to come out the right way.
///
/// **Route V1, deliberately.** `/v2` accepts the connection on this server and
/// then never answers; `/v1` returns the file. Measured, unexplained, and
/// written up in the design.
pub async fn fetch(
    tls: TlsReference<'_>,
    stack: &Stack<'_>,
    server: &str,
    insecure: bool,
    ruid: [u8; 8],
) -> Result<Fetched, Error> {
    let mut name = [0u8; MAX_NAME];
    let (name_len, port) = split_server(server, &mut name)?;
    let host = core::str::from_utf8(&name[..name_len - 1]).map_err(|_| Error::MalformedServer)?;

    let addresses = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|_| Error::NoSuchHost)?;
    let address = *addresses.first().ok_or(Error::NoSuchHost)?;

    // The socket and the session borrow these, so they have to be built in the
    // frame that uses them — which is why this repeats `probe`'s setup rather
    // than calling a helper that returns a session.
    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let mut socket = TcpSocket::new(*stack, &mut rx, &mut tx);
    socket.set_timeout(Some(CONNECT_TIMEOUT));
    socket
        .connect((address, port))
        .await
        .map_err(|_| Error::Connect)?;

    let server_name =
        CStr::from_bytes_with_nul(&name[..name_len]).map_err(|_| Error::MalformedServer)?;
    let mut session = Session::new(
        tls,
        socket,
        &SessionConfig::Client(client_config(insecure, server_name)),
    )
    .map_err(Error::Handshake)?;

    let mut uid = ruid;
    uid.reverse();
    let request = ContentRequest {
        uid,
        route: Route::V1,
        etag: None,
        server,
        from: None,
    };

    let started = Instant::now();
    let mut buf = [0u8; 1024];
    let begun = stream::begin(&mut session, &request, &mut buf)
        .await
        .map_err(Error::Cloud)?;

    let (body_length, prefix) = match begun {
        Begun::NotFound | Begun::Unchanged => return Err(Error::NoContent),
        Begun::Content {
            body_length,
            prefix,
            ..
        } => (body_length, prefix),
    };

    let mut crc = Crc32::new();
    crc.update(&buf[prefix.clone()]);
    let mut received = prefix.len() as u32;
    let mut body = Body::new(body_length, received);
    esp_println::println!("teddiebox: get {body_length} bytes to read");

    // A forty-megabyte file takes long enough that silence is indistinguishable
    // from a hang, which is the thing this bench has mistaken twice already.
    let mut announced = 0u32;
    while !body.is_complete() {
        let n = body
            .read(&mut session, &mut buf)
            .await
            .map_err(Error::Cloud)?;
        crc.update(&buf[..n]);
        received += n as u32;
        if received - announced >= 1_048_576 {
            announced = received;
            esp_println::println!("teddiebox: get {received}/{body_length}");
        }
    }

    let _ = session.close().await;
    Ok(Fetched {
        bytes: received,
        crc32: crc.finish(),
        seconds: started.elapsed().as_secs() as u32,
    })
}
