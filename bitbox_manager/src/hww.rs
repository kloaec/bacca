//! Communication with a BitBox02 running its firmware ("HWW" API).
//!
//! We need an encrypted (noise) channel to request a reboot into the bootloader. The `bitbox-api`
//! crate implements the channel but does not expose a reboot call, nor its transport or cipher
//! states, so the (small) protocol is reimplemented here, following:
//! - `bitbox-api-rs/src/communication.rs` (HWW framing and the unencrypted `i` info call),
//! - `bitbox-api-rs/src/lib.rs` (unlock, noise XX handshake, pairing code, encrypted queries),
//! - `bitbox02-firmware/src/rust/bitbox02-rust/src/hww.rs` and `hww/noise.rs` (device side),
//! - `bitbox02-firmware/messages/{hww,system,bitbox02_system}.proto` (protobuf messages),
//! - `reboot()` in `bitbox-wallet-app/vendor/github.com/BitBoxSwiss/bitbox02-api-go/api/firmware/system.go`.

use std::{fmt, thread, time::Duration};

use noise_protocol::{CipherState, HandshakeState, U8Array, DH};
use noise_rust_crypto::{sensitive::Sensitive, ChaCha20Poly1305, Sha256, X25519};

use crate::{
    noise_config::{ConfigError, NoiseConfig},
    product::{Product, Version},
    u2fhid::{HidError, U2fHidDevice},
};

/// U2F HID command used by the firmware ("endpoint").
pub const FIRMWARE_CMD: u8 = 0x80 + 0x40 + 0x01;

// Replies are immediate: long running operations are polled (HWW_RSP_NOTREADY).
const TIMEOUT: Duration = Duration::from_secs(30);

// HWW framing, since firmware v7.0.0.
const HWW_REQ_NEW: u8 = 0x00;
const HWW_REQ_RETRY: u8 = 0x01;
const HWW_INFO: u8 = b'i';
const HWW_RSP_ACK: u8 = 0x00;
const HWW_RSP_NOTREADY: u8 = 0x01;
const HWW_RSP_BUSY: u8 = 0x02;
const HWW_RSP_NACK: u8 = 0x03;

const OP_I_CAN_HAS_HANDSHAEK: u8 = b'h';
const OP_HER_COMEZ_TEH_HANDSHAEK: u8 = b'H';
const OP_I_CAN_HAS_PAIRIN_VERIFICASHUN: u8 = b'v';
const OP_NOISE_MSG: u8 = b'n';
const OP_UNLOCK: u8 = b'u';
const RESPONSE_SUCCESS: u8 = 0x00;

const NOISE_PROLOGUE: &[u8] = b"Noise_XX_25519_ChaChaPoly_SHA256";

type Handshake = HandshakeState<X25519, ChaCha20Poly1305, Sha256>;

#[derive(Debug)]
pub enum HwwError {
    Hid(HidError),
    /// Firmwares before 7.0.0 use an older protocol that is not supported.
    FirmwareTooOld(Version),
    UnexpectedResponse(String),
    Noise,
    PairingRejected,
    Config(ConfigError),
    /// The device returned an error (protobuf `Error`); 104 is "user aborted".
    Device {
        code: i32,
        message: String,
    },
}

impl HwwError {
    pub fn is_user_abort(&self) -> bool {
        matches!(self, HwwError::Device { code: 104, .. })
    }
}

impl fmt::Display for HwwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hid(e) => write!(f, "{}", e),
            Self::FirmwareTooOld(v) => write!(
                f,
                "firmware v{} is too old to be supported (7.0.0 or later required)",
                v
            ),
            Self::UnexpectedResponse(s) => write!(f, "unexpected response from device: {}", s),
            Self::Noise => write!(f, "encrypted channel (noise) error"),
            Self::PairingRejected => write!(f, "pairing rejected on the device"),
            Self::Config(e) => write!(f, "{}", e),
            Self::Device { code: 104, .. } => write!(f, "aborted by the user on the device"),
            Self::Device { code, message } => {
                write!(f, "device returned error {}: {}", code, message)
            }
        }
    }
}

impl std::error::Error for HwwError {}

impl From<HidError> for HwwError {
    fn from(e: HidError) -> Self {
        HwwError::Hid(e)
    }
}

impl From<ConfigError> for HwwError {
    fn from(e: ConfigError) -> Self {
        HwwError::Config(e)
    }
}

/// Information returned by the unencrypted `i` call. Available without pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwwInfo {
    pub version: Version,
    /// `None` if the platform/edition bytes are unknown.
    pub product: Option<Product>,
    pub unlocked: bool,
    /// `None` before firmware v9.20.0.
    pub initialized: Option<bool>,
}

/// Parse the response to the `i` call. See `get_info()` in `bitbox-api-rs/src/communication.rs`.
pub fn parse_info(response: &[u8]) -> Result<HwwInfo, HwwError> {
    let err = || HwwError::UnexpectedResponse(format!("info: {}", hex::encode(response)));
    let (&vlen, rest) = response.split_first().ok_or_else(err)?;
    let vbytes = rest.get(..vlen as usize).ok_or_else(err)?;
    let rest = &rest[vlen as usize..];
    let vstr = std::str::from_utf8(vbytes).map_err(|_| err())?;
    let version = vstr
        .strip_prefix('v')
        .and_then(Version::parse)
        .ok_or_else(err)?;
    const PLATFORM_BITBOX02: u8 = 0x00;
    const PLATFORM_BITBOX02_NOVA: u8 = 0x02;
    const EDITION_MULTI: u8 = 0x00;
    const EDITION_BTCONLY: u8 = 0x01;
    let platform = *rest.first().ok_or_else(err)?;
    let edition = *rest.get(1).ok_or_else(err)?;
    let unlocked = match rest.get(2) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(err()),
    };
    let initialized = match rest.get(3) {
        None => None,
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => return Err(err()),
    };
    let product = match (platform, edition) {
        (PLATFORM_BITBOX02, EDITION_MULTI) => Some(Product::BitBox02Multi),
        (PLATFORM_BITBOX02, EDITION_BTCONLY) => Some(Product::BitBox02BtcOnly),
        (PLATFORM_BITBOX02_NOVA, EDITION_MULTI) => Some(Product::BitBox02NovaMulti),
        (PLATFORM_BITBOX02_NOVA, EDITION_BTCONLY) => Some(Product::BitBox02NovaBtcOnly),
        _ => None,
    };
    Ok(HwwInfo {
        version,
        product,
        unlocked,
        initialized,
    })
}

/// Device information returned by the encrypted `DeviceInfo` call (`DeviceInfoResponse` in
/// `bitbox02-firmware/messages/bitbox02_system.proto`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub initialized: bool,
    pub version: String,
    pub mnemonic_passphrase_enabled: bool,
    pub monotonic_increments_remaining: u32,
    pub securechip_model: String,
    /// Not present on legacy bootloaders.
    pub bootloader_version: Option<String>,
}

/// A BitBox02 in firmware mode, before pairing.
pub struct FirmwareDevice {
    comm: U2fHidDevice,
    info: HwwInfo,
}

impl FirmwareDevice {
    /// Open the connection and query the unencrypted device info.
    pub fn new(device: hidapi::HidDevice) -> Result<Self, HwwError> {
        let comm = U2fHidDevice::new(device, FIRMWARE_CMD);
        let info = parse_info(&comm.query(&[HWW_INFO], TIMEOUT)?)?;
        if info.version < Version::new(7, 0, 0) {
            return Err(HwwError::FirmwareTooOld(info.version));
        }
        Ok(FirmwareDevice { comm, info })
    }

    pub fn info(&self) -> &HwwInfo {
        &self.info
    }

    /// Unlock the device (the user enters their password on the device if it is initialized and
    /// locked; returns immediately otherwise) and establish the encrypted channel.
    ///
    /// If a pairing confirmation is needed, `on_pairing_code` is called with the code, which the
    /// user must compare with the one on the device screen and confirm on the device.
    pub fn unlock_and_pair(
        self,
        noise_config: &dyn NoiseConfig,
        on_pairing_code: &mut dyn FnMut(&str),
    ) -> Result<PairedDevice, HwwError> {
        // The status is ignored like bitbox-api does: an uninitialized device returns
        // OP_STATUS_FAILURE_UNINITIALIZED but can still be paired with.
        hww_query(&self.comm, &[OP_UNLOCK])?;

        let mut config = noise_config.read_config()?;
        let host_key = match config.app_static_privkey {
            Some(k) => <Sensitive<[u8; 32]> as U8Array>::from_slice(&k),
            None => {
                let k = X25519::genkey();
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&k[..]);
                config.app_static_privkey = Some(arr);
                noise_config.store_config(&config)?;
                k
            }
        };
        let mut host = Handshake::new(
            noise_protocol::patterns::noise_xx(),
            true,
            NOISE_PROLOGUE,
            Some(host_key),
            None,
            None,
            None,
        );

        if hww_query(&self.comm, &[OP_I_CAN_HAS_HANDSHAEK])? != [RESPONSE_SUCCESS] {
            return Err(HwwError::Noise);
        }
        let host_handshake_1 = host.write_message_vec(b"").map_err(|_| HwwError::Noise)?;
        let bb02_handshake_1 = handshake_query(&self.comm, &host_handshake_1)?;
        host.read_message_vec(&bb02_handshake_1)
            .map_err(|_| HwwError::Noise)?;
        let host_handshake_2 = host.write_message_vec(b"").map_err(|_| HwwError::Noise)?;
        let bb02_handshake_2 = handshake_query(&self.comm, &host_handshake_2)?;

        let remote_static = host.get_rs().ok_or(HwwError::Noise)?;
        let required_by_app = !config.contains_device_static_pubkey(&remote_static);
        let required_by_device = bb02_handshake_2 == [0x01];
        if required_by_app || required_by_device {
            let hash: [u8; 32] = host.get_hash().try_into().map_err(|_| HwwError::Noise)?;
            on_pairing_code(&format_pairing_code(&hash));
            let r = hww_query(&self.comm, &[OP_I_CAN_HAS_PAIRIN_VERIFICASHUN])?;
            if r != [RESPONSE_SUCCESS] {
                return Err(HwwError::PairingRejected);
            }
            let mut config = noise_config.read_config()?;
            config.add_device_static_pubkey(&remote_static);
            noise_config.store_config(&config)?;
        }
        let (send, recv) = host.get_ciphers();
        Ok(PairedDevice {
            comm: self.comm,
            info: self.info,
            send,
            recv,
        })
    }
}

/// A BitBox02 in firmware mode with an established encrypted channel.
pub struct PairedDevice {
    comm: U2fHidDevice,
    info: HwwInfo,
    send: CipherState<ChaCha20Poly1305>,
    recv: CipherState<ChaCha20Poly1305>,
}

impl PairedDevice {
    pub fn info(&self) -> &HwwInfo {
        &self.info
    }

    fn encrypted_query(&mut self, request: &[u8]) -> Result<Vec<u8>, HwwError> {
        let mut msg = vec![OP_NOISE_MSG];
        msg.extend_from_slice(&self.send.encrypt_vec(request));
        let response = hww_query(&self.comm, &msg)?;
        match response.split_first() {
            Some((&RESPONSE_SUCCESS, encrypted)) => {
                let decrypted = self
                    .recv
                    .decrypt_vec(encrypted)
                    .map_err(|_| HwwError::Noise)?;
                check_response_error(&decrypted)?;
                Ok(decrypted)
            }
            _ => Err(HwwError::UnexpectedResponse(
                "encrypted query failed".to_string(),
            )),
        }
    }

    /// Query the device info over the encrypted channel.
    pub fn device_info(&mut self) -> Result<DeviceInfo, HwwError> {
        let response = self.encrypted_query(&proto::device_info_request())?;
        proto::parse_device_info_response(&response)
    }

    /// Ask the device to reboot into the bootloader to upgrade the firmware. The user must
    /// confirm on the device ("Proceed to upgrade?"). This blocks until the user confirms (the
    /// device then disconnects) or rejects (`HwwError::Device { code: 104 }`).
    pub fn reboot_to_bootloader(mut self) -> Result<(), HwwError> {
        match self.encrypted_query(&proto::reboot_upgrade_request()) {
            Ok(_) => Ok(()),
            // Like the Go library, we only return errors from the device. Otherwise we assume it's
            // an IO error due to the device rebooting.
            Err(e @ HwwError::Device { .. }) => Err(e),
            Err(e) => {
                log::debug!(
                    "Error after reboot request, assuming the device rebooted: {}",
                    e
                );
                Ok(())
            }
        }
    }
}

fn handshake_query(comm: &U2fHidDevice, msg: &[u8]) -> Result<Vec<u8>, HwwError> {
    let mut framed = vec![OP_HER_COMEZ_TEH_HANDSHAEK];
    framed.extend_from_slice(msg);
    let response = hww_query(comm, &framed)?;
    match response.split_first() {
        Some((&RESPONSE_SUCCESS, rest)) => Ok(rest.to_vec()),
        _ => Err(HwwError::Noise),
    }
}

/// A query with the HWW framing: busy devices are retried, pending requests polled.
fn hww_query(comm: &U2fHidDevice, msg: &[u8]) -> Result<Vec<u8>, HwwError> {
    let mut framed = vec![HWW_REQ_NEW];
    framed.extend_from_slice(msg);
    let mut response = loop {
        let r = comm.query(&framed, TIMEOUT)?;
        if r.first() == Some(&HWW_RSP_BUSY) {
            thread::sleep(Duration::from_millis(1000));
            continue;
        }
        break r;
    };
    loop {
        match response.first() {
            Some(&HWW_RSP_ACK) => return Ok(response.split_off(1)),
            Some(&HWW_RSP_NOTREADY) => {
                thread::sleep(Duration::from_millis(200));
                response = comm.query(&[HWW_REQ_RETRY], TIMEOUT)?;
            }
            Some(&HWW_RSP_BUSY) => {
                return Err(HwwError::UnexpectedResponse("device busy".to_string()))
            }
            Some(&HWW_RSP_NACK) => {
                return Err(HwwError::UnexpectedResponse(
                    "request rejected (NACK)".to_string(),
                ))
            }
            _ => {
                return Err(HwwError::UnexpectedResponse(format!(
                    "unknown HWW response {}",
                    hex::encode(&response)
                )))
            }
        }
    }
}

fn check_response_error(response: &[u8]) -> Result<(), HwwError> {
    if let Some((code, message)) = proto::parse_error_response(response)? {
        return Err(HwwError::Device { code, message });
    }
    Ok(())
}

/// Format the pairing code from the handshake hash, as shown on the device. See `pair()` in
/// `bitbox-api-rs/src/lib.rs` and `format_hash()` in the firmware `workflow/pairing.rs`.
pub fn format_pairing_code(hash: &[u8; 32]) -> String {
    let encoded = base32_encode(hash);
    format!(
        "{} {}\n{} {}",
        &encoded[0..5],
        &encoded[5..10],
        &encoded[10..15],
        &encoded[15..20]
    )
}

/// RFC 4648 base32 with padding.
fn base32_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let n = u64::from_be_bytes([0, 0, 0, buf[0], buf[1], buf[2], buf[3], buf[4]]);
        let chars = (chunk.len() * 8).div_ceil(5);
        for i in 0..8 {
            if i < chars {
                let idx = ((n >> (35 - i * 5)) & 0x1f) as usize;
                out.push(ALPHABET[idx] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Minimal protobuf encoding/decoding for the few messages we need.
mod proto {
    use super::{DeviceInfo, HwwError};

    // Field numbers from bitbox02-firmware/messages/hww.proto.
    const REQUEST_DEVICE_INFO: u64 = 4;
    const REQUEST_REBOOT: u64 = 18;
    const RESPONSE_ERROR: u64 = 2;
    const RESPONSE_DEVICE_INFO: u64 = 4;

    fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn encode_len_delimited(field: u64, data: &[u8], out: &mut Vec<u8>) {
        encode_varint((field << 3) | 2, out);
        encode_varint(data.len() as u64, out);
        out.extend_from_slice(data);
    }

    /// `Request { device_info: DeviceInfoRequest {} }`.
    pub fn device_info_request() -> Vec<u8> {
        let mut out = Vec::new();
        encode_len_delimited(REQUEST_DEVICE_INFO, &[], &mut out);
        out
    }

    /// `Request { reboot: RebootRequest { purpose: UPGRADE } }`. UPGRADE is 0, the default
    /// value, hence not serialized in proto3.
    pub fn reboot_upgrade_request() -> Vec<u8> {
        let mut out = Vec::new();
        encode_len_delimited(REQUEST_REBOOT, &[], &mut out);
        out
    }

    pub enum Value<'a> {
        Varint(u64),
        Bytes(&'a [u8]),
        Fixed,
    }

    fn err() -> HwwError {
        HwwError::UnexpectedResponse("invalid protobuf".to_string())
    }

    fn read_varint(data: &mut &[u8]) -> Result<u64, HwwError> {
        let mut v = 0u64;
        for i in 0..10 {
            let (&b, rest) = data.split_first().ok_or_else(err)?;
            *data = rest;
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(err())
    }

    /// Parse the top-level fields of a message.
    pub fn fields(mut data: &[u8]) -> Result<Vec<(u64, Value<'_>)>, HwwError> {
        let mut out = Vec::new();
        while !data.is_empty() {
            let key = read_varint(&mut data)?;
            let field = key >> 3;
            let value = match key & 7 {
                0 => Value::Varint(read_varint(&mut data)?),
                1 | 5 => {
                    let n = if key & 7 == 1 { 8 } else { 4 };
                    if data.len() < n {
                        return Err(err());
                    }
                    data = &data[n..];
                    Value::Fixed
                }
                2 => {
                    let len = read_varint(&mut data)? as usize;
                    if data.len() < len {
                        return Err(err());
                    }
                    let (v, rest) = data.split_at(len);
                    data = rest;
                    Value::Bytes(v)
                }
                _ => return Err(err()),
            };
            out.push((field, value));
        }
        Ok(out)
    }

    fn string(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    /// If the response is a `Response { error: Error { code, message } }`, return it.
    pub fn parse_error_response(data: &[u8]) -> Result<Option<(i32, String)>, HwwError> {
        for (field, value) in fields(data)? {
            if let (RESPONSE_ERROR, Value::Bytes(b)) = (field, value) {
                let mut code = 0;
                let mut message = String::new();
                for (f, v) in fields(b)? {
                    match (f, v) {
                        (1, Value::Varint(c)) => code = c as i32,
                        (2, Value::Bytes(m)) => message = string(m),
                        _ => {}
                    }
                }
                return Ok(Some((code, message)));
            }
        }
        Ok(None)
    }

    pub fn parse_device_info_response(data: &[u8]) -> Result<DeviceInfo, HwwError> {
        for (field, value) in fields(data)? {
            if let (RESPONSE_DEVICE_INFO, Value::Bytes(b)) = (field, value) {
                let mut info = DeviceInfo::default();
                for (f, v) in fields(b)? {
                    match (f, v) {
                        (1, Value::Bytes(s)) => info.name = string(s),
                        (2, Value::Varint(v)) => info.initialized = v != 0,
                        (3, Value::Bytes(s)) => info.version = string(s),
                        (4, Value::Varint(v)) => info.mnemonic_passphrase_enabled = v != 0,
                        (5, Value::Varint(v)) => info.monotonic_increments_remaining = v as u32,
                        (6, Value::Bytes(s)) => info.securechip_model = string(s),
                        (9, Value::Bytes(s)) => info.bootloader_version = Some(string(s)),
                        _ => {}
                    }
                }
                return Ok(info);
            }
        }
        Err(HwwError::UnexpectedResponse(
            "expected a device info response".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info() {
        // "v9.27.1", BitBox02 Nova, btc-only, unlocked, initialized.
        let mut r = vec![7];
        r.extend_from_slice(b"v9.27.1");
        r.extend_from_slice(&[0x02, 0x01, 0x01, 0x01]);
        assert_eq!(
            parse_info(&r).unwrap(),
            HwwInfo {
                version: Version::new(9, 27, 1),
                product: Some(Product::BitBox02NovaBtcOnly),
                unlocked: true,
                initialized: Some(true),
            }
        );
        // Before 9.20.0 there is no initialized byte.
        let mut r = vec![6];
        r.extend_from_slice(b"v9.1.0");
        r.extend_from_slice(&[0x00, 0x00, 0x00]);
        let info = parse_info(&r).unwrap();
        assert_eq!(info.product, Some(Product::BitBox02Multi));
        assert_eq!(info.initialized, None);
        assert!(!info.unlocked);
        // Unknown platform.
        let mut r = vec![6];
        r.extend_from_slice(b"v9.1.0");
        r.extend_from_slice(&[0x05, 0x00, 0x00]);
        assert_eq!(parse_info(&r).unwrap().product, None);
        // Malformed.
        assert!(parse_info(&[]).is_err());
        assert!(parse_info(&[10, b'v']).is_err());
        let mut r = vec![6];
        r.extend_from_slice(b"9.1.0x");
        r.extend_from_slice(&[0x00, 0x00, 0x00]);
        assert!(parse_info(&r).is_err());
        let mut r = vec![6];
        r.extend_from_slice(b"v9.1.0");
        r.extend_from_slice(&[0x00, 0x00, 0x03]);
        assert!(parse_info(&r).is_err());
    }

    #[test]
    fn base32() {
        // RFC 4648 test vectors.
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "MY======");
        assert_eq!(base32_encode(b"fo"), "MZXQ====");
        assert_eq!(base32_encode(b"foo"), "MZXW6===");
        assert_eq!(base32_encode(b"foob"), "MZXW6YQ=");
        assert_eq!(base32_encode(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI======");
        let code = format_pairing_code(&[0u8; 32]);
        assert_eq!(code, "AAAAA AAAAA\nAAAAA AAAAA");
        let code = format_pairing_code(&[0xffu8; 32]);
        assert_eq!(code, "77777 77777\n77777 77777");
    }

    #[test]
    fn protobuf() {
        assert_eq!(proto::reboot_upgrade_request(), vec![0x92, 0x01, 0x00]);
        assert_eq!(proto::device_info_request(), vec![0x22, 0x00]);

        // Response { success: Success {} }
        assert!(check_response_error(&[0x0a, 0x00]).is_ok());
        // Response { error: Error { code: 104, message: "aborted" } }
        let mut err = vec![0x12, 11, 0x08, 104, 0x12, 7];
        err.extend_from_slice(b"aborted");
        let e = check_response_error(&err).unwrap_err();
        assert!(e.is_user_abort());
        assert!(matches!(e, HwwError::Device { code: 104, ref message } if message == "aborted"));

        // Response { device_info: { name: "My BitBox", initialized: true, version: "v9.27.1",
        //   monotonic_increments_remaining: 300 (varint 0xac 0x02), securechip_model: "OPTIGA",
        //   bootloader_version: "v1.2.2" } }
        let mut inner = vec![0x0a, 9];
        inner.extend_from_slice(b"My BitBox");
        inner.extend_from_slice(&[0x10, 0x01, 0x1a, 7]);
        inner.extend_from_slice(b"v9.27.1");
        inner.extend_from_slice(&[0x28, 0xac, 0x02, 0x32, 6]);
        inner.extend_from_slice(b"OPTIGA");
        // An unknown optional message field 7 (bluetooth), must be skipped.
        inner.extend_from_slice(&[0x3a, 2, 0x18, 0x01]);
        inner.extend_from_slice(&[0x4a, 6]);
        inner.extend_from_slice(b"v1.2.2");
        let mut resp = vec![0x22, inner.len() as u8];
        resp.extend_from_slice(&inner);
        let info = proto::parse_device_info_response(&resp).unwrap();
        assert_eq!(
            info,
            DeviceInfo {
                name: "My BitBox".to_string(),
                initialized: true,
                version: "v9.27.1".to_string(),
                mnemonic_passphrase_enabled: false,
                monotonic_increments_remaining: 300,
                securechip_model: "OPTIGA".to_string(),
                bootloader_version: Some("v1.2.2".to_string()),
            }
        );
        assert!(proto::parse_device_info_response(&[0x0a, 0x00]).is_err());
        assert!(proto::fields(&[0x22, 0x05, 0x00]).is_err());
    }

    #[test]
    fn noise_handshake_with_simulated_device() {
        // Check our use of the noise crates against a responder, as the device does (see
        // `bitbox02-firmware/src/rust/bitbox02-rust/src/hww/noise.rs`).
        let host_key = X25519::genkey();
        let device_key = X25519::genkey();
        let device_pub = X25519::pubkey(&device_key);
        let mut host = Handshake::new(
            noise_protocol::patterns::noise_xx(),
            true,
            NOISE_PROLOGUE,
            Some(host_key),
            None,
            None,
            None,
        );
        let mut device = Handshake::new(
            noise_protocol::patterns::noise_xx(),
            false,
            NOISE_PROLOGUE,
            Some(device_key),
            None,
            None,
            None,
        );
        let m1 = host.write_message_vec(b"").unwrap();
        device.read_message_vec(&m1).unwrap();
        let m2 = device.write_message_vec(b"").unwrap();
        host.read_message_vec(&m2).unwrap();
        let m3 = host.write_message_vec(b"").unwrap();
        device.read_message_vec(&m3).unwrap();
        assert!(host.completed() && device.completed());
        assert_eq!(host.get_rs().unwrap(), device_pub);
        assert_eq!(host.get_hash(), device.get_hash());
        let (mut hs, _) = host.get_ciphers();
        let (mut dr, _) = device.get_ciphers();
        let c = hs.encrypt_vec(&proto::reboot_upgrade_request());
        assert_eq!(dr.decrypt_vec(&c).unwrap(), vec![0x92, 0x01, 0x00]);
    }
}
