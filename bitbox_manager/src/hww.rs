//! Communication with a BitBox02 running its firmware ("HWW" API).
//!
//! We need an encrypted (noise) channel to request a reboot into the bootloader. The `bitbox-api`
//! crate implements the channel but does not expose a reboot call, nor its transport or cipher
//! states, so the (small) protocol is reimplemented here, following:
//! - `bitbox-api-rs/src/communication.rs` (HWW framing and the unencrypted `i` info call),
//! - `bitbox-api-rs/src/lib.rs` (unlock, noise XX handshake, pairing code, encrypted queries),
//! - `bitbox-api-rs/src/noise.rs` (persisted pairing),
//! - `bitbox02-firmware/src/rust/bitbox02-rust/src/hww.rs` and `hww/noise.rs` (device side),
//! - `bitbox02-firmware/messages/{hww,system}.proto` (protobuf messages),
//! - `reboot()` in `bitbox-wallet-app/vendor/github.com/BitBoxSwiss/bitbox02-api-go/api/firmware/system.go`.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

use noise_protocol::{CipherState, HandshakeState, U8Array, DH};
use noise_rust_crypto::{sensitive::Sensitive, ChaCha20Poly1305, Sha256, X25519};

use crate::{u2fhid::U2fHid, Error, Product, Version};

/// U2F HID command used by the firmware ("endpoint").
const FIRMWARE_CMD: u8 = 0x80 + 0x40 + 0x01;
// Replies are immediate: long running operations are polled (HWW_RSP_NOTREADY).
const TIMEOUT: Duration = Duration::from_secs(30);

// HWW framing, since firmware v7.0.0.
const HWW_REQ_NEW: u8 = 0x00;
const HWW_REQ_RETRY: u8 = 0x01;
const HWW_RSP_ACK: u8 = 0x00;
const HWW_RSP_NOTREADY: u8 = 0x01;
const HWW_RSP_BUSY: u8 = 0x02;
const HWW_RSP_NACK: u8 = 0x03;

/// Sent without the HWW framing, to get the version first.
const OP_INFO: u8 = b'i';
const OP_UNLOCK: u8 = b'u';
const OP_I_CAN_HAS_HANDSHAEK: u8 = b'h';
const OP_HER_COMEZ_TEH_HANDSHAEK: u8 = b'H';
const OP_I_CAN_HAS_PAIRIN_VERIFICASHUN: u8 = b'v';
const OP_NOISE_MSG: u8 = b'n';
const RESPONSE_SUCCESS: u8 = 0x00;

const NOISE_PROLOGUE: &[u8] = b"Noise_XX_25519_ChaChaPoly_SHA256";
type Handshake = HandshakeState<X25519, ChaCha20Poly1305, Sha256>;

/// `Request { reboot: RebootRequest { purpose: UPGRADE } }`: field 18 (key `18 << 3 | 2`, varint
/// encoded as `0x92 0x01`) with an empty message, as UPGRADE is 0, the proto3 default.
const REBOOT_UPGRADE_REQUEST: &[u8] = &[0x92, 0x01, 0x00];

fn noise_error() -> Error {
    Error::Other("encrypted channel (noise) error".to_string())
}

fn unexpected(s: impl std::fmt::Display) -> Error {
    Error::Other(format!("unexpected response from device: {}", s))
}

/// Information returned by the unencrypted `i` call. Available without pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareInfo {
    pub version: Version,
    /// `None` if the platform/edition bytes are unknown.
    pub product: Option<Product>,
    pub unlocked: bool,
    /// `None` before firmware v9.20.0.
    pub initialized: Option<bool>,
}

/// Parse the response to the `i` call. See `get_info()` in `bitbox-api-rs/src/communication.rs`:
/// `version len | version | platform | edition | unlocked | initialized (since v9.20.0)`.
fn parse_info(response: &[u8]) -> Result<FirmwareInfo, Error> {
    let err = || unexpected(format!("info: {}", hex::encode(response)));
    let (&vlen, rest) = response.split_first().ok_or_else(err)?;
    let vstr = rest.get(..vlen as usize).ok_or_else(err)?;
    let version = std::str::from_utf8(vstr)
        .ok()
        .and_then(|s| s.strip_prefix('v'))
        .and_then(Version::parse)
        .ok_or_else(err)?;
    let bool_at = |i: usize| match rest.get(vlen as usize + i) {
        None => Ok(None),
        Some(0) => Ok(Some(false)),
        Some(1) => Ok(Some(true)),
        _ => Err(err()),
    };
    let product = match rest.get(vlen as usize..vlen as usize + 2).ok_or_else(err)? {
        [0x00, 0x00] => Some(Product::BitBox02Multi),
        [0x00, 0x01] => Some(Product::BitBox02BtcOnly),
        [0x02, 0x00] => Some(Product::BitBox02NovaMulti),
        [0x02, 0x01] => Some(Product::BitBox02NovaBtcOnly),
        _ => None,
    };
    Ok(FirmwareInfo {
        version,
        product,
        unlocked: bool_at(2)?.ok_or_else(err)?,
        initialized: bool_at(3)?,
    })
}

/// A BitBox02 in firmware mode, before pairing.
pub(crate) struct FirmwareDevice {
    comm: U2fHid,
    pub info: FirmwareInfo,
}

impl FirmwareDevice {
    /// Open the connection and query the unencrypted device info.
    pub(crate) fn open(device: hidapi::HidDevice) -> Result<Self, Error> {
        let comm = U2fHid::new(device, FIRMWARE_CMD);
        let info = parse_info(&comm.query(&[OP_INFO], TIMEOUT)?)?;
        if info.version < Version::new(7, 0, 0) {
            return Err(Error::Other(format!(
                "firmware v{} is too old to be supported (7.0.0 or later required)",
                info.version
            )));
        }
        Ok(FirmwareDevice { comm, info })
    }

    /// Unlock the device (the user enters their password on the device if it is initialized and
    /// locked; returns immediately otherwise) and establish the encrypted channel.
    ///
    /// If a pairing confirmation is needed, `on_pairing_code` is called with the code, which the
    /// user must compare with the one on the device screen and confirm on the device. The pairing
    /// is remembered in `config_dir`, if any.
    pub(crate) fn unlock_and_pair(
        self,
        config_dir: Option<&Path>,
        on_pairing_code: &mut dyn FnMut(&str),
    ) -> Result<PairedDevice, Error> {
        // The status is ignored like bitbox-api does: an uninitialized device returns
        // OP_STATUS_FAILURE_UNINITIALIZED but can still be paired with.
        hww_query(&self.comm, &[OP_UNLOCK])?;

        let mut config = read_config(config_dir)?;
        let host_key = match config.app_static_privkey {
            Some(k) => <Sensitive<[u8; 32]> as U8Array>::from_slice(&k),
            None => {
                let k = X25519::genkey();
                config.app_static_privkey = Some(<[u8; 32]>::try_from(&k[..]).unwrap());
                store_config(config_dir, &config)?;
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
            return Err(noise_error());
        }
        let handshake_query = |msg: Vec<u8>| -> Result<Vec<u8>, Error> {
            match hww_query(
                &self.comm,
                &[&[OP_HER_COMEZ_TEH_HANDSHAEK], &msg[..]].concat(),
            )? {
                r if r.first() == Some(&RESPONSE_SUCCESS) => Ok(r[1..].to_vec()),
                _ => Err(noise_error()),
            }
        };
        let host_handshake_1 = host.write_message_vec(b"").map_err(|_| noise_error())?;
        let bb02_handshake_1 = handshake_query(host_handshake_1)?;
        host.read_message_vec(&bb02_handshake_1)
            .map_err(|_| noise_error())?;
        let host_handshake_2 = host.write_message_vec(b"").map_err(|_| noise_error())?;
        let bb02_handshake_2 = handshake_query(host_handshake_2)?;

        let remote_static = host.get_rs().ok_or_else(noise_error)?;
        let required_by_app = !config
            .device_static_pubkeys
            .iter()
            .any(|k| k[..] == remote_static[..]);
        let required_by_device = bb02_handshake_2 == [0x01];
        if required_by_app || required_by_device {
            let hash: [u8; 32] = host.get_hash().try_into().map_err(|_| noise_error())?;
            on_pairing_code(&format_pairing_code(&hash));
            let r = hww_query(&self.comm, &[OP_I_CAN_HAS_PAIRIN_VERIFICASHUN])?;
            if r != [RESPONSE_SUCCESS] {
                return Err(Error::Other("pairing rejected on the device".to_string()));
            }
            let mut config = read_config(config_dir)?;
            if !config
                .device_static_pubkeys
                .iter()
                .any(|k| k[..] == remote_static[..])
            {
                config.device_static_pubkeys.push(remote_static.to_vec());
            }
            store_config(config_dir, &config)?;
        }
        let (send, recv) = host.get_ciphers();
        Ok(PairedDevice {
            comm: self.comm,
            send,
            recv,
        })
    }
}

/// A BitBox02 in firmware mode with an established encrypted channel.
pub(crate) struct PairedDevice {
    comm: U2fHid,
    send: CipherState<ChaCha20Poly1305>,
    recv: CipherState<ChaCha20Poly1305>,
}

impl PairedDevice {
    /// Send an encrypted protobuf `Request`, return an `Error::Device` if the device replies with
    /// an error `Response`.
    fn encrypted_query(&mut self, request: &[u8]) -> Result<(), Error> {
        let msg = [&[OP_NOISE_MSG], &self.send.encrypt_vec(request)[..]].concat();
        let response = hww_query(&self.comm, &msg)?;
        match response.split_first() {
            Some((&RESPONSE_SUCCESS, encrypted)) => {
                let decrypted = self
                    .recv
                    .decrypt_vec(encrypted)
                    .map_err(|_| noise_error())?;
                check_response_error(&decrypted)
            }
            _ => Err(unexpected("encrypted query failed")),
        }
    }

    /// Ask the device to reboot into the bootloader to upgrade the firmware. The user must
    /// confirm on the device ("Proceed to upgrade?"). This blocks until the user confirms (the
    /// device then disconnects) or rejects (`Error::Device`).
    pub(crate) fn reboot_to_bootloader(mut self) -> Result<(), Error> {
        match self.encrypted_query(REBOOT_UPGRADE_REQUEST) {
            // Like the Go library, we only return errors from the device. Otherwise we assume it's
            // an IO error due to the device rebooting.
            Err(e @ Error::Device(_)) => Err(e),
            Err(e) => {
                log::debug!(
                    "Error after reboot request, assuming the device rebooted: {}",
                    e
                );
                Ok(())
            }
            Ok(()) => Ok(()),
        }
    }
}

/// A query with the HWW framing: busy devices are retried, pending requests polled.
fn hww_query(comm: &U2fHid, msg: &[u8]) -> Result<Vec<u8>, Error> {
    let framed = [&[HWW_REQ_NEW], msg].concat();
    let mut response = loop {
        let r = comm.query(&framed, TIMEOUT)?;
        if r.first() != Some(&HWW_RSP_BUSY) {
            break r;
        }
        thread::sleep(Duration::from_millis(1000));
    };
    loop {
        match response.first() {
            Some(&HWW_RSP_ACK) => return Ok(response.split_off(1)),
            Some(&HWW_RSP_NOTREADY) => {
                thread::sleep(Duration::from_millis(200));
                response = comm.query(&[HWW_REQ_RETRY], TIMEOUT)?;
            }
            Some(&HWW_RSP_BUSY) => return Err(unexpected("device busy")),
            Some(&HWW_RSP_NACK) => return Err(unexpected("request rejected (NACK)")),
            _ => {
                return Err(unexpected(format!(
                    "unknown HWW response {}",
                    hex::encode(&response)
                )))
            }
        }
    }
}

/// Format the pairing code from the handshake hash, as shown on the device. See `pair()` in
/// `bitbox-api-rs/src/lib.rs` and `format_hash()` in the firmware `workflow/pairing.rs`.
fn format_pairing_code(hash: &[u8; 32]) -> String {
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
        let mut buf = [0u8; 8];
        buf[3..3 + chunk.len()].copy_from_slice(chunk);
        let n = u64::from_be_bytes(buf);
        let chars = (chunk.len() * 8).div_ceil(5);
        for i in 0..8 {
            if i < chars {
                out.push(ALPHABET[((n >> (35 - i * 5)) & 0x1f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// If the protobuf `Response` is an `error` (field 2, with `code` field 1 and `message` field 2,
/// see `hww.proto`), return it as an `Error::Device`.
fn check_response_error(response: &[u8]) -> Result<(), Error> {
    for (field, value) in proto_fields(response)? {
        if let (2, ProtoValue::Bytes(error)) = (field, value) {
            let mut code = 0;
            let mut message = String::new();
            for (f, v) in proto_fields(error)? {
                match (f, v) {
                    (1, ProtoValue::Varint(c)) => code = c as i32,
                    (2, ProtoValue::Bytes(m)) => message = String::from_utf8_lossy(m).into_owned(),
                    _ => {}
                }
            }
            return Err(Error::Device(if code == 104 {
                "aborted by the user on the device".to_string()
            } else {
                format!("device returned error {}: {}", code, message)
            }));
        }
    }
    Ok(())
}

enum ProtoValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// Parse the top-level fields of a protobuf message.
fn proto_fields(mut data: &[u8]) -> Result<Vec<(u64, ProtoValue<'_>)>, Error> {
    let err = || unexpected("invalid protobuf");
    let read_varint = |data: &mut &[u8]| -> Result<u64, Error> {
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
    };
    let mut out = Vec::new();
    while !data.is_empty() {
        let key = read_varint(&mut data)?;
        let len = match key & 7 {
            0 => {
                out.push((key >> 3, ProtoValue::Varint(read_varint(&mut data)?)));
                continue;
            }
            1 => 8,
            5 => 4,
            2 => read_varint(&mut data)? as usize,
            _ => return Err(err()),
        };
        if data.len() < len {
            return Err(err());
        }
        let (v, rest) = data.split_at(len);
        data = rest;
        let value = if key & 7 == 2 {
            ProtoValue::Bytes(v)
        } else {
            ProtoValue::Fixed
        };
        out.push((key >> 3, value));
    }
    Ok(out)
}

/// The persisted pairing, in the same format as `PersistedNoiseConfig` of `bitbox-api`
/// (`bitbox-api-rs/src/noise.rs`) so a pairing file can be shared with apps using it.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct NoiseConfigData {
    app_static_privkey: Option<[u8; 32]>,
    device_static_pubkeys: Vec<Vec<u8>>,
}

fn config_error(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("pairing config error: {}", e))
}

/// Read `<dir>/bitbox.json`. Without a directory, nothing is remembered.
fn read_config(dir: Option<&Path>) -> Result<NoiseConfigData, Error> {
    let path = match dir {
        Some(dir) => dir.join("bitbox.json"),
        None => return Ok(NoiseConfigData::default()),
    };
    if !path.exists() {
        return Ok(NoiseConfigData::default());
    }
    let contents = fs::read_to_string(&path).map_err(config_error)?;
    serde_json::from_str(&contents).map_err(config_error)
}

/// Write `<dir>/bitbox.json`, creating the directory if needed. On Unix, the directory (if
/// created) and the file are only accessible by the user.
fn store_config(dir: Option<&Path>, conf: &NoiseConfigData) -> Result<(), Error> {
    let Some(dir) = dir else { return Ok(()) };
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir).map_err(config_error)?;

    let data = serde_json::to_string(conf).map_err(config_error)?;
    let path = dir.join("bitbox.json");
    let mut options = fs::File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).map_err(config_error)?;
    // The mode above only applies to a new file.
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(config_error)?;
    file.write_all(data.as_bytes()).map_err(config_error)
}

/// The default directory for the pairing file: `<user config dir>/bacca`.
/// (`$XDG_CONFIG_HOME` or `~/.config` on Linux, `~/Library/Application Support` on macOS,
/// `%APPDATA%` on Windows.)
pub fn default_config_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("bacca"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info() {
        // "v9.27.1", BitBox02 Nova, btc-only, unlocked, initialized.
        let r = [&[7], &b"v9.27.1"[..], &[0x02, 0x01, 0x01, 0x01]].concat();
        assert_eq!(
            parse_info(&r).unwrap(),
            FirmwareInfo {
                version: Version::new(9, 27, 1),
                product: Some(Product::BitBox02NovaBtcOnly),
                unlocked: true,
                initialized: Some(true),
            }
        );
        let info = |rest: &[u8]| parse_info(&[&[6], &b"v9.1.0"[..], rest].concat());
        // Before 9.20.0 there is no initialized byte.
        let i = info(&[0x00, 0x00, 0x00]).unwrap();
        assert_eq!(i.product, Some(Product::BitBox02Multi));
        assert_eq!(i.initialized, None);
        assert!(!i.unlocked);
        // Unknown platform.
        assert_eq!(info(&[0x05, 0x00, 0x00]).unwrap().product, None);
        // Malformed.
        assert!(info(&[0x00, 0x00, 0x03]).is_err());
        assert!(info(&[0x00, 0x00]).is_err());
        assert!(parse_info(&[]).is_err());
        assert!(parse_info(&[10, b'v']).is_err());
        assert!(parse_info(&[&[6], &b"9.1.0x"[..], &[0, 0, 0]].concat()).is_err());
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
        assert_eq!(format_pairing_code(&[0xff; 32]), "77777 77777\n77777 77777");
    }

    #[test]
    fn protobuf() {
        // Response { success: Success {} }
        assert!(check_response_error(&[0x0a, 0x00]).is_ok());
        // Response { error: Error { code: 104, message: "aborted" } }
        let err = [&[0x12, 11, 0x08, 104, 0x12, 7], &b"aborted"[..]].concat();
        let e = check_response_error(&err).unwrap_err();
        assert!(matches!(e, Error::Device(ref m) if m == "aborted by the user on the device"));
        // Response { error: Error { code: 101 (varint 0x65), message: "invalid input" } }, after a
        // fixed64 and a fixed32 fields that must be skipped.
        let mut err = vec![0x09, 1, 2, 3, 4, 5, 6, 7, 8, 0x0d, 1, 2, 3, 4];
        err.extend_from_slice(&[0x12, 17, 0x08, 0x65, 0x12, 13]);
        err.extend_from_slice(b"invalid input");
        let e = check_response_error(&err).unwrap_err();
        assert_eq!(e.to_string(), "device returned error 101: invalid input");
        assert!(check_response_error(&[0x22, 0x05, 0x00]).is_err());
    }

    #[test]
    fn noise_handshake_with_simulated_device() {
        // Check our use of the noise crates against a responder, as the device does (see
        // `bitbox02-firmware/src/rust/bitbox02-rust/src/hww/noise.rs`).
        let device_key = X25519::genkey();
        let device_pub = X25519::pubkey(&device_key);
        let new = |initiator, key| {
            let pattern = noise_protocol::patterns::noise_xx();
            Handshake::new(
                pattern,
                initiator,
                NOISE_PROLOGUE,
                Some(key),
                None,
                None,
                None,
            )
        };
        let mut host = new(true, X25519::genkey());
        let mut device = new(false, device_key);
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
        let c = hs.encrypt_vec(REBOOT_UPGRADE_REQUEST);
        assert_eq!(dr.decrypt_vec(&c).unwrap(), REBOOT_UPGRADE_REQUEST);
    }

    #[test]
    fn config_roundtrip_and_compat() {
        let dir = std::env::temp_dir().join(format!("bacca-noise-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sub = dir.join("sub");
        assert!(read_config(Some(&sub))
            .unwrap()
            .app_static_privkey
            .is_none());
        let data = NoiseConfigData {
            app_static_privkey: Some([7; 32]),
            device_static_pubkeys: vec![vec![1, 2, 3]],
        };
        store_config(Some(&sub), &data).unwrap();
        let read = read_config(Some(&sub)).unwrap();
        assert_eq!(read.app_static_privkey, Some([7; 32]));
        assert_eq!(read.device_static_pubkeys, vec![vec![1, 2, 3]]);
        let path = sub.join("bitbox.json");
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Same JSON layout as bitbox-api's PersistedNoiseConfig (serde of the same struct).
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json["device_static_pubkeys"],
            serde_json::json!([[1, 2, 3]])
        );
        assert_eq!(json["app_static_privkey"].as_array().unwrap().len(), 32);
        let _ = fs::remove_dir_all(&dir);
    }
}
