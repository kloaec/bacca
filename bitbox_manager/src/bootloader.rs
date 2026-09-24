//! The BitBox02 bootloader API.
//!
//! References: `bitbox02-firmware/py/bitbox02/bitbox02/bitbox02/bootloader.py`,
//! `bitbox-wallet-app/vendor/github.com/BitBoxSwiss/bitbox02-api-go/api/bootloader/device.go` and
//! the device side in `bitbox02-firmware/src/bootloader/bootloader.c`.

use std::time::Duration;

use crate::{
    signed_firmware::{SighashScheme, SignedFirmware, CHUNK_SIZE},
    u2fhid::U2fHid,
    Error, Product, Version,
};

/// U2F HID command used by the bootloader ("endpoint").
const BOOTLOADER_CMD: u8 = 0x80 + 0x40 + 0x03;
// Erasing and writing flash may take a few seconds; the python library waits "forever".
const TIMEOUT: Duration = Duration::from_secs(120);

const OP_ERASE: u8 = b'e';
const OP_REBOOT: u8 = b'r';
const OP_WRITE_FIRMWARE_CHUNK: u8 = b'w';
const OP_WRITE_SIG_DATA: u8 = b's';
const OP_VERSIONS: u8 = b'v';
const OP_HASHES: u8 = b'h';
const OP_SET_SHOW_FIRMWARE_HASH: u8 = b'H';

/// A connection to a BitBox02 in bootloader mode.
pub(crate) struct Bootloader {
    device: U2fHid,
    pub product: Product,
    /// From the HID serial number string.
    pub version: Version,
}

impl Bootloader {
    pub(crate) fn new(device: hidapi::HidDevice, product: Product, version: Version) -> Self {
        Bootloader {
            device: U2fHid::new(device, BOOTLOADER_CMD),
            product,
            version,
        }
    }

    pub(crate) fn sighash_scheme(&self) -> SighashScheme {
        SighashScheme::for_bootloader(self.version)
    }

    /// Send `op | data`, check the response `op | status | data` and return its data.
    fn query(&self, op: u8, data: &[u8]) -> Result<Vec<u8>, Error> {
        let response = self.device.query(&[&[op], data].concat(), TIMEOUT)?;
        parse_response(op, &response).map(|r| r.to_vec())
    }

    /// Returns `(monotonic firmware version, signing pubkeys version)`.
    pub(crate) fn versions(&self) -> Result<(u32, u32), Error> {
        let r = self.query(OP_VERSIONS, &[])?;
        if r.len() < 8 {
            return Err(unexpected(format!(
                "versions response too short ({} bytes)",
                r.len()
            )));
        }
        let u32_at = |i: usize| u32::from_le_bytes(r[i..i + 4].try_into().unwrap());
        Ok((u32_at(0), u32_at(4)))
    }

    /// The firmware hash as computed by the bootloader (not displayed on the device).
    pub(crate) fn firmware_hash(&self) -> Result<[u8; 32], Error> {
        // Response: firmware hash | signing keydata hash.
        let r = self.query(OP_HASHES, &[0, 0])?;
        if r.len() < 64 {
            return Err(unexpected(format!(
                "hashes response too short ({} bytes)",
                r.len()
            )));
        }
        Ok(r[..32].try_into().unwrap())
    }

    /// Whether the bootloader shows the firmware hash when booting.
    pub(crate) fn show_firmware_hash_enabled(&self) -> Result<bool, Error> {
        let r = self.query(OP_SET_SHOW_FIRMWARE_HASH, &[0xff])?;
        r.first()
            .map(|b| *b == 1)
            .ok_or_else(|| unexpected("empty show hash response".to_string()))
    }

    pub(crate) fn set_show_firmware_hash(&self, enable: bool) -> Result<(), Error> {
        self.query(OP_SET_SHOW_FIRMWARE_HASH, &[enable as u8])?;
        Ok(())
    }

    /// Flash a signed firmware: erase, write the chunks (padded with 0xFF), then write the
    /// signature data, which makes the bootloader verify the signatures and the downgrade
    /// protection. `progress(done, total)` is called after the erase and after every chunk.
    pub(crate) fn flash(
        &self,
        firmware: &SignedFirmware,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<(), Error> {
        if firmware.product() != self.product {
            return Err(Error::WrongProduct {
                device: self.product,
                firmware: firmware.product(),
            });
        }
        let num_chunks = firmware.binary.len().div_ceil(CHUNK_SIZE);
        self.query(OP_ERASE, &[num_chunks as u8])?;
        progress(0, num_chunks);
        for (i, chunk) in firmware.binary.chunks(CHUNK_SIZE).enumerate() {
            let mut msg = vec![i as u8];
            msg.extend_from_slice(chunk);
            msg.resize(1 + CHUNK_SIZE, 0xff);
            self.query(OP_WRITE_FIRMWARE_CHUNK, &msg)?;
            progress(i + 1, num_chunks);
        }
        self.query(OP_WRITE_SIG_DATA, &firmware.sigdata)?;
        Ok(())
    }

    /// Reboot the device. The bootloader does not reply, and the device disconnects.
    pub(crate) fn reboot(self) -> Result<(), Error> {
        self.device.write(&[OP_REBOOT])
    }
}

fn unexpected(s: String) -> Error {
    Error::Other(format!("unexpected bootloader response: {}", s))
}

fn parse_response(op: u8, response: &[u8]) -> Result<&[u8], Error> {
    match response {
        [r_op, 0, rest @ ..] if *r_op == op => Ok(rest),
        [r_op, code, ..] if *r_op == op => {
            // `OP_STATUS_*` in `bootloader.c`.
            let reason = match code {
                b'Z' => "error (e.g. invalid signature)",
                b'V' => "downgrade rejected",
                b'N' => "invalid length",
                b'M' => "invalid macro",
                b'W' => "flash write error",
                b'C' => "flash check error",
                b'A' => "aborted",
                b'E' => "flash erase error",
                b'L' => "not ready to load (erase first)",
                b'I' => "invalid command",
                b'U' => "flash unlock error",
                b'K' => "flash lock error",
                _ => "unknown error",
            };
            Err(Error::Other(format!(
                "bootloader command '{}' failed: {} (status {:#04x})",
                op as char, reason, code
            )))
        }
        _ => Err(unexpected(format!(
            "expected reply to '{}', got {}",
            op as char,
            hex::encode(response)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses() {
        assert_eq!(parse_response(b'v', &[b'v', 0, 1, 2]).unwrap(), &[1, 2]);
        let e = parse_response(b'v', b"vV").unwrap_err().to_string();
        assert!(e.contains("downgrade rejected"), "{}", e);
        assert!(parse_response(b'v', &[b'h', 0]).is_err());
        assert!(parse_response(b'v', b"v").is_err());
        assert!(parse_response(b'v', &[]).is_err());
    }
}
