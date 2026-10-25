//! The other end of the board's Ethernet cable: a host, as small as one
//! can be and still talk to Hubris's network stack.
//!
//! Hubris speaks IPv6 only, on its link-local address, so this does too:
//! it has a MAC and the link-local address made from it, answers neighbour
//! solicitations for that address (which is how the board finds where to
//! send a reply) and echo requests, sends UDP datagrams and echo requests,
//! and hands back what UDP and echo replies it hears. It learns the board's
//! addresses from what the board sends; until it has, it sends to all
//! nodes, ff02::1, which the board's stack accepts.
//!
//! It is frames in and frames out; whoever owns the board moves them
//! between it and the board's Ethernet.

/// The host's MAC: locally administered.
pub const MAC: [u8; 6] = [0x02, 0x00, 0x5A, 0x51, 0x00, 0x01];
const ALL_NODES: [u8; 16] = [0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const ALL_NODES_MAC: [u8; 6] = [0x33, 0x33, 0, 0, 0, 1];
/// The port this host sends from.
pub const PORT: u16 = 50_000;

const IPV6: [u8; 2] = [0x86, 0xDD];
const ICMPV6: u8 = 58;
const UDP: u8 = 17;

/// The link-local address an interface makes from its MAC: fe80::, then
/// the modified EUI-64.
pub fn link_local(mac: [u8; 6]) -> [u8; 16] {
    let mut ip = [0; 16];
    ip[0..2].copy_from_slice(&[0xFE, 0x80]);
    ip[8..16].copy_from_slice(&[mac[0] ^ 2, mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]]);
    ip
}

/// An address, as `fe80::5aff:fe51:1`.
pub fn show(ip: &[u8; 16]) -> String {
    let groups: Vec<u16> = (0..8).map(|i| u16::from_be_bytes([ip[2 * i], ip[2 * i + 1]])).collect();
    // The longest run of zero groups, if it is two or more, becomes `::`.
    let (mut best, mut run) = ((0, 0), (0, 0));
    for (i, &g) in groups.iter().enumerate() {
        run = if g == 0 { (if run.1 == 0 { i } else { run.0 }, run.1 + 1) } else { (0, 0) };
        if run.1 > best.1 {
            best = run;
        }
    }
    let hex = |gs: &[u16]| gs.iter().map(|g| format!("{g:x}")).collect::<Vec<_>>().join(":");
    if best.1 < 2 {
        return hex(&groups);
    }
    format!("{}::{}", hex(&groups[..best.0]), hex(&groups[best.0 + best.1..]))
}

/// Something the host heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// A datagram; `multicast` if it was sent to a group (all nodes, say)
    /// rather than to this host.
    Udp { from: [u8; 16], source: u16, port: u16, payload: Vec<u8>, multicast: bool },
    Pong { from: [u8; 16], sequence: u16 },
}

#[derive(Clone, Debug)]
pub struct Peer {
    pub mac: [u8; 6],
    pub ip: [u8; 16],
    /// The board's MAC and address, once it has said something.
    pub board: Option<([u8; 6], [u8; 16])>,
}

impl Default for Peer {
    fn default() -> Self {
        Peer { mac: MAC, ip: link_local(MAC), board: None }
    }
}

impl Peer {
    fn destination(&self) -> ([u8; 6], [u8; 16]) {
        self.board.unwrap_or((ALL_NODES_MAC, ALL_NODES))
    }

    /// A UDP datagram to the board's `port`.
    pub fn udp(&self, port: u16, payload: &[u8]) -> Vec<u8> {
        let mut udp = Vec::with_capacity(8 + payload.len());
        udp.extend_from_slice(&PORT.to_be_bytes());
        udp.extend_from_slice(&port.to_be_bytes());
        udp.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(payload);
        let (mac, ip) = self.destination();
        self.ipv6(mac, ip, UDP, udp)
    }

    /// An echo request to the board.
    pub fn ping(&self, sequence: u16) -> Vec<u8> {
        let mut icmp = vec![128, 0, 0, 0, 0x5A, 0x51];
        icmp.extend_from_slice(&sequence.to_be_bytes());
        icmp.extend_from_slice(b"zaqaru");
        let (mac, ip) = self.destination();
        self.ipv6(mac, ip, ICMPV6, icmp)
    }

    /// A frame from the board: what to send back, and what was heard.
    pub fn receive(&mut self, frame: &[u8]) -> (Vec<Vec<u8>>, Vec<Heard>) {
        let (mut replies, mut heard) = (Vec::new(), Vec::new());
        let Some(packet) = Packet::parse(frame) else { return (replies, heard) };
        if packet.source_mac == self.mac {
            return (replies, heard);
        }
        if packet.source[0..2] == [0xFE, 0x80] {
            self.board = Some((packet.source_mac, packet.source));
        }
        let to_us = packet.destination == self.ip || packet.destination[0] == 0xFF;
        match packet.next {
            ICMPV6 if packet.body.len() >= 8 => {
                let body = packet.body;
                match body[0] {
                    // A neighbour solicitation for this host's address.
                    135 if body.len() >= 24 && body[8..24] == self.ip => {
                        let solicited = packet.source != [0; 16];
                        let mut na = vec![136, 0, 0, 0, if solicited { 0x60 } else { 0x20 }, 0, 0, 0];
                        na.extend_from_slice(&self.ip);
                        na.extend_from_slice(&[2, 1]);
                        na.extend_from_slice(&self.mac);
                        let to = if solicited { (packet.source_mac, packet.source) } else { (ALL_NODES_MAC, ALL_NODES) };
                        replies.push(self.ipv6(to.0, to.1, ICMPV6, na));
                    }
                    128 if to_us => {
                        let mut reply = body.to_vec();
                        reply[0] = 129;
                        replies.push(self.ipv6(packet.source_mac, packet.source, ICMPV6, reply));
                    }
                    129 if to_us => {
                        heard.push(Heard::Pong { from: packet.source, sequence: u16::from_be_bytes([body[6], body[7]]) })
                    }
                    _ => {}
                }
            }
            UDP if to_us && packet.body.len() >= 8 => {
                let body = packet.body;
                let length = (u16::from_be_bytes([body[4], body[5]]) as usize).clamp(8, body.len());
                heard.push(Heard::Udp {
                    from: packet.source,
                    source: u16::from_be_bytes([body[0], body[1]]),
                    port: u16::from_be_bytes([body[2], body[3]]),
                    payload: body[8..length].to_vec(),
                    multicast: packet.destination[0] == 0xFF,
                });
            }
            _ => {}
        }
        (replies, heard)
    }

    /// An Ethernet frame carrying an IPv6 packet from this host, with the
    /// upper layer's checksum filled in at bytes 2–3 of ICMPv6 and 6–7 of
    /// UDP.
    fn ipv6(&self, mac: [u8; 6], to: [u8; 16], next: u8, mut body: Vec<u8>) -> Vec<u8> {
        let at = if next == UDP { 6 } else { 2 };
        body[at..at + 2].copy_from_slice(&[0, 0]);
        let sum = checksum(&self.ip, &to, next, &body);
        body[at..at + 2].copy_from_slice(&sum.to_be_bytes());
        let mut frame = Vec::with_capacity(54 + body.len());
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&self.mac);
        frame.extend_from_slice(&IPV6);
        frame.extend_from_slice(&[0x60, 0, 0, 0]);
        frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
        // Hop limit 255, which neighbour discovery requires.
        frame.extend_from_slice(&[next, 255]);
        frame.extend_from_slice(&self.ip);
        frame.extend_from_slice(&to);
        frame.extend_from_slice(&body);
        frame
    }
}

/// An IPv6 packet in an Ethernet frame, as far as this host reads one: no
/// extension headers but hop-by-hop, which it skips.
struct Packet<'a> {
    source_mac: [u8; 6],
    source: [u8; 16],
    destination: [u8; 16],
    next: u8,
    body: &'a [u8],
}

impl<'a> Packet<'a> {
    fn parse(frame: &'a [u8]) -> Option<Packet<'a>> {
        if frame.len() < 54 || frame[12..14] != IPV6 {
            return None;
        }
        let ip = &frame[14..];
        let length = u16::from_be_bytes([ip[4], ip[5]]) as usize;
        let mut next = ip[6];
        let mut body = ip.get(40..40 + length)?;
        if next == 0 && body.len() >= 8 {
            let skip = 8 * (body[1] as usize + 1);
            next = body[0];
            body = body.get(skip..)?;
        }
        Some(Packet {
            source_mac: frame[6..12].try_into().unwrap(),
            source: ip[8..24].try_into().unwrap(),
            destination: ip[24..40].try_into().unwrap(),
            next,
            body,
        })
    }
}

/// The Internet checksum over IPv6's pseudo-header and `body`.
fn checksum(source: &[u8; 16], destination: &[u8; 16], next: u8, body: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut add = |bytes: &[u8]| {
        for pair in bytes.chunks(2) {
            sum += u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]));
        }
    };
    add(source);
    add(destination);
    add(&(body.len() as u32).to_be_bytes());
    add(&[0, 0, 0, next]);
    add(body);
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    let folded = !(sum as u16);
    // UDP over IPv6 may not send zero, which means no checksum.
    if folded == 0 { 0xFFFF } else { folded }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_local_address_is_the_modified_eui64() {
        assert_eq!(show(&link_local(MAC)), "fe80::5aff:fe51:1");
        assert_eq!(show(&ALL_NODES), "ff02::1");
    }

    #[test]
    fn a_neighbour_solicitation_for_the_host_is_answered() {
        let board = Peer { mac: [0x0E, 1, 2, 3, 4, 5], ip: link_local([0x0E, 1, 2, 3, 4, 5]), board: None };
        let mut host = Peer::default();
        // The board asks, from its own address, who has the host's.
        let mut ns = vec![135, 0, 0, 0, 0, 0, 0, 0];
        ns.extend_from_slice(&host.ip);
        let frame = board.ipv6([0x33, 0x33, 0xFF, 0x51, 0, 1], host.ip, ICMPV6, ns);
        let (replies, heard) = host.receive(&frame);
        assert!(heard.is_empty());
        assert_eq!(host.board, Some((board.mac, board.ip)), "learned from what the board sent");
        let na = Packet::parse(&replies[0]).unwrap();
        assert_eq!((na.next, na.body[0], na.destination), (ICMPV6, 136, board.ip));
        assert_eq!(&na.body[8..24], &host.ip);
        assert_eq!(checksum(&na.source, &na.destination, ICMPV6, na.body), 0xFFFF, "sums to all ones");
    }

    #[test]
    fn a_datagram_is_heard_with_its_ports() {
        let mut host = Peer::default();
        let mut board = Peer { mac: [0x0E, 1, 2, 3, 4, 5], ip: link_local([0x0E, 1, 2, 3, 4, 5]), board: None };
        board.board = Some((host.mac, host.ip));
        let (_, heard) = host.receive(&board.udp(7, b"hello"));
        assert_eq!(heard, [Heard::Udp { from: board.ip, source: PORT, port: 7, payload: b"hello".to_vec(), multicast: false }]);
    }
}
