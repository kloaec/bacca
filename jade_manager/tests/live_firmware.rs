//! Fetch the firmware index of every supported model from the live firmware server, and download
//! one firmware. Needs network access, ignored by default. Run with
//! `cargo test -p jade_manager -- --ignored`.

use jade_manager::releases::{download, latest_release};

#[test]
#[ignore]
fn live_index_and_download() {
    let mut releases = Vec::new();
    for hw_target in ["jade", "jade1.1", "jade2.0", "jade2.0c"] {
        for config in ["ble", "noradio"] {
            let release = latest_release(hw_target, config).unwrap();
            println!(
                "{} {}: v{} {} fwhash {}",
                hw_target,
                config,
                release.version,
                release.url,
                hex::encode(release.fwhash)
            );
            assert_eq!(release.config, config);
            let prefix = format!(
                "https://jadefw.blockstream.com/bin/{}/{}_{}_",
                hw_target, release.version, config
            );
            assert!(release.url.starts_with(&prefix), "{}", release.url);
            releases.push(release);
        }
    }
    // The smallest one. Downloading checks the hash from the index.
    let release = releases.iter().min_by_key(|r| r.fwsize).unwrap();
    let firmware = download(release).unwrap();
    println!("Downloaded {} ({} bytes)", release.url, firmware.len());
    assert!(!firmware.is_empty() && firmware.len() < release.fwsize as usize);
}
