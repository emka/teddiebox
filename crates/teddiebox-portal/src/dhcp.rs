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

    /// A DISCOVER an Android handset actually sent, captured off the box on
    /// 2026-09-16 while a phone joined the setup access point and was given a
    /// lease. Byte-exact except the client's hardware address, which is
    /// substituted in both `chaddr` and option 61 — the fixture's worth is its
    /// option layout, not somebody's MAC.
    ///
    /// It exists because [`discover`] above was assembled from the RFC, and a
    /// synthetic datagram agrees with whatever reading of the spec produced
    /// it. Two of its assumptions turned out to be wrong about real clients:
    ///
    /// * **It sets the broadcast flag; this phone does not.** `discover` uses
    ///   `0x8000` and calls it what a handset sends. A real one sent `0x0000`.
    ///   Nothing breaks — [`reply`] broadcasts whatever the flag says, because
    ///   a client with no address cannot be reached any other way — but the
    ///   comment claimed something untrue.
    /// * **It carries one option; this carries seven** (53, 61, 57, 60, 12,
    ///   55, 80), and option 80 has **length zero**. So the option walk was
    ///   only ever exercised against a single option sitting first, which is
    ///   the case that cannot fail.
    fn captured_discover() -> [u8; 300] {
        [
            0x01, 0x01, 0x06, 0x00, 0x64, 0xEC, 0x7F, 0x98, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x02, 0x11, 0x22, 0x33, 0x44, 0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x63, 0x82,
            0x53, 0x63, 0x35, 0x01, 0x01, 0x3D, 0x07, 0x01, 0x02, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x39, 0x02, 0x05, 0xDC, 0x3C, 0x0F, 0x61, 0x6E, 0x64, 0x72, 0x6F, 0x69, 0x64, 0x2D,
            0x64, 0x68, 0x63, 0x70, 0x2D, 0x31, 0x37, 0x0C, 0x08, 0x50, 0x69, 0x78, 0x65, 0x6C,
            0x2D, 0x37, 0x61, 0x37, 0x0C, 0x01, 0x03, 0x06, 0x0F, 0x1A, 0x1C, 0x33, 0x3A, 0x3B,
            0x2B, 0x72, 0x6C, 0x50, 0x00, 0xFF,
        ]
    }

    /// The parser against bytes it did not have a hand in shaping.
    #[test]
    fn a_real_handsets_discover_is_recognised() {
        let got = parse(&captured_discover()).unwrap();
        assert_eq!(got.kind, Kind::Discover);
        assert_eq!(got.chaddr, [0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        // Not the broadcast flag the synthetic fixture assumes.
        assert_eq!(got.flags, [0x00, 0x00]);
        assert_eq!(got.xid, [0x64, 0xEC, 0x7F, 0x98]);
    }

    /// Option 53 is found when it sits *behind* a zero-length option.
    ///
    /// The first version of this test moved 53's value in place and proved
    /// nothing: 53 is the first option in both fixtures, so a walk that reads
    /// offset 240 and stops still finds it. Confirmed by mutation — breaking
    /// the walk to `break` after one option left all 52 tests green.
    ///
    /// Real clients are free to order options however they like, and this one
    /// sent a zero-length option 80. So the case that matters is 53 arriving
    /// last, behind an option whose length byte is zero: get the stride wrong
    /// and the walk either stops early or never advances.
    fn discover_with_message_type_last() -> Vec<u8, MAX_DATAGRAM> {
        let captured = captured_discover();
        let mut d: Vec<u8, MAX_DATAGRAM> = Vec::new();
        d.extend_from_slice(&captured[..OPTIONS]).unwrap();
        // The options this handset actually sent, minus 53, in its own order.
        for option in [
            &[61u8, 7, 1, 0x02, 0x11, 0x22, 0x33, 0x44, 0x55][..],
            &[57, 2, 0x05, 0xDC],
            &[12, 3, b'a', b'b', b'c'],
            &[80, 0], // Rapid Commit: a length byte of zero.
        ] {
            d.extend_from_slice(option).unwrap();
        }
        // Only now the message type, followed by the terminator.
        d.extend_from_slice(&[53, 1, 1]).unwrap();
        d.push(OPTION_END).unwrap();
        d
    }

    #[test]
    fn the_option_walk_reaches_a_message_type_behind_a_zero_length_option() {
        assert_eq!(
            parse(&discover_with_message_type_last()).unwrap().kind,
            Kind::Discover
        );
    }

    #[test]
    fn the_option_walk_reads_a_request_behind_a_zero_length_option() {
        let mut d = discover_with_message_type_last();
        let last = d.len() - 2;
        d[last] = 3; // DHCPREQUEST
        assert_eq!(parse(&d).unwrap().kind, Kind::Request);
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
