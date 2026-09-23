//! The BitBox02 bootloader API.
//!
//! References: `bitbox02-firmware/py/bitbox02/bitbox02/bitbox02/bootloader.py`,
//! `bitbox-wallet-app/vendor/github.com/BitBoxSwiss/bitbox02-api-go/api/bootloader/device.go` and
//! the device side in `bitbox02-firmware/src/bootloader/bootloader.c`.

use std::{fmt, time::Duration};

use crate::{
    product::{Product, Version},
    signed_firmware::{
        empty_firmware_hash, SighashScheme, SignedFirmware, CHUNK_SIZE, FIRMWARE_CHUNKS,
        SIGDATA_LEN,
    },
    u2fhid::{HidError, U2fHidDevice},
};

/// U2F HID command used by the bootloader ("endpoint").
pub const BOOTLOADER_CMD: u8 = 0x80 + 0x40 + 0x03;

// Erasing and writing flash may take a few seconds; the python library waits "forever".
const TIMEOUT: Duration = Duration::from_secs(120);

const OP_ERASE: u8 = b'e';
const OP_REBOOT: u8 = b'r';
const OP_WRITE_FIRMWARE_CHUNK: u8 = b'w';
const OP_WRITE_SIG_DATA: u8 = b's';
const OP_VERSIONS: u8 = b'v';
const OP_HASHES: u8 = b'h';
const OP_SET_SHOW_FIRMWARE_HASH: u8 = b'H';
const OP_HARDWARE: u8 = b'W';

#[derive(Debug)]
pub enum BootloaderError {
    Hid(HidError),
    /// The bootloader returned a non-zero status code, see `OP_STATUS_*` in `bootloader.c`.
    Status {
        op: u8,
        code: u8,
    },
    UnexpectedResponse(String),
    InvalidInput(String),
}

impl fmt::Display for BootloaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hid(e) => write!(f, "{}", e),
            Self::Status { op, code } => {
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
                write!(
                    f,
                    "bootloader command '{}' failed: {} (status {:#04x})",
                    *op as char, reason, code
                )
            }
            Self::UnexpectedResponse(s) => write!(f, "unexpected bootloader response: {}", s),
            Self::InvalidInput(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for BootloaderError {}

impl From<HidError> for BootloaderError {
    fn from(e: HidError) -> Self {
        BootloaderError::Hid(e)
    }
}

/// Secure chip model, as reported by bootloaders >= 1.1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureChipModel {
    Atecc,
    Optiga,
}

/// A connection to a BitBox02 in bootloader mode.
pub struct Bootloader {
    device: U2fHidDevice,
    product: Product,
    version: Version,
}

impl Bootloader {
    /// `product` and `version` come from the HID product string and serial number.
    pub fn new(device: hidapi::HidDevice, product: Product, version: Version) -> Self {
        Bootloader {
            device: U2fHidDevice::new(device, BOOTLOADER_CMD),
            product,
            version,
        }
    }

    pub fn product(&self) -> Product {
        self.product
    }

    /// The bootloader version (from the HID serial number string).
    pub fn version(&self) -> Version {
        self.version
    }

    /// The scheme used by this bootloader to compute the firmware hash.
    pub fn sighash_scheme(&self) -> SighashScheme {
        SighashScheme::for_bootloader(self.version)
    }

    fn query(&self, op: u8, data: &[u8]) -> Result<Vec<u8>, BootloaderError> {
        let mut msg = Vec::with_capacity(1 + data.len());
        msg.push(op);
        msg.extend_from_slice(data);
        let response = self.device.query(&msg, TIMEOUT)?;
        parse_response(op, &response).map(|r| r.to_vec())
    }

    /// Returns `(monotonic firmware version, signing pubkeys version)`.
    pub fn versions(&self) -> Result<(u32, u32), BootloaderError> {
        let r = self.query(OP_VERSIONS, &[])?;
        parse_versions(&r)
    }

    /// Returns `(firmware hash, signing keydata hash)`. If the `display_*` flags are set, the
    /// hashes are also shown on the device screen.
    pub fn get_hashes(
        &self,
        display_firmware_hash: bool,
        display_signing_keydata_hash: bool,
    ) -> Result<([u8; 32], [u8; 32]), BootloaderError> {
        let r = self.query(
            OP_HASHES,
            &[
                display_firmware_hash as u8,
                display_signing_keydata_hash as u8,
            ],
        )?;
        if r.len() < 64 {
            return Err(BootloaderError::UnexpectedResponse(format!(
                "hashes response too short ({} bytes)",
                r.len()
            )));
        }
        Ok((
            r[..32].try_into().expect("32 bytes"),
            r[32..64].try_into().expect("32 bytes"),
        ))
    }

    /// Whether the bootloader automatically shows the firmware hash when booting.
    pub fn show_firmware_hash_enabled(&self) -> Result<bool, BootloaderError> {
        let r = self.query(OP_SET_SHOW_FIRMWARE_HASH, &[0xff])?;
        r.first().map(|b| *b == 1).ok_or_else(|| {
            BootloaderError::UnexpectedResponse("empty show hash response".to_string())
        })
    }

    /// Enable or disable showing the firmware hash on boot.
    pub fn set_show_firmware_hash(&self, enable: bool) -> Result<(), BootloaderError> {
        self.query(OP_SET_SHOW_FIRMWARE_HASH, &[enable as u8])?;
        Ok(())
    }

    /// Secure chip model. Bootloaders before 1.1.0 do not support the call and have an ATECC.
    pub fn hardware(&self) -> Result<SecureChipModel, BootloaderError> {
        if self.version < Version::new(1, 1, 0) {
            return Ok(SecureChipModel::Atecc);
        }
        let r = self.query(OP_HARDWARE, &[])?;
        match r.first() {
            Some(0) => Ok(SecureChipModel::Atecc),
            Some(1) => Ok(SecureChipModel::Optiga),
            other => Err(BootloaderError::UnexpectedResponse(format!(
                "unknown secure chip model {:?}",
                other
            ))),
        }
    }

    /// Whether the device contains no firmware.
    pub fn erased(&self) -> Result<bool, BootloaderError> {
        let (firmware_version, _) = self.versions()?;
        let (firmware_hash, _) = self.get_hashes(false, false)?;
        Ok(firmware_hash == empty_firmware_hash(self.version, self.product, firmware_version))
    }

    /// Erase the firmware app area, preparing to write `num_chunks` chunks. `0` erases the
    /// firmware entirely.
    fn erase_chunks(&self, num_chunks: u8) -> Result<(), BootloaderError> {
        self.query(OP_ERASE, &[num_chunks])?;
        Ok(())
    }

    /// Erase the firmware.
    pub fn erase(&self) -> Result<(), BootloaderError> {
        self.erase_chunks(0)
    }

    fn write_chunk(&self, chunk_num: u8, chunk: &[u8]) -> Result<(), BootloaderError> {
        let msg = chunk_message(chunk_num, chunk)?;
        self.query(OP_WRITE_FIRMWARE_CHUNK, &msg)?;
        Ok(())
    }

    fn write_sigdata(&self, sigdata: &[u8]) -> Result<(), BootloaderError> {
        if sigdata.len() != SIGDATA_LEN {
            return Err(BootloaderError::InvalidInput(format!(
                "signature data must be {} bytes",
                SIGDATA_LEN
            )));
        }
        self.query(OP_WRITE_SIG_DATA, sigdata)?;
        Ok(())
    }

    /// Flash a signed firmware: erase, write the chunks, then write the signature data (which
    /// makes the bootloader verify the signatures and the downgrade protection).
    ///
    /// `progress(done, total)` is called after the erase and after every chunk written.
    ///
    /// This does not check the firmware version: use [`crate::check_flashable`] beforehand.
    pub fn flash_signed_firmware(
        &self,
        firmware: &SignedFirmware,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<(), BootloaderError> {
        if firmware.product() != self.product {
            return Err(BootloaderError::InvalidInput(format!(
                "firmware is for {} but the device is a {}",
                firmware.product(),
                self.product
            )));
        }
        let binary = firmware.firmware();
        let num_chunks = firmware.num_chunks();
        if num_chunks > FIRMWARE_CHUNKS {
            return Err(BootloaderError::InvalidInput(
                "firmware too big".to_string(),
            ));
        }
        self.erase_chunks(num_chunks as u8)?;
        progress(0, num_chunks);
        for (i, chunk) in binary.chunks(CHUNK_SIZE).enumerate() {
            self.write_chunk(i as u8, chunk)?;
            progress(i + 1, num_chunks);
        }
        self.write_sigdata(firmware.sigdata())
    }

    /// Reboot the device. The bootloader does not reply, and the device disconnects.
    pub fn reboot(self) -> Result<(), BootloaderError> {
        self.device.write(&[OP_REBOOT])?;
        Ok(())
    }
}

/// Check a bootloader response `op | status | data` and return the data.
fn parse_response(op: u8, response: &[u8]) -> Result<&[u8], BootloaderError> {
    match response {
        [r_op, 0, rest @ ..] if *r_op == op => Ok(rest),
        [r_op, code, ..] if *r_op == op => Err(BootloaderError::Status { op, code: *code }),
        _ => Err(BootloaderError::UnexpectedResponse(format!(
            "expected reply to '{}', got {}",
            op as char,
            hex::encode(response)
        ))),
    }
}

fn parse_versions(r: &[u8]) -> Result<(u32, u32), BootloaderError> {
    if r.len() < 8 {
        return Err(BootloaderError::UnexpectedResponse(format!(
            "versions response too short ({} bytes)",
            r.len()
        )));
    }
    Ok((
        u32::from_le_bytes(r[..4].try_into().expect("4 bytes")),
        u32::from_le_bytes(r[4..8].try_into().expect("4 bytes")),
    ))
}

/// `chunk num | chunk padded with 0xFF to CHUNK_SIZE`.
fn chunk_message(chunk_num: u8, chunk: &[u8]) -> Result<Vec<u8>, BootloaderError> {
    if chunk.len() > CHUNK_SIZE {
        return Err(BootloaderError::InvalidInput(
            "chunk must be at most 4kB".to_string(),
        ));
    }
    let mut msg = Vec::with_capacity(1 + CHUNK_SIZE);
    msg.push(chunk_num);
    msg.extend_from_slice(chunk);
    msg.resize(1 + CHUNK_SIZE, 0xff);
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses() {
        assert_eq!(parse_response(b'v', &[b'v', 0, 1, 2]).unwrap(), &[1, 2]);
        assert!(matches!(
            parse_response(b'v', b"vV"),
            Err(BootloaderError::Status {
                op: b'v',
                code: b'V'
            })
        ));
        assert!(matches!(
            parse_response(b'v', &[b'h', 0]),
            Err(BootloaderError::UnexpectedResponse(_))
        ));
        assert!(matches!(
            parse_response(b'v', b"v"),
            Err(BootloaderError::UnexpectedResponse(_))
        ));
        assert!(parse_response(b'v', &[]).is_err());
    }

    #[test]
    fn versions() {
        assert_eq!(
            parse_versions(&[0x37, 0, 0, 0, 4, 0, 0, 0]).unwrap(),
            (55, 4)
        );
        assert!(parse_versions(&[0; 7]).is_err());
    }

    #[test]
    fn chunks() {
        let msg = chunk_message(3, &[1, 2, 3]).unwrap();
        assert_eq!(msg.len(), 1 + CHUNK_SIZE);
        assert_eq!(&msg[..4], &[3, 1, 2, 3]);
        assert!(msg[4..].iter().all(|b| *b == 0xff));
        assert!(chunk_message(0, &[0; CHUNK_SIZE]).is_ok());
        assert!(chunk_message(0, &[0; CHUNK_SIZE + 1]).is_err());
    }
}
