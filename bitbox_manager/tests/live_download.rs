//! Download the signed firmwares of a real release from GitHub and check they parse and are
//! detected as the right product. Needs network access, ignored by default. Run with
//! `cargo test -p bitbox_manager -- --ignored`.
//!
//! This doesn't use the GitHub API (rate limited), only the release download URLs.

use bitbox_manager::{
    releases::{download, FirmwareRelease},
    Product, Version,
};

#[test]
#[ignore]
fn live_download_all_products() {
    for (product, asset_name) in [
        (
            Product::BitBox02Multi,
            "firmware-bitbox02-multi.v9.27.1.signed.bin",
        ),
        (
            Product::BitBox02BtcOnly,
            "firmware-bitbox02-btconly.v9.27.1.signed.bin",
        ),
        (
            Product::BitBox02NovaMulti,
            "firmware-bitbox02nova-multi.v9.27.1.signed.bin",
        ),
        (
            Product::BitBox02NovaBtcOnly,
            "firmware-bitbox02nova-btconly.v9.27.1.signed.bin",
        ),
    ] {
        let release = FirmwareRelease {
            product,
            version: Version::new(9, 27, 1),
            url: format!(
                "https://github.com/BitBoxSwiss/bitbox02-firmware/releases/download/firmware%2Fv9.27.1/{}",
                asset_name
            ),
            asset_name: asset_name.to_string(),
            published_sighash: None,
        };
        let firmware = download(&release).unwrap_or_else(|e| panic!("{product}: {e}"));
        assert_eq!(firmware.product(), product);
        println!(
            "{product}: monotonic version {}, hash {}",
            firmware.firmware_version(),
            hex::encode(firmware.published_sighash())
        );
    }
}
