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

#[test]
#[ignore]
fn live_language_packs() {
    for (model, info) in devices() {
        let packs = ledger_manager::language_packages_for_device(&info).unwrap();
        let supported =
            ledger_manager::device::is_device_localization_supported(&info.version, Some(model));
        let languages: Vec<&str> = packs.iter().map(|p| p.language.as_str()).collect();
        println!("{model} {}: {:?}", info.version, languages);
        if supported {
            // Not all the languages are available for all the firmwares (at the time of writing
            // there is no French pack for the Nano S Plus 1.5.0).
            assert!(languages.len() > 2, "{model}: language packs");
            assert!(packs
                .iter()
                .all(|p| p.apdu_install_url.starts_with("https://")));
        } else {
            assert!(packs.is_empty(), "{model}: no language pack");
        }
    }
}

#[test]
#[ignore]
fn live_apps_catalog() {
    for (model, info) in devices() {
        let catalog = ledger_manager::apps_catalog(&info).unwrap();
        assert!(catalog.len() > 20, "{model}: {} apps", catalog.len());
        assert!(catalog.iter().any(|a| a.version_name == "Bitcoin"));
        // The dependencies are in the catalog.
        for app in &catalog {
            if let Some(parent) = app.parent_name.as_deref().filter(|p| !p.is_empty()) {
                assert!(
                    catalog.iter().any(|a| a.version_name == parent),
                    "{model}: {} depends on {parent}",
                    app.version_name
                );
            }
        }
        println!("{model}: {} apps in the catalog", catalog.len());
    }
}

#[test]
#[ignore]
fn live_bitcoin_apps_have_no_dependency() {
    // After a firmware update only the Bitcoin apps are reinstalled: check they don't depend on
    // another app (`parentName`), so the dependencies don't need to be resolved.
    for (model, info) in devices() {
        let url = format!(
            "https://manager.api.live.ledger.com/api/v2/apps/by-target?livecommonversion=38.0.0&provider=1&target_id={}&firmware_version_name={}",
            info.target_id, info.version
        );
        let catalog: Vec<serde_json::Value> = minreq::get(url).send().unwrap().json().unwrap();
        assert!(catalog.len() > 20, "{model}: {} apps", catalog.len());
        for name in ["Bitcoin", "Bitcoin Test"] {
            let app = catalog
                .iter()
                .find(|a| a["versionName"] == name)
                .unwrap_or_else(|| panic!("{model}: {name} in the catalog"));
            let parent = app["parentName"].as_str().unwrap_or_default();
            assert!(parent.is_empty(), "{model}: {name} depends on {parent}");
        }
    }
}
