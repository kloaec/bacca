//! The custom lock screen picture of the devices with a large screen (Stax, Flex, Nano Gen5):
//! backing it up from the device and loading it back.
//!
//! Ported from:
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/customLockScreenFetch.ts
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/customLockScreenFetchHash.ts
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/customLockScreenFetchSize.ts
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/customLockScreenLoad.ts
//!
//! The picture is stored on the device in this format (`generateCustomLockScreenImageFormat` in
//! customLockScreenLoad.ts):
//! - width (u16, little endian), height (u16, little endian);
//! - one byte: the "bpp" indicator in the high nibble, the compression (0 or 1) in the low one;
//! - the length of the data (24 bits, little endian);
//! - the data: the pixels, or if compressed a sequence of gzip-compressed chunks each prefixed
//!   with its length (u16, little endian).
//!
//! Ledger Live decompresses the picture it fetches, and compresses it again when loading it back.
//! We keep the picture in the format the device returned it, and load it back as is: this is the
//! same format, and it avoids depending on the output of another gzip implementation.

use crate::{
    device::ApduExchange,
    error::{Error, StatusCode},
    model::DeviceModel,
};

use ledger_apdu::APDUCommand;
use ledger_transport_hidapi::TransportNativeHID;

/// Maximum size of the response to a fetch APDU (data and status).
const FETCH_MAX_APDU_SIZE: usize = 240;
/// The size of the chunks of picture data fetched from the device.
pub(crate) const FETCH_CHUNK_SIZE: usize = FETCH_MAX_APDU_SIZE - 2;
/// Maximum size of the data of a load APDU.
const LOAD_MAX_APDU_SIZE: usize = 255;
/// The size of the chunks of picture data sent to the device (the first 4 bytes of the data of
/// the APDU are the offset of the chunk).
pub(crate) const LOAD_CHUNK_SIZE: usize = LOAD_MAX_APDU_SIZE - 4;

/// The size of the header of the picture format.
const HEADER_SIZE: usize = 8;

/// Status returned by the device when no custom picture is set.
const CUSTOM_IMAGE_EMPTY: u16 = 0x662e;
/// Status returned by firmwares not supporting the custom lock screen.
const UNKNOWN_APDU: u16 = 0x6d02;

fn command(ins: u8, data: Vec<u8>) -> APDUCommand<Vec<u8>> {
    APDUCommand {
        cla: 0xe0,
        ins,
        p1: 0x00,
        p2: 0x00,
        data,
    }
}

/// Create a picture of this size (in bytes), to be loaded chunk by chunk. The user has to allow
/// it on the device.
pub(crate) fn create_image_command(size: u32) -> APDUCommand<Vec<u8>> {
    command(0x60, size.to_be_bytes().to_vec())
}

/// Load a chunk of the picture at this offset.
pub(crate) fn load_chunk_command(offset: u32, chunk: &[u8]) -> APDUCommand<Vec<u8>> {
    let mut data = offset.to_be_bytes().to_vec();
    data.extend_from_slice(chunk);
    command(0x61, data)
}

/// Commit the loaded picture. The user has to confirm it on the device.
pub(crate) fn commit_image_command() -> APDUCommand<Vec<u8>> {
    command(0x62, Vec::new())
}

/// Get the size of the picture set on the device.
pub(crate) fn fetch_size_command() -> APDUCommand<Vec<u8>> {
    command(0x64, Vec::new())
}

/// Fetch `size` bytes of the picture from this offset.
pub(crate) fn fetch_chunk_command(offset: u32, size: u8) -> APDUCommand<Vec<u8>> {
    let mut data = offset.to_be_bytes().to_vec();
    data.push(size);
    command(0x65, data)
}

/// Get the hash of the picture set on the device.
pub(crate) fn fetch_hash_command() -> APDUCommand<Vec<u8>> {
    command(0x66, Vec::new())
}

/// The (offset, size) of the chunks to fetch a picture of this length.
pub(crate) fn fetch_chunks(length: u32) -> Vec<(u32, u8)> {
    let mut chunks = Vec::new();
    let mut offset = 0u32;
    while offset < length {
        let size = (FETCH_CHUNK_SIZE as u32).min(length - offset);
        chunks.push((offset, size as u8));
        offset += size;
    }
    chunks
}

fn common_status_error(status: u16) -> Error {
    match status {
        s if s == StatusCode::LockedDevice as u16 => Error::DeviceLocked,
        s if s == StatusCode::DeviceInRecoveryMode as u16 => Error::DeviceInRecoveryMode,
        s => Error::DeviceStatus(s),
    }
}

/// Get the hash of the custom picture set on the device (hex-encoded), `None` if there is none.
pub(crate) fn fetch_image_hash<T: ApduExchange>(transport: &T) -> Result<Option<String>, Error> {
    let resp = transport.exchange_apdu(&fetch_hash_command())?;
    match resp.retcode() {
        0x9000 if resp.data().is_empty() => Ok(None),
        0x9000 => Ok(Some(hex::encode(resp.data()))),
        CUSTOM_IMAGE_EMPTY => Ok(None),
        s => Err(common_status_error(s)),
    }
}

/// Get the size of the custom picture set on the device, 0 if there is none.
pub(crate) fn fetch_image_size<T: ApduExchange>(transport: &T) -> Result<u32, Error> {
    let resp = transport.exchange_apdu(&fetch_size_command())?;
    match resp.retcode() {
        0x9000 => {
            let data = resp.data();
            if data.len() < 4 {
                return Err(Error::InvalidDeviceData(
                    "custom lock screen: invalid size".into(),
                ));
            }
            Ok(u32::from_be_bytes([data[0], data[1], data[2], data[3]]))
        }
        // For firmwares which don't support the custom lock screen.
        UNKNOWN_APDU | CUSTOM_IMAGE_EMPTY => Ok(0),
        s => Err(common_status_error(s)),
    }
}

/// A picture fetched from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedImage {
    /// The picture, in the format of the device.
    pub data: Vec<u8>,
    /// Its hash, as computed by the device (hex-encoded).
    pub hash: String,
}

/// Fetch the custom lock screen picture from the device. Returns `None` if no picture is set.
/// `progress` is called with the progress, between 0 and 1. The device may ask the user to allow
/// the backup (`Error::UserRefusedOnDevice` if refused).
///
/// The device must be on its dashboard.
pub fn fetch_image<P: FnMut(f32)>(
    transport: &TransportNativeHID,
    progress: P,
) -> Result<Option<FetchedImage>, Error> {
    fetch_image_from(transport, progress)
}

pub(crate) fn fetch_image_from<T: ApduExchange, P: FnMut(f32)>(
    transport: &T,
    mut progress: P,
) -> Result<Option<FetchedImage>, Error> {
    let hash = match fetch_image_hash(transport)? {
        Some(h) => h,
        None => return Ok(None),
    };

    let resp = transport.exchange_apdu(&fetch_size_command())?;
    if resp.retcode() != StatusCode::OK as u16 {
        return Err(common_status_error(resp.retcode()));
    }
    let length = match resp.data() {
        [a, b, c, d, ..] => u32::from_be_bytes([*a, *b, *c, *d]),
        _ => {
            return Err(Error::InvalidDeviceData(
                "custom lock screen: invalid size".into(),
            ))
        }
    };
    if length == 0 {
        // It should never happen since we fetched the hash earlier.
        return Ok(None);
    }
    log::debug!("Fetching the lock screen picture ({} bytes).", length);

    let mut data = Vec::with_capacity(length as usize);
    for (offset, size) in fetch_chunks(length) {
        progress((offset + 1) as f32 / length as f32);
        let resp = transport.exchange_apdu(&fetch_chunk_command(offset, size))?;
        match resp.retcode() {
            s if s == StatusCode::OK as u16 => {}
            s if s == StatusCode::UserRefusedOnDevice as u16
                || s == StatusCode::ConditionsOfUseNotSatisfied as u16 =>
            {
                return Err(Error::UserRefusedOnDevice)
            }
            s => return Err(common_status_error(s)),
        }
        if resp.data().len() != size as usize {
            return Err(Error::InvalidDeviceData(format!(
                "custom lock screen: expected {} bytes at offset {}, got {}",
                size,
                offset,
                resp.data().len()
            )));
        }
        data.extend_from_slice(resp.data());
    }
    progress(1.0);
    Ok(Some(FetchedImage { data, hash }))
}

/// The header of a picture in the format of the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    pub width: u16,
    pub height: u16,
    /// The "bpp" indicator (see `ScreenSpecs::bpp_indicator`).
    pub bpp: u8,
    /// 0: raw pixels, 1: gzip-compressed chunks.
    pub compression: u8,
    /// The length of the data following the header.
    pub data_length: u32,
}

/// Parse the header of a picture, checking its length is consistent.
pub fn parse_image_header(image: &[u8]) -> Result<ImageHeader, Error> {
    if image.len() < HEADER_SIZE {
        return Err(Error::InvalidImage(format!(
            "{} bytes is too short",
            image.len()
        )));
    }
    let header = ImageHeader {
        width: u16::from_le_bytes([image[0], image[1]]),
        height: u16::from_le_bytes([image[2], image[3]]),
        bpp: image[4] >> 4,
        compression: image[4] & 0x0f,
        data_length: u32::from_le_bytes([image[5], image[6], image[7], 0]),
    };
    if image.len() != HEADER_SIZE + header.data_length as usize {
        return Err(Error::InvalidImage(format!(
            "the header announces {} bytes of data, there are {}",
            header.data_length,
            image.len() - HEADER_SIZE
        )));
    }
    Ok(header)
}

/// Check the picture is in the format expected by this model.
pub fn check_image_for_model(image: &[u8], model: DeviceModel) -> Result<ImageHeader, Error> {
    let specs = model
        .screen_specs()
        .ok_or_else(|| Error::InvalidImage(format!("{} has no custom lock screen", model)))?;
    let header = parse_image_header(image)?;
    if (header.width, header.height) != (specs.width, specs.height) {
        return Err(Error::InvalidImage(format!(
            "the picture is {}x{}, the screen of the {} is {}x{}",
            header.width, header.height, model, specs.width, specs.height
        )));
    }
    if header.bpp != specs.bpp_indicator() {
        return Err(Error::InvalidImage(format!(
            "unexpected number of bits per pixel (indicator {})",
            header.bpp
        )));
    }
    match header.compression {
        0 if header.data_length as usize != specs.raw_data_size() => {
            Err(Error::InvalidImage(format!(
                "{} bytes of uncompressed data, expected {}",
                header.data_length,
                specs.raw_data_size()
            )))
        }
        0 | 1 => Ok(header),
        c => Err(Error::InvalidImage(format!("unknown compression {}", c))),
    }
}

/// A step of the loading of a lock screen picture.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadImageStep {
    /// The user must allow loading the picture on the device.
    LoadPermissionRequested,
    /// Transferring the picture. `progress` is between 0 and 1.
    Loading { progress: f32 },
    /// The user must confirm the new lock screen picture on the device.
    CommitPermissionRequested,
}

/// Load this picture (in the format of the device, as returned by `fetch_image`) as the custom
/// lock screen of the device. The user has to allow it, and then to confirm the picture, on the
/// device. Returns the hash of the picture now set on the device, if it could be queried.
///
/// The device must be on its dashboard.
pub fn load_image<P: FnMut(LoadImageStep)>(
    transport: &TransportNativeHID,
    image: &[u8],
    progress: P,
) -> Result<Option<String>, Error> {
    load_image_to(transport, image, progress)
}

pub(crate) fn load_image_to<T: ApduExchange, P: FnMut(LoadImageStep)>(
    transport: &T,
    image: &[u8],
    mut progress: P,
) -> Result<Option<String>, Error> {
    let length = u32::try_from(image.len())
        .map_err(|_| Error::InvalidImage("the picture is too large".into()))?;

    progress(LoadImageStep::LoadPermissionRequested);
    let resp = transport.exchange_apdu(&create_image_command(length))?;
    match resp.retcode() {
        s if s == StatusCode::OK as u16 => {}
        s if s == StatusCode::UserRefusedOnDevice as u16 => {
            return Err(Error::ImageLoadRefusedOnDevice)
        }
        s if s == StatusCode::NotEnoughSpace as u16 => return Err(Error::NotEnoughSpace),
        s => return Err(common_status_error(s)),
    }

    for (i, chunk) in image.chunks(LOAD_CHUNK_SIZE).enumerate() {
        let offset = (i * LOAD_CHUNK_SIZE) as u32;
        progress(LoadImageStep::Loading {
            progress: (offset + 1) as f32 / length as f32,
        });
        let resp = transport.exchange_apdu(&load_chunk_command(offset, chunk))?;
        if resp.retcode() != StatusCode::OK as u16 {
            return Err(common_status_error(resp.retcode()));
        }
    }

    progress(LoadImageStep::CommitPermissionRequested);
    let resp = transport.exchange_apdu(&commit_image_command())?;
    match resp.retcode() {
        s if s == StatusCode::OK as u16 => {}
        s if s == StatusCode::UserRefusedOnDevice as u16 => {
            return Err(Error::ImageCommitRefusedOnDevice)
        }
        s => return Err(common_status_error(s)),
    }

    // Like Ledger Live, query the size and the hash of the new picture.
    match fetch_image_size(transport) {
        Ok(size) => log::debug!("Lock screen picture loaded ({} bytes).", size),
        Err(e) => log::warn!("Could not query the size of the lock screen picture: {}", e),
    }
    match fetch_image_hash(transport) {
        Ok(hash) => Ok(hash),
        Err(e) => {
            log::warn!("Could not query the hash of the lock screen picture: {}", e);
            Ok(None)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::device::tests_support::MockDevice;
    use std::{cell::RefCell, rc::Rc};

    /// A picture in the format of the device for this model (uncompressed).
    pub fn test_image(model: DeviceModel) -> Vec<u8> {
        let specs = model.screen_specs().unwrap();
        let size = specs.raw_data_size();
        let mut image = Vec::new();
        image.extend_from_slice(&specs.width.to_le_bytes());
        image.extend_from_slice(&specs.height.to_le_bytes());
        image.push(specs.bpp_indicator() << 4);
        image.extend_from_slice(&(size as u32).to_le_bytes()[..3]);
        image.extend((0..size).map(|i| (i % 251) as u8));
        image
    }

    /// The state of a simulated device's lock screen.
    #[derive(Default)]
    pub struct Screen {
        pub image: Vec<u8>,
        pub pending: Vec<u8>,
        pub refuse_load: bool,
        pub refuse_commit: bool,
        pub refuse_fetch: bool,
    }

    /// Simulate the lock screen APDUs of a device.
    pub fn screen_device(screen: Rc<RefCell<Screen>>) -> MockDevice {
        MockDevice::new(move |c| {
            let mut s = screen.borrow_mut();
            match c.ins {
                0x66 if s.image.is_empty() => (vec![], CUSTOM_IMAGE_EMPTY),
                // Not a real hash, but deterministic.
                0x66 => (
                    vec![s.image.len() as u8, s.image[s.image.len() / 2], 0x42],
                    0x9000,
                ),
                0x64 => ((s.image.len() as u32).to_be_bytes().to_vec(), 0x9000),
                0x65 if s.refuse_fetch => (vec![], 0x5501),
                0x65 => {
                    let offset =
                        u32::from_be_bytes([c.data[0], c.data[1], c.data[2], c.data[3]]) as usize;
                    let size = c.data[4] as usize;
                    (s.image[offset..offset + size].to_vec(), 0x9000)
                }
                0x60 if s.refuse_load => (vec![], 0x5501),
                0x60 => {
                    let size =
                        u32::from_be_bytes([c.data[0], c.data[1], c.data[2], c.data[3]]) as usize;
                    s.pending = vec![0; size];
                    (vec![], 0x9000)
                }
                0x61 => {
                    let offset =
                        u32::from_be_bytes([c.data[0], c.data[1], c.data[2], c.data[3]]) as usize;
                    let chunk = &c.data[4..];
                    s.pending[offset..offset + chunk.len()].copy_from_slice(chunk);
                    (vec![], 0x9000)
                }
                0x62 if s.refuse_commit => (vec![], 0x5501),
                0x62 => {
                    s.image = std::mem::take(&mut s.pending);
                    (vec![], 0x9000)
                }
                _ => (vec![], 0x6d00),
            }
        })
    }

    #[test]
    fn apdus() {
        assert_eq!(
            create_image_command(0x0102_0304).serialize(),
            vec![0xe0, 0x60, 0x00, 0x00, 0x04, 0x01, 0x02, 0x03, 0x04]
        );
        assert_eq!(
            load_chunk_command(251, &[0xaa, 0xbb]).serialize(),
            vec![0xe0, 0x61, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0xfb, 0xaa, 0xbb]
        );
        assert_eq!(
            commit_image_command().serialize(),
            vec![0xe0, 0x62, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            fetch_size_command().serialize(),
            vec![0xe0, 0x64, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            fetch_chunk_command(476, 238).serialize(),
            vec![0xe0, 0x65, 0x00, 0x00, 0x05, 0x00, 0x00, 0x01, 0xdc, 0xee]
        );
        assert_eq!(
            fetch_hash_command().serialize(),
            vec![0xe0, 0x66, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn chunking() {
        assert!(fetch_chunks(0).is_empty());
        assert_eq!(fetch_chunks(10), vec![(0, 10)]);
        assert_eq!(fetch_chunks(238), vec![(0, 238)]);
        assert_eq!(fetch_chunks(239), vec![(0, 238), (238, 1)]);
        let chunks = fetch_chunks(134_408);
        assert_eq!(chunks.len(), 134_408usize.div_ceil(238));
        assert_eq!(
            chunks.iter().map(|(_, s)| *s as usize).sum::<usize>(),
            134_408
        );
        assert!(chunks.windows(2).all(|w| w[0].0 + w[0].1 as u32 == w[1].0));
    }

    #[test]
    fn image_headers() {
        for model in [DeviceModel::Stax, DeviceModel::Flex, DeviceModel::NanoGen5] {
            let image = test_image(model);
            let header = check_image_for_model(&image, model).unwrap();
            assert_eq!(header.compression, 0);
            assert_eq!(
                header.data_length as usize,
                model.screen_specs().unwrap().raw_data_size()
            );
        }
        let stax = test_image(DeviceModel::Stax);
        // Another model.
        assert!(check_image_for_model(&stax, DeviceModel::Flex).is_err());
        assert!(check_image_for_model(&stax, DeviceModel::NanoX).is_err());
        // Truncated.
        assert!(parse_image_header(&stax[..stax.len() - 1]).is_err());
        assert!(parse_image_header(&stax[..4]).is_err());
        // A compressed picture of any (consistent) length is accepted.
        let mut compressed = stax[..5].to_vec();
        compressed[4] |= 1;
        compressed.extend_from_slice(&[3, 0, 0, 1, 2, 3]);
        let header = check_image_for_model(&compressed, DeviceModel::Stax).unwrap();
        assert_eq!((header.compression, header.data_length), (1, 3));
        // Unknown compression.
        compressed[4] = 0x25;
        assert!(check_image_for_model(&compressed, DeviceModel::Stax).is_err());
        // Uncompressed with the wrong size.
        let mut bad = stax[..5].to_vec();
        bad.extend_from_slice(&[3, 0, 0, 1, 2, 3]);
        assert!(check_image_for_model(&bad, DeviceModel::Stax).is_err());
    }

    #[test]
    fn fetch_and_load() {
        let image = test_image(DeviceModel::Stax);
        let screen = Rc::new(RefCell::new(Screen {
            image: image.clone(),
            ..Default::default()
        }));
        let device = screen_device(screen.clone());

        let mut last = 0.0;
        let fetched = fetch_image_from(&device, |p| {
            assert!(p >= last && p <= 1.0);
            last = p;
        })
        .unwrap()
        .unwrap();
        assert_eq!(fetched.data, image);
        assert_eq!(last, 1.0);
        let hash = fetched.hash.clone();

        // Load it back on a device without picture.
        screen.borrow_mut().image.clear();
        assert_eq!(fetch_image_from(&device, |_| {}).unwrap(), None);
        let mut steps = Vec::new();
        let new_hash = load_image_to(&device, &fetched.data, |s| steps.push(s)).unwrap();
        assert_eq!(new_hash, Some(hash));
        assert_eq!(screen.borrow().image, image);
        assert_eq!(steps[0], LoadImageStep::LoadPermissionRequested);
        assert_eq!(
            steps.last(),
            Some(&LoadImageStep::CommitPermissionRequested)
        );
        let loads = device.sent().iter().filter(|a| a[1] == 0x61).count();
        assert_eq!(loads, image.len().div_ceil(LOAD_CHUNK_SIZE));
        assert!(device
            .sent()
            .iter()
            .filter(|a| a[1] == 0x61)
            .all(|a| a.len() <= 5 + 4 + LOAD_CHUNK_SIZE));

        // Refusals.
        screen.borrow_mut().refuse_load = true;
        assert!(matches!(
            load_image_to(&device, &image, |_| {}),
            Err(Error::ImageLoadRefusedOnDevice)
        ));
        screen.borrow_mut().refuse_load = false;
        screen.borrow_mut().refuse_commit = true;
        assert!(matches!(
            load_image_to(&device, &image, |_| {}),
            Err(Error::ImageCommitRefusedOnDevice)
        ));
        screen.borrow_mut().refuse_fetch = true;
        assert!(matches!(
            fetch_image_from(&device, |_| {}),
            Err(Error::UserRefusedOnDevice)
        ));
    }

    #[test]
    fn hash_and_size_statuses() {
        let device = MockDevice::new(|c| match c.ins {
            0x66 => (vec![], 0x662f),
            _ => (vec![], 0x6d02),
        });
        assert!(matches!(
            fetch_image_hash(&device),
            Err(Error::DeviceInRecoveryMode)
        ));
        assert_eq!(fetch_image_size(&device).unwrap(), 0);
        let device = MockDevice::new(|_| (vec![], 0x5515));
        assert!(matches!(
            fetch_image_hash(&device),
            Err(Error::DeviceLocked)
        ));
        assert!(matches!(
            fetch_image_from(&device, |_| {}),
            Err(Error::DeviceLocked)
        ));
    }
}
