//! The Wi-Fi radio.
//!
//! The box usually plays files already on the card, so Wi-Fi stays off until
//! something needs the network. Between [`Radio::acquire`] and [`release`]
//! the modem is powered and connected; otherwise it is off.
//!
//! This module only owns the [`WifiController`] and the [`embassy_net`]
//! runner. It does not use the card or open sockets. Credentials come in as
//! a [`teddiebox_config::Config`]; the download code opens the sockets.

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
use teddiebox_core::heapless;

/// The network the box raises when both ears are held at boot.
///
/// **Built in, never read from the card**, so a broken `CONFIG.TXT` can
/// still be fixed.
pub const SETUP_SSID: &str = "teddiebox-setup";

/// The passphrase for that network, documented in `README.md`.
///
/// Public on purpose: a person holding a broken box must be able to find it.
///
/// **So it does not keep the traffic secret.** Someone in range who knows
/// this passphrase can capture (or force) the WPA2 handshake and read the
/// page, including the home Wi-Fi passphrase. `max_connections(1)` in
/// [`Radio::serve`] only limits *joining*, not listening.
///
/// WPA2 still keeps the network from being open: phones do not join by
/// themselves, and reading the page needs intent, being nearby, and a
/// capture during the ten minutes the portal is up. If that is not enough,
/// set up the box away from untrusted people, or change the home passphrase
/// afterwards.
///
/// `setup_password` in `CONFIG.TXT` can set a different one. This stays the
/// fallback for a box with no card, an unreadable card, or a broken
/// `CONFIG.TXT`.
pub const SETUP_PASSWORD: &str = "teddiebox";

/// Socket slots a station (client) is given, the default for [`Radio`].
///
/// `embassy-net` uses two sockets itself: one for DNS (with the `dns`
/// feature) and one for DHCP (with `Config::dhcpv4`). That leaves one for the
/// download's TCP connection, which is enough: the box talks to one server at
/// a time. Measured: with 3 everything works; with 2, the station panics in
/// smoltcp (`socket_set.rs:83`, full `SocketSet`) when TLS opens its socket.
///
/// The slots live in the `Radio`, which a normal boot keeps in `.bss`, so
/// every slot here is taken from the stack region.
pub const STATION_SOCKETS: usize = 3;

/// Socket slots the setup portal is given.
///
/// `embassy-net` spends one on DNS even with the portal's static IP; the
/// portal adds two TCP listeners for HTTP and a UDP socket for its DHCP
/// *server*. Measured: with 3, entering setup panics in smoltcp
/// (`socket_set.rs:83`, full `SocketSet`). Its `Radio` lives in the decode
/// scratch, not in `.bss`, so slots here cost the stack region nothing.
pub const PORTAL_SOCKETS: usize = 4;

/// Why the radio could not be brought up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A session is already using the station interface.
    ///
    /// The interface is a singleton in `esp-radio`, so this means a previous
    /// session was not released, not a hardware fault.
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
/// Kept for the whole time the firmware runs. It only holds the socket
/// memory; power and connection belong to a [`Session`].
///
/// `SOCKETS` is how many socket slots the stack gets; see
/// [`STATION_SOCKETS`].
pub struct Radio<'d, const SOCKETS: usize = STATION_SOCKETS> {
    wifi: WIFI<'d>,
    resources: StackResources<SOCKETS>,
    /// The access point the last join reached, so the next one can go
    /// straight to it. See [`Joined`].
    joined: RefCell<Option<Joined>>,
}

/// Where the last join ended up: which access point, on which channel.
///
/// With this hint a join took 1.8-1.9 s, against 3.0-3.1 s with a scan
/// (measured, with the access point on channel 11, which the scan reaches
/// last). Kept in RAM only: a stale hint after moving the box would cost a
/// failed join every time.
struct Joined {
    /// The network it was for. Only used for the same SSID.
    ssid: heapless::String<{ teddiebox_config::MAX_SSID }>,
    bssid: [u8; 6],
    channel: u8,
}

/// A powered, configured radio.
///
/// Dropping it powers the modem down, so hold it only while the network is
/// needed.
pub struct Session<'d> {
    controller: WifiController<'d>,
    stack: Stack<'d>,
    /// The last successful join, updated by [`Session::connect`].
    joined: &'d RefCell<Option<Joined>>,
    ssid: heapless::String<{ teddiebox_config::MAX_SSID }>,
    /// The configuration without the hint, to fall back to. `None` if the
    /// join had no hint.
    unhinted: Option<StationConfig>,
    /// The same join with the passphrase, in case the access point refuses
    /// the stored key. `None` if the key already is the passphrase.
    passphrase: Option<Attempt>,
}

/// One set of credentials to join with: with the hint first (if there is
/// one), and without it as a fallback.
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
/// `embassy-net` separates running the stack from using it. [`Link::run`]
/// never returns, so it runs alongside (via `select`) the code that uses the
/// [`Stack`].
pub struct Link<'d> {
    runner: Runner<'d, Interface>,
}

impl<'d, const SOCKETS: usize> Radio<'d, SOCKETS> {
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
    /// Needs no credentials, [`Interface`] or network stack: a controller is
    /// created, used and dropped, so the modem is off afterwards and the
    /// station interface is not touched.
    ///
    /// A scan cannot fail because of a password or an address, so an empty
    /// result points at the radio, the antenna or the surroundings.
    ///
    /// Fills `out` and returns how many entries were written; the scan stops
    /// at `out.len()` results.
    ///
    /// **This allocates.** `scan_async` returns a `Vec` on the Wi-Fi heap. It
    /// is copied into `out` and dropped before returning, because that heap is
    /// sized for the Wi-Fi driver only.
    ///
    /// **No timeout here**; the caller decides how long to wait, as for
    /// [`Session::connect`].
    pub async fn scan(&mut self, out: &mut [AccessPointInfo]) -> Result<usize, Error> {
        let mut controller =
            WifiController::new(self.wifi.reborrow(), ControllerConfig::default())?;
        // With the driver's default time per channel (10-20 ms), a scan
        // misses networks: four scans in a row found 7, 3, 6 and 8 access
        // points, and one missed a network right next to the box at -29 dBm.
        // So it waits longer per channel. Fourteen channels at 300 ms is
        // still well within the caller's timeout.
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
    /// Returns before connecting; [`Session::connect`] does that, since it
    /// can take seconds and fail for reasons outside the box.
    ///
    /// `seed` must differ between boots (it seeds port and transaction
    /// numbers); see [`seed`].
    ///
    /// `key` is either `config`'s passphrase or the key derived from it, as 64
    /// hex characters, which the driver uses directly: a join took 64–72 ms
    /// with the key against 1.81 s with the passphrase.
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
            // WPA2-Personal is a *minimum*, so WPA3 access points still work;
            // open and WEP networks are refused.
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

        // Taken before creating the controller, so the modem is not powered
        // if the interface is busy.
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
    /// Used like [`Radio::acquire`]: the caller runs [`Link::run`] and opens
    /// sockets on [`Session::stack`]. There is no `connect` step; the access
    /// point is up as soon as the controller starts.
    ///
    /// `max_connections(1)`, so no second device can *join* while someone
    /// types their Wi-Fi passphrase. It does not stop listening; see
    /// [`SETUP_PASSWORD`].
    ///
    /// `password` is the card's `setup_password` if set, otherwise
    /// [`SETUP_PASSWORD`]. Passed in, because this module does not read the
    /// card.
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
                dns_servers: heapless::Vec::new(),
            }),
            &mut self.resources,
            seed,
        );

        // An access point does not join anything, so there is no hint and
        // `connect` is not called.
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
    /// **May wait forever.** `connect_async` only returns on a connection or
    /// a disconnection, and an access point that never answers produces
    /// neither. The caller (the download task) sets the timeout.
    ///
    /// If a join using the last access point as a hint fails, it is tried
    /// once more without the hint (the router may have changed channel, or
    /// the box moved), unless the passphrase was refused.
    ///
    /// Measured with a wrong channel: the hinted attempt gave up after 1.6 s,
    /// and the scan joined 3.0 s later. The driver sometimes finds the
    /// network despite a wrong channel (3.15 s).
    ///
    /// If a join with the stored key fails for any reason, it is tried again
    /// with the passphrase (the router's password may have changed). Any
    /// reason, not only [`refused_credentials`]: another access point may
    /// reject a raw key differently.
    pub async fn connect(&mut self) -> Result<JoinedWith, ConnectionError> {
        let mut with = JoinedWith::GivenKey;
        let mut result = self.join().await;
        if let (Err(error), Some(passphrase)) = (&result, self.passphrase.take()) {
            esp_println::println!(
                "teddiebox: net the stored key did not join — {error:?} — trying the passphrase"
            );
            // If the hinted attempt already fell back to a scan, the hint was
            // stale, so do not use it again for the passphrase (it would
            // cost another ~1.6 s timeout).
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

    /// One join with the current configuration, falling back to a scan if a
    /// hinted join fails for any reason except the key.
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
    /// `Copy`, so the caller can keep it while [`Session::connect`] borrows
    /// the session mutably.
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
/// Only returns a bool; what to do about it is decided in the reducer, where
/// it is tested.
///
/// **Narrow on purpose.** A wrong passphrase produced
/// `FourWayHandshakeTimeout` (measured). Similar reasons (`MicFailure`,
/// `AkmpInvalid`, `CipherSuiteRejected`) are left out until seen. Anything
/// else counts as "could not be reached": wrongly telling someone their
/// passphrase is wrong is the worse mistake.
pub fn refused_credentials(error: &ConnectionError) -> bool {
    matches!(
        error,
        ConnectionError::Failed(info) if info.reason == DisconnectReason::FourWayHandshakeTimeout
    )
}

/// Takes the radio down.
///
/// Dropping does the work: the interface is released, and the controller's
/// `Drop` shuts down the driver and powers the modem off. This function fixes
/// the order: the interface must go before the driver.
pub fn release(session: Session<'_>, link: Link<'_>) {
    drop(link);
    drop(session);
}

/// A seed for the network stack's port, sequence and transaction numbers.
///
/// `embassy_net::new` derives TCP sequence numbers, source ports and the
/// DHCP transaction id from this value, so it must not be guessable. The time
/// since boot alone would be.
///
/// Uses the hardware random number generator (as [`crate::tls::HardwareRng`]
/// does). It is only cryptographically secure while the radio is on, and
/// this runs before that, so the clock is mixed in too.
pub fn seed() -> u64 {
    let rng = esp_hal::rng::Rng::new();
    let random = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    random ^ embassy_time::Instant::now().as_micros()
}
