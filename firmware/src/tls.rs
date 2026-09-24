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
//! The server is verified against a CA read from the card. See
//! [`client_config`] and [`SERVER_IDENTITY`].

use core::cell::RefCell;
use core::ffi::CStr;
use critical_section::Mutex as CsMutex;

use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{AES, RSA, SHA};
use mbedtls_rs::sys::hook::backend::esp::{EspAccel, EspAccelQueue, EspHooksGuard};
use mbedtls_rs::{
    AuthMode, Certificate, ClientSessionConfig, Credentials, PrivateKey, Session, SessionConfig,
    SessionError, Tls, TlsReference, TlsVersion, X509,
};
use static_cell::StaticCell;
use teddiebox_cloud::stream::{self, Begun, Body, Probed};
use teddiebox_cloud::{CloudError, ContentRequest, ETag, Route};
use teddiebox_core::checksum::Crc32;
use teddiebox_download::Continue;

/// How long to wait for the TCP connect and the handshake.
///
/// Still generous. The SHA, RSA and AES peripherals now carry the handshake
/// (see [`init`]), but the ECDHE point multiplication has no accelerator on
/// this chip and stays in software, so the handshake is not uniformly fast.
/// How long it actually takes is a bench measurement nobody has taken.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the data socket may sit idle before `embassy-net` abandons it.
///
/// Longer than the connect timeout on purpose: once a download is throttled it
/// stops reading for as long as a story plays, and at twenty seconds the
/// connection would be aborted a few frames in. teddyCloud advertises
/// `Keep-Alive: timeout=300`, so this sits under what the server will hold and
/// above any pause the throttle can impose.
const IDLE_TIMEOUT: Duration = Duration::from_secs(240);

/// How long to wait before asking the throttle again.
const THROTTLE_WAIT: Duration = Duration::from_millis(50);

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
    /// Nobody wants these bytes any more — the figure that asked for them was
    /// lifted. Not a failure: the transfer was told to stop and did.
    Abandoned,
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

/// How much room each of the box's two credential files is given.
///
/// The real ones are 1030 and 1190 bytes; this leaves room for a re-issued
/// certificate without silently failing to fit, and a file that fills its
/// buffer is refused rather than truncated.
pub const CERT_BYTES: usize = 1536;

/// The common name teddyCloud's server certificate carries.
///
/// `mbedtls_ssl_set_hostname` sets two things at once: the name a certificate
/// is verified against, *and* the SNI extension sent to the server. That looks
/// like a dilemma, because neither value available here works for both — this
/// CN contains a space and is not a valid SNI host, the real host
/// `teddycloud.local` matches nothing in the certificate, and leaving it unset
/// makes mbedtls refuse to verify at all with
/// `CERTIFICATE_VERIFICATION_WITHOUT_HOSTNAME`.
///
/// It is not a dilemma, because the two are only welded together by a build
/// option. `MBEDTLS_SSL_SERVER_NAME_INDICATION` guards the client's SNI write
/// and nothing else; certificate verification reads the same hostname through
/// `get_hostname_for_verification`, which that option does not touch. So
/// `scripts/vendor-mbedtls-rs-sys.sh` takes that define out of the vendored
/// crate — it cannot be done from `Cargo.toml`, because `mbedtls-rs` names
/// `tls-core` in its own dependency rather than behind a feature. No SNI goes
/// out, and this name does exactly one job: naming the entity to verify.
///
/// Sending an SNI is not merely useless here, it is harmful. teddyCloud
/// switches certificates on it — any non-empty SNI selects a `tbs2.tonie.cloud`
/// chain on secp384r1, a curve this build does not carry, and the handshake
/// ends in `MBEDTLS_ERR_SSL_FATAL_ALERT_MESSAGE`. Without one it answers with
/// the RSA certificate this constant names, issued by the CA on the card.
///
/// Hardcoded because it comes from teddyCloud's own certificate generation
/// rather than from this deployment; if it ever varies it belongs in the card's
/// config beside the server address.
const SERVER_IDENTITY: &CStr = c"TeddyCloud Server";

/// The box's certificate and the key that proves it.
///
/// Held for the life of the firmware because a session borrows the certificate
/// rather than copying it, and there is a session per download.
struct Identity {
    certificate: [u8; CERT_BYTES],
    certificate_len: usize,
    key: [u8; CERT_BYTES],
    key_len: usize,
}

/// The authority the server's certificate is checked against.
struct Anchor {
    certificate: [u8; CERT_BYTES],
    len: usize,
}

static ANCHOR_CELL: StaticCell<Anchor> = StaticCell::new();
static ANCHOR: CsMutex<RefCell<Option<&'static Anchor>>> = CsMutex::new(RefCell::new(None));

/// Publishes the CA the server is verified against, once.
pub fn set_anchor(certificate: &[u8]) -> bool {
    if certificate.len() > CERT_BYTES {
        return false;
    }
    let mut held = Anchor {
        certificate: [0; CERT_BYTES],
        len: certificate.len(),
    };
    held.certificate[..certificate.len()].copy_from_slice(certificate);
    match ANCHOR_CELL.try_init(held) {
        Some(anchor) => {
            critical_section::with(|cs| *ANCHOR.borrow_ref_mut(cs) = Some(anchor));
            true
        }
        None => false,
    }
}

fn anchor() -> Option<Certificate<'static>> {
    let held = critical_section::with(|cs| *ANCHOR.borrow_ref(cs))?;
    Certificate::new_no_copy(&held.certificate[..held.len]).ok()
}

impl Identity {
    fn certificate(&self) -> &[u8] {
        &self.certificate[..self.certificate_len]
    }

    fn key(&self) -> &[u8] {
        &self.key[..self.key_len]
    }
}

static IDENTITY_CELL: StaticCell<Identity> = StaticCell::new();
static IDENTITY: CsMutex<RefCell<Option<&'static Identity>>> = CsMutex::new(RefCell::new(None));

/// Publishes the box's identity, once.
///
/// Called by [`crate::identity::load`], once at boot, reading flash. Returns
/// whether it took: a second call is refused rather than replacing
/// credentials a session may be holding.
pub fn set_identity(certificate: &[u8], key: &[u8]) -> bool {
    if certificate.len() > CERT_BYTES || key.len() > CERT_BYTES {
        return false;
    }
    let mut held = Identity {
        certificate: [0; CERT_BYTES],
        certificate_len: certificate.len(),
        key: [0; CERT_BYTES],
        key_len: key.len(),
    };
    held.certificate[..certificate.len()].copy_from_slice(certificate);
    held.key[..key.len()].copy_from_slice(key);

    match IDENTITY_CELL.try_init(held) {
        Some(id) => {
            critical_section::with(|cs| *IDENTITY.borrow_ref_mut(cs) = Some(id));
            true
        }
        None => false,
    }
}

fn identity() -> Option<&'static Identity> {
    critical_section::with(|cs| *IDENTITY.borrow_ref(cs))
}

/// The one-time mbedtls context, and the RNG it holds.
///
/// `Tls::new` wants a `&'static mut`, so both live in `StaticCell`s: this is
/// initialised once and lives as long as the firmware.
static RNG: StaticCell<HardwareRng> = StaticCell::new();
static TLS: StaticCell<Tls<'static>> = StaticCell::new();

/// The crypto accelerators, and the hooks that route mbedtls onto them.
///
/// `mbedtls-rs` treats dropping the hook guard as undefined behaviour while
/// mbedtls state initialised under the hooks is still alive, and dropping a
/// running backend out from under a registered hook leaves the next call on
/// that algorithm waiting on a queue nobody services. [`TLS`] above is exactly
/// such state and is never dropped, so neither may these — hence statics,
/// rather than values held in a scope that could end.
static ACCEL: StaticCell<EspAccel<'static>> = StaticCell::new();
static QUEUES: StaticCell<EspAccelQueue<'static, 'static>> = StaticCell::new();
static HOOKS: StaticCell<EspHooksGuard<'static>> = StaticCell::new();

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

/// Starts the crypto accelerators and builds the mbedtls context. Call once.
///
/// The three peripherals are taken by value because that is the only way to
/// say "nothing else may drive them". Nothing else in this firmware does; if
/// something ever wants to, it has to take them from here rather than from
/// `Peripherals::take`, and the type system will say so.
///
/// **The order in this function is the whole point.** `mbedtls-rs` routes an
/// algorithm to hardware in two independent steps: a *backend* services the
/// `esp-hal` work queue, and a *hook* points mbedtls at that queue's client.
/// A hooked algorithm whose queue nobody services blocks forever, and a hook
/// registered after mbedtls state already exists leaves that state holding a
/// software context the hardware path cannot read. So: start the backends,
/// hook exactly what was started, and only then call `Tls::new` — which
/// initialises the CTR-DRBG, the first mbedtls state in this firmware.
/// `EspAccelQueue::hook` is what makes the first half of that safe by
/// construction; the second half is this ordering.
///
/// SHA-256/384/512 and RSA carry the handshake, AES-GCM carries the bulk. The
/// ESP32-S3 has no ECC peripheral, so the ECDHE point multiplication stays in
/// software — `EspAccel` has no `with_ecc` on this chip at all.
///
/// Returns `None` if it has already been called, because the statics behind it
/// can only be filled once and a second caller would otherwise panic.
pub fn init(sha: SHA<'static>, rsa: RSA<'static>, aes: AES<'static>) -> Option<Client> {
    let accel = ACCEL.try_init(EspAccel::new().with_sha(sha).with_rsa(rsa).with_aes(aes))?;
    let queues: &'static EspAccelQueue<'static, 'static> = QUEUES.try_init(accel.start())?;

    // SAFETY: the hooks are registered before the first mbedtls call in this
    // firmware — `Tls::new` below is that call, and nothing reaches mbedtls
    // earlier. `hook` selects exactly the algorithms whose queues `queues`
    // services, so none of them can block on an unserviced queue. The guard
    // goes into a static and is never dropped, so it cannot be unregistered
    // while the `Tls` below is alive.
    let hooks = unsafe { queues.hook() };
    HOOKS.try_init(hooks)?;

    let rng = RNG.try_init(HardwareRng(esp_hal::rng::Rng::new()))?;
    let tls = Tls::new(rng).ok()?;
    Some(Client {
        tls: TLS.try_init(tls)?.reference(),
        credentials: RefCell::new(None),
    })
}

/// What every connection needs and none of them should rebuild: the mbedtls
/// context, and the box's identity parsed once.
///
/// **The parse is the expensive part, and not for the reason it looks.**
/// Parsing takes 2-3 ms. But mbedtls blinds every RSA private operation, and
/// the first one on a freshly parsed key sets that blinding up with a
/// constant-time modular inverse in software: measured on the box at 1.36 s,
/// inside a handshake call that does not yield. Parsed per session, every
/// handshake paid it; parsed once, only the first after boot does — 2.68 s
/// against 1.27-1.36 s for the ones after.
///
/// Owned by the net task and lent to each connection, rather than held in a
/// static: the parsed key's reference count is not atomic, and one owner is
/// the whole of what keeps that sound.
pub struct Client {
    tls: TlsReference<'static>,
    /// Filled on first use, not at [`init`]: the net task can build this
    /// before `identity::load` has run, and a key that was not there yet must
    /// not be remembered as absent.
    credentials: RefCell<Option<Credentials<'static>>>,
}

impl Client {
    /// The box's credentials, sharing one parsed key across every session.
    fn credentials(&self) -> Option<Credentials<'static>> {
        let mut held = self.credentials.borrow_mut();
        if held.is_none() {
            *held = parse_credentials();
        }
        held.clone()
    }
}

/// The client configuration for one connection.
///
/// **The clock is not the obstacle it looked like.** This build compiles
/// mbedtls with `MBEDTLS_HAVE_TIME_DATE` undefined, so `notBefore` and
/// `notAfter` are never examined — the checks are inside `#if defined(...)` in
/// `x509_crt.c` and are simply absent. A box that has no idea what year it is
/// can still verify a chain; it just cannot notice an expired certificate.
///
/// So the server *is* verified, on every connection and with no way to ask for
/// less: the chain is checked against `CN=TeddyCloud CA Root Certificate`,
/// which the card supplies as `TCCA.DER`. A card without it cannot download —
/// there is no anchor to check against, and that is the intended failure rather
/// than a reason to skip the check.
///
/// **The name checked is the server's identity, not its address.** teddyCloud's
/// certificate carries no `subjectAltName` and a common name of "TeddyCloud
/// Server", which is not a hostname and will never match `teddycloud.local`. So
/// that CN is what is asserted here, and mbedtls falls back to comparing
/// against it when there is no SAN.
///
/// That is a real check rather than a formality: it says the chain reaches our
/// CA *and* the certificate names the entity we meant to reach. What it does
/// not say is that `teddycloud.local` resolves to that entity — against a private
/// authority signing exactly one server, that distinction is thin, but it is
/// the honest description.
///
/// Leaving the name unset is not an option: mbedtls refuses to verify at all
/// without one, with `CERTIFICATE_VERIFICATION_WITHOUT_HOSTNAME`. It
/// distinguishes "forgot" from "deliberately opted out", and this wrapper can
/// only express the former.
///
/// `min_version` is pinned to 1.2 rather than left at the default because this
/// server offers nothing else; a build that quietly negotiated something else
/// would be talking to a server this project has not measured.
/// Builds the session configuration for a checked connection.
///
/// **Checking the server is not optional, but identifying the box to it is not
/// conditional either.** `creds` is whatever the caller passed — and the fetch
/// passes the box's own certificate and key, read from flash at boot — while
/// the tag's 32-byte token rides the same session as an `Authorization: BD …`
/// header. So whatever answers at `server` gets a client-certificate-
/// authenticated session it can relay to the real teddyCloud, plus a token
/// that authorises a cloud fetch for the figure on the plate. The private key
/// itself never leaves the box; the ability to use it for the length of one
/// handshake does.
///
/// Verifying the chain narrows who that can be to whoever holds a certificate
/// the card's CA signed — but `server` is still a name the card gives, and
/// keeping `CONFIG.TXT` right is still what protects this.
pub fn client_config<'a>(creds: Option<Credentials<'a>>) -> ClientSessionConfig<'a> {
    ClientSessionConfig {
        creds,
        ca_chain: anchor(),
        // Names the entity to verify, and — because this build carries no
        // SNI — nothing else. See [`SERVER_IDENTITY`].
        server_name: Some(SERVER_IDENTITY),
        auth_mode: AuthMode::Required,
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

/// Resolves a `host:port` string to something [`connect`] can dial.
///
/// The name is borrowed out of the caller's buffer rather than returned by
/// value: `split_server` writes a NUL-terminated name into a fixed array for
/// mbedtls, and the `&str` view of it has to keep pointing at the caller's
/// frame. That is the whole reason this takes `name` instead of owning one.
async fn resolve<'a>(
    stack: &Stack<'_>,
    server: &str,
    name: &'a mut [u8; MAX_NAME],
) -> Result<(&'a str, IpAddress, u16), Error> {
    let (name_len, port) = split_server(server, name)?;
    let host = core::str::from_utf8(&name[..name_len - 1]).map_err(|_| Error::MalformedServer)?;

    let addresses = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|_| Error::NoSuchHost)?;
    let address = *addresses.first().ok_or(Error::NoSuchHost)?;
    Ok((host, address, port))
}

/// Opens a TCP connection on the caller's buffers.
///
/// `rx` and `tx` stay with the caller because the socket borrows them and the
/// session then borrows the socket, so both have to outlive everything built on
/// top — a helper that owned them could only return a socket pointing into its
/// own dead frame. Passing them in is what lets the dial itself be written
/// once.
///
/// Leaves the connect deadline in place. Whoever goes on to read a body wants
/// the longer idle one instead, and says so.
async fn connect<'a>(
    stack: &Stack<'a>,
    address: IpAddress,
    port: u16,
    rx: &'a mut [u8],
    tx: &'a mut [u8],
) -> Result<TcpSocket<'a>, Error> {
    let mut socket = TcpSocket::new(*stack, rx, tx);
    socket.set_timeout(Some(CONNECT_TIMEOUT));
    socket
        .connect((address, port))
        .await
        .map_err(|_| Error::Connect)?;
    Ok(socket)
}

/// Opens a TLS connection and asks the server one question.
///
/// A `HEAD` for `/`, which every teddyCloud answers and which costs one record
/// in each direction — enough to prove the handshake, the cipher suite and the
/// record layer without pulling a body through a buffer sized for headers.
///
/// **Run against hardware on 2026-09-23**, with certificates checked: the
/// server answered `404` to the `HEAD`, which is the answer that proves the
/// handshake rather than a fault.
pub async fn probe(client: &Client, stack: &Stack<'_>, server: &str) -> Result<(), Error> {
    let mut name = [0u8; MAX_NAME];
    let (host, address, port) = resolve(stack, server, &mut name).await?;
    esp_println::println!("teddiebox: tls {host}:{port} is {address}, certificates checked");

    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let socket = connect(stack, address, port, &mut rx, &mut tx).await?;
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

    let mut session = Session::new(
        client.tls,
        socket,
        &SessionConfig::Client(client_config(None)),
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

/// Asks how long a figure's story is on the server now, and downloads none of
/// it.
///
/// A probe and a fetch must name the same file the same way, which is why one
/// writer builds both requests.
pub async fn length(
    client: &Client,
    stack: &Stack<'_>,
    wanted: &Wanted<'_>,
) -> Result<Probed, Error> {
    let Wanted {
        server,
        ruid,
        token,
        ..
    } = *wanted;
    let mut name = [0u8; MAX_NAME];
    let (_host, address, port) = resolve(stack, server, &mut name).await?;

    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let mut socket = connect(stack, address, port, &mut rx, &mut tx).await?;
    socket.set_timeout(Some(IDLE_TIMEOUT));

    let mut session = Session::new(
        client.tls,
        socket,
        &SessionConfig::Client(client_config(client.credentials())),
    )
    .map_err(Error::Handshake)?;

    let mut uid = ruid;
    uid.reverse();
    let request = ContentRequest {
        uid,
        // The same rule the fetch follows: the route and the token are one
        // decision, and a probe that asked a different route could be answered
        // about a different file.
        route: if token.is_some() {
            Route::V2
        } else {
            Route::V1
        },
        etag: None,
        server,
        from: None,
        auth: token,
    };

    let mut buf = [0u8; 1024];
    let mut wire = Reporting::new(&mut session);
    let probed = stream::probe_length(&mut wire, &request, &mut buf).await;
    let answer = probed.map_err(|e| wire.explain(e))?;
    let _ = session.close().await;
    Ok(answer)
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
/// `token` is the tag's own memory, and it is what makes a figure teddyCloud
/// does not already hold fetchable at all: the server forwards it to the tonies
/// cloud, which is what accepts or rejects it. `None` is right for content the
/// server already has, which needs no token.
///
/// `ruid` is the identifier as it appears in the URL. `ContentRequest` reverses
/// what it is given, so it is handed over backwards to come out the right way.
///
/// **The route follows the token.** `/v1` passes `noPassword = TRUE` at the
/// server and so ignores any token sent with it; `/v2` is the endpoint that
/// spends one. Without a token `/v1` is the only one that has ever answered on
/// this server.
/// What to fetch, and what to prove it with.
///
/// A struct rather than three more parameters because they belong together and
/// travel together: the server, which figure, and the token that authorises
/// it. Passing them separately is also how a
/// caller silently transposes two of them.
pub struct Wanted<'a> {
    /// `host:port` of the teddyCloud server.
    pub server: &'a str,
    /// The identifier as it appears in the URL.
    pub ruid: [u8; 8],
    /// The tag's own memory, or `None` for content the server already holds.
    pub token: Option<&'a [u8; 32]>,
    /// Where to continue an interrupted download, or `None` for the whole file.
    pub from: Option<u32>,
    /// What the server should check that resume against, when there is one.
    ///
    /// Sent as `If-Range`, and **this server ignores it**. Measured against
    /// teddyCloud v0.7.0: a range request carrying a deliberately wrong
    /// validator was answered `206` with the tail, where RFC 7233 requires
    /// `200` and the whole file. Content it serves from its own disk carries no
    /// `ETag` to send in the first place.
    ///
    /// So this is sent in hope, not in trust, and nothing depends on it. What
    /// actually stops a resume against a file the server has replaced is the
    /// length comparison in [`teddiebox_download::place`] — worth knowing
    /// before anyone deletes that check as redundant.
    pub etag: Option<&'a ETag>,
}

/// What the response head said, handed over before a single body byte moves.
///
/// The card's owner needs this and cannot ask for it: it is on the far side of
/// a pipe, and by the time bytes arrive the head is gone. Chiefly it needs
/// `total`, because that is what the `.MET` sidecar records and the sidecar has
/// to be on the card *before* the content it vouches for.
pub struct Head<'a> {
    /// Length of the whole file, when the server said.
    pub total: Option<u32>,
    /// Where this body's first byte belongs in the file. Zero unless a range
    /// was asked for and honoured — so zero is also how a declined range
    /// arrives, and means "start again".
    pub offset: u32,
    /// What a later resume can be validated against.
    pub etag: Option<&'a ETag>,
}

pub async fn fetch(
    client: &Client,
    stack: &Stack<'_>,
    wanted: &Wanted<'_>,
    on_head: &mut dyn FnMut(Head<'_>),
    sink: &mut dyn FnMut(&[u8]) -> usize,
    may_fetch: &mut dyn FnMut() -> Continue,
) -> Result<Fetched, Error> {
    let Wanted {
        server,
        ruid,
        token,
        from,
        etag,
    } = *wanted;
    let mut name = [0u8; MAX_NAME];
    let (_host, address, port) = resolve(stack, server, &mut name).await?;

    // Still built here rather than inside a helper: the socket borrows them and
    // the session borrows the socket, so both have to outlive this call.
    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let mut socket = connect(stack, address, port, &mut rx, &mut tx).await?;
    // Connected, so the deadline that matters now is the idle one.
    socket.set_timeout(Some(IDLE_TIMEOUT));

    let mut session = Session::new(
        client.tls,
        socket,
        &SessionConfig::Client(client_config(client.credentials())),
    )
    .map_err(Error::Handshake)?;

    let mut uid = ruid;
    uid.reverse();
    let request = ContentRequest {
        uid,
        // The route follows the token, because the two are the same decision.
        // `/v1` passes `noPassword = TRUE` at the server, which tells it not to
        // use the token at all — so sending one there is wasted and the
        // upstream fetch is refused. `/v2` is the box's real endpoint and the
        // one that spends it. Without a token `/v1` is what answers at all,
        // since `/v2` has never returned a byte on this server.
        route: if token.is_some() {
            Route::V2
        } else {
            Route::V1
        },
        etag,
        server,
        from,
        auth: token,
    };

    let started = Instant::now();
    let mut buf = [0u8; 1024];
    // Everything downstream speaks to the session through this, so a failure
    // can be reported with the number mbedtls actually gave.
    let mut wire = Reporting::new(&mut session);
    let opened = stream::begin(&mut wire, &request, &mut buf).await;
    let begun = opened.map_err(|e| wire.explain(e))?;

    let (body_length, prefix) = match begun {
        Begun::NotFound | Begun::Unchanged => return Err(Error::NoContent),
        Begun::Content {
            body_length,
            prefix,
            offset,
            total,
            etag,
        } => {
            // Before `hand_over` below puts the first byte in the pipe, so the
            // card's owner has the file open and the sidecar written by the
            // time anything needs writing.
            on_head(Head {
                total,
                offset,
                etag: etag.as_ref(),
            });
            (body_length, prefix)
        }
    };

    let mut crc = Crc32::new();
    crc.update(&buf[prefix.clone()]);
    let mut received = prefix.len() as u32;
    hand_over(&buf[prefix.clone()], sink).await;
    let mut body = Body::new(body_length, received);
    esp_println::println!("teddiebox: get {body_length} bytes to read");

    // A forty-megabyte file takes long enough that silence is indistinguishable
    // from a hang, which is the thing this bench has mistaken twice already.
    let mut announced = 0u32;
    while !body.is_complete() {
        // Asked before reading rather than after: the point is to leave the
        // radio quiet while audio has a deadline, and a read already taken is
        // a packet already received.
        //
        // This loop used to ask only whether it might read, so a transfer
        // nobody wanted any more ran to the last byte regardless: lifting a
        // figure pulled the rest of a whole story over the radio and threw it
        // away. Abandonment is the third answer, and is checked ahead of the
        // throttle so that a transfer paused behind a playing story can still
        // be ended.
        loop {
            match may_fetch() {
                Continue::Now => break,
                Continue::Abandon => return Err(Error::Abandoned),
                Continue::Wait => Timer::after(THROTTLE_WAIT).await,
            }
        }
        let got = body.read(&mut wire, &mut buf).await;
        let n = got.map_err(|e| wire.explain(e))?;
        crc.update(&buf[..n]);
        received += n as u32;
        hand_over(&buf[..n], sink).await;
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

/// Keeps the transport error that `CloudError` throws away.
///
/// `teddiebox-cloud` is deliberately transport-agnostic, so everything that
/// fails inside it surfaces as a bare `CloudError::Transport`: a read did not
/// work, and nothing whatever about why. mbedtls answers in numbers, and this
/// bench has already spent an afternoon on a variant that carried none. This
/// sits in the one place that knows the transport is a TLS session and
/// remembers the last error it handed on.
struct Reporting<'a, T> {
    inner: &'a mut T,
    last: Option<SessionError>,
}

impl<'a, T> Reporting<'a, T> {
    fn new(inner: &'a mut T) -> Self {
        Self { inner, last: None }
    }

    /// Says what the transport said, when the transport is what failed.
    ///
    /// Silent for every other `CloudError`, because those already name
    /// themselves and a second line would only dilute them.
    fn explain(&self, error: CloudError) -> Error {
        if let (CloudError::Transport, Some(inner)) = (&error, self.last) {
            esp_println::println!("teddiebox: get transport failed — {inner:?}");
        }
        Error::Cloud(error)
    }
}

impl<T: embedded_io_async::ErrorType<Error = SessionError>> embedded_io_async::ErrorType
    for Reporting<'_, T>
{
    type Error = SessionError;
}

impl<T: embedded_io_async::Read<Error = SessionError>> embedded_io_async::Read
    for Reporting<'_, T>
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        match self.inner.read(buf).await {
            Ok(n) => Ok(n),
            Err(e) => {
                self.last = Some(e);
                Err(e)
            }
        }
    }
}

impl<T: embedded_io_async::Write<Error = SessionError>> embedded_io_async::Write
    for Reporting<'_, T>
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        match self.inner.write(buf).await {
            Ok(n) => Ok(n),
            Err(e) => {
                self.last = Some(e);
                Err(e)
            }
        }
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        match self.inner.flush().await {
            Ok(()) => Ok(()),
            Err(e) => {
                self.last = Some(e);
                Err(e)
            }
        }
    }
}

/// Pushes every byte into the sink, waiting when it will not take them.
///
/// The sink is a fixed-size queue drained by whoever owns the card, so a full
/// one is the ordinary way it says "not yet" rather than a fault. Waiting here
/// is what turns that into back-pressure on the server: the socket stops being
/// read, the window closes, and the download slows to the speed the card is
/// being written at instead of overflowing.
///
/// It yields rather than spins, so the task draining the other end actually
/// gets to run.
async fn hand_over(bytes: &[u8], sink: &mut dyn FnMut(&[u8]) -> usize) {
    let mut at = 0;
    while at < bytes.len() {
        let taken = sink(&bytes[at..]);
        at += taken;
        if taken == 0 {
            // Back off to roughly the consumer's own cadence. Two
            // milliseconds meant waking five hundred times a second to take a
            // critical section and find the pipe still full — and critical
            // sections mask interrupts, including the one that keeps the audio
            // DMA fed. Retrying faster than the drain can possibly happen buys
            // nothing and costs the thing that must not be interrupted.
            Timer::after(Duration::from_millis(20)).await;
        }
    }
}

/// Parses the box's credentials, read from flash at boot by
/// [`crate::identity::load`]. Called once per boot, by [`Client::credentials`].
///
/// `None` is not a failure: without them the box can still fetch anything the
/// server already holds. They are what let teddyCloud tell *which* box is
/// asking, and so what lets it fetch a figure it has no copy of.
fn parse_credentials() -> Option<Credentials<'static>> {
    let held = identity()?;
    Some(Credentials {
        // Borrowed rather than copied, which is why the identity is a static:
        // mbedtls reads it for the length of the session.
        certificate: Certificate::new_no_copy(held.certificate()).ok()?,
        private_key: PrivateKey::new(X509::DER(held.key()), None).ok()?,
    })
}
