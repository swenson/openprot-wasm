// Licensed under the Apache-2.0 license

//! The host's side of the I3C bus: the controller that the MCU's I3C target
//! talks to.
//!
//! OpenPRoT's host tests reach the emulated target through a TCP socket and a
//! controller thread. A browser has neither, so this drives the target's
//! queues in the emulator directly. The transfers are the same: a private
//! write carries a packet and its PEC to the target, and the target raises an
//! in-band interrupt (IBI) when it has a packet for the host to read.

use mcu_emulator_periph::I3cTarget;
use mcu_hw_model::McuHwModel;
use mcu_testing_common::i3c::{I3cTcriCommand, I3cTcriCommandXfer};

use crate::host::Host;

/// Bit 29 of a transfer command's first word: read (1) or write (0).
const COMMAND_READ: u32 = 1 << 29;

/// The data length is the high 16 bits of a transfer command's second word.
const COMMAND_LENGTH_SHIFT: u32 = 16;

/// The I3C controller, with the MCU as its one target.
pub struct I3cHost {
    host: Host,
    target: I3cTarget,
    /// The target's dynamic address.
    address: u8,
    /// IBIs the target has raised that no read has answered yet.
    pending_ibis: usize,
}

impl I3cHost {
    pub fn new(host: &Host) -> Result<Self, String> {
        let (target, address) = host.with_model(|m| (m.i3c_target(), m.i3c_address()));
        Ok(Self {
            host: host.clone(),
            target,
            address: address.ok_or("the model has no I3C target")?,
            pending_ibis: 0,
        })
    }

    /// Sends `packet` to the target as a private write, followed by its PEC.
    pub fn private_write(&mut self, packet: &[u8]) {
        let mut data = packet.to_vec();
        data.push(pec(self.address << 1, packet));
        self.send(false, data);
    }

    /// Waits for the target to raise an IBI, reads the packet it has ready,
    /// and returns it with the PEC checked and removed.
    pub async fn private_read(&mut self, timeout: u64) -> Result<Vec<u8>, String> {
        let mut read_requested = false;
        let data = self
            .host
            .clone()
            .wait_until("a packet from the I3C target", timeout, |_| {
                self.pending_ibis += self.target.get_ibis().len();
                if !read_requested && self.pending_ibis > 0 {
                    self.pending_ibis -= 1;
                    read_requested = true;
                    // A length of zero reads the whole packet.
                    self.send(true, Vec::new());
                }
                if read_requested {
                    self.target.get_response().map(|response| response.data)
                } else {
                    None
                }
            })
            .await?;
        let Some((&received, packet)) = data.split_last() else {
            return Err("the I3C target answered a read with no data".into());
        };
        let expected = pec((self.address << 1) | 1, packet);
        if received != expected {
            return Err(format!(
                "bad PEC on a packet from the I3C target: got 0x{received:02x}, expected 0x{expected:02x}"
            ));
        }
        Ok(packet.to_vec())
    }

    /// Queues a regular transfer command for the target: a write carrying
    /// `data`, or a read.
    fn send(&mut self, read: bool, data: Vec<u8>) {
        let words = [
            if read { COMMAND_READ } else { 0 },
            (data.len() as u32) << COMMAND_LENGTH_SHIFT,
        ];
        let cmd = I3cTcriCommand::try_from(words).expect("a regular transfer command");
        self.target.send_command(I3cTcriCommandXfer { cmd, data });
    }
}

/// The packet error code of `data` sent to or from `address_byte` (the 7-bit
/// address, then the read/write bit): CRC-8 with polynomial 0x07, as SMBus
/// uses.
fn pec(address_byte: u8, data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in std::iter::once(&address_byte).chain(data) {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::pec;

    #[test]
    fn pec_matches_crc8_smbus() {
        // The CRC-8/SMBUS check value.
        assert_eq!(pec(b'1', b"23456789"), 0xf4);
        assert_eq!(pec(0, &[]), 0);
    }
}
