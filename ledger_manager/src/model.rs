//! Ledger device models.
//!
//! The list of models and their identifiers is taken from the `@ledgerhq/devices` package, which
//! used to live in the ledger-live monorepo (libs/ledgerjs/packages/devices/src/index.ts) and is
//! now published from https://github.com/LedgerHQ/ts-libs (packages/devices/src/index.ts).

use crate::version::SemVer;

use std::fmt;

/// The USB vendor id of all Ledger devices.
/// https://github.com/LedgerHQ/ts-libs (packages/devices/src/index.ts, `ledgerUSBVendorId`)
pub const LEDGER_USB_VENDOR_ID: u16 = 0x2c97;

/// A model of Ledger hardware wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceModel {
    /// Ledger Blue (legacy, not sold anymore).
    Blue,
    /// Ledger Nano S.
    NanoS,
    /// Ledger Nano S Plus.
    NanoSPlus,
    /// Ledger Nano X.
    NanoX,
    /// Ledger Stax.
    Stax,
    /// Ledger Flex (internally named "europa" by Ledger).
    Flex,
    /// Ledger Nano Gen5 (internally named "apex" by Ledger).
    NanoGen5,
}

/// Static information about a device model, as defined in `@ledgerhq/devices`.
struct ModelSpec {
    model: DeviceModel,
    /// The Ledger Live identifier for this model (`DeviceModelId`).
    id: &'static str,
    product_name: &'static str,
    /// The model part ("MM") of the USB product id, which is defined as 0xMMII.
    product_id_mm: u8,
    /// The product id used by older firmwares (and in bootloader mode for some models).
    legacy_usb_product_id: u16,
    /// Masks applied to the first two bytes of the target id.
    masks: &'static [u32],
    memory_size: u32,
    usb_only: bool,
}

const MODELS: &[ModelSpec] = &[
    ModelSpec {
        model: DeviceModel::Blue,
        id: "blue",
        product_name: "Ledger Blue",
        product_id_mm: 0x00,
        legacy_usb_product_id: 0x0000,
        masks: &[0x3100_0000, 0x3101_0000],
        memory_size: 480 * 1024,
        usb_only: true,
    },
    ModelSpec {
        model: DeviceModel::NanoS,
        id: "nanoS",
        product_name: "Ledger Nano S",
        product_id_mm: 0x10,
        legacy_usb_product_id: 0x0001,
        masks: &[0x3110_0000],
        memory_size: 320 * 1024,
        usb_only: true,
    },
    ModelSpec {
        model: DeviceModel::NanoX,
        id: "nanoX",
        product_name: "Ledger Nano X",
        product_id_mm: 0x40,
        legacy_usb_product_id: 0x0004,
        masks: &[0x3300_0000],
        memory_size: 2 * 1024 * 1024,
        usb_only: false,
    },
    ModelSpec {
        model: DeviceModel::NanoSPlus,
        id: "nanoSP",
        product_name: "Ledger Nano S Plus",
        product_id_mm: 0x50,
        legacy_usb_product_id: 0x0005,
        masks: &[0x3310_0000],
        memory_size: 1533 * 1024,
        usb_only: true,
    },
    ModelSpec {
        model: DeviceModel::NanoGen5,
        id: "apex",
        product_name: "Ledger Nano Gen5",
        product_id_mm: 0x80,
        legacy_usb_product_id: 0x0008,
        masks: &[0x3340_0000],
        memory_size: 1533 * 1024,
        usb_only: false,
    },
    ModelSpec {
        model: DeviceModel::Stax,
        id: "stax",
        product_name: "Ledger Stax",
        product_id_mm: 0x60,
        legacy_usb_product_id: 0x0006,
        masks: &[0x3320_0000],
        memory_size: 1533 * 1024,
        usb_only: false,
    },
    ModelSpec {
        model: DeviceModel::Flex,
        id: "europa",
        product_name: "Ledger Flex",
        product_id_mm: 0x70,
        legacy_usb_product_id: 0x0007,
        masks: &[0x3330_0000],
        memory_size: 1533 * 1024,
        usb_only: false,
    },
];

impl DeviceModel {
    /// All the known models.
    pub const ALL: [DeviceModel; 7] = [
        DeviceModel::Blue,
        DeviceModel::NanoS,
        DeviceModel::NanoSPlus,
        DeviceModel::NanoX,
        DeviceModel::Stax,
        DeviceModel::Flex,
        DeviceModel::NanoGen5,
    ];

    fn spec(&self) -> &'static ModelSpec {
        MODELS
            .iter()
            .find(|s| s.model == *self)
            .expect("All models are in the MODELS table.")
    }

    /// Identify a device model from a target id (as returned by the GetVersion APDU), based on its
    /// first two bytes.
    /// https://github.com/LedgerHQ/ts-libs (packages/devices/src/index.ts, `identifyTargetId`)
    pub fn from_target_id(target_id: u32) -> Option<Self> {
        MODELS
            .iter()
            .find(|s| s.masks.contains(&(target_id & 0xffff_0000)))
            .map(|s| s.model)
    }

    /// Identify a device model from a USB product id. The legacy product ids are checked first,
    /// then the most significant byte of the product id (0xMMII).
    /// https://github.com/LedgerHQ/ts-libs (packages/devices/src/index.ts, `identifyUSBProductId`)
    pub fn from_usb_product_id(product_id: u16) -> Option<Self> {
        if let Some(s) = MODELS
            .iter()
            .find(|s| s.legacy_usb_product_id == product_id)
        {
            return Some(s.model);
        }
        let mm = (product_id >> 8) as u8;
        MODELS
            .iter()
            .find(|s| s.product_id_mm == mm)
            .map(|s| s.model)
    }

    /// Identify a model from its Ledger Live identifier ("nanoS", "nanoSP", "nanoX", "stax",
    /// "europa", "apex", "blue").
    pub fn from_id(id: &str) -> Option<Self> {
        MODELS.iter().find(|s| s.id == id).map(|s| s.model)
    }

    /// The Ledger Live identifier of this model (`DeviceModelId`).
    pub fn id(&self) -> &'static str {
        self.spec().id
    }

    /// The official product name, e.g. "Ledger Nano S Plus".
    pub fn product_name(&self) -> &'static str {
        self.spec().product_name
    }

    /// The model part of the USB product id.
    pub fn usb_product_id_mm(&self) -> u8 {
        self.spec().product_id_mm
    }

    /// The legacy USB product id.
    pub fn legacy_usb_product_id(&self) -> u16 {
        self.spec().legacy_usb_product_id
    }

    /// The target id masks of this model.
    pub fn target_id_masks(&self) -> &'static [u32] {
        self.spec().masks
    }

    /// The memory available for applications, in bytes.
    pub fn memory_size(&self) -> u32 {
        self.spec().memory_size
    }

    /// Whether this model can only be connected through USB (no Bluetooth).
    pub fn is_usb_only(&self) -> bool {
        self.spec().usb_only
    }

    /// The size of a memory block on the device, in bytes, for the given firmware version.
    /// https://github.com/LedgerHQ/ts-libs (packages/devices/src/index.ts, `getBlockSize`)
    pub fn block_size(&self, firmware_version: &str) -> u32 {
        match self {
            DeviceModel::Blue | DeviceModel::NanoX => 4 * 1024,
            DeviceModel::NanoS => {
                let v = SemVer::coerce(firmware_version);
                if v.map(|v| v < SemVer::new(2, 0, 0)).unwrap_or(true) {
                    4 * 1024
                } else {
                    2 * 1024
                }
            }
            DeviceModel::NanoSPlus
            | DeviceModel::Stax
            | DeviceModel::Flex
            | DeviceModel::NanoGen5 => 512,
        }
    }

    /// Whether this device has a touch screen.
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/capabilities/devicesWithTouchScreen.ts
    pub fn has_touch_screen(&self) -> bool {
        matches!(
            self,
            DeviceModel::Stax | DeviceModel::Flex | DeviceModel::NanoGen5
        )
    }

    /// Minimum firmware version from which Ledger Live supports updating the firmware of this
    /// model through USB.
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/isFirmwareUpdateVersionSupported.ts
    pub(crate) fn usb_update_min_version(&self) -> Option<(u64, u64, u64)> {
        match self {
            DeviceModel::Blue => None,
            DeviceModel::NanoS => Some((1, 6, 1)),
            DeviceModel::NanoX => Some((1, 3, 0)),
            DeviceModel::NanoSPlus => Some((1, 0, 0)),
            DeviceModel::Stax => Some((1, 0, 0)),
            DeviceModel::Flex | DeviceModel::NanoGen5 => Some((0, 0, 0)),
        }
    }
}

impl fmt::Display for DeviceModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.product_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_id_to_model() {
        // Target ids from the ledger-live test fixtures and the known model masks.
        assert_eq!(
            DeviceModel::from_target_id(0x3110_0004),
            Some(DeviceModel::NanoS)
        );
        assert_eq!(
            DeviceModel::from_target_id(0x3300_0004),
            Some(DeviceModel::NanoX)
        );
        assert_eq!(
            DeviceModel::from_target_id(0x3310_0004),
            Some(DeviceModel::NanoSPlus)
        );
        assert_eq!(
            DeviceModel::from_target_id(0x3320_0004),
            Some(DeviceModel::Stax)
        );
        assert_eq!(
            DeviceModel::from_target_id(0x3330_0004),
            Some(DeviceModel::Flex)
        );
        assert_eq!(
            DeviceModel::from_target_id(0x3340_0004),
            Some(DeviceModel::NanoGen5)
        );
        assert_eq!(
            DeviceModel::from_target_id(0x3100_0002),
            Some(DeviceModel::Blue)
        );
        assert_eq!(
            DeviceModel::from_target_id(0x3101_0004),
            Some(DeviceModel::Blue)
        );
        // A bootloader (MCU) target id isn't a SE target id.
        assert_eq!(DeviceModel::from_target_id(0x0501_0003), None);
        assert_eq!(DeviceModel::from_target_id(0), None);
    }

    #[test]
    fn usb_product_id_to_model() {
        assert_eq!(
            DeviceModel::from_usb_product_id(0x0001),
            Some(DeviceModel::NanoS)
        );
        assert_eq!(
            DeviceModel::from_usb_product_id(0x1011),
            Some(DeviceModel::NanoS)
        );
        assert_eq!(
            DeviceModel::from_usb_product_id(0x4015),
            Some(DeviceModel::NanoX)
        );
        assert_eq!(
            DeviceModel::from_usb_product_id(0x5011),
            Some(DeviceModel::NanoSPlus)
        );
        assert_eq!(
            DeviceModel::from_usb_product_id(0x6011),
            Some(DeviceModel::Stax)
        );
        assert_eq!(
            DeviceModel::from_usb_product_id(0x7011),
            Some(DeviceModel::Flex)
        );
        assert_eq!(
            DeviceModel::from_usb_product_id(0x8011),
            Some(DeviceModel::NanoGen5)
        );
        assert_eq!(
            DeviceModel::from_usb_product_id(0x0008),
            Some(DeviceModel::NanoGen5)
        );
        assert_eq!(DeviceModel::from_usb_product_id(0xf011), None);
    }

    #[test]
    fn names_and_ids() {
        for model in DeviceModel::ALL {
            assert_eq!(DeviceModel::from_id(model.id()), Some(model));
            assert!(model.product_name().starts_with("Ledger "));
            assert_eq!(model.to_string(), model.product_name());
        }
        assert_eq!(DeviceModel::Flex.id(), "europa");
        assert_eq!(DeviceModel::NanoGen5.id(), "apex");
        assert_eq!(DeviceModel::NanoS.block_size("1.6.1"), 4096);
        assert_eq!(DeviceModel::NanoS.block_size("2.1.0"), 2048);
        assert_eq!(DeviceModel::Stax.block_size("1.0.0"), 512);
    }
}
