//! One lease, four messages.
//!
//! `embassy-net`'s `dhcpv4` is the *client*, and smoltcp carries no server, so
//! this is the box's own. It is deliberately the smallest thing that gets a
//! phone an address: one fixed lease handed to whoever asks, `DISCOVER` and
//! `REQUEST` answered, everything else ignored.

use heapless::Vec;

pub const SERVER_IP: [u8; 4] = [192, 168, 4, 1];
pub const CLIENT_IP: [u8; 4] = [192, 168, 4, 2];
const SUBNET_MASK: [u8; 4] = [255, 255, 255, 0];
/// An hour, in seconds, big-endian. Longer than any setup session.
const LEASE_SECONDS: [u8; 4] = [0, 0, 14, 16];

/// The fixed fields, the cookie, and room for the options.
pub const MAX_DATAGRAM: usize = 590;

const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const FIXED: usize = 236;
const OPTIONS: usize = 240;
const OPTION_MESSAGE_TYPE: u8 = 53;
const OPTION_SERVER_ID: u8 = 54;
const OPTION_SUBNET_MASK: u8 = 1;
const OPTION_LEASE_TIME: u8 = 51;
const OPTION_END: u8 = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Discover,
    Request,
    /// A message this server has nothing to say about.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Offer,
    Ack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incoming {
    pub kind: Kind,
    pub xid: [u8; 4],
    pub chaddr: [u8; 6],
    pub flags: [u8; 2],
}

/// Reads a client's datagram, or `None` if it is not one.
pub fn parse(datagram: &[u8]) -> Option<Incoming> {
    if datagram.len() < OPTIONS || datagram[0] != 1 {
        return None;
    }
    if datagram[FIXED..OPTIONS] != COOKIE {
        return None;
    }

    let mut kind = Kind::Other;
    let mut i = OPTIONS;
    loop {
        let code = *datagram.get(i)?;
        if code == OPTION_END {
            break;
        }
        // Option 0 is padding and carries no length byte.
        if code == 0 {
            i += 1;
            continue;
        }
        let length = *datagram.get(i + 1)? as usize;
        let value = datagram.get(i + 2..i + 2 + length)?;
        if code == OPTION_MESSAGE_TYPE {
            kind = match value.first()? {
                1 => Kind::Discover,
                3 => Kind::Request,
                _ => Kind::Other,
            };
        }
        i += 2 + length;
    }

    Some(Incoming {
        kind,
        xid: datagram[4..8].try_into().ok()?,
        chaddr: datagram[28..34].try_into().ok()?,
        flags: datagram[10..12].try_into().ok()?,
    })
}

/// Builds the answer: the same lease every time, to whoever asked.
pub fn reply(to: &Incoming, kind: Reply) -> Vec<u8, MAX_DATAGRAM> {
    let mut out: Vec<u8, MAX_DATAGRAM> = Vec::new();
    out.resize_default(OPTIONS).expect("fits MAX_DATAGRAM");

    out[0] = 2; // BOOTREPLY
    out[1] = 1; // Ethernet
    out[2] = 6; // MAC length
    out[4..8].copy_from_slice(&to.xid);
    out[10..12].copy_from_slice(&to.flags);
    out[16..20].copy_from_slice(&CLIENT_IP); // yiaddr
    out[20..24].copy_from_slice(&SERVER_IP); // siaddr
    out[28..34].copy_from_slice(&to.chaddr);
    out[FIXED..OPTIONS].copy_from_slice(&COOKIE);

    let message_type = match kind {
        Reply::Offer => 2,
        Reply::Ack => 5,
    };
    let options = [
        &[OPTION_MESSAGE_TYPE, 1, message_type][..],
        &[OPTION_SERVER_ID, 4][..],
        &SERVER_IP[..],
        &[OPTION_SUBNET_MASK, 4][..],
        &SUBNET_MASK[..],
        &[OPTION_LEASE_TIME, 4][..],
        &LEASE_SECONDS[..],
        &[OPTION_END][..],
    ];
    for part in options {
        out.extend_from_slice(part).expect("fits MAX_DATAGRAM");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DISCOVER as a handset sends one: BOOTREQUEST over Ethernet, a
    /// six-byte MAC, the cookie, option 53 = 1, and the terminator.
    fn discover() -> [u8; 244] {
        let mut d = [0u8; 244];
        d[0] = 1; // op: BOOTREQUEST
        d[1] = 1; // htype: Ethernet
        d[2] = 6; // hlen
        d[4..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // xid
        d[10..12].copy_from_slice(&[0x80, 0x00]); // flags: broadcast
        d[28..34].copy_from_slice(&[0x02, 0x11, 0x22, 0x33, 0x44, 0x55]); // chaddr
        d[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]); // cookie
        d[240..243].copy_from_slice(&[53, 1, 1]); // DHCPDISCOVER
        d[243] = 0xFF;
        d
    }

    #[test]
    fn a_discover_is_recognised() {
        let got = parse(&discover()).unwrap();
        assert_eq!(got.kind, Kind::Discover);
        assert_eq!(got.xid, [0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(got.chaddr, [0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(got.flags, [0x80, 0x00]);
    }

    #[test]
    fn a_request_is_recognised() {
        let mut d = discover();
        d[242] = 3; // DHCPREQUEST
        assert_eq!(parse(&d).unwrap().kind, Kind::Request);
    }

    #[test]
    fn a_release_is_neither() {
        let mut d = discover();
        d[242] = 7; // DHCPRELEASE
        assert_eq!(parse(&d).unwrap().kind, Kind::Other);
    }

    #[test]
    fn a_reply_from_another_server_is_not_ours_to_answer() {
        let mut d = discover();
        d[0] = 2; // BOOTREPLY
        assert!(parse(&d).is_none());
    }

    #[test]
    fn a_datagram_without_the_cookie_is_not_dhcp() {
        let mut d = discover();
        d[236] = 0;
        assert!(parse(&d).is_none());
    }

    #[test]
    fn a_datagram_too_short_for_the_fixed_fields_is_refused() {
        assert!(parse(&[0u8; 100]).is_none());
    }

    #[test]
    fn an_option_running_past_the_end_does_not_panic() {
        let mut d = discover();
        d[240..243].copy_from_slice(&[53, 200, 1]); // length lies
        assert!(parse(&d).is_none());
    }

    #[test]
    fn an_offer_is_exactly_these_bytes() {
        let built = reply(&parse(&discover()).unwrap(), Reply::Offer);

        assert_eq!(built[0], 2); // op: BOOTREPLY
        assert_eq!(built[1], 1); // htype
        assert_eq!(built[2], 6); // hlen
        assert_eq!(&built[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]); // xid echoed
        assert_eq!(&built[10..12], &[0x80, 0x00]); // flags echoed
        assert_eq!(&built[16..20], &[192, 168, 4, 2]); // yiaddr
        assert_eq!(&built[28..34], &[0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(&built[236..240], &[0x63, 0x82, 0x53, 0x63]);
        assert_eq!(
            &built[240..],
            &[
                53, 1, 2, // message type: OFFER
                54, 4, 192, 168, 4, 1, // server identifier
                1, 4, 255, 255, 255, 0, // subnet mask
                51, 4, 0, 0, 14, 16,  // lease: 3600s
                255, // end
            ]
        );
    }

    #[test]
    fn an_ack_differs_from_an_offer_only_in_its_message_type() {
        let incoming = parse(&discover()).unwrap();
        let offer = reply(&incoming, Reply::Offer);
        let ack = reply(&incoming, Reply::Ack);
        assert_eq!(offer[242], 2);
        assert_eq!(ack[242], 5);
        assert_eq!(offer[..242], ack[..242]);
        assert_eq!(offer[243..], ack[243..]);
    }

    /// The box routes nothing and resolves nothing. Offering a gateway the
    /// phone cannot reach makes it wait on one.
    #[test]
    fn no_router_or_dns_option_is_offered() {
        let built = reply(&parse(&discover()).unwrap(), Reply::Offer);
        let options = &built[240..];
        let mut i = 0;
        while i < options.len() && options[i] != 255 {
            assert_ne!(options[i], 3, "a router option was offered");
            assert_ne!(options[i], 6, "a DNS option was offered");
            i += 2 + options[i + 1] as usize;
        }
    }
}
