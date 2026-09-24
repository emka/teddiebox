//! The radio, and only the radio.
//!
//! The box spends nearly all of its life playing a file that is already on the
//! card, so the Wi-Fi stays off and comes up only when something wants the
//! network. That is what [`Radio::acquire`] and [`release`] are for: between
//! them the modem is powered and associated, and outside them it is not.
//!
//! This module owns the [`WifiController`] and the [`embassy_net`] runner and
//! nothing else. It does not open the card, does not read credentials from it,
//! and does not open a socket. Credentials arrive as a
//! [`teddiebox_config::Config`] from whoever owns the card; sockets are opened
//! by whoever owns the download.
//!
//! Nothing here has been run on hardware. It compiles and links, which is the
//! whole of what is known about it.

use core::cell::RefCell;

use embassy_net::{Runner, Stack, StackResources};
use esp_hal::peripherals::WIFI;
use esp_hal::time::Duration as EspDuration;
use esp_radio::wifi::{
    ap::AccessPointConfig, ap::AccessPointInfo, scan::ScanConfig, scan::ScanTypeConfig,
    sta::StationConfig, AuthenticationMethodConfig, Config as WifiConfig, ConnectionError,
    ControllerConfig, DisconnectReason, Interface, Password, Ssid, WifiController, WifiError,
};
use teddiebox_config::Config;

/// The network the box raises when both ears are held at boot.
///
/// **Compiled in, and never read from the card.** This is what makes a
/// truncated or unparseable `CONFIG.TXT` recoverable rather than terminal: the
/// way back in cannot depend on the file being repaired.
pub const SETUP_SSID: &str = "teddiebox-setup";

/// The passphrase for that network, documented in `README.md`.
///
/// Published, and not meant to be otherwise: it has to be written down
/// somewhere a person can reach it while holding a box that will not start,
/// and that somewhere is a public repository.
///
/// **So it is not confidentiality, and an earlier version of this comment
/// claimed it was.** A listener in range who has this repository can capture
/// the four-way handshake — or force one with a deauthentication — derive the
/// session key, and read the page off the air, home WiFi passphrase included.
/// [`Radio::serve`]'s `max_connections(1)` gates *joining*, which is a
/// different thing from reading.
///
/// What WPA2 buys here is that the network is not simply open: a passer-by's
/// phone does not associate on its own, and reading the page takes intent,
/// proximity and a capture during the ten minutes the portal is up. Somebody
/// who needs more than that should set the box up out of range of anyone they
/// do not trust, and change the home passphrase afterwards if they did not.
///
/// A card may name its own with `setup_password` in `CONFIG.TXT`, which narrows
/// the window to people who know that value. This stays the fallback, and has
/// to: it is what a box with no card, an unreadable card or an unparseable
/// `CONFIG.TXT` answers to, and those are the boxes somebody is holding both
/// ears on.
pub const SETUP_PASSWORD: &str = "teddiebox";

/// Socket slots the stack is given.
///
/// Two of the three the station path needs are spent before any of our code
/// runs: `embassy-net` opens a DNS socket unconditionally when the `dns`
/// feature is on, and a DHCP socket whenever the stack is built with
/// `Config::dhcpv4`. That leaves exactly one for the TCP connection the
/// downloader opens — which is enough, because the box talks to one server at
/// a time by design: a second concurrent download would only be competing for
/// the same card.
///
/// The other two are the setup portal's, built on a static IP rather than
/// DHCP: one TCP listener for the HTTP server, and one UDP socket for the
/// DHCP *server* it runs for whichever device joins the access point.
///
/// **Measured on 2026-09-16, in both directions.** The two paths never run at
/// once — the portal only comes up over [`Radio::serve`], holding a different
/// singleton than [`Radio::acquire`] — so this covers the worse of the two
/// rather than their sum, and the worse of the two is the station path's three.
///
/// At **3** the whole station path works: associate, lease, resolve, TLS, and a
/// download running at length. At **2** it gets further than it looks like it
/// should — it associates, takes a lease, and even resolves the host — and then
/// panics inside smoltcp (`socket_set.rs:83`, a full `SocketSet`) the moment
/// TLS asks for its TCP socket. That failure is the accounting above confirmed
/// from the other side: DNS and DHCP really are spent before our code runs.
///
/// It was 5 until this was measured, which was two slots of `StackResources`
/// bought against an estimate.
const SOCKETS: usize = 3;

/// Why the radio could not be brought up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A session is already using the station interface.
    ///
    /// The interface is a singleton in `esp-radio`, so this means a previous
    /// session was never released rather than that anything is wrong with the
    /// hardware.
    Busy,
    /// The driver refused the credentials or the configuration.
    Wifi(WifiError),
}

impl From<WifiError> for Error {
    fn from(error: WifiError) -> Self {
        Error::Wifi(error)
    }
}

/// The Wi-Fi peripheral and the memory its network stack needs.
///
/// Held for as long as the firmware runs. Holding it costs the socket storage
/// and nothing else: no clock, no power domain, no association. Those belong
/// to a [`Session`].
pub struct Radio<'d> {
    wifi: WIFI<'d>,
    resources: StackResources<SOCKETS>,
    /// The access point the last join reached, so the next one can go
    /// straight to it. See [`Joined`].
    joined: RefCell<Option<Joined>>,
}

/// Where the last join ended up: which access point, on which channel.
///
/// Offered back as a hint, a join takes 1.8-1.9 s against 3.0-3.1 s for one
/// that has to scan for the network first — measured on the box, with the
/// access point on channel 11, which the driver's default scan reaches last
/// of the ones it tries. RAM only: a boot is roughly a session, and a hint
/// that outlived a move to another room or another router would cost a
/// failed join before every scan.
struct Joined {
    /// The network it was for. A hint for another SSID is no hint.
    ssid: heapless::String<{ teddiebox_config::MAX_SSID }>,
    bssid: [u8; 6],
    channel: u8,
}

/// A powered, configured radio.
///
/// Dropping it takes the modem down, so it is held for exactly as long as the
/// network is wanted.
pub struct Session<'d> {
    controller: WifiController<'d>,
    stack: Stack<'d>,
    /// The radio's memory of the last join, updated by [`Session::connect`].
    joined: &'d RefCell<Option<Joined>>,
    ssid: heapless::String<{ teddiebox_config::MAX_SSID }>,
    /// The configuration without the hint, to fall back to. `None` when the
    /// join was not hinted and so has nothing to fall back from.
    unhinted: Option<StationConfig>,
    /// The same join with the passphrase, for when the key it was given is
    /// the stored one and the access point refuses it. `None` when the key
    /// already is the passphrase.
    passphrase: Option<Attempt>,
}

/// One set of credentials to join with: hinted first when there is a hint,
/// and the same without it to fall back to.
struct Attempt {
    first: StationConfig,
    unhinted: Option<StationConfig>,
}

/// Which key a join succeeded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinedWith {
    /// The key [`Radio::acquire`] was given.
    GivenKey,
    /// The passphrase, after the access point refused the given key.
    Passphrase,
}

/// The half of a session that has to be polled.
///
/// `embassy-net` splits driving the stack from using it, because the two
/// happen concurrently: [`Link::run`] never returns, so it is selected against
/// the work that uses the [`Stack`] rather than awaited before it.
pub struct Link<'d> {
    runner: Runner<'d, Interface>,
}

impl<'d> Radio<'d> {
    /// Claims the Wi-Fi peripheral without powering it.
    pub const fn new(wifi: WIFI<'d>) -> Self {
        Self {
            wifi,
            resources: StackResources::new(),
            joined: RefCell::new(None),
        }
    }

    /// Asks the radio what access points it can hear.
    ///
    /// Needs no credentials, no [`Interface`] and no network stack: a
    /// controller is built, asked, and dropped again, so this both leaves the
    /// modem powered down afterwards and never touches the station singleton
    /// that [`acquire`](Self::acquire) competes for.
    ///
    /// That independence is the point. A scan is the one radio operation whose
    /// failure cannot be blamed on a password or an address, which makes it
    /// the right first thing to ask of a radio that has never been up: an
    /// empty result is a statement about the radio, the antenna or the room.
    ///
    /// Fills `out` and returns how many entries were written; anything the
    /// scan finds beyond `out.len()` is discarded, and the scan is asked to
    /// stop there rather than collect more and throw them away.
    ///
    /// **This allocates.** `scan_async` builds its results in a `Vec` on the
    /// radio heap, which is the only allocator this firmware has. They are
    /// copied into `out` and the `Vec` dropped before returning, so the
    /// allocation lives no longer than the call — which matters because that
    /// heap is sized for the Wi-Fi driver's own buffers and nothing else.
    ///
    /// **Waits as long as the driver does**, for the same reason
    /// [`Session::connect`] does: the caller is the one that knows how long a
    /// box should wait, so the timeout belongs there and not here.
    pub async fn scan(&mut self, out: &mut [AccessPointInfo]) -> Result<usize, Error> {
        let mut controller =
            WifiController::new(self.wifi.reborrow(), ControllerConfig::default())?;
        // The driver's default dwell — 10 ms minimum, 20 ms maximum per
        // channel — makes a single scan a *sample* rather than a census. Four
        // consecutive scans at the bench on 2026-09-03 returned 7, 3, 6 and 8
        // access points, and one of them missed the network the box was
        // standing next to at -29 dBm. A scan that can miss the target answers
        // "is it in range?" with a coin toss, which is the one question this
        // exists to answer, so it waits appreciably longer per channel.
        // Fourteen channels at 300 ms is still comfortably inside the caller's
        // timeout.
        let config =
            ScanConfig::default()
                .with_max(out.len())
                .with_scan_type(ScanTypeConfig::Active {
                    min: EspDuration::from_millis(100),
                    max: EspDuration::from_millis(300),
                });
        let found = controller.scan_async(&config).await?;
        let n = found.len().min(out.len());
        out[..n].clone_from_slice(&found[..n]);
        Ok(n)
    }

    /// Powers the modem and builds a DHCP network stack on it.
    ///
    /// Returns before anything is associated: [`Session::connect`] does that,
    /// and it is separate because association is the part that can take
    /// seconds and fail for reasons outside the box.
    ///
    /// `seed` must differ between boots — it seeds the stack's port and
    /// transaction identifiers — and comes from the caller because this module
    /// does not own an entropy source.
    ///
    /// `key` is either `config`'s passphrase or the key derived from it, as
    /// 64 hex characters, which the driver takes as the key itself: a join
    /// took 64–72 ms with it against 1.81 s with the passphrase, measured
    /// 2026-09-24. Which one is the caller's business, like the credentials.
    pub fn acquire(
        &mut self,
        config: &Config,
        key: &str,
        seed: u64,
    ) -> Result<(Session<'_>, Link<'_>), Error> {
        let hint = self
            .joined
            .borrow()
            .as_ref()
            .filter(|joined| joined.ssid == config.ssid)
            .map(|joined| (joined.bssid, joined.channel));
        let attempt = |key: &str| -> Result<Attempt, Error> {
            // WPA2-Personal is a *minimum* threshold rather than an exact
            // mode, so a WPA3 access point still associates; it is open and
            // WEP networks this refuses, which is the refusal worth having.
            let station = StationConfig::default()
                .with_ssid(Ssid::try_from(config.ssid.as_str())?)
                .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                    Password::try_from(key)?,
                ));
            Ok(match hint {
                Some((bssid, channel)) => Attempt {
                    first: station.clone().with_bssid(bssid).with_channel(channel),
                    unhinted: Some(station),
                },
                None => Attempt {
                    first: station,
                    unhinted: None,
                },
            })
        };
        let given = attempt(key)?;
        let passphrase = if key == config.password.as_str() {
            None
        } else {
            Some(attempt(&config.password)?)
        };

        // Taken before the controller so that a session refused for a
        // singleton already in use has not powered the modem on the way out.
        let interface = Interface::try_station().ok_or(Error::Busy)?;

        let mut controller =
            WifiController::new(self.wifi.reborrow(), ControllerConfig::default())?;
        controller.set_config(&WifiConfig::Station(given.first))?;

        let (stack, runner) = embassy_net::new(
            interface,
            embassy_net::Config::dhcpv4(Default::default()),
            &mut self.resources,
            seed,
        );

        Ok((
            Session {
                controller,
                stack,
                joined: &self.joined,
                ssid: config.ssid.clone(),
                unhinted: given.unhinted,
                passphrase,
            },
            Link { runner },
        ))
    }

    /// Raises the box's own access point and builds a static stack on it.
    ///
    /// The counterpart to [`Radio::acquire`], and deliberately the same shape:
    /// the caller drives [`Link::run`] and opens sockets on [`Session::stack`]
    /// exactly as it would for a station. There is no `connect` step — an
    /// access point is up the moment the controller starts.
    ///
    /// `max_connections(1)`, so a second device cannot *join* while somebody
    /// types their WiFi passphrase into the page. It does not stop anyone in
    /// range reading it — see [`SETUP_PASSWORD`] for why not.
    ///
    /// `password` is the card's `setup_password` when it has one and
    /// [`SETUP_PASSWORD`] when it does not. It is an argument rather than a
    /// read from here because this module does not open the card — the same
    /// rule that keeps the station's credentials a caller's business.
    pub fn serve(&mut self, seed: u64, password: &str) -> Result<(Session<'_>, Link<'_>), Error> {
        let ap = AccessPointConfig::default()
            .with_ssid(Ssid::try_from(SETUP_SSID)?)
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                Password::try_from(password)?,
            ))
            .with_max_connections(1);

        let interface = Interface::try_access_point().ok_or(Error::Busy)?;

        let mut controller =
            WifiController::new(self.wifi.reborrow(), ControllerConfig::default())?;
        controller.set_config(&WifiConfig::AccessPoint(ap))?;

        let (stack, runner) = embassy_net::new(
            interface,
            embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
                address: embassy_net::Ipv4Cidr::new(
                    embassy_net::Ipv4Address::from(teddiebox_portal::dhcp::SERVER_IP),
                    24,
                ),
                gateway: None,
                // Not `heapless::Vec::new()`: this firmware pins `heapless`
                // 0.8 for `teddiebox_config::Config`, but `embassy-net` 0.9.1
                // pulls in `heapless` 0.9 for its own `Vec` — two crates with
                // the same name, so naming the type here would build the
                // wrong one. `Default` lets inference pick whichever
                // `StaticConfigV4::dns_servers` actually asks for.
                dns_servers: Default::default(),
            }),
            &mut self.resources,
            seed,
        );

        // An access point never joins anything, so there is nothing to hint
        // and nothing to remember; `connect` is not called on it.
        Ok((
            Session {
                controller,
                stack,
                joined: &self.joined,
                ssid: heapless::String::new(),
                unhinted: None,
                passphrase: None,
            },
            Link { runner },
        ))
    }
}

impl<'d> Session<'d> {
    /// Associates with the configured access point.
    ///
    /// **Waits indefinitely.** `connect_async` returns when the driver reports
    /// either a connection or a disconnection, and an access point that simply
    /// never answers produces neither. The timeout belongs to the caller,
    /// which is the download task the device plan builds: it is the one that
    /// knows how long a box should sit with its radio on before giving up and
    /// going back to playing from the card. Racing this against a timer here
    /// would only guess that number a layer too low.
    ///
    /// A join hinted at the last access point that fails is tried once more
    /// without the hint — the router may have changed channel, or the box may
    /// have been carried to another access point of the same network — unless
    /// it failed on the passphrase, which a scan would not change.
    ///
    /// Measured with a deliberately wrong channel: the hinted attempt gave up
    /// after 1.6 s and the scan joined 3.0 s later, so a stale hint costs one
    /// slow join and is then replaced by the one that worked. The driver
    /// sometimes finds the network despite a wrong channel (3.15 s).
    ///
    /// A join with the stored key that fails for any reason is tried again
    /// with the passphrase — the router may have been re-keyed with the card
    /// edited to match, or not at all — so a stale key costs one failed join,
    /// never the network. Any reason, not only [`refused_credentials`]: that
    /// one is the refusal *this* router gives, and an access point that
    /// refuses a raw key differently, or cannot take one at all, must not
    /// leave a box with a correct passphrase off its network.
    pub async fn connect(&mut self) -> Result<JoinedWith, ConnectionError> {
        let mut with = JoinedWith::GivenKey;
        let mut result = self.join().await;
        if let (Err(error), Some(passphrase)) = (&result, self.passphrase.take()) {
            esp_println::println!(
                "teddiebox: net the stored key did not join — {error:?} — trying the passphrase"
            );
            // A hinted attempt whose fallback is already spent went to the
            // scan, so the hint was stale: aiming the passphrase at it again
            // costs another ~1.6 s timeout before its own scan.
            let first = match (self.unhinted.is_none(), passphrase.unhinted) {
                (true, Some(unhinted)) => unhinted,
                (_, unhinted) => {
                    self.unhinted = unhinted;
                    passphrase.first
                }
            };
            self.controller.set_config(&WifiConfig::Station(first))?;
            with = JoinedWith::Passphrase;
            result = self.join().await;
        }
        let info = result?;
        *self.joined.borrow_mut() = Some(Joined {
            ssid: self.ssid.clone(),
            bssid: info.bssid,
            channel: info.channel,
        });
        Ok(with)
    }

    /// One join with the configuration set, falling back to a scan if a
    /// hinted one fails for any reason but the key.
    async fn join(&mut self) -> Result<esp_radio::wifi::sta::ConnectedInfo, ConnectionError> {
        let mut result = self.controller.connect_async().await;
        if let (Err(error), Some(unhinted)) = (&result, self.unhinted.take()) {
            if !refused_credentials(error) {
                esp_println::println!(
                    "teddiebox: net the last access point did not answer — {error:?} — scanning for it"
                );
                *self.joined.borrow_mut() = None;
                self.controller.set_config(&WifiConfig::Station(unhinted))?;
                result = self.controller.connect_async().await;
            }
        }
        result
    }

    /// The stack sockets are opened on.
    ///
    /// `Copy`, so the caller can hold it alongside the session rather than
    /// borrowing through it — which matters, because the session is borrowed
    /// mutably by [`Session::connect`].
    pub fn stack(&self) -> Stack<'d> {
        self.stack
    }
}

impl Link<'_> {
    /// Polls the network stack. Never returns.
    pub async fn run(&mut self) -> ! {
        self.runner.run().await
    }
}

/// Whether an association failed because the access point refused the key.
///
/// The one thing this module says about *why* an association failed, and it
/// answers a bool rather than naming a reason of its own: what the box does
/// about a refused passphrase belongs to the reducer, which is tested. This is
/// only the part that cannot be — the reason codes live in `esp-radio` and
/// there is no host to run them on.
///
/// **Narrow on purpose, and measured.** A wrong passphrase on this bench
/// produced `FourWayHandshakeTimeout`: the access point answered, the key did
/// not verify, and the handshake ran out. Several neighbouring reasons —
/// `MicFailure`, `AkmpInvalid`, `CipherSuiteRejected` — also mean a key or a
/// cipher the access point would not take, and are deliberately *not* here.
/// Everything unlisted stays "could not be reached", because the expensive
/// mistake is the other one: telling somebody their passphrase is wrong when
/// the router was merely off sends them to retype something already correct.
/// Widening this is one line, and wants its own observation behind it.
pub fn refused_credentials(error: &ConnectionError) -> bool {
    matches!(
        error,
        ConnectionError::Failed(info) if info.reason == DisconnectReason::FourWayHandshakeTimeout
    )
}

/// Takes the radio down.
///
/// Dropping is what does the work: the runner's interface releases the station
/// singleton, and the controller's `Drop` deinitialises the driver and powers
/// the modem down. This function exists to fix the order — the interface has
/// to go before the driver that feeds it — and to give the moment a name at
/// the call site.
pub fn release(session: Session<'_>, link: Link<'_>) {
    drop(link);
    drop(session);
}

/// A seed for the network stack's port, sequence and transaction numbers.
///
/// `embassy_net::new` derives TCP initial sequence numbers, ephemeral source
/// ports and the DHCP transaction id from this one value. The boot-relative
/// microsecond clock that used to supply it is guessable by anyone who knows
/// roughly when the box was switched on, which is exactly what those three
/// numbers are meant not to be.
///
/// The hardware generator needs no setup and is what [`crate::tls::HardwareRng`]
/// wraps. It is only *cryptographically* secure with the RF subsystem running,
/// and this is called before the radio is powered — so the clock stays in, as
/// the low bits of something that also differs between two boxes powered on at
/// the same moment. Neither source has to be perfect for the result to be a
/// long way better than a counter starting at zero.
pub fn seed() -> u64 {
    let rng = esp_hal::rng::Rng::new();
    let random = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    random ^ embassy_time::Instant::now().as_micros()
}
