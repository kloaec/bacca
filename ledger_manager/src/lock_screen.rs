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
    device::{apdu, ApduExchange, DeviceModel},
    error::*,
};

use ledger_apdu::APDUCommand;

/// The size of the chunks of picture data fetched from the device: the maximum size of the
/// answer to a fetch APDU (240) minus the status word.
const FETCH_CHUNK_SIZE: u32 = 240 - 2;
/// The size of the chunks of picture data sent to the device: the maximum size of the data of an
/// APDU minus the offset of the chunk.
const LOAD_CHUNK_SIZE: usize = 255 - 4;

/// Returned by the device when no custom picture is set.
const SW_CUSTOM_IMAGE_EMPTY: u16 = 0x662e;
/// Returned by the firmwares not supporting the custom lock screen.
const SW_UNKNOWN_APDU: u16 = 0x6d02;

fn command(ins: u8, data: Vec<u8>) -> APDUCommand<Vec<u8>> {
    apdu(0xe0, ins, 0x00, data)
}

/// The (width, height, bits per pixel) of the screen of the models supporting a custom lock
/// screen picture.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/customLockScreen/screenSpecs.ts
pub(crate) fn screen_specs(model: DeviceModel) -> Option<(u16, u16, u8)> {
    match model {
        DeviceModel::Stax => Some((400, 672, 4)),
        DeviceModel::Flex => Some((480, 600, 4)),
        DeviceModel::NanoGen5 => Some((300, 400, 1)),
        _ => None,
    }
}

fn status_error(status: u16) -> Error {
    match status {
        SW_LOCKED => Error::DeviceLocked,
        SW_RECOVERY_MODE => Error::Other("Device is in recovery mode.".into()),
        s => Error::DeviceStatus(s),
    }
}

/// Get the hash of the custom picture set on the device (hex-encoded), `None` if there is none.
pub(crate) fn fetch_image_hash(transport: &impl ApduExchange) -> Result<Option<String>, Error> {
    let resp = transport.exchange_apdu(&command(0x66, vec![]))?;
    match resp.retcode() {
        SW_OK if resp.data().is_empty() => Ok(None),
        SW_OK => Ok(Some(hex::encode(resp.data()))),
        SW_CUSTOM_IMAGE_EMPTY => Ok(None),
        s => Err(status_error(s)),
    }
}

/// Get the size of the custom picture set on the device, 0 if there is none (or with `strict`,
/// an error if the device doesn't answer with a size).
fn fetch_image_size(transport: &impl ApduExchange, strict: bool) -> Result<u32, Error> {
    let resp = transport.exchange_apdu(&command(0x64, vec![]))?;
    match (resp.retcode(), resp.data()) {
        (SW_OK, [a, b, c, d, ..]) => Ok(u32::from_be_bytes([*a, *b, *c, *d])),
        (SW_OK, _) => Err(Error::InvalidDeviceData(
            "custom lock screen: invalid size".into(),
        )),
        // For firmwares which don't support the custom lock screen.
        (SW_UNKNOWN_APDU | SW_CUSTOM_IMAGE_EMPTY, _) if !strict => Ok(0),
        (s, _) => Err(status_error(s)),
    }
}

/// A picture fetched from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchedImage {
    /// The picture, in the format of the device.
    pub data: Vec<u8>,
    /// Its hash, as computed by the device (hex-encoded).
    pub hash: String,
}

/// Fetch the custom lock screen picture from the device. Returns `None` if no picture is set.
/// `progress` is called with the progress, between 0 and 1. The device may ask the user to allow
/// the backup. The device must be on its dashboard.
pub(crate) fn fetch_image(
    transport: &impl ApduExchange,
    mut progress: impl FnMut(f32),
) -> Result<Option<FetchedImage>, Error> {
    let Some(hash) = fetch_image_hash(transport)? else {
        return Ok(None);
    };
    let length = fetch_image_size(transport, true)?;
    if length == 0 {
        // It should never happen since we fetched the hash earlier.
        return Ok(None);
    }
    log::debug!("Fetching the lock screen picture ({} bytes).", length);

    let mut data = Vec::with_capacity(length as usize);
    let mut offset = 0;
    while offset < length {
        let size = FETCH_CHUNK_SIZE.min(length - offset);
        progress((offset + 1) as f32 / length as f32);
        let mut params = offset.to_be_bytes().to_vec();
        params.push(size as u8);
        let resp = transport.exchange_apdu(&command(0x65, params))?;
        match resp.retcode() {
            SW_OK => {}
            SW_USER_REFUSED | SW_CONDITIONS_NOT_SATISFIED => {
                return Err(Error::RefusedOnDevice(
                    "The backup of the lock screen picture",
                ))
            }
            s => return Err(status_error(s)),
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
        offset += size;
    }
    progress(1.0);
    Ok(Some(FetchedImage { data, hash }))
}

/// Check the picture (in the format of the device) is in the format expected by this model.
pub(crate) fn check_image(image: &[u8], model: DeviceModel) -> Result<(), Error> {
    let invalid = |s: String| Err(Error::Other(format!("Invalid lock screen picture: {}", s)));
    let Some((width, height, bits_per_pixel)) = screen_specs(model) else {
        return invalid(format!("{} has no custom lock screen", model));
    };
    if image.len() < 8 {
        return invalid(format!("{} bytes is too short", image.len()));
    }
    let data_length = u32::from_le_bytes([image[5], image[6], image[7], 0]) as usize;
    if image.len() != 8 + data_length {
        return invalid(format!(
            "the header announces {} bytes of data, there are {}",
            data_length,
            image.len() - 8
        ));
    }
    let size = (
        u16::from_le_bytes([image[0], image[1]]),
        u16::from_le_bytes([image[2], image[3]]),
    );
    if size != (width, height) {
        return invalid(format!(
            "the picture is {}x{}, the screen of the {} is {}x{}",
            size.0, size.1, model, width, height
        ));
    }
    // `bitsPerPixelToBppIndicator` in customLockScreenLoad.ts.
    let bpp = image[4] >> 4;
    if bpp != if bits_per_pixel == 1 { 0 } else { 2 } {
        return invalid(format!(
            "unexpected number of bits per pixel (indicator {})",
            bpp
        ));
    }
    let raw_data_size = (width as usize * height as usize * bits_per_pixel as usize).div_ceil(8);
    match image[4] & 0x0f {
        0 if data_length != raw_data_size => invalid(format!(
            "{} bytes of uncompressed data, expected {}",
            data_length, raw_data_size
        )),
        0 | 1 => Ok(()),
        c => invalid(format!("unknown compression {}", c)),
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
/// device. Returns the hash of the picture now set on the device, if it could be queried. The
/// device must be on its dashboard.
pub(crate) fn load_image(
    transport: &impl ApduExchange,
    image: &[u8],
    mut progress: impl FnMut(LoadImageStep),
) -> Result<Option<String>, Error> {
    let length = u32::try_from(image.len())
        .map_err(|_| Error::Other("Invalid lock screen picture: too large".into()))?;

    progress(LoadImageStep::LoadPermissionRequested);
    let resp = transport.exchange_apdu(&command(0x60, length.to_be_bytes().to_vec()))?;
    match resp.retcode() {
        SW_OK => {}
        SW_USER_REFUSED => return Err(Error::RefusedOnDevice("Loading the lock screen picture")),
        SW_NOT_ENOUGH_SPACE => return Err(Error::NotEnoughSpace),
        s => return Err(status_error(s)),
    }

    for (i, chunk) in image.chunks(LOAD_CHUNK_SIZE).enumerate() {
        let offset = (i * LOAD_CHUNK_SIZE) as u32;
        progress(LoadImageStep::Loading {
            progress: (offset + 1) as f32 / length as f32,
        });
        let mut data = offset.to_be_bytes().to_vec();
        data.extend_from_slice(chunk);
        let status = transport.exchange_apdu(&command(0x61, data))?.retcode();
        if status != SW_OK {
            return Err(status_error(status));
        }
    }

    progress(LoadImageStep::CommitPermissionRequested);
    match transport.exchange_apdu(&command(0x62, vec![]))?.retcode() {
        SW_OK => {}
        SW_USER_REFUSED => return Err(Error::RefusedOnDevice("The new lock screen picture")),
        s => return Err(status_error(s)),
    }

    // Like Ledger Live, query the size and the hash of the new picture.
    match fetch_image_size(transport, false) {
        Ok(size) => log::debug!("Lock screen picture loaded ({} bytes).", size),
        Err(e) => log::warn!("Could not query the size of the lock screen picture: {}", e),
    }
    Ok(fetch_image_hash(transport).unwrap_or_else(|e| {
        log::warn!("Could not query the hash of the lock screen picture: {}", e);
        None
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::device::MockDevice;
    use std::{cell::RefCell, rc::Rc};

    /// A picture in the format of the device for this model (uncompressed).
    pub fn test_image(model: DeviceModel) -> Vec<u8> {
        let (width, height, bpp) = screen_specs(model).unwrap();
        let size = (width as usize * height as usize * bpp as usize).div_ceil(8);
        let mut image = Vec::new();
        image.extend_from_slice(&width.to_le_bytes());
        image.extend_from_slice(&height.to_le_bytes());
        image.push(if bpp == 1 { 0 } else { 2 << 4 });
        image.extend_from_slice(&(size as u32).to_le_bytes()[..3]);
        image.extend((0..size).map(|i| (i % 251) as u8));
        image
    }

    /// The state of a simulated device's lock screen.
    #[derive(Default)]
    struct Screen {
        image: Vec<u8>,
        pending: Vec<u8>,
        refuse_load: bool,
        refuse_commit: bool,
        refuse_fetch: bool,
    }

    /// Simulate the lock screen APDUs of a device.
    fn screen_device(screen: Rc<RefCell<Screen>>) -> MockDevice {
        MockDevice::new(move |c| {
            let mut s = screen.borrow_mut();
            let offset = || u32::from_be_bytes(c.data[..4].try_into().unwrap()) as usize;
            match c.ins {
                0x66 if s.image.is_empty() => (vec![], SW_CUSTOM_IMAGE_EMPTY),
                // Not a real hash, but deterministic.
                0x66 => (
                    vec![s.image.len() as u8, s.image[s.image.len() / 2], 0x42],
                    0x9000,
                ),
                0x64 => ((s.image.len() as u32).to_be_bytes().to_vec(), 0x9000),
                0x65 if s.refuse_fetch => (vec![], 0x5501),
                0x65 => (
                    s.image[offset()..offset() + c.data[4] as usize].to_vec(),
                    0x9000,
                ),
                0x60 if s.refuse_load => (vec![], 0x5501),
                0x60 => {
                    s.pending = vec![0; offset()];
                    (vec![], 0x9000)
                }
                0x61 => {
                    let chunk = &c.data[4..];
                    s.pending[offset()..offset() + chunk.len()].copy_from_slice(chunk);
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
    fn image_checks() {
        for model in [DeviceModel::Stax, DeviceModel::Flex, DeviceModel::NanoGen5] {
            check_image(&test_image(model), model).unwrap();
        }
        let stax = test_image(DeviceModel::Stax);
        assert_eq!(stax.len(), 400 * 672 / 2 + 8);
        // Another model.
        assert!(check_image(&stax, DeviceModel::Flex).is_err());
        assert!(check_image(&stax, DeviceModel::NanoX).is_err());
        // Truncated.
        assert!(check_image(&stax[..stax.len() - 1], DeviceModel::Stax).is_err());
        assert!(check_image(&stax[..4], DeviceModel::Stax).is_err());
        // A compressed picture of any (consistent) length is accepted.
        let mut compressed = stax[..5].to_vec();
        compressed[4] |= 1;
        compressed.extend_from_slice(&[3, 0, 0, 1, 2, 3]);
        check_image(&compressed, DeviceModel::Stax).unwrap();
        // Unknown compression.
        compressed[4] = 0x25;
        assert!(check_image(&compressed, DeviceModel::Stax).is_err());
        // Uncompressed with the wrong size.
        let mut bad = stax[..5].to_vec();
        bad.extend_from_slice(&[3, 0, 0, 1, 2, 3]);
        assert!(check_image(&bad, DeviceModel::Stax).is_err());
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
        let fetched = fetch_image(&device, |p| {
            assert!(p >= last && p <= 1.0);
            last = p;
        })
        .unwrap()
        .unwrap();
        assert_eq!(fetched.data, image);
        assert_eq!(last, 1.0);
        {
            let sent = device.sent.borrow();
            assert_eq!(sent[0], vec![0xe0, 0x66, 0x00, 0x00, 0x00]);
            assert_eq!(sent[1], vec![0xe0, 0x64, 0x00, 0x00, 0x00]);
            assert_eq!(
                sent[3],
                vec![0xe0, 0x65, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0xee, 0xee]
            );
            let last_offset = (image.len() as u32 - 1) / 238 * 238;
            let mut last_fetch = vec![0xe0, 0x65, 0x00, 0x00, 0x05];
            last_fetch.extend_from_slice(&last_offset.to_be_bytes());
            last_fetch.push((image.len() as u32 - last_offset) as u8);
            assert_eq!(sent.last(), Some(&last_fetch));
        }

        // Load it back on a device without picture.
        screen.borrow_mut().image.clear();
        assert_eq!(fetch_image(&device, |_| {}).unwrap(), None);
        device.sent.borrow_mut().clear();
        let mut steps = Vec::new();
        let new_hash = load_image(&device, &fetched.data, |s| steps.push(s)).unwrap();
        assert_eq!(new_hash, Some(fetched.hash.clone()));
        assert_eq!(screen.borrow().image, image);
        assert_eq!(steps[0], LoadImageStep::LoadPermissionRequested);
        assert_eq!(
            steps.last(),
            Some(&LoadImageStep::CommitPermissionRequested)
        );
        let sent = device.sent.borrow().clone();
        let mut create = vec![0xe0, 0x60, 0x00, 0x00, 0x04];
        create.extend_from_slice(&(image.len() as u32).to_be_bytes());
        assert_eq!(sent[0], create);
        assert_eq!(
            &sent[2][..9],
            &[0xe0, 0x61, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xfb]
        );
        let loads = sent.iter().filter(|a| a[1] == 0x61).count();
        assert_eq!(loads, image.len().div_ceil(LOAD_CHUNK_SIZE));
        assert!(sent.contains(&vec![0xe0, 0x62, 0x00, 0x00, 0x00]));

        // Refusals.
        screen.borrow_mut().refuse_load = true;
        let err = load_image(&device, &image, |_| {}).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Loading the lock screen picture was refused on the device."
        );
        screen.borrow_mut().refuse_load = false;
        screen.borrow_mut().refuse_commit = true;
        let err = load_image(&device, &image, |_| {}).unwrap_err();
        assert_eq!(
            err.to_string(),
            "The new lock screen picture was refused on the device."
        );
        screen.borrow_mut().refuse_fetch = true;
        assert!(matches!(
            fetch_image(&device, |_| {}),
            Err(Error::RefusedOnDevice(_))
        ));
    }

    #[test]
    fn hash_and_size_statuses() {
        let device = MockDevice::new(|c| match c.ins {
            0x66 => (vec![], 0x662f),
            _ => (vec![], 0x6d02),
        });
        let err = fetch_image_hash(&device).unwrap_err();
        assert_eq!(err.to_string(), "Device is in recovery mode.");
        assert_eq!(fetch_image_size(&device, false).unwrap(), 0);
        assert!(fetch_image_size(&device, true).is_err());
        let device = MockDevice::new(|_| (vec![], 0x5515));
        assert!(matches!(
            fetch_image(&device, |_| {}),
            Err(Error::DeviceLocked)
        ));
    }
}
