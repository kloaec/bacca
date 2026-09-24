//! The signed firmware binary format and the firmware hash computed by the bootloader.
//!
//! A signed firmware file is `magic (4 bytes, BE) | sigdata (SIGDATA_LEN) | firmware`, where
//! `sigdata = signing_pubkeys_data | firmware_data` and
//! - `signing_pubkeys_data = signing pubkeys version (u32 LE) | 3 signing pubkeys | 3 root sigs`,
//! - `firmware_data = monotonic firmware version (u32 LE) | 3 firmware signatures`.
//!
//! References: `bitbox02-firmware/scripts/signed_firmware.py`,
//! `bitbox02-firmware/releases/describe_signed_firmware.py`,
//! `parse_signed_firmware()` in `bitbox02-firmware/py/bitbox02/bitbox02/bitbox02/bootloader.py`,
//! `ParseSignedFirmware()`/`HashFirmware()` in
//! `bitbox-wallet-app/vendor/github.com/BitBoxSwiss/bitbox02-api-go/api/bootloader/util.go`, and
//! `_firmware_hash()` in `bitbox02-firmware/src/bootloader/bootloader.c`.

use std::fmt;

use sha2::{Digest, Sha256};

use crate::product::{Product, Version};

pub const MAGIC_LEN: usize = 4;
pub const VERSION_LEN: usize = 4;
pub const NUM_ROOT_KEYS: usize = 3;
pub const NUM_SIGNING_KEYS: usize = 3;
pub const SIGNING_PUBKEYS_DATA_LEN: usize =
    VERSION_LEN + NUM_SIGNING_KEYS * 64 + NUM_ROOT_KEYS * 64;
pub const FIRMWARE_DATA_LEN: usize = VERSION_LEN + NUM_SIGNING_KEYS * 64;
pub const SIGDATA_LEN: usize = SIGNING_PUBKEYS_DATA_LEN + FIRMWARE_DATA_LEN;
/// 928kB - 64kB. Same for all BitBox02 products (`FLASH_APP_LEN` in the bootloader).
pub const MAX_FIRMWARE_SIZE: usize = 884736;
/// Size of a firmware chunk written by the bootloader `w` command.
pub const CHUNK_SIZE: usize = 4096;
/// Max number of chunks of a firmware.
pub const FIRMWARE_CHUNKS: usize = MAX_FIRMWARE_SIZE / CHUNK_SIZE;
const _: () = assert!(MAX_FIRMWARE_SIZE.is_multiple_of(CHUNK_SIZE));
const _: () = assert!(FIRMWARE_CHUNKS <= u8::MAX as usize);

/// Firmwares with a monotonic version >= this are hashed with the new sighash scheme
/// (`NEW_SIGHASH_VERSION_CUTOFF` in `releases/describe_signed_firmware.py`).
pub const NEW_SIGHASH_FIRMWARE_VERSION_CUTOFF: u32 = 50;
/// Bootloaders from this version compute the firmware hash with the new scheme
/// (`BOOTLOADER_NEW_SIGHASH_VERSION` in `py/bitbox02/bitbox02/bitbox02/bootloader.py`).
pub const BOOTLOADER_NEW_SIGHASH_VERSION: Version = Version::new(1, 2, 0);
/// The first monotonic firmware version (v9.26.3) only signed for the new sighash scheme, so that
/// a bootloader older than [`BOOTLOADER_NEW_SIGHASH_VERSION`] rejects its signatures. Checked on
/// the official releases: v9.26.2 (monotonic version 50, the bootloader upgrade) is only signed
/// for the legacy scheme, v9.26.3 (51) and later only for the new one.
pub const FIRST_FIRMWARE_VERSION_REQUIRING_NEW_BOOTLOADER: u32 = 51;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareFormatError {
    /// The file does not start with a known signed-firmware magic. May be an unsigned binary.
    InvalidMagic([u8; 4]),
    TooSmall(usize),
    TooBig(usize),
}

impl fmt::Display for FirmwareFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic(m) => write!(
                f,
                "unrecognized firmware magic {}: this is not a signed BitBox02 firmware",
                hex::encode(m)
            ),
            Self::TooSmall(l) => write!(f, "firmware file too small ({} bytes)", l),
            Self::TooBig(l) => write!(
                f,
                "firmware too big: {} bytes (max {} bytes)",
                l, MAX_FIRMWARE_SIZE
            ),
        }
    }
}

impl std::error::Error for FirmwareFormatError {}

/// How the bootloader computes the firmware hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SighashScheme {
    /// `sha256(sha256(version_le32 | padded firmware))`, bootloaders < 1.2.0.
    Legacy,
    /// `sha256(product_id_le16 | version_le32 | padded firmware)`, bootloaders >= 1.2.0. (The
    /// bootloader then hashes it once more for ECDSA verification, but this is the value it
    /// displays and returns.)
    ProductId,
}

impl SighashScheme {
    /// The scheme used by a bootloader of the given version.
    pub fn for_bootloader(bootloader_version: Version) -> Self {
        if bootloader_version >= BOOTLOADER_NEW_SIGHASH_VERSION {
            SighashScheme::ProductId
        } else {
            SighashScheme::Legacy
        }
    }

    /// Whether a bootloader of this version can accept the signatures of a firmware with this
    /// monotonic version.
    pub fn bootloader_accepts(bootloader_version: Version, firmware_version: u32) -> bool {
        bootloader_version >= BOOTLOADER_NEW_SIGHASH_VERSION
            || firmware_version < FIRST_FIRMWARE_VERSION_REQUIRING_NEW_BOOTLOADER
    }

    /// The scheme used for the hashes published in the release notes of a firmware with the
    /// given monotonic version.
    pub fn for_firmware_version(firmware_version: u32) -> Self {
        if firmware_version >= NEW_SIGHASH_FIRMWARE_VERSION_CUTOFF {
            SighashScheme::ProductId
        } else {
            SighashScheme::Legacy
        }
    }
}

/// A parsed and validated signed firmware.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedFirmware {
    product: Product,
    sigdata: Vec<u8>,
    firmware: Vec<u8>,
}

impl fmt::Debug for SignedFirmware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedFirmware")
            .field("product", &self.product)
            .field("firmware_version", &self.firmware_version())
            .field("signing_pubkeys_version", &self.signing_pubkeys_version())
            .field("firmware_len", &self.firmware.len())
            .finish()
    }
}

impl SignedFirmware {
    /// Parse and validate a signed firmware binary: known magic, sigdata present, non-empty
    /// firmware no bigger than [`MAX_FIRMWARE_SIZE`].
    pub fn parse(data: &[u8]) -> Result<Self, FirmwareFormatError> {
        if data.len() < MAGIC_LEN {
            return Err(FirmwareFormatError::TooSmall(data.len()));
        }
        let magic: [u8; 4] = data[..MAGIC_LEN].try_into().expect("checked length");
        let product = Product::from_sigdata_magic(u32::from_be_bytes(magic))
            .ok_or(FirmwareFormatError::InvalidMagic(magic))?;
        if data.len() <= MAGIC_LEN + SIGDATA_LEN {
            return Err(FirmwareFormatError::TooSmall(data.len()));
        }
        let sigdata = data[MAGIC_LEN..MAGIC_LEN + SIGDATA_LEN].to_vec();
        let firmware = data[MAGIC_LEN + SIGDATA_LEN..].to_vec();
        if firmware.len() > MAX_FIRMWARE_SIZE {
            return Err(FirmwareFormatError::TooBig(firmware.len()));
        }
        Ok(SignedFirmware {
            product,
            sigdata,
            firmware,
        })
    }

    /// The product (platform + edition) this firmware is signed for.
    pub fn product(&self) -> Product {
        self.product
    }

    /// The signature data, as sent to the bootloader `s` command.
    pub fn sigdata(&self) -> &[u8] {
        &self.sigdata
    }

    /// The unsigned firmware binary, as built reproducibly.
    pub fn firmware(&self) -> &[u8] {
        &self.firmware
    }

    /// The monotonic firmware version. This is what the bootloader uses for downgrade protection
    /// and reports with its `v` command (not the X.Y.Z marketing version).
    pub fn firmware_version(&self) -> u32 {
        u32::from_le_bytes(
            self.sigdata[SIGNING_PUBKEYS_DATA_LEN..SIGNING_PUBKEYS_DATA_LEN + VERSION_LEN]
                .try_into()
                .expect("fixed size"),
        )
    }

    /// The version of the signing pubkeys contained in the sigdata. Also downgrade-protected.
    pub fn signing_pubkeys_version(&self) -> u32 {
        u32::from_le_bytes(self.sigdata[..VERSION_LEN].try_into().expect("fixed size"))
    }

    /// Number of 4kB chunks to write.
    pub fn num_chunks(&self) -> usize {
        self.firmware.len().div_ceil(CHUNK_SIZE)
    }

    /// The sha256 of the unsigned firmware. Compare with the reproducible build assertions in
    /// `bitbox02-firmware/releases/`.
    pub fn unsigned_hash(&self) -> [u8; 32] {
        Sha256::digest(&self.firmware).into()
    }

    /// The firmware hash as computed (and optionally displayed) by the bootloader.
    pub fn sighash(&self, scheme: SighashScheme) -> [u8; 32] {
        firmware_sighash(
            scheme,
            self.product,
            self.firmware_version(),
            &self.firmware,
        )
    }

    /// The firmware hash as published in the release notes and shown by an up to date
    /// bootloader (scheme chosen from the monotonic version, like `describe_signed_firmware.py`).
    pub fn published_sighash(&self) -> [u8; 32] {
        self.sighash(SighashScheme::for_firmware_version(self.firmware_version()))
    }
}

/// Compute the firmware hash as the bootloader does for a firmware (padded with 0xFF to
/// [`MAX_FIRMWARE_SIZE`]) with the given monotonic version.
pub fn firmware_sighash(
    scheme: SighashScheme,
    product: Product,
    firmware_version: u32,
    firmware: &[u8],
) -> [u8; 32] {
    assert!(firmware.len() <= MAX_FIRMWARE_SIZE);
    let padding = vec![0xffu8; MAX_FIRMWARE_SIZE - firmware.len()];
    let mut hasher = Sha256::new();
    if scheme == SighashScheme::ProductId {
        hasher.update(product.bootloader_product_id().to_le_bytes());
    }
    hasher.update(firmware_version.to_le_bytes());
    hasher.update(firmware);
    hasher.update(&padding);
    let first: [u8; 32] = hasher.finalize().into();
    match scheme {
        SighashScheme::ProductId => first,
        SighashScheme::Legacy => Sha256::digest(first).into(),
    }
}

/// Hash reported by the bootloader when no firmware is installed. See `_empty_firmware_hash()` in
/// `py/bitbox02/bitbox02/bitbox02/bootloader.py`.
pub fn empty_firmware_hash(
    bootloader_version: Version,
    product: Product,
    firmware_version: u32,
) -> [u8; 32] {
    firmware_sighash(
        SighashScheme::for_bootloader(bootloader_version),
        product,
        firmware_version,
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // The official signed BitBox02 Bitcoin-only v9.26.2 firmware, as embedded by the BitBoxApp in
    // `backend/devices/bitbox02bootloader/assets/firmware-bitbox02-btconly.v9.26.2.signed.bin.gz`.
    // It is the smallest official firmware (mostly padding), hence used as a fixture.
    const FIXTURE_GZ: &[u8] =
        include_bytes!("../tests/data/firmware-bitbox02-btconly.v9.26.2.signed.bin.gz");

    fn fixture() -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(FIXTURE_GZ)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn layout_constants() {
        assert_eq!(SIGNING_PUBKEYS_DATA_LEN, 388);
        assert_eq!(FIRMWARE_DATA_LEN, 196);
        assert_eq!(SIGDATA_LEN, 584);
        assert_eq!(FIRMWARE_CHUNKS, 216);
    }

    #[test]
    fn parse_official_firmware() {
        let fw = SignedFirmware::parse(&fixture()).unwrap();
        assert_eq!(fw.product(), Product::BitBox02BtcOnly);
        assert_eq!(fw.firmware_version(), 50);
        assert_eq!(fw.signing_pubkeys_version(), 3);
        assert_eq!(fw.sigdata().len(), SIGDATA_LEN);
        assert_eq!(fw.firmware().len(), 881228 - MAGIC_LEN - SIGDATA_LEN);
        assert_eq!(fw.num_chunks(), fw.firmware().len().div_ceil(4096));
        // From bitbox02-firmware/releases/firmware-v9.26.2/assertion-bitbox02-btconly.txt (the
        // reproducible build hash signed by the maintainers).
        assert_eq!(
            hex::encode(fw.unsigned_hash()),
            "25dee90b71e95fa38d9eda304d65b35a2eb8d75d6362ccbfcc718441b1a6a55f"
        );
        // Computed with `sighash()` of bitbox02-firmware/scripts/signed_firmware.py.
        assert_eq!(
            hex::encode(fw.published_sighash()),
            "2b31aac25b05bbbc7ff5898ecf233744dbc9a8de1e68e082c4b94f17f38e05a2"
        );
        // Computed with the legacy formula of releases/describe_signed_firmware.py.
        assert_eq!(
            hex::encode(fw.sighash(SighashScheme::Legacy)),
            "96891c9a4eba8bb5a49e2b64fd6ae8b2b92963c0f1f800fe4ba3212ba44aca2a"
        );
    }

    #[test]
    fn empty_hash() {
        // `_empty_bare_flash_hash` in bitbox02-firmware/src/bootloader/bootloader.c is
        // sha256d(0xff * MAX_FIRMWARE_SIZE), i.e. our legacy scheme without the version prefix.
        // Check our hashing of the padding against it by rebuilding the preimage manually.
        let padded = vec![0xffu8; MAX_FIRMWARE_SIZE];
        let h: [u8; 32] = Sha256::digest(Sha256::digest(&padded)).into();
        assert_eq!(
            hex::encode(h),
            "bf7180c123dfb5961d93cd5fd98aa43500b8af3a79f8c6564a9b02e18ab821ae"
        );
        // Legacy: sha256d(version | padding).
        let mut pre = 7u32.to_le_bytes().to_vec();
        pre.extend_from_slice(&padded);
        let expected: [u8; 32] = Sha256::digest(Sha256::digest(&pre)).into();
        assert_eq!(
            empty_firmware_hash(Version::new(1, 0, 6), Product::BitBox02Multi, 7),
            expected
        );
        // New: sha256(product id | version | padding).
        let mut pre = 4u16.to_le_bytes().to_vec();
        pre.extend_from_slice(&7u32.to_le_bytes());
        pre.extend_from_slice(&padded);
        let expected: [u8; 32] = Sha256::digest(&pre).into();
        assert_eq!(
            empty_firmware_hash(Version::new(1, 2, 0), Product::BitBox02NovaBtcOnly, 7),
            expected
        );
    }

    #[test]
    fn parse_errors() {
        let data = fixture();
        assert_eq!(
            SignedFirmware::parse(&data[..2]),
            Err(FirmwareFormatError::TooSmall(2))
        );
        let mut bad = data.clone();
        bad[0] = 0;
        assert!(matches!(
            SignedFirmware::parse(&bad),
            Err(FirmwareFormatError::InvalidMagic(_))
        ));
        assert_eq!(
            SignedFirmware::parse(&data[..MAGIC_LEN + SIGDATA_LEN]),
            Err(FirmwareFormatError::TooSmall(MAGIC_LEN + SIGDATA_LEN))
        );
        assert!(SignedFirmware::parse(&data[..MAGIC_LEN + SIGDATA_LEN + 1]).is_ok());
        let mut big = data[..MAGIC_LEN + SIGDATA_LEN].to_vec();
        big.extend(std::iter::repeat_n(0u8, MAX_FIRMWARE_SIZE));
        assert!(SignedFirmware::parse(&big).is_ok());
        big.push(0);
        assert_eq!(
            SignedFirmware::parse(&big),
            Err(FirmwareFormatError::TooBig(MAX_FIRMWARE_SIZE + 1))
        );
    }

    #[test]
    fn schemes() {
        assert_eq!(
            SighashScheme::for_bootloader(Version::new(1, 1, 9)),
            SighashScheme::Legacy
        );
        assert_eq!(
            SighashScheme::for_bootloader(Version::new(1, 2, 0)),
            SighashScheme::ProductId
        );
        assert_eq!(
            SighashScheme::for_firmware_version(49),
            SighashScheme::Legacy
        );
        assert_eq!(
            SighashScheme::for_firmware_version(50),
            SighashScheme::ProductId
        );
    }
}
