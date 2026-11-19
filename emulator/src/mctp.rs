// Licensed under the Apache-2.0 license

//! MCTP over I3C (DSP0233) from the host's side: the bus owner that sends
//! requests to the device's endpoint and reads its responses.

use crate::host::Host;
use crate::i3c::I3cHost;

/// The MCTP transport header: version, destination EID, source EID, flags.
const HEADER_LEN: usize = 4;

/// The most message bytes the host puts in one packet: the MCTP baseline
/// transmission unit. OpenPRoT's host tests send the same.
const MAX_PACKET_BODY: usize = 64;

/// The largest packet the device sends, header included.
const MAX_REPLY_PACKET: usize = 254;

/// Cycles to let the device drain one packet from its receive FIFO before
/// the next is written.
const WRITE_GAP_CYCLES: u64 = 150_000;

/// Cycles to wait for each packet of a response.
const REPLY_TIMEOUT_CYCLES: u64 = 50_000_000;

const FLAG_SOM: u8 = 0x80;
const FLAG_EOM: u8 = 0x40;
const FLAG_TAG_OWNER: u8 = 0x08;
const TAG_MASK: u8 = 0x07;

/// The host's MCTP endpoint.
pub struct MctpHost {
    host: Host,
    i3c: I3cHost,
    /// The host's EID.
    own_eid: u8,
    /// The device's EID.
    device_eid: u8,
    /// The tag of the next request.
    tag: u8,
}

impl MctpHost {
    pub fn new(host: &Host, own_eid: u8, device_eid: u8) -> Result<Self, String> {
        Ok(Self {
            host: host.clone(),
            i3c: I3cHost::new(host)?,
            own_eid,
            device_eid,
            tag: 0,
        })
    }

    /// Sends `payload` to the device as a request of message type `msg_type`
    /// and returns the payload of the response.
    pub async fn request(&mut self, msg_type: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        let tag = self.tag;
        self.tag = (self.tag + 1) & TAG_MASK;
        let mut body = vec![msg_type];
        body.extend_from_slice(payload);
        for packet in packets(&body, tag, true, self.device_eid, self.own_eid) {
            self.i3c.private_write(&packet);
            self.host.run_for(WRITE_GAP_CYCLES).await?;
        }

        let (reply_tag, tag_owner, reply) = self.read_message().await?;
        if tag_owner {
            return Err("expected a response, but the device sent a request".into());
        }
        if reply_tag != tag {
            return Err(format!(
                "the response has tag {reply_tag}, the request had {tag}"
            ));
        }
        match reply.split_first() {
            Some((&reply_type, payload)) if reply_type & 0x7f == msg_type => Ok(payload.to_vec()),
            Some((&reply_type, _)) => Err(format!(
                "the response has message type {}, the request had {msg_type}",
                reply_type & 0x7f
            )),
            None => Err("the response is empty".into()),
        }
    }

    /// Reads one message from the device and reassembles it. Returns the tag,
    /// the tag owner bit and the body (the type byte, then the payload).
    async fn read_message(&mut self) -> Result<(u8, bool, Vec<u8>), String> {
        let mut body = Vec::new();
        loop {
            let packet = self.i3c.private_read(REPLY_TIMEOUT_CYCLES).await?;
            if packet.len() <= HEADER_LEN || packet.len() > MAX_REPLY_PACKET {
                return Err(format!("MCTP packet of {} bytes", packet.len()));
            }
            let (header, chunk) = packet.split_at(HEADER_LEN);
            let flags = header[3];
            if header[0] & 0x0f != 0x01 {
                return Err(format!("MCTP header version {}", header[0] & 0x0f));
            }
            if header[1] != self.own_eid || header[2] != self.device_eid {
                return Err(format!(
                    "MCTP packet from EID {} to EID {}, expected from {} to {}",
                    header[2], header[1], self.device_eid, self.own_eid
                ));
            }
            if (flags & FLAG_SOM != 0) != body.is_empty() {
                return Err("MCTP packet out of order".into());
            }
            body.extend_from_slice(chunk);
            if flags & FLAG_EOM != 0 {
                return Ok((flags & TAG_MASK, flags & FLAG_TAG_OWNER != 0, body));
            }
        }
    }
}

/// The packets of a message: `body` (the type byte, then the payload) cut
/// into packets of at most `MAX_PACKET_BODY` bytes, each behind a transport
/// header. The flags mark the first and last packet and carry a 2-bit packet
/// sequence number, the tag owner bit (set for a request) and the tag.
fn packets(body: &[u8], tag: u8, tag_owner: bool, dest: u8, src: u8) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = body.chunks(MAX_PACKET_BODY).collect();
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut flags = (tag & TAG_MASK) | (((i & 0x03) as u8) << 4);
            if tag_owner {
                flags |= FLAG_TAG_OWNER;
            }
            if i == 0 {
                flags |= FLAG_SOM;
            }
            if i == last {
                flags |= FLAG_EOM;
            }
            let mut packet = vec![0x01, dest, src, flags];
            packet.extend_from_slice(chunk);
            packet
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::packets;

    #[test]
    fn one_packet_has_both_flags() {
        let p = packets(&[5, 0x10, 0x84, 0, 0], 3, true, 8, 9);
        assert_eq!(
            p,
            vec![vec![0x01, 8, 9, 0xc0 | 0x08 | 3, 5, 0x10, 0x84, 0, 0]]
        );
    }

    #[test]
    fn long_message_is_cut_at_the_baseline() {
        let body: Vec<u8> = (0..130).collect();
        let p = packets(&body, 0, false, 9, 8);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0][3], 0x80);
        assert_eq!(p[1][3], 0x10);
        assert_eq!(p[2][3], 0x40 | 0x20);
        assert_eq!(p[0].len(), 68);
        assert_eq!(p[2].len(), 4 + 2);
    }
}
