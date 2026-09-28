//! A datagram from any device on the setup portal's open access point, and
//! the reply the portal would send to it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_portal::dhcp;

fuzz_target!(|data: &[u8]| {
    if let Some(incoming) = dhcp::parse(data) {
        let _ = dhcp::reply(&incoming, dhcp::Reply::Offer);
        let _ = dhcp::reply(&incoming, dhcp::Reply::Ack);
    }
});
