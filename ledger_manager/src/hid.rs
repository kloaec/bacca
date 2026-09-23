//! A minimal HID transport to Ledger devices with a configurable read timeout, used for the
//! websocket sessions of firmware updates.
//!
//! `ledger_transport_hidapi::TransportNativeHID` waits about 2.8 hours for an answer from the
//! device, and doesn't let us change it. A device which stops answering in the middle of a
//! firmware update would make us hang (nearly) forever. This transport uses the same framing
//! (see `write_apdu` and `read_apdu` in ledger-transport-hidapi 0.10, Apache-2.0, and
//! https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledgerjs/packages/devices/src/hid-framing.ts)
//! but lets the caller set a timeout for each exchange. It also sends the APDUs as raw bytes, the
//! same way Ledger Live forwards the APDUs of the HSM.

use crate::{device::find_ledger, error::Error};

use ledger_apdu::APDUAnswer;
use ledger_transport_hidapi::{
    hidapi::{HidApi, HidDevice},
    LedgerHIDError,
};

use std::time::{Duration, Instant};

const LEDGER_CHANNEL: u16 = 0x0101;
const TAG_APDU: u8 = 0x05;
/// For Windows compatibility, the buffer is prepended with a 0x00 report id so the actual packet
/// is 64 bytes.
const PACKET_WRITE_SIZE: usize = 65;
const PACKET_READ_SIZE: usize = 64;

/// A HID transport to a Ledger device with a read timeout.
pub(crate) struct HidTransport {
    device: HidDevice,
}

fn hid_err(e: ledger_transport_hidapi::hidapi::HidError) -> Error {
    Error::Hid(LedgerHIDError::Hid(e))
}

fn comm_err(s: &'static str) -> Error {
    Error::Hid(LedgerHIDError::Comm(s))
}

impl HidTransport {
    /// Refresh the list of HID devices and open the first Ledger found.
    pub fn connect(hid_api: &mut HidApi) -> Result<Self, Error> {
        hid_api.refresh_devices().map_err(hid_err)?;
        let dev = find_ledger(hid_api).ok_or(Error::DeviceNotFound)?;
        let device = dev.open_device(hid_api).map_err(hid_err)?;
        let _ = device.set_blocking_mode(true);
        Ok(Self { device })
    }

    fn write_apdu(&self, apdu: &[u8]) -> Result<(), Error> {
        for packet in frame_apdu(apdu)? {
            let written = self.device.write(&packet).map_err(hid_err)?;
            if written < packet.len() {
                return Err(comm_err("USB write error. Could not send whole message"));
            }
        }
        Ok(())
    }

    fn read_apdu(&self, timeout: Option<Duration>) -> Result<Vec<u8>, Error> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut buffer = [0u8; PACKET_READ_SIZE];
        let mut deframer = Deframer::default();
        loop {
            let timeout_ms = match deadline {
                // hidapi blocks without timeout with a negative value.
                None => -1,
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    // At least 1ms, 0 would mean non-blocking.
                    i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1)
                }
            };
            let read = self
                .device
                .read_timeout(&mut buffer, timeout_ms)
                .map_err(hid_err)?;
            if read == 0 {
                if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                    return Err(Error::Timeout("waiting for the device to answer"));
                }
                continue;
            }
            if let Some(answer) = deframer.push(&buffer, read)? {
                return Ok(answer);
            }
        }
    }

    /// Send this raw APDU to the device and wait for its answer, for at most `timeout` (forever
    /// if `None`).
    pub fn exchange_raw(
        &self,
        apdu: &[u8],
        timeout: Option<Duration>,
    ) -> Result<APDUAnswer<Vec<u8>>, Error> {
        self.write_apdu(apdu)?;
        let answer = self.read_apdu(timeout)?;
        APDUAnswer::from_answer(answer).map_err(|_| comm_err("response was too short"))
    }
}

/// Split an APDU into the HID packets to write (each prefixed with the 0x00 report id).
fn frame_apdu(apdu: &[u8]) -> Result<Vec<[u8; PACKET_WRITE_SIZE]>, Error> {
    let len = u16::try_from(apdu.len()).map_err(|_| comm_err("APDU too long"))?;
    let mut in_data = Vec::with_capacity(apdu.len() + 2);
    in_data.extend_from_slice(&len.to_be_bytes());
    in_data.extend_from_slice(apdu);

    let mut packets = Vec::new();
    for (seq, chunk) in in_data.chunks(PACKET_WRITE_SIZE - 6).enumerate() {
        let seq = u16::try_from(seq).map_err(|_| comm_err("APDU too long"))?;
        let mut packet = [0u8; PACKET_WRITE_SIZE];
        packet[1..3].copy_from_slice(&LEDGER_CHANNEL.to_be_bytes());
        packet[3] = TAG_APDU;
        packet[4..6].copy_from_slice(&seq.to_be_bytes());
        packet[6..6 + chunk.len()].copy_from_slice(chunk);
        packets.push(packet);
    }
    Ok(packets)
}

/// Reassemble an answer from the HID packets read from the device.
#[derive(Default)]
struct Deframer {
    answer: Vec<u8>,
    expected_len: usize,
    seq: u16,
}

impl Deframer {
    /// Process a packet of which `read` bytes were read. Returns the answer once complete.
    fn push(&mut self, packet: &[u8], read: usize) -> Result<Option<Vec<u8>>, Error> {
        let header_len = if self.seq == 0 { 7 } else { 5 };
        if read < header_len || packet.len() < header_len {
            return Err(comm_err("Read error. Incomplete header"));
        }
        if u16::from_be_bytes([packet[0], packet[1]]) != LEDGER_CHANNEL {
            return Err(comm_err("Invalid channel"));
        }
        if packet[2] != TAG_APDU {
            return Err(comm_err("Invalid tag"));
        }
        if u16::from_be_bytes([packet[3], packet[4]]) != self.seq {
            return Err(comm_err("Invalid sequence idx"));
        }
        if self.seq == 0 {
            self.expected_len = u16::from_be_bytes([packet[5], packet[6]]) as usize;
        }
        // Like ledger-transport-hidapi, consider the whole packet: the length of the answer tells
        // how much of it is data.
        let chunk = &packet[header_len..];
        let missing = self.expected_len - self.answer.len();
        self.answer
            .extend_from_slice(&chunk[..chunk.len().min(missing)]);
        if self.answer.len() >= self.expected_len {
            return Ok(Some(std::mem::take(&mut self.answer)));
        }
        self.seq = self.seq.checked_add(1).ok_or(comm_err("Answer too long"))?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        // GetVersion.
        let packets = frame_apdu(&[0xe0, 0x01, 0x00, 0x00, 0x00]).unwrap();
        assert_eq!(packets.len(), 1);
        let mut expected = [0u8; PACKET_WRITE_SIZE];
        expected[..12].copy_from_slice(&[
            0x00, 0x01, 0x01, 0x05, 0x00, 0x00, 0x00, 0x05, 0xe0, 0x01, 0x00, 0x00,
        ]);
        assert_eq!(packets[0], expected);

        // Raw APDUs are sent as is, e.g. without data length byte.
        let packets = frame_apdu(&[0xe0, 0x51, 0x00, 0x00]).unwrap();
        assert_eq!(
            &packets[0][6..12],
            &[0x00, 0x04, 0xe0, 0x51, 0x00, 0x00,][..]
        );
        assert!(packets[0][12..].iter().all(|b| *b == 0));

        // A long APDU is split in several packets, with increasing sequence index.
        let apdu: Vec<u8> = (0..=255u8).cycle().take(200).collect();
        let packets = frame_apdu(&apdu).unwrap();
        assert_eq!(packets.len(), 4); // 2 + 200 bytes, 59 per packet.
        let mut sent = Vec::new();
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(&p[..6], &[0x00, 0x01, 0x01, 0x05, 0x00, i as u8]);
            sent.extend_from_slice(&p[6..]);
        }
        assert_eq!(&sent[..2], &[0x00, 200]);
        assert_eq!(&sent[2..202], &apdu[..]);

        assert!(frame_apdu(&vec![0; 0x10000]).is_err());
    }

    fn device_packet(seq: u16, payload: &[u8]) -> [u8; PACKET_READ_SIZE] {
        let mut p = [0u8; PACKET_READ_SIZE];
        p[..3].copy_from_slice(&[0x01, 0x01, 0x05]);
        p[3..5].copy_from_slice(&seq.to_be_bytes());
        p[5..5 + payload.len()].copy_from_slice(payload);
        p
    }

    #[test]
    fn deframing() {
        // A short answer: 0x9000.
        let mut d = Deframer::default();
        let p = device_packet(0, &[0x00, 0x02, 0x90, 0x00]);
        assert_eq!(d.push(&p, 64).unwrap(), Some(vec![0x90, 0x00]));

        // An answer over two packets.
        let answer: Vec<u8> = (0..100u8).collect();
        let mut first = vec![0x00, 100];
        first.extend_from_slice(&answer[..57]);
        let mut d = Deframer::default();
        assert_eq!(d.push(&device_packet(0, &first), 64).unwrap(), None);
        assert_eq!(
            d.push(&device_packet(1, &answer[57..]), 64).unwrap(),
            Some(answer.clone())
        );

        // Errors.
        let mut d = Deframer::default();
        assert!(d
            .push(&device_packet(1, &[0x00, 0x02, 0x90, 0x00]), 64)
            .is_err());
        let mut d = Deframer::default();
        assert!(d.push(&device_packet(0, &[0x00, 0x02]), 6).is_err());
        let mut p = device_packet(0, &[0x00, 0x02, 0x90, 0x00]);
        p[2] = 0x02;
        assert!(Deframer::default().push(&p, 64).is_err());
        p[2] = 0x05;
        p[0] = 0x00;
        assert!(Deframer::default().push(&p, 64).is_err());
    }
}
