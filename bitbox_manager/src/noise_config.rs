//! Persistence of the noise pairing between this host and BitBox02 devices.
//!
//! The file format (`bitbox.json`) is the same as `PersistedNoiseConfig` of the `bitbox-api`
//! crate (`bitbox-api-rs/src/noise.rs`), so a pairing file can be shared with apps using it.

use std::{fmt, fs, io::Write, path::PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct NoiseConfigData {
    pub app_static_privkey: Option<[u8; 32]>,
    pub device_static_pubkeys: Vec<Vec<u8>>,
}

impl fmt::Debug for NoiseConfigData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the private key.
        f.debug_struct("NoiseConfigData")
            .field(
                "app_static_privkey",
                &self.app_static_privkey.map(|_| "<redacted>"),
            )
            .field("device_static_pubkeys", &self.device_static_pubkeys.len())
            .finish()
    }
}

impl NoiseConfigData {
    pub fn contains_device_static_pubkey(&self, pubkey: &[u8]) -> bool {
        self.device_static_pubkeys.iter().any(|k| k == pubkey)
    }

    pub fn add_device_static_pubkey(&mut self, pubkey: &[u8]) {
        if !self.contains_device_static_pubkey(pubkey) {
            self.device_static_pubkeys.push(pubkey.to_vec());
        }
    }
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pairing config error: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Where to store the pairing information.
pub trait NoiseConfig {
    fn read_config(&self) -> Result<NoiseConfigData, ConfigError> {
        Ok(NoiseConfigData::default())
    }
    fn store_config(&self, _conf: &NoiseConfigData) -> Result<(), ConfigError> {
        Ok(())
    }
}

/// Do not persist the pairing: the pairing code must be confirmed on every connection.
pub struct NoiseConfigNoCache;
impl NoiseConfig for NoiseConfigNoCache {}

/// Persist the pairing in `<dir>/bitbox.json`. The directory is created (0700 on Unix) if
/// needed, and the file is only readable by the user on Unix.
pub struct PersistedNoiseConfig {
    dir: PathBuf,
}

impl PersistedNoiseConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        PersistedNoiseConfig { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("bitbox.json")
    }
}

impl NoiseConfig for PersistedNoiseConfig {
    fn read_config(&self) -> Result<NoiseConfigData, ConfigError> {
        let path = self.path();
        if !path.exists() {
            return Ok(NoiseConfigData::default());
        }
        let contents = fs::read_to_string(&path).map_err(|e| ConfigError(e.to_string()))?;
        serde_json::from_str(&contents).map_err(|e| ConfigError(e.to_string()))
    }

    fn store_config(&self, conf: &NoiseConfigData) -> Result<(), ConfigError> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&self.dir)
            .map_err(|e| ConfigError(e.to_string()))?;

        let data = serde_json::to_string(conf).map_err(|e| ConfigError(e.to_string()))?;
        let path = self.path();
        let mut options = fs::File::options();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&path)
            .map_err(|e| ConfigError(e.to_string()))?;
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| ConfigError(e.to_string()))?;
        file.write_all(data.as_bytes())
            .map_err(|e| ConfigError(e.to_string()))
    }
}

/// The default directory for the pairing file: `<user config dir>/bacca`.
/// (`$XDG_CONFIG_HOME` or `~/.config` on Linux, `~/Library/Application Support` on macOS,
/// `%APPDATA%` on Windows.)
pub fn default_config_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("bacca"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_compat() {
        let dir = std::env::temp_dir().join(format!("bacca-noise-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let config = PersistedNoiseConfig::new(dir.join("sub"));
        assert!(config.read_config().unwrap().app_static_privkey.is_none());
        let mut data = NoiseConfigData {
            app_static_privkey: Some([7; 32]),
            ..Default::default()
        };
        data.add_device_static_pubkey(&[1, 2, 3]);
        data.add_device_static_pubkey(&[1, 2, 3]);
        assert_eq!(data.device_static_pubkeys.len(), 1);
        config.store_config(&data).unwrap();
        let read = config.read_config().unwrap();
        assert_eq!(read.app_static_privkey, Some([7; 32]));
        assert!(read.contains_device_static_pubkey(&[1, 2, 3]));
        #[cfg(unix)]
        {
            let mode = fs::metadata(config.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Same JSON layout as bitbox-api's PersistedNoiseConfig (serde of the same struct).
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(config.path()).unwrap()).unwrap();
        assert_eq!(
            json["device_static_pubkeys"],
            serde_json::json!([[1, 2, 3]])
        );
        assert_eq!(json["app_static_privkey"].as_array().unwrap().len(), 32);
        let _ = fs::remove_dir_all(&dir);
    }
}
