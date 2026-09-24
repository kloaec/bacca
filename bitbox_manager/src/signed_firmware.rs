//! The signed firmware binary format and the firmware hash ("sighash") computed by the bootloader.
//!
//! A signed firmware file is `magic (4 bytes, BE) | sigdata (SIGDATA_LEN) | firmware`, where
//! `sigdata = signing_pubkeys_data | firmware_data` and
//! - `signing_pubkeys_data = signing pubkeys version (u32 LE) | 3 signing pubkeys | 3 root sigs`,
//! - `firmware_data = monotonic firmware version (u32 LE) | 3 firmware signatures`.
//!
//! References: `bitbox02-firmware/scripts/signed_firmware.py`,
//! `bitbox02-firmware/releases/describe_signed_firmware.py`, `parse_signed_firmware()` in
//! `bitbox02-firmware/py/bitbox02/bitbox02/bitbox02/bootloader.py` and `_firmware_hash()` in
//! `bitbox02-firmware/src/bootloader/bootloader.c`.

use sha2::{Digest, Sha256};

use crate::{Error, Product, Version};

const MAGIC_LEN: usize = 4;
const SIGNING_PUBKEYS_DATA_LEN: usize = 4 + 3 * 64 + 3 * 64;
const FIRMWARE_DATA_LEN: usize = 4 + 3 * 64;
pub(crate) const SIGDATA_LEN: usize = SIGNING_PUBKEYS_DATA_LEN + FIRMWARE_DATA_LEN;
/// 928kB - 64kB. Same for all BitBox02 products (`FLASH_APP_LEN` in the bootloader).
const MAX_FIRMWARE_SIZE: usize = 884736;
/// Size of a firmware chunk written by the bootloader `w` command.
pub(crate) const CHUNK_SIZE: usize = 4096;
// The chunk index is sent as a single byte.
const _: () = assert!(MAX_FIRMWARE_SIZE / CHUNK_SIZE <= u8::MAX as usize);
const _: () = assert!(MAX_FIRMWARE_SIZE.is_multiple_of(CHUNK_SIZE));

/// How the bootloader computes the firmware hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SighashScheme {
    /// `sha256(sha256(version_le32 | padded firmware))`, bootloaders < 1.2.0.
    Legacy,
    /// `sha256(product_id_le16 | version_le32 | padded firmware)`, bootloaders >= 1.2.0. (The
    /// bootloader then hashes it once more for ECDSA verification, but this is the value it
    /// displays and returns.)
    ProductId,
}

/// Bootloaders from this version use the new scheme (`BOOTLOADER_NEW_SIGHASH_VERSION` in
/// `py/bitbox02/bitbox02/bitbox02/bootloader.py`).
const BOOTLOADER_NEW_SIGHASH_VERSION: Version = Version::new(1, 2, 0);

impl SighashScheme {
    pub(crate) fn for_bootloader(bootloader_version: Version) -> Self {
        if bootloader_version >= BOOTLOADER_NEW_SIGHASH_VERSION {
            SighashScheme::ProductId
        } else {
            SighashScheme::Legacy
        }
    }

    /// Whether a bootloader of this version accepts the signatures of a firmware with this
    /// monotonic version. Checked on the official releases: v9.26.2 (monotonic version 50, the
    /// bootloader upgrade) is only signed for the legacy scheme, v9.26.3 (51) and later only for
    /// the new one.
    pub(crate) fn bootloader_accepts(bootloader_version: Version, firmware_version: u32) -> bool {
        bootloader_version >= BOOTLOADER_NEW_SIGHASH_VERSION || firmware_version < 51
    }

    /// The scheme of the hash published in the release notes of a firmware with this monotonic
    /// version (`NEW_SIGHASH_VERSION_CUTOFF` in `releases/describe_signed_firmware.py`).
    fn for_firmware_version(firmware_version: u32) -> Self {
        if firmware_version >= 50 {
            SighashScheme::ProductId
        } else {
            SighashScheme::Legacy
        }
    }
}

/// A parsed and validated signed firmware.
pub struct SignedFirmware {
    product: Product,
    /// The signature data, as sent to the bootloader `s` command.
    pub(crate) sigdata: Vec<u8>,
    /// The unsigned firmware binary, as built reproducibly.
    pub(crate) binary: Vec<u8>,
}

impl SignedFirmware {
    /// Parse and validate a signed firmware: known magic, sigdata present, non-empty firmware no
    /// bigger than the flash.
    pub(crate) fn parse(data: &[u8]) -> Result<Self, Error> {
        if data.len() <= MAGIC_LEN + SIGDATA_LEN {
            return Err(Error::Other(format!(
                "firmware file too small ({} bytes)",
                data.len()
            )));
        }
        let (magic, rest) = data.split_at(MAGIC_LEN);
        let product = Product::from_sigdata_magic(u32::from_be_bytes(magic.try_into().unwrap()))
            .ok_or_else(|| {
                Error::Other(format!(
                    "unrecognized firmware magic {}: this is not a signed BitBox02 firmware",
                    hex::encode(magic)
                ))
            })?;
        let (sigdata, binary) = rest.split_at(SIGDATA_LEN);
        if binary.len() > MAX_FIRMWARE_SIZE {
            return Err(Error::Other(format!(
                "firmware too big: {} bytes (max {} bytes)",
                binary.len(),
                MAX_FIRMWARE_SIZE
            )));
        }
        Ok(SignedFirmware {
            product,
            sigdata: sigdata.to_vec(),
            binary: binary.to_vec(),
        })
    }

    /// The product (platform + edition) this firmware is signed for.
    pub fn product(&self) -> Product {
        self.product
    }

    /// The monotonic firmware version, used by the bootloader for downgrade protection (not the
    /// X.Y.Z version).
    pub fn firmware_version(&self) -> u32 {
        let v = &self.sigdata[SIGNING_PUBKEYS_DATA_LEN..SIGNING_PUBKEYS_DATA_LEN + 4];
        u32::from_le_bytes(v.try_into().unwrap())
    }

    /// The version of the signing pubkeys, also downgrade-protected.
    pub(crate) fn signing_pubkeys_version(&self) -> u32 {
        u32::from_le_bytes(self.sigdata[..4].try_into().unwrap())
    }

    /// The sha256 of the unsigned firmware, as in the reproducible build assertions in
    /// `bitbox02-firmware/releases/`.
    pub(crate) fn unsigned_hash(&self) -> [u8; 32] {
        Sha256::digest(&self.binary).into()
    }

    /// The firmware hash as computed (and optionally displayed) by the bootloader.
    pub(crate) fn sighash(&self, scheme: SighashScheme) -> [u8; 32] {
        sighash(scheme, self.product, self.firmware_version(), &self.binary)
    }

    /// The firmware hash as published in the release notes (and shown by an up to date
    /// bootloader).
    pub fn published_sighash(&self) -> [u8; 32] {
        self.sighash(SighashScheme::for_firmware_version(self.firmware_version()))
    }
}

/// The firmware hash of `binary` padded with 0xFF to the flash size.
fn sighash(
    scheme: SighashScheme,
    product: Product,
    firmware_version: u32,
    binary: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    if scheme == SighashScheme::ProductId {
        hasher.update(product.bootloader_product_id().to_le_bytes());
    }
    hasher.update(firmware_version.to_le_bytes());
    hasher.update(binary);
    hasher.update(vec![0xffu8; MAX_FIRMWARE_SIZE - binary.len()]);
    let hash: [u8; 32] = hasher.finalize().into();
    match scheme {
        SighashScheme::ProductId => hash,
        SighashScheme::Legacy => Sha256::digest(hash).into(),
    }
}

/// The hash reported by the bootloader when no firmware is installed. See `_empty_firmware_hash()`
/// in `py/bitbox02/bitbox02/bitbox02/bootloader.py`.
pub(crate) fn empty_firmware_hash(
    bootloader_version: Version,
    product: Product,
    firmware_version: u32,
) -> [u8; 32] {
    let scheme = SighashScheme::for_bootloader(bootloader_version);
    sighash(scheme, product, firmware_version, &[])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The official signed BitBox02 Bitcoin-only v9.26.2 firmware, as embedded by the BitBoxApp in
    /// `backend/devices/bitbox02bootloader/assets/`. It is the smallest official firmware (mostly
    /// padding), hence used as a fixture.
    pub(crate) fn fixture() -> Vec<u8> {
        use std::io::Read;
        let gz: &[u8] =
            include_bytes!("../tests/data/firmware-bitbox02-btconly.v9.26.2.signed.bin.gz");
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(gz)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn parse_official_firmware() {
        assert_eq!(SIGDATA_LEN, 584);
        let fw = SignedFirmware::parse(&fixture()).unwrap();
        assert_eq!(fw.product(), Product::BitBox02BtcOnly);
        assert_eq!(fw.firmware_version(), 50);
        assert_eq!(fw.signing_pubkeys_version(), 3);
        assert_eq!(fw.binary.len(), 881228 - MAGIC_LEN - SIGDATA_LEN);
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
        assert!(SignedFirmware::parse(&data[..2]).is_err());
        let mut bad = data.clone();
        bad[0] = 0;
        assert!(SignedFirmware::parse(&bad).is_err());
        assert!(SignedFirmware::parse(&data[..MAGIC_LEN + SIGDATA_LEN]).is_err());
        assert!(SignedFirmware::parse(&data[..MAGIC_LEN + SIGDATA_LEN + 1]).is_ok());
        let mut big = data[..MAGIC_LEN + SIGDATA_LEN].to_vec();
        big.resize(big.len() + MAX_FIRMWARE_SIZE, 0);
        assert!(SignedFirmware::parse(&big).is_ok());
        big.push(0);
        assert!(SignedFirmware::parse(&big).is_err());
    }
}
