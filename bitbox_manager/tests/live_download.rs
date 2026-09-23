//! Download the signed firmwares of a real release from GitHub and check they parse and are
//! detected as the right product. Needs network access, ignored by default. Run with
//! `cargo test -p bitbox_manager -- --ignored`.
//!
//! This doesn't use the GitHub API (rate limited), only the release download URLs.

use bitbox_manager::{
    releases::{asset_version, download, FirmwareRelease},
    Product, Version,
};

const TAG: &str = "firmware/v9.27.1";

#[test]
#[ignore]
fn live_download_all_products() {
    let version = Version::parse("9.27.1").unwrap();
    for product in [
        Product::BitBox02Multi,
        Product::BitBox02BtcOnly,
        Product::BitBox02NovaMulti,
        Product::BitBox02NovaBtcOnly,
    ] {
        // The current naming scheme is the first prefix.
        let prefix = product.release_asset_prefixes()[0];
        let asset_name = format!("{}.v{}.signed.bin", prefix, version);
        assert_eq!(asset_version(product, &asset_name), Some(version));
        let release = FirmwareRelease {
            product,
            version,
            tag: TAG.to_string(),
            url: format!(
                "https://github.com/BitBoxSwiss/bitbox02-firmware/releases/download/{}/{}",
                TAG.replace('/', "%2F"),
                asset_name
            ),
            asset_name,
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
