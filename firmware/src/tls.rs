//! TLS, limited to the one handshake teddyCloud offers.
//!
//! teddyCloud uses CycloneSSL with TLS 1.2 only, so `embedded-tls` (TLS 1.3
//! only) cannot connect. This module wraps `mbedtls-rs`, limited in
//! `Cargo.toml` to ECDHE-RSA-AES256-GCM-SHA384 over secp256r1.
//!
//! `embassy_net::tcp::TcpSocket`, [`Session`] and `teddiebox-cloud` all use
//! `embedded-io-async` 0.7, so no adapter is needed: a session works like an
//! encrypted socket.
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
/// Generous. The SHA, RSA and AES hardware speeds up the handshake (see
/// [`init`]), but ECDHE has no accelerator on this chip. A full handshake
/// takes about 1.3 s (2.7 s for the first after boot); see [`Client`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the data socket may sit idle before `embassy-net` abandons it.
///
/// Longer than the connect timeout: a throttled download can pause reading
/// for a long time while a story plays. teddyCloud keeps connections for 300
/// s (`Keep-Alive: timeout=300`), so this is below that and above any
/// throttle pause.
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
/// `mbedtls` wants the server name as a C string, and there is no allocator.
const MAX_NAME: usize = 80;

/// Why a TLS connection could not be made.
///
/// The `SessionError` values are only read through `Debug` on the console,
/// which rustc does not count as use. They are needed: for example,
/// `CIPHER_ALLOC_FAILED` (heap too small) and `CA_CHAIN_REQUIRED` (a setting)
/// would otherwise both just be "handshake failed".
#[derive(Debug)]
#[allow(dead_code)]
pub enum Error {
    /// The download is no longer needed (the figure was lifted). Not a
    /// failure.
    Abandoned,
    /// `server` in the config was not `host:port`.
    MalformedServer,
    /// The host name did not resolve.
    NoSuchHost,
    /// The TCP connection could not be made.
    Connect,
    /// The TLS handshake failed.
    ///
    /// Includes mbedtls's error code, which tells apart causes such as a
    /// missing cipher suite, a too-small record buffer, or a missing hash
    /// algorithm.
    Handshake(SessionError),
    /// Reading or writing the encrypted stream failed.
    ///
    /// `mbedtls-rs` may run the handshake on first use, so a handshake failure
    /// can also appear here.
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
/// The real ones are 1030 and 1190 bytes; this leaves room for a new
/// certificate. A file that fills the buffer is refused, not truncated.
pub const CERT_BYTES: usize = 1536;

/// The common name teddyCloud's server certificate carries.
///
/// `mbedtls_ssl_set_hostname` sets two things: the name the certificate is
/// checked against, *and* the SNI sent to the server. No single value works
/// for both: this CN contains a space and is not a valid SNI host name, the
/// real host `teddycloud.local` is not in the certificate, and with no name
/// mbedtls refuses to verify (`CERTIFICATE_VERIFICATION_WITHOUT_HOSTNAME`).
///
/// The build option `MBEDTLS_SSL_SERVER_NAME_INDICATION` only controls
/// sending SNI; verification reads the hostname separately. So
/// `scripts/vendor-mbedtls-rs-sys.sh` removes that define from the vendored
/// crate (it cannot be done in `Cargo.toml`). No SNI is sent, and this name is
/// only used for verification.
///
/// Sending an SNI would break the connection: with any SNI, teddyCloud sends
/// a `tbs2.tonie.cloud` chain on secp384r1, a curve this build does not
/// include, and the handshake fails with `MBEDTLS_ERR_SSL_FATAL_ALERT_MESSAGE`.
/// Without SNI it sends the RSA certificate with this name, signed by the CA
/// on the card.
///
/// Hardcoded because teddyCloud's own certificate generation sets it. If it
/// ever varies, it should move to the card's config.
const SERVER_IDENTITY: &CStr = c"TeddyCloud Server";

/// The box's certificate and the key that proves it.
///
/// Kept for the whole run, because each session borrows the certificate
/// rather than copying it.
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
/// Called once at boot by [`crate::identity::load`]. Returns whether it
/// worked: a second call is refused, since a session may be using the
/// credentials.
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
/// `Tls::new` needs a `&'static mut`, so both live in `StaticCell`s,
/// initialised once.
static RNG: StaticCell<HardwareRng> = StaticCell::new();
static TLS: StaticCell<Tls<'static>> = StaticCell::new();

/// The crypto accelerators, and the hooks that route mbedtls onto them.
///
/// Dropping the hook guard while mbedtls state created under the hooks is
/// alive is undefined behaviour in `mbedtls-rs`, and dropping a backend would
/// leave calls waiting forever. [`TLS`] is such state and is never dropped,
/// so these are statics too.
static ACCEL: StaticCell<EspAccel<'static>> = StaticCell::new();
static QUEUES: StaticCell<EspAccelQueue<'static, 'static>> = StaticCell::new();
static HOOKS: StaticCell<EspHooksGuard<'static>> = StaticCell::new();

/// The ESP32's random number generator, in the shape `rand_core` 0.10 asks for.
///
/// `esp-hal` implements `rand_core` 0.6 and 0.9; `mbedtls-rs` needs 0.10. So
/// this is a thin wrapper around `random()`.
///
/// **Only secure while the radio is on**, as documented for the ESP32's RNG.
/// That is always the case here: a TLS session only exists inside a
/// [`crate::net::Session`], while the modem is powered.
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
        // A word at a time; the last chunk may be shorter than four bytes.
        for chunk in dst.chunks_mut(4) {
            let word = self.0.random().to_ne_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }
}

// `rand_core` 0.10 derives `Rng` and `CryptoRng` from these two
// automatically.

/// Marks the generator as fit for keys; see [`HardwareRng`].
impl rand_core::TryCryptoRng for HardwareRng {}

/// Starts the crypto accelerators and builds the mbedtls context. Call once.
///
/// The three peripherals are taken by value, so nothing else can use them.
///
/// **The order matters.** `mbedtls-rs` sends an algorithm to hardware in two
/// steps: a *backend* services the `esp-hal` work queue, and a *hook* points
/// mbedtls at it. A hook without a running backend blocks forever, and a hook
/// registered after mbedtls state exists leaves that state unusable by the
/// hardware. So: start the backends, hook exactly those, and only then call
/// `Tls::new` (the first mbedtls state in this firmware).
///
/// SHA-256/384/512 and RSA speed up the handshake, AES-GCM the data. The
/// ESP32-S3 has no ECC hardware, so ECDHE stays in software.
///
/// Returns `None` if already called, since the statics can only be filled
/// once.
pub fn init(sha: SHA<'static>, rsa: RSA<'static>, aes: AES<'static>) -> Option<Client> {
    let accel = ACCEL.try_init(EspAccel::new().with_sha(sha).with_rsa(rsa).with_aes(aes))?;
    let queues: &'static EspAccelQueue<'static, 'static> = QUEUES.try_init(accel.start())?;

    // SAFETY: the hooks are registered before the first mbedtls call
    // (`Tls::new` below). `hook` selects exactly the algorithms `queues`
    // services, so none can block. The guard is stored in a static and never
    // dropped, so it stays registered while the `Tls` is alive.
    let hooks = unsafe { queues.hook() };
    HOOKS.try_init(hooks)?;

    let rng = RNG.try_init(HardwareRng(esp_hal::rng::Rng::new()))?;
    let tls = Tls::new(rng).ok()?;
    Some(Client {
        tls: TLS.try_init(tls)?.reference(),
        credentials: RefCell::new(None),
        warmed: core::cell::Cell::new(false),
        resumable: RefCell::new(None),
    })
}

/// What every connection needs and none of them should rebuild: the mbedtls
/// context, and the box's identity parsed once.
///
/// **Parsing the key once matters.** Parsing itself takes 2-3 ms, but the
/// first RSA private operation on a newly parsed key sets up blinding in
/// software, which took 1.36 s on the box without yielding. With the key
/// parsed once, only the first handshake after boot pays this: 2.68 s,
/// against 1.27-1.36 s afterwards. See also [`Client::warm`].
///
/// Owned by the network task and lent to each connection, not a static: the
/// parsed key's reference count is not atomic, so it must have one owner.
pub struct Client {
    tls: TlsReference<'static>,
    /// Filled on first use, not at [`init`], because this may be created
    /// before `identity::load` has run.
    credentials: RefCell<Option<Credentials<'static>>>,
    /// Whether the parsed key has had its first private operation.
    warmed: core::cell::Cell<bool>,
    /// The last session the server agreed, offered back on the next
    /// connection so it can skip the key exchange and the client signature.
    ///
    /// Measured on the box: a resumed handshake takes 57 ms against 1.25 s
    /// for a full one. It uses 1.9 KB of heap. If the server has forgotten the
    /// session, it just does a full handshake.
    ///
    /// Resuming skips checking the server's certificate again, which TLS 1.2
    /// allows because the session was agreed with a verified server.
    /// mbedtls-rs also refuses a session saved for another server name, and
    /// every connection here uses [`SERVER_IDENTITY`].
    resumable: RefCell<Option<mbedtls_rs::SavedSession>>,
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

    /// Runs the handshake, offering the last session for resumption, and keeps
    /// what the server agreed for the next one.
    ///
    /// If the server has forgotten the session, it does a full handshake;
    /// not an error.
    async fn handshake<T>(&self, session: &mut Session<'_, T>) -> Result<(), Error>
    where
        T: mbedtls_rs::io::Read + mbedtls_rs::io::Write,
    {
        // Taken, not borrowed: a `RefCell` borrow cannot be held across the
        // awaits below.
        let offered = self.resumable.borrow_mut().take();
        let started = Instant::now();
        match &offered {
            Some(saved) => session.connect_with_session(saved).await,
            None => session.connect().await,
        }
        .map_err(Error::Handshake)?;
        esp_println::println!(
            "teddiebox: tls handshake {} ms{}",
            started.elapsed().as_millis(),
            if offered.is_some() {
                ", resumption offered"
            } else {
                ""
            }
        );
        *self.resumable.borrow_mut() = session.save().ok();
        Ok(())
    }

    /// Does the first private-key operation now, so no handshake has to.
    ///
    /// The first operation sets up RSA blinding: 1.36 s of CPU (measured),
    /// blocking this executor. The network task calls this while Wi-Fi
    /// connects, which it waits for anyway. Once per boot; without an identity
    /// it does nothing and tries again next time.
    pub fn warm(&self) {
        if self.warmed.get() {
            return;
        }
        let Some(credentials) = self.credentials() else {
            return;
        };
        let started = Instant::now();
        match credentials.private_key.warm(self.tls) {
            Ok(()) => {
                self.warmed.set(true);
                esp_println::println!(
                    "teddiebox: tls key warmed in {} ms",
                    started.elapsed().as_millis()
                );
            }
            // The handshake repeats this and reports its own errors.
            Err(e) => esp_println::println!("teddiebox: tls could not warm the key — {e:?}"),
        }
    }
}

/// The client configuration for one connection.
///
/// **No clock is needed.** mbedtls is built without `MBEDTLS_HAVE_TIME_DATE`,
/// so certificate dates are never checked. The box can verify a chain without
/// knowing the date, but cannot detect an expired certificate.
///
/// The server is always verified: the chain is checked against
/// `CN=TeddyCloud CA Root Certificate`, from `TCCA.DER` on the card. Without
/// that file the box cannot download; the check is never skipped.
///
/// **The name checked is the server's identity, not its address.**
/// teddyCloud's certificate has no `subjectAltName` and the common name
/// "TeddyCloud Server", which will never match `teddycloud.local`. So that CN is
/// checked (mbedtls uses the CN when there is no SAN). This confirms the chain
/// reaches our CA and the certificate names the intended server; it does not
/// confirm that `teddycloud.local` resolves to it. With a private CA that signs
/// one server, that difference is small.
///
/// The name cannot be left unset: mbedtls then refuses to verify
/// (`CERTIFICATE_VERIFICATION_WITHOUT_HOSTNAME`).
///
/// `min_version` is TLS 1.2 because the server offers nothing else.
///
/// The box also identifies itself: `creds` is the box's own certificate and
/// key (from flash), and the tag's 32-byte token goes in an
/// `Authorization: BD …` header. So whatever answers at `server` gets a
/// client-authenticated session it could relay to teddyCloud, plus a token
/// for the figure on the plate. The private key never leaves the box. Chain
/// verification limits this to servers whose certificate the card's CA
/// signed, but `server` comes from the card, so `CONFIG.TXT` must be correct.
pub fn client_config<'a>(creds: Option<Credentials<'a>>) -> ClientSessionConfig<'a> {
    ClientSessionConfig {
        creds,
        ca_chain: anchor(),
        // Only used to verify the certificate; no SNI is sent. See
        // [`SERVER_IDENTITY`].
        server_name: Some(SERVER_IDENTITY),
        auth_mode: AuthMode::Required,
        min_version: TlsVersion::Tls1_2,
        ..ClientSessionConfig::new()
    }
}

/// Splits `host:port`, the way it is written in `CONFIG.TXT`.
///
/// Writes the host with a NUL appended, as `mbedtls` requires.
fn split_server(server: &str, name: &mut [u8; MAX_NAME]) -> Result<(usize, u16), Error> {
    // The split itself is host-tested in `teddiebox_config`; every reason it
    // can fail collapses to `MalformedServer` here.
    let (host, port) = teddiebox_config::split_host_port(server, name.len() - 1)
        .map_err(|_| Error::MalformedServer)?;
    let bytes = host.as_bytes();
    name[..bytes.len()].copy_from_slice(bytes);
    name[bytes.len()] = 0;
    Ok((bytes.len() + 1, port))
}

/// Resolves a `host:port` string to something [`connect`] can dial.
///
/// The name is written into the caller's buffer (NUL-terminated, for
/// mbedtls), and the returned `&str` points into it.
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
/// `rx` and `tx` belong to the caller, because the socket borrows them and
/// the session borrows the socket, so they must outlive both.
///
/// Sets the connect timeout; callers that read a body set the longer idle
/// timeout afterwards.
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
/// A `HEAD` for `/`, which teddyCloud answers with one small record. Enough
/// to test the handshake, cipher suite and record layer. teddyCloud answers
/// `404`, which still shows TLS works.
pub async fn probe(client: &Client, stack: &Stack<'_>, server: &str) -> Result<(), Error> {
    let mut name = [0u8; MAX_NAME];
    let (host, address, port) = resolve(stack, server, &mut name).await?;
    esp_println::println!("teddiebox: tls {host}:{port} is {address}, certificates checked");

    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let socket = connect(stack, address, port, &mut rx, &mut tx).await?;
    esp_println::println!("teddiebox: tls connected, starting the handshake");

    // mbedtls shares the Wi-Fi driver's heap. When a session does not fit,
    // the error is `MBEDTLS_ERR_CIPHER_ALLOC_FAILED`, or a fatal alert from
    // the server that looks like a protocol fault. So the free heap is
    // printed.
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
    // Only the status line: enough to show the record layer works.
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

/// Opens a session with the box's identity and closes it again, so that the
/// next connection can resume it.
///
/// The first request after boot is slow: it scans for the network, sets up
/// the key and runs a full handshake (5.1 s from placement to playing,
/// against about 2.3 s later). Doing this at boot makes the first figure
/// fast too.
pub async fn prime(client: &Client, stack: &Stack<'_>, server: &str) -> Result<(), Error> {
    let mut name = [0u8; MAX_NAME];
    let (_host, address, port) = resolve(stack, server, &mut name).await?;
    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let socket = connect(stack, address, port, &mut rx, &mut tx).await?;
    let mut session = Session::new(
        client.tls,
        socket,
        &SessionConfig::Client(client_config(client.credentials())),
    )
    .map_err(Error::Handshake)?;
    client.handshake(&mut session).await?;
    let _ = session.close().await;
    Ok(())
}

/// Asks how long a figure's story is on the server now, and downloads none of
/// it.
///
/// Uses the same request builder as a download, so both ask for the same
/// file.
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
    client.handshake(&mut session).await?;

    let mut uid = ruid;
    uid.reverse();
    let request = ContentRequest {
        uid,
        // Same route choice as `fetch`, so the probe asks about the same file.
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

/// What to download, and the token that authorises it.
///
/// A struct rather than separate parameters, so they cannot be swapped by
/// mistake.
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
    /// Sent as `If-Range`, but **teddyCloud ignores it**: with a wrong value,
    /// teddyCloud v0.7.0 still answered `206` with the rest of the file, where
    /// RFC 7233 requires `200` and the whole file. For its own files it sends
    /// no `ETag` anyway.
    ///
    /// So nothing depends on it. What actually prevents resuming a replaced
    /// file is the length check in [`teddiebox_download::place`]; do not remove
    /// it as redundant.
    pub etag: Option<&'a ETag>,
}

/// What the response head said, handed over before a single body byte moves.
///
/// The task that owns the card only receives body bytes through a pipe, so it
/// gets the head this way. Mainly it needs `total`, which the `.MET` sidecar
/// records, and the sidecar must be written *before* the content.
pub struct Head<'a> {
    /// Length of the whole file, when the server said.
    pub total: Option<u32>,
    /// Where this body's first byte belongs in the file. Zero unless a range
    /// was requested and granted; zero after a range request means "start
    /// again".
    pub offset: u32,
    /// What a later resume can be validated against.
    pub etag: Option<&'a ETag>,
}

/// Downloads one content file, passing the body to `sink`, and returns its
/// length and CRC-32.
///
/// `on_head` is called with the response head before any body bytes go to
/// `sink`. `may_fetch` is asked before each read, so the download can pause
/// (throttle) or stop (figure lifted).
///
/// `token` is the tag's memory. teddyCloud forwards it to the Tonies cloud,
/// which accepts or rejects it; it is needed for figures teddyCloud does not
/// have yet. `None` works for content teddyCloud already has.
///
/// `ruid` is the identifier as it appears in the URL. `ContentRequest`
/// reverses what it gets, so it is reversed here first.
///
/// **The route depends on the token**; see the comment on `route` below.
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

    // Created here: the socket borrows them and the session borrows the
    // socket.
    let mut rx = [0u8; TCP_BUFFER];
    let mut tx = [0u8; TCP_BUFFER];
    let mut socket = connect(stack, address, port, &mut rx, &mut tx).await?;
    // Connected; now use the idle timeout.
    socket.set_timeout(Some(IDLE_TIMEOUT));

    let mut session = Session::new(
        client.tls,
        socket,
        &SessionConfig::Client(client_config(client.credentials())),
    )
    .map_err(Error::Handshake)?;
    client.handshake(&mut session).await?;

    let mut uid = ruid;
    uid.reverse();
    let request = ContentRequest {
        uid,
        // `/v1` sets `noPassword = TRUE` in teddyCloud, so it ignores the
        // token and the upstream fetch would be refused. `/v2` uses the token.
        // Without a token only `/v1` answers on our server (`/v2` hangs).
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
    // Wraps the session, so a failure can be reported with mbedtls's error.
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
            // Before `hand_over` sends the first byte, so the card's owner can
            // open the file and write the sidecar first.
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

    // Print progress every megabyte, so a long download does not look like a
    // hang.
    let mut announced = 0u32;
    while !body.is_complete() {
        // Asked before reading, so Wi-Fi stays quiet while audio needs the
        // CPU. It also stops a download that is no longer needed, even while
        // paused.
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

/// Downloads the file at `path` on `server`, passing the body to `sink`, and
/// returns how many bytes it carried.
///
/// For firmware updates, which fetch a manifest and an image by path rather
/// than a figure's content by uid. `sink` returns `false` to stop the
/// download, which ends it with [`Error::Abandoned`]. A missing file is
/// [`Error::NoContent`].
///
/// `server` is always the card's `update_url` host, never one a manifest
/// names; see the trust model in `teddiebox_ota`.
pub async fn get_path(
    client: &Client,
    stack: &Stack<'_>,
    server: &str,
    path: &str,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<u32, Error> {
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
    client.handshake(&mut session).await?;

    let mut request = [0u8; 256];
    let request_len = teddiebox_cloud::build_path_request(&mut request, path, server, None)
        .map_err(Error::Cloud)?;

    let mut buf = [0u8; 1024];
    let mut wire = Reporting::new(&mut session);
    let opened = stream::begin_prepared(&mut wire, &request[..request_len], &mut buf).await;
    let (body_length, prefix) = match opened.map_err(|e| wire.explain(e))? {
        Begun::NotFound | Begun::Unchanged => return Err(Error::NoContent),
        Begun::Content {
            body_length,
            prefix,
            ..
        } => (body_length, prefix),
    };

    let mut received = prefix.len() as u32;
    if !sink(&buf[prefix]) {
        return Err(Error::Abandoned);
    }
    let mut body = Body::new(body_length, received);
    while !body.is_complete() {
        let got = body.read(&mut wire, &mut buf).await;
        let n = got.map_err(|e| wire.explain(e))?;
        received += n as u32;
        if !sink(&buf[..n]) {
            return Err(Error::Abandoned);
        }
    }

    let _ = session.close().await;
    Ok(received)
}

/// Keeps the transport error that `CloudError` throws away.
///
/// `teddiebox-cloud` does not know the transport, so its errors are just
/// `CloudError::Transport`, with no reason. This wrapper remembers the last
/// mbedtls error, so it can be printed.
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
    /// Prints nothing for other `CloudError`s, which explain themselves.
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
/// The sink is a fixed-size queue emptied by the task that owns the card; a
/// full queue just means "not yet". Waiting here slows the download to the
/// speed of the card: the socket is not read, so TCP slows the server down.
///
/// It waits rather than spins, so the other task can run.
async fn hand_over(bytes: &[u8], sink: &mut dyn FnMut(&[u8]) -> usize) {
    let mut at = 0;
    while at < bytes.len() {
        let taken = sink(&bytes[at..]);
        at += taken;
        if taken == 0 {
            // Wait about as long as the consumer takes. Retrying more often
            // takes more critical sections, which block interrupts, including
            // the one that keeps the audio DMA fed.
            Timer::after(Duration::from_millis(20)).await;
        }
    }
}

/// Parses the box's credentials, read from flash at boot by
/// [`crate::identity::load`]. Called once per boot, by [`Client::credentials`].
///
/// `None` is not a failure: without credentials the box can still download
/// what teddyCloud already has. They identify the box, which teddyCloud needs
/// to fetch a figure it does not have.
fn parse_credentials() -> Option<Credentials<'static>> {
    let held = identity()?;
    Some(Credentials {
        // Borrowed, not copied, which is why the identity is a static.
        certificate: Certificate::new_no_copy(held.certificate()).ok()?,
        private_key: PrivateKey::new(X509::DER(held.key()), None).ok()?,
    })
}
