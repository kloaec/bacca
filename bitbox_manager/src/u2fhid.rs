//! U2F-over-HID framing, used by BitBox02 devices in both firmware and bootloader mode.
//!
//! A message is sent as an INIT packet `cid (u32 BE) | cmd | len (u16 BE) | data` followed by
//! CONT packets `cid | seq | data`, each packet being a 64 bytes USB report.
//!
//! References: `bitbox02-firmware/py/bitbox02/bitbox02/communication/u2fhid/u2fhid.py` and
//! `bitbox-api-rs/src/u2fframing.rs`. The `bitbox-api` crate does not expose its framing layer
//! publicly, hence this (small) reimplementation.

use std::{fmt, time::Duration};

pub const USB_REPORT_SIZE: usize = 64;
const INIT_HEADER_LEN: usize = 7;
const CONT_HEADER_LEN: usize = 5;
const INIT_DATA_LEN: usize = USB_REPORT_SIZE - INIT_HEADER_LEN;
const CONT_DATA_LEN: usize = USB_REPORT_SIZE - CONT_HEADER_LEN;
/// The sequence number of CONT packets goes from 0 to 127.
const MAX_CONT_PACKETS: usize = 128;
/// Max payload: 57 + 128 * 59 = 7609 bytes.
pub const MAX_PAYLOAD_LEN: usize = INIT_DATA_LEN + MAX_CONT_PACKETS * CONT_DATA_LEN;

/// U2F HID error command.
const CMD_ERROR: u8 = 0x80 | 0x3f;
pub const CID_BROADCAST: u32 = 0xffff_ffff;

#[derive(Debug)]
pub enum HidError {
    Hid(hidapi::HidError),
    /// A read timed out.
    Timeout,
    /// The payload is too large to be framed.
    PayloadTooLarge(usize),
    /// Received a malformed or unexpected packet.
    Framing(String),
    /// The device replied with a U2F HID error.
    Device(u8),
}

impl fmt::Display for HidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hid(e) => write!(f, "HID error: {}", e),
            Self::Timeout => write!(f, "timeout waiting for the device"),
            Self::PayloadTooLarge(l) => write!(f, "payload too large: {} bytes", l),
            Self::Framing(s) => write!(f, "USB framing error: {}", s),
            Self::Device(code) => {
                let s = match code {
                    0x01 => "invalid command",
                    0x02 => "invalid parameter",
                    0x03 => "invalid length",
                    0x04 => "invalid sequence",
                    0x05 => "message timeout",
                    0x06 => "channel busy",
                    0x0a => "lock required",
                    0x0b => "invalid channel id",
                    0x7e => "encryption failed",
                    _ => "other error",
                };
                write!(f, "device returned an U2F HID error: {} ({:#04x})", s, code)
            }
        }
    }
}

impl std::error::Error for HidError {}

impl From<hidapi::HidError> for HidError {
    fn from(e: hidapi::HidError) -> Self {
        HidError::Hid(e)
    }
}

/// Split a message into 64 bytes USB reports (without the leading HID report id).
pub fn encode(cid: u32, cmd: u8, data: &[u8]) -> Result<Vec<[u8; USB_REPORT_SIZE]>, HidError> {
    if data.len() > MAX_PAYLOAD_LEN || data.len() > u16::MAX as usize {
        return Err(HidError::PayloadTooLarge(data.len()));
    }
    let mut packets = Vec::new();

    let mut init = [0u8; USB_REPORT_SIZE];
    init[..4].copy_from_slice(&cid.to_be_bytes());
    init[4] = cmd;
    init[5..7].copy_from_slice(&(data.len() as u16).to_be_bytes());
    let n = data.len().min(INIT_DATA_LEN);
    init[INIT_HEADER_LEN..INIT_HEADER_LEN + n].copy_from_slice(&data[..n]);
    packets.push(init);

    for (seq, chunk) in data[n..].chunks(CONT_DATA_LEN).enumerate() {
        let mut cont = [0u8; USB_REPORT_SIZE];
        cont[..4].copy_from_slice(&cid.to_be_bytes());
        cont[4] = seq as u8;
        cont[CONT_HEADER_LEN..CONT_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        packets.push(cont);
    }
    Ok(packets)
}

/// Incremental decoder of a framed response.
#[derive(Debug)]
pub struct Decoder {
    cid: u32,
    cmd: u8,
    expected_len: Option<usize>,
    next_seq: u8,
    data: Vec<u8>,
}

impl Decoder {
    pub fn new(cid: u32, cmd: u8) -> Self {
        Decoder {
            cid,
            cmd,
            expected_len: None,
            next_seq: 0,
            data: Vec::new(),
        }
    }

    /// Feed one USB report. Returns the full message once all packets were received.
    pub fn push(&mut self, packet: &[u8]) -> Result<Option<Vec<u8>>, HidError> {
        if packet.len() < INIT_HEADER_LEN {
            return Err(HidError::Framing(format!(
                "packet too short ({} bytes)",
                packet.len()
            )));
        }
        let cid = u32::from_be_bytes(packet[..4].try_into().expect("4 bytes"));
        if cid != self.cid {
            return Err(HidError::Framing(format!(
                "channel id mismatch: {:#x} != {:#x}",
                cid, self.cid
            )));
        }
        match self.expected_len {
            None => {
                let cmd = packet[4];
                if cmd == CMD_ERROR {
                    return Err(HidError::Device(packet[INIT_HEADER_LEN]));
                }
                if cmd != self.cmd {
                    return Err(HidError::Framing(format!(
                        "command mismatch: {:#x} != {:#x}",
                        cmd, self.cmd
                    )));
                }
                let len = u16::from_be_bytes([packet[5], packet[6]]) as usize;
                let data = &packet[INIT_HEADER_LEN..];
                self.data.extend_from_slice(&data[..len.min(data.len())]);
                self.expected_len = Some(len);
            }
            Some(len) => {
                let seq = packet[4];
                if seq != self.next_seq {
                    return Err(HidError::Framing(format!(
                        "unexpected sequence number {} (expected {})",
                        seq, self.next_seq
                    )));
                }
                self.next_seq = self.next_seq.wrapping_add(1);
                let data = &packet[CONT_HEADER_LEN..];
                let missing = len - self.data.len();
                self.data
                    .extend_from_slice(&data[..missing.min(data.len())]);
            }
        }
        let len = self.expected_len.expect("set above");
        if self.data.len() >= len {
            Ok(Some(std::mem::take(&mut self.data)))
        } else {
            Ok(None)
        }
    }
}

/// Generate a random channel id, excluding 0 and the broadcast cid.
pub fn generate_cid() -> u32 {
    loop {
        let mut buf = [0u8; 4];
        if getrandom::getrandom(&mut buf).is_err() {
            return 0xff00ff00;
        }
        let cid = u32::from_be_bytes(buf);
        if cid != 0 && cid != CID_BROADCAST {
            return cid;
        }
    }
}

/// A HID device speaking U2F HID framing on a given command ("endpoint").
pub struct U2fHidDevice {
    device: hidapi::HidDevice,
    cmd: u8,
    cid: u32,
}

impl U2fHidDevice {
    pub fn new(device: hidapi::HidDevice, cmd: u8) -> Self {
        U2fHidDevice {
            device,
            cmd,
            cid: generate_cid(),
        }
    }

    /// Send a message.
    pub fn write(&self, data: &[u8]) -> Result<(), HidError> {
        for packet in encode(self.cid, self.cmd, data)? {
            let mut report = [0u8; USB_REPORT_SIZE + 1];
            // Report id 0.
            report[1..].copy_from_slice(&packet);
            self.device.write(&report)?;
        }
        Ok(())
    }

    /// Read a message. `timeout` applies to each USB report.
    pub fn read(&self, timeout: Duration) -> Result<Vec<u8>, HidError> {
        let mut decoder = Decoder::new(self.cid, self.cmd);
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        loop {
            let mut buf = [0u8; USB_REPORT_SIZE];
            let n = self.device.read_timeout(&mut buf, timeout_ms)?;
            if n == 0 {
                return Err(HidError::Timeout);
            }
            if let Some(msg) = decoder.push(&buf[..n])? {
                return Ok(msg);
            }
        }
    }

    pub fn query(&self, data: &[u8], timeout: Duration) -> Result<Vec<u8>, HidError> {
        self.write(data)?;
        self.read(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_single() {
        // Same vector as `test_u2fhid_encode_single` in bitbox-api-rs/src/u2fframing.rs.
        let packets = encode(0xEEEEEEEE, 0x55, b"\x01\x02\x03\x04").unwrap();
        assert_eq!(packets.len(), 1);
        let mut expect = [0u8; 64];
        expect[..11].copy_from_slice(b"\xEE\xEE\xEE\xEE\x55\x00\x04\x01\x02\x03\x04");
        assert_eq!(packets[0], expect);
    }

    #[test]
    fn encode_multi() {
        // Same vector as `test_u2fhid_encode_multi` in bitbox-api-rs/src/u2fframing.rs.
        let payload: Vec<u8> = (0..65u8).collect();
        let packets = encode(0xEEEEEEEE, 0x55, &payload).unwrap();
        assert_eq!(packets.len(), 2);
        let mut expect = [0u8; 128];
        expect[..7].copy_from_slice(b"\xEE\xEE\xEE\xEE\x55\x00\x41");
        expect[7..64].copy_from_slice(&payload[..57]);
        expect[64..69].copy_from_slice(b"\xEE\xEE\xEE\xEE\x00");
        expect[69..77].copy_from_slice(&payload[57..]);
        assert_eq!(packets[0][..], expect[..64]);
        assert_eq!(packets[1][..], expect[64..]);
    }

    #[test]
    fn encode_empty_and_limits() {
        let packets = encode(1, 0xc3, b"").unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(&packets[0][..7], &[0, 0, 0, 1, 0xc3, 0, 0]);
        // A bootloader write chunk: 'w' | chunk num | 4096 bytes.
        let chunk = vec![0xab; 4098];
        let packets = encode(1, 0xc3, &chunk).unwrap();
        assert_eq!(packets.len(), 1 + (4098 - 57usize).div_ceil(59));
        assert!(encode(1, 0xc3, &vec![0; MAX_PAYLOAD_LEN]).is_ok());
        assert!(matches!(
            encode(1, 0xc3, &vec![0; MAX_PAYLOAD_LEN + 1]),
            Err(HidError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn roundtrip() {
        for len in [
            0usize,
            1,
            56,
            57,
            58,
            115,
            116,
            117,
            1000,
            4098,
            MAX_PAYLOAD_LEN,
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let packets = encode(0x12345678, 0xc1, &payload).unwrap();
            let mut dec = Decoder::new(0x12345678, 0xc1);
            let mut out = None;
            for (i, p) in packets.iter().enumerate() {
                let r = dec.push(p).unwrap();
                if i + 1 < packets.len() {
                    assert!(r.is_none());
                } else {
                    out = r;
                }
            }
            assert_eq!(out.unwrap(), payload, "len {}", len);
        }
    }

    #[test]
    fn decode_errors() {
        let packets = encode(0x12345678, 0xc1, &[1, 2, 3]).unwrap();
        // Wrong cid.
        assert!(matches!(
            Decoder::new(0x1, 0xc1).push(&packets[0]),
            Err(HidError::Framing(_))
        ));
        // Wrong cmd.
        assert!(matches!(
            Decoder::new(0x12345678, 0xc3).push(&packets[0]),
            Err(HidError::Framing(_))
        ));
        // Error cmd.
        let err = encode(0x12345678, CMD_ERROR, &[0x06]).unwrap();
        assert!(matches!(
            Decoder::new(0x12345678, 0xc1).push(&err[0]),
            Err(HidError::Device(0x06))
        ));
        // Wrong sequence.
        let packets = encode(0x12345678, 0xc1, &[0u8; 200]).unwrap();
        let mut dec = Decoder::new(0x12345678, 0xc1);
        dec.push(&packets[0]).unwrap();
        assert!(matches!(dec.push(&packets[2]), Err(HidError::Framing(_))));
        // Too short.
        assert!(Decoder::new(0x12345678, 0xc1).push(&[0; 3]).is_err());
    }

    #[test]
    fn cid() {
        for _ in 0..100 {
            let cid = generate_cid();
            assert_ne!(cid, 0);
            assert_ne!(cid, CID_BROADCAST);
        }
    }
}
