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

// Nothing calls this yet — the caller is the download task, which the device
// plan builds once the radio has been up on a bench. It is committed unwired
// rather than left on a branch so that the dependency set it needs is proven
// by the same build gate as the rest of the firmware.
#![allow(dead_code)]

use embassy_net::{Runner, Stack, StackResources};
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::{
    sta::StationConfig, AuthenticationMethodConfig, Config as WifiConfig, ConnectionError,
    ControllerConfig, Interface, Password, Ssid, WifiController, WifiError,
};
use teddiebox_config::Config;

/// Socket slots the stack is given.
///
/// Two of the three are spent before any of our code runs: `embassy-net`
/// opens a DNS socket unconditionally when the `dns` feature is on, and a
/// DHCP socket whenever the stack is built with `Config::dhcpv4`. That leaves
/// exactly one for the TCP connection the downloader opens — which is enough,
/// because the box talks to one server at a time by design: a second
/// concurrent download would only be competing for the same card. Anything
/// that wants a second socket has to raise this number, and a static IP
/// configuration would free one.
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
}

/// A powered, configured radio.
///
/// Dropping it takes the modem down, so it is held for exactly as long as the
/// network is wanted.
pub struct Session<'d> {
    controller: WifiController<'d>,
    stack: Stack<'d>,
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
        }
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
    pub fn acquire(
        &mut self,
        config: &Config,
        seed: u64,
    ) -> Result<(Session<'_>, Link<'_>), Error> {
        // WPA2-Personal is a *minimum* threshold rather than an exact mode, so
        // a WPA3 access point still associates; it is open and WEP networks
        // this refuses, which is the refusal worth having.
        let station = StationConfig::default()
            .with_ssid(Ssid::try_from(config.ssid.as_str())?)
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                Password::try_from(config.password.as_str())?,
            ));

        // Taken before the controller so that a session refused for a
        // singleton already in use has not powered the modem on the way out.
        let interface = Interface::try_station().ok_or(Error::Busy)?;

        let mut controller =
            WifiController::new(self.wifi.reborrow(), ControllerConfig::default())?;
        controller.set_config(&WifiConfig::Station(station))?;

        let (stack, runner) = embassy_net::new(
            interface,
            embassy_net::Config::dhcpv4(Default::default()),
            &mut self.resources,
            seed,
        );

        Ok((Session { controller, stack }, Link { runner }))
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
    pub async fn connect(&mut self) -> Result<(), ConnectionError> {
        self.controller.connect_async().await?;
        Ok(())
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
