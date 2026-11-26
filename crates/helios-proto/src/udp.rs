//! Lightweight binary UDP framing (`HLP1`) shared between `helios-relay` and
//! `helios-transport` for:
//! 1. NAT public `ip:port` discovery (STUN-like observed address echo)
//! 2. Simultaneous UDP hole-punching probes for direct P2P QUIC connections
//! 3. Transparent encrypted QUIC UDP relay fallback via the DigitalOcean droplet

use std::net::SocketAddr;

use uuid::Uuid;

pub const UDP_MAGIC: &[u8; 4] = b"HLP1";

/// Packet type tags for `HLP1` UDP datagrams.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpPacketKind {
    /// Peer -> Relay: register UDP socket for `(session_id, role)` & ask for observed public address.
    Register = 0x01,
    /// Relay -> Peer: UTF-8 `SocketAddr` string of the peer's observed external NAT address.
    ObservedAddr = 0x02,
    /// Peer <-> Relay: forward an opaque TLS-1.3-encrypted QUIC datagram through the droplet.
    RelayQuic = 0x03,
    /// Peer -> Peer: direct UDP hole-punching probe.
    PunchProbe = 0x04,
    /// Peer -> Peer: direct UDP hole-punching acknowledgement.
    PunchAck = 0x05,
}

/// Role byte inside `HLP1` packets.
pub const ROLE_HOST: u8 = 0;
pub const ROLE_CLIENT: u8 = 1;

/// Header length for `Register` and `RelayQuic` packets: 4 (magic) + 1 (kind) + 16 (uuid) + 1 (role).
pub const HEADER_LEN: usize = 22;

pub fn encode_register(session_id: Uuid, role_byte: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN);
    buf.extend_from_slice(UDP_MAGIC);
    buf.push(UdpPacketKind::Register as u8);
    buf.extend_from_slice(session_id.as_bytes());
    buf.push(role_byte);
    buf
}

pub fn encode_observed_addr(addr: SocketAddr) -> Vec<u8> {
    let text = addr.to_string();
    let mut buf = Vec::with_capacity(5 + text.len());
    buf.extend_from_slice(UDP_MAGIC);
    buf.push(UdpPacketKind::ObservedAddr as u8);
    buf.extend_from_slice(text.as_bytes());
    buf
}

pub fn decode_observed_addr(pkt: &[u8]) -> Option<SocketAddr> {
    if pkt.len() < 6 || &pkt[..4] != UDP_MAGIC || pkt[4] != UdpPacketKind::ObservedAddr as u8 {
        return None;
    }
    let s = std::str::from_utf8(&pkt[5..]).ok()?;
    s.parse().ok()
}

pub fn encode_relay_quic(session_id: Uuid, role_byte: u8, quic_payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + quic_payload.len());
    buf.extend_from_slice(UDP_MAGIC);
    buf.push(UdpPacketKind::RelayQuic as u8);
    buf.extend_from_slice(session_id.as_bytes());
    buf.push(role_byte);
    buf.extend_from_slice(quic_payload);
    buf
}

pub fn encode_punch_probe(session_id: Uuid, is_ack: bool) -> [u8; 21] {
    let mut buf = [0u8; 21];
    buf[..4].copy_from_slice(UDP_MAGIC);
    buf[4] = if is_ack {
        UdpPacketKind::PunchAck as u8
    } else {
        UdpPacketKind::PunchProbe as u8
    };
    buf[5..21].copy_from_slice(session_id.as_bytes());
    buf
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodedUdpPacket<'a> {
    Register {
        session_id: Uuid,
        role_byte: u8,
    },
    ObservedAddr(SocketAddr),
    RelayQuic {
        session_id: Uuid,
        role_byte: u8,
        payload: &'a [u8],
    },
    PunchProbe {
        session_id: Uuid,
    },
    PunchAck {
        session_id: Uuid,
    },
}

pub fn decode_udp_packet(pkt: &[u8]) -> Option<DecodedUdpPacket<'_>> {
    if pkt.len() < 5 || &pkt[..4] != UDP_MAGIC {
        return None;
    }
    match pkt[4] {
        x if x == UdpPacketKind::Register as u8 => {
            if pkt.len() < HEADER_LEN {
                return None;
            }
            let session_id = Uuid::from_slice(&pkt[5..21]).ok()?;
            Some(DecodedUdpPacket::Register {
                session_id,
                role_byte: pkt[21],
            })
        }
        x if x == UdpPacketKind::ObservedAddr as u8 => {
            decode_observed_addr(pkt).map(DecodedUdpPacket::ObservedAddr)
        }
        x if x == UdpPacketKind::RelayQuic as u8 => {
            if pkt.len() < HEADER_LEN {
                return None;
            }
            let session_id = Uuid::from_slice(&pkt[5..21]).ok()?;
            Some(DecodedUdpPacket::RelayQuic {
                session_id,
                role_byte: pkt[21],
                payload: &pkt[HEADER_LEN..],
            })
        }
        x if x == UdpPacketKind::PunchProbe as u8 => {
            if pkt.len() < 21 {
                return None;
            }
            let session_id = Uuid::from_slice(&pkt[5..21]).ok()?;
            Some(DecodedUdpPacket::PunchProbe { session_id })
        }
        x if x == UdpPacketKind::PunchAck as u8 => {
            if pkt.len() < 21 {
                return None;
            }
            let session_id = Uuid::from_slice(&pkt[5..21]).ok()?;
            Some(DecodedUdpPacket::PunchAck { session_id })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_packet_roundtrips() {
        let sid = Uuid::new_v4();
        let reg = encode_register(sid, ROLE_HOST);
        assert_eq!(
            decode_udp_packet(&reg),
            Some(DecodedUdpPacket::Register {
                session_id: sid,
                role_byte: ROLE_HOST
            })
        );

        let relay = encode_relay_quic(sid, ROLE_CLIENT, b"encrypted-quic-bytes");
        assert_eq!(
            decode_udp_packet(&relay),
            Some(DecodedUdpPacket::RelayQuic {
                session_id: sid,
                role_byte: ROLE_CLIENT,
                payload: b"encrypted-quic-bytes",
            })
        );

        let addr: SocketAddr = "203.0.113.42:51820".parse().unwrap();
        let obs = encode_observed_addr(addr);
        assert_eq!(
            decode_udp_packet(&obs),
            Some(DecodedUdpPacket::ObservedAddr(addr))
        );
    }
}
