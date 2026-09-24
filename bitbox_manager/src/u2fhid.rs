//! U2F-over-HID framing, used by BitBox02 devices in both firmware and bootloader mode.
//!
//! A message is sent as an INIT packet `cid (u32 BE) | cmd | len (u16 BE) | data` followed by
//! CONT packets `cid | seq | data`, each packet being a 64 bytes USB report.
//!
//! References: `bitbox02-firmware/py/bitbox02/bitbox02/communication/u2fhid/u2fhid.py` and
//! `bitbox-api-rs/src/u2fframing.rs` (not exposed by the `bitbox-api` crate).

use std::time::Duration;

use crate::Error;

const REPORT_SIZE: usize = 64;
const INIT_HEADER_LEN: usize = 7;
const CONT_HEADER_LEN: usize = 5;
/// The sequence number of CONT packets goes from 0 to 127.
const MAX_PAYLOAD_LEN: usize =
    (REPORT_SIZE - INIT_HEADER_LEN) + 128 * (REPORT_SIZE - CONT_HEADER_LEN);
/// U2F HID error command.
const CMD_ERROR: u8 = 0x80 | 0x3f;

/// A HID device speaking U2F HID framing on a given command ("endpoint").
pub(crate) struct U2fHid {
    device: hidapi::HidDevice,
    cmd: u8,
    cid: u32,
}

impl U2fHid {
    pub(crate) fn new(device: hidapi::HidDevice, cmd: u8) -> Self {
        U2fHid {
            device,
            cmd,
            cid: generate_cid(),
        }
    }

    pub(crate) fn write(&self, data: &[u8]) -> Result<(), Error> {
        for packet in encode(self.cid, self.cmd, data)? {
            // Prepend the HID report id 0.
            let mut report = [0u8; REPORT_SIZE + 1];
            report[1..].copy_from_slice(&packet);
            self.device.write(&report)?;
        }
        Ok(())
    }

    /// Send a message and read the response. `timeout` applies to each USB report.
    pub(crate) fn query(&self, data: &[u8], timeout: Duration) -> Result<Vec<u8>, Error> {
        self.write(data)?;
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        decode(self.cid, self.cmd, || {
            let mut buf = [0u8; REPORT_SIZE];
            match self.device.read_timeout(&mut buf, timeout_ms)? {
                0 => Err(Error::Other("timeout waiting for the device".to_string())),
                n => Ok(buf[..n].to_vec()),
            }
        })
    }
}

/// A random channel id, excluding 0 and the broadcast cid.
fn generate_cid() -> u32 {
    loop {
        let mut buf = [0u8; 4];
        if getrandom::getrandom(&mut buf).is_err() {
            return 0xff00ff00;
        }
        let cid = u32::from_be_bytes(buf);
        if cid != 0 && cid != 0xffff_ffff {
            return cid;
        }
    }
}

/// Split a message into USB reports (without the leading HID report id).
fn encode(cid: u32, cmd: u8, data: &[u8]) -> Result<Vec<[u8; REPORT_SIZE]>, Error> {
    if data.len() > MAX_PAYLOAD_LEN {
        return Err(Error::Other(format!(
            "payload too large: {} bytes",
            data.len()
        )));
    }
    let mut init = [0u8; REPORT_SIZE];
    init[..4].copy_from_slice(&cid.to_be_bytes());
    init[4] = cmd;
    init[5..7].copy_from_slice(&(data.len() as u16).to_be_bytes());
    let n = data.len().min(REPORT_SIZE - INIT_HEADER_LEN);
    init[INIT_HEADER_LEN..INIT_HEADER_LEN + n].copy_from_slice(&data[..n]);
    let mut packets = vec![init];
    for (seq, chunk) in data[n..].chunks(REPORT_SIZE - CONT_HEADER_LEN).enumerate() {
        let mut cont = [0u8; REPORT_SIZE];
        cont[..4].copy_from_slice(&cid.to_be_bytes());
        cont[4] = seq as u8;
        cont[CONT_HEADER_LEN..CONT_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        packets.push(cont);
    }
    Ok(packets)
}

/// Reassemble a message from the USB reports returned by `next_packet`.
fn decode(
    cid: u32,
    cmd: u8,
    mut next_packet: impl FnMut() -> Result<Vec<u8>, Error>,
) -> Result<Vec<u8>, Error> {
    let framing = |s: String| Error::Other(format!("USB framing error: {}", s));
    let mut read = || -> Result<Vec<u8>, Error> {
        let packet = next_packet()?;
        if packet.len() < INIT_HEADER_LEN {
            return Err(framing(format!(
                "packet too short ({} bytes)",
                packet.len()
            )));
        }
        let packet_cid = u32::from_be_bytes(packet[..4].try_into().unwrap());
        if packet_cid != cid {
            return Err(framing(format!(
                "channel id mismatch: {:#x} != {:#x}",
                packet_cid, cid
            )));
        }
        Ok(packet)
    };

    let init = read()?;
    if init[4] == CMD_ERROR {
        return Err(u2f_error(init[INIT_HEADER_LEN]));
    }
    if init[4] != cmd {
        return Err(framing(format!(
            "command mismatch: {:#x} != {:#x}",
            init[4], cmd
        )));
    }
    let len = u16::from_be_bytes([init[5], init[6]]) as usize;
    let mut data = init[INIT_HEADER_LEN..].to_vec();
    data.truncate(len);
    let mut seq = 0u8;
    while data.len() < len {
        let cont = read()?;
        if cont[4] != seq {
            return Err(framing(format!(
                "unexpected sequence number {} (expected {})",
                cont[4], seq
            )));
        }
        seq = seq.wrapping_add(1);
        let chunk = &cont[CONT_HEADER_LEN..];
        data.extend_from_slice(&chunk[..chunk.len().min(len - data.len())]);
    }
    Ok(data)
}

fn u2f_error(code: u8) -> Error {
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
    Error::Other(format!(
        "device returned an U2F HID error: {} ({:#04x})",
        s, code
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_packets(cid: u32, cmd: u8, packets: &[[u8; REPORT_SIZE]]) -> Result<Vec<u8>, Error> {
        let mut it = packets.iter();
        decode(cid, cmd, || {
            Ok(it.next().expect("more packets needed").to_vec())
        })
    }

    #[test]
    fn encode_vectors() {
        // Same vectors as `test_u2fhid_encode_single/multi` in bitbox-api-rs/src/u2fframing.rs.
        let packets = encode(0xEEEEEEEE, 0x55, b"\x01\x02\x03\x04").unwrap();
        let mut expect = [0u8; 64];
        expect[..11].copy_from_slice(b"\xEE\xEE\xEE\xEE\x55\x00\x04\x01\x02\x03\x04");
        assert_eq!(packets, vec![expect]);

        let payload: Vec<u8> = (0..65u8).collect();
        let packets = encode(0xEEEEEEEE, 0x55, &payload).unwrap();
        let mut expect = [[0u8; 64]; 2];
        expect[0][..7].copy_from_slice(b"\xEE\xEE\xEE\xEE\x55\x00\x41");
        expect[0][7..].copy_from_slice(&payload[..57]);
        expect[1][..5].copy_from_slice(b"\xEE\xEE\xEE\xEE\x00");
        expect[1][5..13].copy_from_slice(&payload[57..]);
        assert_eq!(packets, expect.to_vec());

        assert!(encode(1, 0xc3, &vec![0; MAX_PAYLOAD_LEN + 1]).is_err());
    }

    #[test]
    fn roundtrip() {
        for len in [0usize, 1, 56, 57, 58, 115, 116, 117, 4098, MAX_PAYLOAD_LEN] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let packets = encode(0x12345678, 0xc1, &payload).unwrap();
            // `decode_packets` panics if more packets than sent are read.
            assert_eq!(decode_packets(0x12345678, 0xc1, &packets).unwrap(), payload);
        }
    }

    #[test]
    fn decode_errors() {
        let packets = encode(0x12345678, 0xc1, &[1, 2, 3]).unwrap();
        assert!(decode_packets(0x1, 0xc1, &packets).is_err());
        assert!(decode_packets(0x12345678, 0xc3, &packets).is_err());
        let err = encode(0x12345678, CMD_ERROR, &[0x06]).unwrap();
        let e = decode_packets(0x12345678, 0xc1, &err).unwrap_err();
        assert!(e.to_string().contains("channel busy"));
        let packets = encode(0x12345678, 0xc1, &[0u8; 200]).unwrap();
        assert!(decode_packets(0x12345678, 0xc1, &[packets[0], packets[2]]).is_err());
        assert!(decode(1, 0xc1, || Ok(vec![0; 3])).is_err());
    }
}
