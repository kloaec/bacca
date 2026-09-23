//! Tests against the live Ledger Manager API. They need network access and are ignored by
//! default. Run them with `cargo test -p ledger_manager -- --ignored`.
//!
//! The devices are simulated: we build the GetVersion response a device running an old firmware
//! would send, and check the API gives us sensible update and app information for it.

use ledger_manager::{current_firmware, get_latest_apps, latest_firmware, DeviceInfo, DeviceModel};

/// Encode a GetVersion response (without the status word) for a device in normal mode.
fn get_version_response(target_id: u32, se: &str, mcu: &str, bootloader: &str) -> Vec<u8> {
    let mut data = target_id.to_be_bytes().to_vec();
    for field in [
        se.as_bytes(),
        &[0xee, 0x00, 0x00, 0x00], // flags
        mcu.as_bytes(),
        bootloader.as_bytes(),
        &[0x01], // hardware version
        &[0x00], // language id
    ] {
        data.push(field.len() as u8);
        data.extend_from_slice(field);
    }
    data
}

// (model, target id, SE version, MCU version, bootloader version) of released firmwares, as
// listed by the Manager API.
const DEVICES: &[(DeviceModel, u32, &str, &str, &str)] = &[
    (DeviceModel::NanoS, 0x3110_0004, "2.0.0", "1.12", "0.11"),
    (DeviceModel::NanoX, 0x3300_0004, "2.6.1", "2.39.2", "1.25.2"),
    (
        DeviceModel::NanoSPlus,
        0x3310_0004,
        "1.5.0",
        "4.7.0",
        "3.16.0",
    ),
    (DeviceModel::Stax, 0x3320_0004, "1.9.0", "5.31.1", "4.55.1"),
    (DeviceModel::Flex, 0x3330_0004, "1.5.0", "6.8.1", "5.8.1"),
    (
        DeviceModel::NanoGen5,
        0x3340_0004,
        "1.0.4",
        "8.0.9",
        "7.0.9",
    ),
];

fn devices() -> impl Iterator<Item = (DeviceModel, DeviceInfo)> {
    DEVICES.iter().map(|&(model, target_id, se, mcu, bl)| {
        let resp = get_version_response(target_id, se, mcu, bl);
        (model, DeviceInfo::from_get_version_response(&resp).unwrap())
    })
}

#[test]
#[ignore]
fn live_firmware_updates() {
    for (model, info) in devices() {
        assert_eq!(info.model(), Some(model));
        let current = current_firmware(&info).unwrap();
        assert_eq!(current.name, info.version);
        let update = latest_firmware(&info)
            .unwrap()
            .unwrap_or_else(|| panic!("{model}: an update from {} exists", info.version));
        assert_ne!(update.version(), info.version, "{model}");
        println!(
            "{model}: {} -> {} (osu {}, flash mcu: {})",
            info.version,
            update.version(),
            update.osu.name,
            update.should_flash_mcu
        );
    }
}

#[test]
#[ignore]
fn live_bitcoin_apps() {
    for (model, info) in devices() {
        let (bitcoin, test) = get_latest_apps(&info).unwrap();
        let bitcoin = bitcoin.unwrap_or_else(|| panic!("{model}: Bitcoin app in catalog"));
        assert!(test.is_some(), "{model}: Bitcoin Test app in catalog");
        let by_hash =
            ledger_manager::bitcoin_apps_by_hashes(vec![hex::decode(&bitcoin.hash).unwrap()])
                .unwrap();
        assert_eq!(
            by_hash[0].as_ref().map(|a| &a.version),
            Some(&bitcoin.version)
        );
        println!("{model}: Bitcoin app {}", bitcoin.version);
    }
}

#[test]
#[ignore]
fn live_mcus() {
    // At the time of writing the API lists 169 MCU versions, which all parse.
    let mcus = ledger_manager::fetch_mcus().unwrap();
    assert!(mcus.len() > 100, "{} MCU versions", mcus.len());
}
