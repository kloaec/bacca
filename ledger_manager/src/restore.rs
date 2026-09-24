//! Backing up the settings of the device before a firmware update, and restoring them after.
//!
//! A firmware update uninstalls all the apps, and may reset the language and the custom lock
//! screen picture of the device. Before the update we back up:
//! - which of the Bitcoin and Bitcoin Test apps are installed (the other apps are not
//!   reinstalled: this is a Bitcoin-only tool);
//! - the language of the device;
//! - the custom lock screen picture (Stax, Flex, Nano Gen5);
//!
//! and after the update we restore, in this order: the language (by installing the language pack
//! for the new firmware), the lock screen picture, and the Bitcoin apps (their latest version for
//! the new firmware). This follows Ledger Live's
//! https://github.com/LedgerHQ/ledger-live/blob/develop/apps/ledger-live-mobile/src/screens/FirmwareUpdate/useUpdateFirmwareAndRestoreSettings.ts
//! and the desktop firmware update modal
//! (https://github.com/LedgerHQ/ledger-live/tree/develop/apps/ledger-live-desktop/src/renderer/modals/UpdateFirmwareModal).
//!
//! Each part is independent: a failure of one does not prevent the others. As in Ledger Live, a
//! failure to back up something never prevents the firmware update. The data stored inside the
//! apps (for instance the wallet policies registered in the Bitcoin app) is lost.
//!
//! Unlike Ledger Live, the backup is saved to a file before starting the update, so it can still
//! be restored (`restore_device_settings`) if the update gets interrupted.

use crate::{
    api::{catalog_apps, FirmwareUpdateInfo},
    apps::{
        find_app, install_app, list_installed_apps_raw, AppInstallStep, BITCOIN_APPS,
        MANAGER_INSTALL_DELAY,
    },
    device::{connect, is_device_localization_supported, quit_app, DeviceInfo},
    error::Error,
    firmware::{check_firmware_update_supported, update_firmware, FirmwareUpdateStep},
    language::{
        install_language, language_display_name, language_name, LanguageInstallStep,
        ENGLISH_LANGUAGE_ID,
    },
    lock_screen::{check_image, fetch_image, fetch_image_hash, load_image, LoadImageStep},
};

use ledger_transport_hidapi::{hidapi::HidApi, TransportNativeHID};
use serde_derive::{Deserialize, Serialize};

use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The version of the format of the backup files.
const BACKUP_FORMAT_VERSION: u32 = 1;
const BACKUP_FILE_PREFIX: &str = "ledger-backup-";
/// When resuming an interrupted update (device in updater mode), the most recent backup of the
/// device is used if it is not older than this.
const RESUME_BACKUP_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// (De)serialize bytes as a hex string.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        hex::decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// An app installed on the device when it was backed up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackedUpApp {
    /// "Bitcoin" or "Bitcoin Test".
    pub name: String,
}

/// The backup of the custom lock screen picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LockScreenBackup {
    /// The model doesn't support a custom lock screen picture.
    NotSupported,
    /// No custom picture was set.
    NotSet,
    /// The picture, in the format of the device, and its hash as computed by the device.
    Saved {
        #[serde(with = "hex_bytes")]
        image: Vec<u8>,
        hash: String,
    },
    /// The user refused the backup on the device.
    Refused,
    Failed {
        error: String,
    },
}

/// The settings of a device backed up before a firmware update, saved as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceBackup {
    /// `BACKUP_FORMAT_VERSION`.
    pub format_version: u32,
    /// When the backup was made (seconds since the Unix epoch).
    pub created_at: u64,
    pub target_id: u32,
    /// Ledger Live's id of the model, e.g. "stax".
    #[serde(default)]
    pub model: Option<String>,
    /// The firmware version of the device when it was backed up.
    pub firmware_version: String,
    /// The installed Bitcoin apps.
    #[serde(default)]
    pub apps: Vec<BackedUpApp>,
    /// Set if the installed apps could not be listed (then `apps` is empty).
    #[serde(default)]
    pub apps_error: Option<String>,
    /// `None` if the firmware doesn't support changing the language.
    #[serde(default)]
    pub language_id: Option<u8>,
    pub lock_screen: LockScreenBackup,
}

impl DeviceBackup {
    /// What was backed up, for humans, one line per item.
    pub fn summary(&self) -> Vec<String> {
        let names: Vec<&str> = self.apps.iter().map(|a| a.name.as_str()).collect();
        vec![
            match (&self.apps_error, names.is_empty()) {
                (Some(e), _) => format!("Apps: could not be listed ({})", e),
                (None, true) => "Apps: no Bitcoin app installed".to_string(),
                (None, false) => format!("Apps: {}", names.join(", ")),
            },
            match self.language_id {
                None => "Language: not supported by the firmware".to_string(),
                Some(id) => format!("Language: {}", language_display_name(id)),
            },
            match &self.lock_screen {
                LockScreenBackup::NotSupported => "Lock screen picture: not supported".to_string(),
                LockScreenBackup::NotSet => "Lock screen picture: none set".to_string(),
                LockScreenBackup::Saved { image, .. } => {
                    format!("Lock screen picture: saved ({} bytes)", image.len())
                }
                LockScreenBackup::Refused => {
                    "Lock screen picture: NOT saved (refused on the device)".to_string()
                }
                LockScreenBackup::Failed { error } => {
                    format!("Lock screen picture: NOT saved ({})", error)
                }
            },
        ]
    }
}

/// A step of the backup.
#[derive(Debug, Clone, PartialEq)]
pub enum BackupStep {
    /// Listing the installed apps. The user may have to allow the Ledger manager on the device.
    ListingApps,
    /// Backing up the lock screen picture. The user may have to approve it on the device.
    FetchingLockScreen,
    /// `progress` is between 0 and 1.
    FetchingLockScreenProgress {
        progress: f32,
    },
    Done,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Back up the settings of the device, which must run its firmware normally and be on its
/// dashboard. A failure (or refusal) to back up the apps or the picture doesn't make the backup
/// fail: it is recorded in the backup.
fn backup_device_settings(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    mut progress: impl FnMut(BackupStep),
) -> Result<DeviceBackup, Error> {
    device_info.check_normal_mode()?;
    log::info!("Backing up the device settings.");

    progress(BackupStep::ListingApps);
    let (apps, apps_error) = match list_installed_apps_raw(transport) {
        Ok(mut listed) => {
            // Like Ledger Live (apps/listApps.ts), ignore what isn't really an app (sideloaded
            // apps, language packs).
            listed.retain(|a| a.hash_code_data.iter().any(|b| *b != 0));
            let apps = BITCOIN_APPS
                .iter()
                .filter(|name| find_app(&listed, name).is_some())
                .map(|name| BackedUpApp {
                    name: name.to_string(),
                })
                .collect();
            (apps, None)
        }
        Err(e) => {
            log::warn!("Could not list the installed apps: {}", e);
            (Vec::new(), Some(e.to_string()))
        }
    };

    // Like Ledger Live Desktop, only on a set up device.
    let model = device_info.model;
    let lock_screen = match model.filter(|m| m.has_touch_screen()) {
        None => LockScreenBackup::NotSupported,
        Some(_) if !device_info.onboarded => LockScreenBackup::NotSet,
        Some(model) => {
            progress(BackupStep::FetchingLockScreen);
            let fetched = fetch_image(transport, |p| {
                progress(BackupStep::FetchingLockScreenProgress { progress: p })
            });
            match fetched {
                Ok(None) => LockScreenBackup::NotSet,
                Ok(Some(image)) => {
                    if let Err(e) = check_image(&image.data, model) {
                        log::warn!("The lock screen picture has an unexpected format: {}", e);
                    }
                    LockScreenBackup::Saved {
                        image: image.data,
                        hash: image.hash,
                    }
                }
                Err(Error::RefusedOnDevice(_)) => LockScreenBackup::Refused,
                Err(e) => {
                    log::warn!("Could not back up the lock screen picture: {}", e);
                    LockScreenBackup::Failed {
                        error: e.to_string(),
                    }
                }
            }
        }
    };

    let backup = DeviceBackup {
        format_version: BACKUP_FORMAT_VERSION,
        created_at: now(),
        target_id: device_info.target_id,
        model: model.map(|m| m.id().to_string()),
        firmware_version: device_info.version.clone(),
        apps,
        apps_error,
        language_id: device_info.language_id,
        lock_screen,
    };
    log::info!("Backup: {}", backup.summary().join("; "));
    progress(BackupStep::Done);
    Ok(backup)
}

/// The default directory for the backup files: `<user config dir>/bacca` (`$XDG_CONFIG_HOME` or
/// `~/.config` on Linux, `~/Library/Application Support` on macOS, `%APPDATA%` on Windows).
pub fn default_backup_dir() -> Option<PathBuf> {
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

/// Format a Unix timestamp as a UTC date and time, e.g. "20260924T110203Z".
fn format_timestamp(secs: u64) -> String {
    // From Howard Hinnant's `civil_from_days`.
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Save the backup in this directory (created if needed), in a new file named after the device
/// and the date of the backup, e.g. "ledger-backup-stax-33200004-20260924T110203Z.json". Returns
/// the path of the file.
///
/// The backup is written to a temporary file which is then renamed, so a backup file is always
/// complete. On Unix it is only readable by the user.
fn save_backup(backup: &DeviceBackup, dir: &Path) -> Result<PathBuf, Error> {
    let err = |path: &Path, e: &dyn fmt::Display| {
        Error::BackupNotSaved(format!("{}: {}", path.display(), e))
    };
    fs::create_dir_all(dir).map_err(|e| err(dir, &e))?;
    let path = dir.join(format!(
        "{}{}-{:08x}-{}.json",
        BACKUP_FILE_PREFIX,
        backup.model.as_deref().unwrap_or("unknown"),
        backup.target_id,
        format_timestamp(backup.created_at)
    ));
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(backup).map_err(|e| err(&path, &e))?;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write = || -> std::io::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(err(&path, &e));
    }
    // Check it can be read back.
    if load_backup(&path).map_err(|e| err(&path, &e))? != *backup {
        return Err(err(&path, &"the file doesn't contain the backup"));
    }
    log::info!("Device backup saved to {}.", path.display());
    Ok(path)
}

/// Load a backup saved with `save_backup`.
pub fn load_backup(path: &Path) -> Result<DeviceBackup, Error> {
    let invalid = |e: &dyn fmt::Display| Error::InvalidBackup(format!("{}: {}", path.display(), e));
    let backup: DeviceBackup = serde_json::from_slice(&fs::read(path)?).map_err(|e| invalid(&e))?;
    if backup.format_version > BACKUP_FORMAT_VERSION {
        return Err(invalid(&format!(
            "unsupported format version {}",
            backup.format_version
        )));
    }
    Ok(backup)
}

/// Find the most recent backup of a device with this target id in this directory, made at most
/// `max_age` ago.
pub fn find_latest_backup(
    dir: &Path,
    target_id: u32,
    max_age: Duration,
) -> Option<(PathBuf, DeviceBackup)> {
    let now = now();
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(BACKUP_FILE_PREFIX) && n.ends_with(".json"))
        })
        .filter_map(|p| load_backup(&p).ok().map(|b| (p, b)))
        .filter(|(_, b)| {
            b.target_id == target_id && now.saturating_sub(b.created_at) <= max_age.as_secs()
        })
        .max_by_key(|(_, b)| b.created_at)
}

/// The outcome of the restoration of a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored,
    /// There was nothing to restore, with the reason.
    Skipped(String),
    /// It could not be restored, with the reason.
    Failed(String),
}

impl fmt::Display for RestoreOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreOutcome::Restored => write!(f, "restored"),
            RestoreOutcome::Skipped(r) => write!(f, "skipped ({})", r),
            RestoreOutcome::Failed(r) => write!(f, "FAILED ({})", r),
        }
    }
}

/// What was restored after a firmware update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub language: RestoreOutcome,
    pub lock_screen: RestoreOutcome,
    /// The outcome for each app to reinstall, by name.
    pub apps: Vec<(String, RestoreOutcome)>,
    /// Set if the apps could not be reinstalled at all (or not listed before the update).
    pub apps_error: Option<String>,
}

impl RestoreReport {
    /// The names of the apps which were reinstalled.
    pub fn reinstalled_apps(&self) -> Vec<&str> {
        self.apps
            .iter()
            .filter(|(_, o)| *o == RestoreOutcome::Restored)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Whether everything was restored (nothing failed).
    pub fn is_complete(&self) -> bool {
        let failed = |o: &RestoreOutcome| matches!(o, RestoreOutcome::Failed(_));
        self.apps_error.is_none()
            && !failed(&self.language)
            && !failed(&self.lock_screen)
            && !self.apps.iter().any(|(_, o)| failed(o))
    }

    /// The report for humans, one line per item.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Language: {}", self.language),
            format!("Lock screen picture: {}", self.lock_screen),
        ];
        match &self.apps_error {
            Some(e) => lines.push(format!("Apps: FAILED ({})", e)),
            None if self.apps.is_empty() => lines.push("Apps: none to reinstall".to_string()),
            None => {}
        }
        for (name, outcome) in &self.apps {
            lines.push(format!("App {}: {}", name, outcome));
        }
        lines
    }
}

/// A step of the restoration.
#[derive(Debug, Clone, PartialEq)]
pub enum RestoreStep {
    /// Installing the language pack of this language (for humans, e.g. "French").
    InstallingLanguage {
        language: String,
    },
    /// A step of the language pack installation (the user must allow it on the device).
    Language(LanguageInstallStep),
    RestoringLockScreen,
    /// A step of the lock screen picture loading (the user must approve it on the device).
    LockScreen(LoadImageStep),
    /// Listing the installed apps. The user may have to allow the Ledger manager on the device.
    ListingApps,
    /// Installing this app, the `index`-th (starting at 1) out of `total`.
    InstallingApp {
        name: String,
        index: usize,
        total: usize,
    },
    App(AppInstallStep),
    Done,
}

fn restore_language(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    backup: &DeviceBackup,
    progress: &mut impl FnMut(RestoreStep),
) -> RestoreOutcome {
    let id = match backup.language_id {
        None => {
            return RestoreOutcome::Skipped(
                "the previous firmware did not support changing the language".into(),
            )
        }
        Some(ENGLISH_LANGUAGE_ID) => {
            return RestoreOutcome::Skipped("the device language was English".into())
        }
        Some(id) => id,
    };
    let Some(name) = language_name(id) else {
        return RestoreOutcome::Failed(format!("unknown language id {:#04x}", id));
    };
    if !is_device_localization_supported(&device_info.version, device_info.model) {
        return RestoreOutcome::Failed(
            "the new firmware doesn't support changing the language".into(),
        );
    }
    progress(RestoreStep::InstallingLanguage {
        language: language_display_name(id),
    });
    match install_language(transport, device_info, name, |s| {
        progress(RestoreStep::Language(s))
    }) {
        Ok(()) => RestoreOutcome::Restored,
        Err(e) => {
            log::warn!("Could not restore the language: {}", e);
            RestoreOutcome::Failed(e.to_string())
        }
    }
}

fn restore_lock_screen(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    backup: &DeviceBackup,
    progress: &mut impl FnMut(RestoreStep),
) -> RestoreOutcome {
    let (image, hash) = match &backup.lock_screen {
        LockScreenBackup::NotSupported => {
            return RestoreOutcome::Skipped("not supported by this device".into())
        }
        LockScreenBackup::NotSet => {
            return RestoreOutcome::Skipped("no custom picture was set".into())
        }
        LockScreenBackup::Refused => {
            return RestoreOutcome::Failed(
                "it was not backed up (refused on the device), set it again from Ledger Live"
                    .into(),
            )
        }
        LockScreenBackup::Failed { error } => {
            return RestoreOutcome::Failed(format!("it could not be backed up: {}", error))
        }
        LockScreenBackup::Saved { image, hash } => (image, hash),
    };
    let Some(model) = device_info.model.filter(|m| m.has_touch_screen()) else {
        return RestoreOutcome::Failed("not supported by this device".into());
    };
    if let Err(e) = check_image(image, model) {
        return RestoreOutcome::Failed(e.to_string());
    }
    // Don't ask the user to approve loading the picture on the device if it is still there.
    match fetch_image_hash(transport) {
        Ok(Some(current)) if current.eq_ignore_ascii_case(hash) => {
            return RestoreOutcome::Skipped("the picture is still set on the device".into())
        }
        Ok(_) => {}
        Err(e) => log::debug!("Could not query the lock screen picture hash: {}", e),
    }
    progress(RestoreStep::RestoringLockScreen);
    match load_image(transport, image, |s| progress(RestoreStep::LockScreen(s))) {
        Ok(new_hash) => {
            if let Some(h) = new_hash.filter(|h| !h.eq_ignore_ascii_case(hash)) {
                log::warn!(
                    "The hash of the restored lock screen picture ({}) differs from the backed up one ({}).",
                    h,
                    hash
                );
            }
            RestoreOutcome::Restored
        }
        Err(e) => {
            log::warn!("Could not restore the lock screen picture: {}", e);
            RestoreOutcome::Failed(e.to_string())
        }
    }
}

/// Reinstall the Bitcoin apps of the backup (their latest version for the new firmware). They
/// don't depend on any other app.
fn restore_apps(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    backup: &DeviceBackup,
    progress: &mut impl FnMut(RestoreStep),
) -> (Vec<(String, RestoreOutcome)>, Option<String>) {
    if let Some(e) = &backup.apps_error {
        let reason = format!("the apps could not be listed before the update: {}", e);
        return (Vec::new(), Some(reason));
    }
    let names: Vec<&str> = BITCOIN_APPS
        .iter()
        .copied()
        .filter(|name| backup.apps.iter().any(|a| a.name == *name))
        .collect();
    if names.is_empty() {
        return (Vec::new(), None);
    }
    let fail_all = |reason: String| {
        let outcomes = names
            .iter()
            .map(|n| (n.to_string(), RestoreOutcome::Failed(reason.clone())))
            .collect();
        (outcomes, Some(reason))
    };

    progress(RestoreStep::ListingApps);
    let installed = match list_installed_apps_raw(transport) {
        Ok(apps) => apps,
        Err(e) => return fail_all(format!("could not list the installed apps: {}", e)),
    };
    let catalog = match catalog_apps(device_info, BITCOIN_APPS) {
        Ok(c) => c,
        Err(e) => return fail_all(format!("could not get the catalog of apps: {}", e)),
    };

    let mut skipped = Vec::new();
    let mut queue = Vec::new();
    for name in names {
        if find_app(&installed, name).is_some() {
            skipped.push((
                name.to_string(),
                RestoreOutcome::Skipped("already installed".into()),
            ));
        } else if let Some(app) = catalog.iter().find(|a| a.version_name == name) {
            queue.push(app);
        } else {
            let reason = "not available for the new firmware in the Ledger catalog";
            skipped.push((name.to_string(), RestoreOutcome::Failed(reason.into())));
        }
    }
    let mut outcomes = Vec::new();
    for (i, app) in queue.iter().enumerate() {
        if i > 0 {
            thread::sleep(MANAGER_INSTALL_DELAY);
        }
        progress(RestoreStep::InstallingApp {
            name: app.version_name.clone(),
            index: i + 1,
            total: queue.len(),
        });
        let res = install_app(transport, device_info.target_id, app, |s| {
            progress(RestoreStep::App(s))
        });
        let outcome = match res {
            Ok(()) | Err(Error::AppAlreadyInstalled) => RestoreOutcome::Restored,
            Err(e) => {
                log::warn!("Could not reinstall {}: {}", app.version_name, e);
                RestoreOutcome::Failed(e.to_string())
            }
        };
        outcomes.push((app.version_name.clone(), outcome));
    }
    outcomes.extend(skipped);
    (outcomes, None)
}

/// Restore the settings of a device from a backup made before a firmware update: the language
/// (installing the language pack for the new firmware, if it wasn't English), the custom lock
/// screen picture, and the Bitcoin apps. The user will have to approve the installation of the
/// language, the loading of the picture, and to allow the Ledger manager on the device.
///
/// Each part is independent, the report tells what was restored. An error is returned only if the
/// device can't be used (not on its dashboard, locked...) or if the backup is of another model.
pub fn restore_device_settings(
    transport: &TransportNativeHID,
    backup: &DeviceBackup,
    mut progress: impl FnMut(RestoreStep),
) -> Result<RestoreReport, Error> {
    quit_app(transport)?;
    let device_info = DeviceInfo::new(transport)?;
    device_info.check_normal_mode()?;
    if device_info.target_id != backup.target_id {
        return Err(Error::InvalidBackup(format!(
            "the backup is for another device (target id {:#010x}, the device's is {:#010x})",
            backup.target_id, device_info.target_id
        )));
    }
    log::info!(
        "Restoring the device settings (backed up on firmware {}, now {}).",
        backup.firmware_version,
        device_info.version
    );

    // The order of Ledger Live: language, lock screen picture, apps.
    let language = restore_language(transport, &device_info, backup, &mut progress);
    let lock_screen = restore_lock_screen(transport, &device_info, backup, &mut progress);
    let (apps, apps_error) = restore_apps(transport, &device_info, backup, &mut progress);
    let report = RestoreReport {
        language,
        lock_screen,
        apps,
        apps_error,
    };
    log::info!("Restore report:\n{}", report.lines().join("\n"));
    progress(RestoreStep::Done);
    Ok(report)
}

/// A step of `update_firmware_and_restore`.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateAndRestoreStep {
    Backup(BackupStep),
    /// The backup was saved to this file.
    BackupSaved {
        path: PathBuf,
    },
    /// The device is in updater mode (an update was interrupted): this backup, made before the
    /// interrupted update, will be restored.
    BackupLoaded {
        path: PathBuf,
    },
    /// The settings could not be backed up (the update continues), with the reason.
    BackupSkipped {
        reason: String,
    },
    Firmware(FirmwareUpdateStep),
    Restore(RestoreStep),
}

/// The result of `update_firmware_and_restore`.
#[derive(Debug, Clone)]
pub struct UpdateAndRestoreResult {
    /// The information of the device after the update (before the restoration).
    pub device_info: DeviceInfo,
    /// The file the backup was saved to (or loaded from), if any.
    pub backup_path: Option<PathBuf>,
    /// What was restored, if there was a backup and the device could be used.
    pub report: Option<RestoreReport>,
    /// Set if the restoration could not be performed at all.
    pub restore_error: Option<String>,
}

/// Back up the settings of the device, update its firmware and restore the settings.
///
/// With a `backup_dir`, the backup is saved to a file in it before the update starts. If the file
/// can't be saved the update is not started and `Error::BackupNotSaved` is returned: the caller
/// may retry without `backup_dir` (the backup is then only kept in memory). A failure to back up
/// (part of) the settings otherwise never prevents the update.
///
/// If the device is in updater mode (a previous update was interrupted), the most recent backup of
/// the device in `backup_dir` (not older than a week) is restored after the update.
///
/// Returns an error if the update fails. The backup file is kept, and can be restored with
/// `load_backup` and `restore_device_settings` once the update is completed.
pub fn update_firmware_and_restore(
    hid_api: &mut HidApi,
    update: &FirmwareUpdateInfo,
    backup_dir: Option<&Path>,
    mut progress: impl FnMut(UpdateAndRestoreStep),
) -> Result<UpdateAndRestoreResult, Error> {
    progress(UpdateAndRestoreStep::Firmware(
        FirmwareUpdateStep::Preparing,
    ));
    let skipped = |reason: String| {
        log::warn!("The device settings are not backed up: {}", reason);
        UpdateAndRestoreStep::BackupSkipped { reason }
    };
    let (backup, backup_path) = {
        let transport = connect(hid_api)?;
        quit_app(&transport)?;
        let device_info = DeviceInfo::new(&transport)?;
        // Don't bother the user with the backup if the update can't be performed.
        check_firmware_update_supported(&device_info)?;

        if device_info.is_osu {
            let found = backup_dir.and_then(|dir| {
                find_latest_backup(dir, device_info.target_id, RESUME_BACKUP_MAX_AGE)
            });
            match (found, backup_dir) {
                (Some((path, backup)), _) => {
                    log::info!("Resuming the update, using the backup {}.", path.display());
                    progress(UpdateAndRestoreStep::BackupLoaded { path: path.clone() });
                    (Some(backup), Some(path))
                }
                (None, Some(_)) => {
                    progress(skipped(
                        "the device is in updater mode and no recent backup of it was found".into(),
                    ));
                    (None, None)
                }
                (None, None) => {
                    progress(skipped(
                        "the device is in updater mode, its settings can't be backed up".into(),
                    ));
                    (None, None)
                }
            }
        } else {
            match backup_device_settings(&transport, &device_info, |s| {
                progress(UpdateAndRestoreStep::Backup(s))
            }) {
                Ok(backup) => {
                    let path = match backup_dir {
                        // Never start the update if the backup can't be saved.
                        Some(dir) => {
                            let path = save_backup(&backup, dir)?;
                            progress(UpdateAndRestoreStep::BackupSaved { path: path.clone() });
                            Some(path)
                        }
                        None => None,
                    };
                    (Some(backup), path)
                }
                Err(e) => {
                    progress(skipped(e.to_string()));
                    (None, None)
                }
            }
        }
        // The transport is dropped here: the update opens its own connections to the device.
    };

    let device_info = update_firmware(hid_api, update, |s| {
        progress(UpdateAndRestoreStep::Firmware(s))
    })?;

    let (report, restore_error) = match &backup {
        None => (None, None),
        Some(backup) => {
            // The device just came back on its new firmware: give it some time.
            thread::sleep(Duration::from_secs(1));
            match connect(hid_api).and_then(|t| {
                restore_device_settings(&t, backup, |s| progress(UpdateAndRestoreStep::Restore(s)))
            }) {
                Ok(report) => (Some(report), None),
                Err(e) => {
                    log::error!("Could not restore the device settings: {}", e);
                    (None, Some(e.to_string()))
                }
            }
        }
    };
    Ok(UpdateAndRestoreResult {
        device_info,
        backup_path,
        report,
        restore_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceModel;

    fn sample_backup() -> DeviceBackup {
        DeviceBackup {
            format_version: BACKUP_FORMAT_VERSION,
            created_at: 1_790_247_723,
            target_id: 0x3320_0004,
            model: Some("stax".into()),
            firmware_version: "1.9.1".into(),
            apps: vec![
                BackedUpApp {
                    name: "Bitcoin".into(),
                },
                BackedUpApp {
                    name: "Bitcoin Test".into(),
                },
            ],
            apps_error: None,
            language_id: Some(1),
            lock_screen: LockScreenBackup::Saved {
                image: crate::lock_screen::tests::test_image(DeviceModel::Stax),
                hash: "0102".into(),
            },
        }
    }

    #[test]
    fn serialization_roundtrip() {
        let backup = sample_backup();
        let json = serde_json::to_string_pretty(&backup).unwrap();
        assert_eq!(serde_json::from_str::<DeviceBackup>(&json).unwrap(), backup);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["lock_screen"]["status"], "saved");
        assert!(value["lock_screen"]["image"]
            .as_str()
            .unwrap()
            .starts_with("9001a002"));
        assert_eq!(value["apps"][0], serde_json::json!({"name": "Bitcoin"}));

        for lock_screen in [
            LockScreenBackup::NotSupported,
            LockScreenBackup::NotSet,
            LockScreenBackup::Refused,
            LockScreenBackup::Failed {
                error: "oops".into(),
            },
        ] {
            let b = DeviceBackup {
                lock_screen,
                language_id: None,
                apps: vec![],
                apps_error: Some("refused".into()),
                ..sample_backup()
            };
            let json = serde_json::to_string(&b).unwrap();
            assert_eq!(serde_json::from_str::<DeviceBackup>(&json).unwrap(), b);
        }
        let v = serde_json::to_value(LockScreenBackup::NotSupported).unwrap();
        assert_eq!(v, serde_json::json!({"status": "not_supported"}));

        // Minimal file, and apps with the fields of the first version of the format.
        let b: DeviceBackup = serde_json::from_value(serde_json::json!({
            "format_version": 1,
            "created_at": 0,
            "target_id": 0x33000004u32,
            "firmware_version": "2.2.3",
            "apps": [{"name": "Bitcoin", "version": "2.4.5", "hash": "aa"}],
            "lock_screen": {"status": "not_supported"},
        }))
        .unwrap();
        assert_eq!(b.apps[0].name, "Bitcoin");
        assert_eq!(b.model, None);
        // Invalid image hex.
        assert!(
            serde_json::from_value::<LockScreenBackup>(serde_json::json!({
                "status": "saved", "image": "zz", "hash": ""
            }))
            .is_err()
        );
    }

    #[test]
    fn backup_files() {
        let dir = std::env::temp_dir().join(format!("bacca-backup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sub = dir.join("sub");
        let mut backup = sample_backup();
        backup.created_at = now();
        let path = save_backup(&backup, &sub).unwrap();
        assert!(path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("ledger-backup-stax-33200004-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(load_backup(&path).unwrap(), backup);

        // The latest recent backup of this device is found.
        let mut older = backup.clone();
        older.created_at -= 3600;
        older.apps.clear();
        save_backup(&older, &sub).unwrap();
        let mut other = backup.clone();
        other.target_id = 0x3330_0004;
        other.created_at += 10;
        save_backup(&other, &sub).unwrap();
        let found = find_latest_backup(&sub, 0x3320_0004, RESUME_BACKUP_MAX_AGE);
        assert_eq!(found, Some((path, backup.clone())));
        assert!(find_latest_backup(&sub, 0x3310_0004, RESUME_BACKUP_MAX_AGE).is_none());
        assert!(
            find_latest_backup(&dir.join("nothing"), 0x3320_0004, RESUME_BACKUP_MAX_AGE).is_none()
        );
        // Too old.
        let mut old = backup.clone();
        old.target_id = 0x3340_0004;
        old.created_at -= RESUME_BACKUP_MAX_AGE.as_secs() + 10;
        save_backup(&old, &sub).unwrap();
        assert!(find_latest_backup(&sub, 0x3340_0004, RESUME_BACKUP_MAX_AGE).is_none());

        // Unsupported format, invalid file.
        let mut future = backup.clone();
        future.format_version = BACKUP_FORMAT_VERSION + 1;
        let f = dir.join("future.json");
        fs::write(&f, serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(matches!(load_backup(&f), Err(Error::InvalidBackup(_))));
        fs::write(&f, b"{").unwrap();
        assert!(matches!(load_backup(&f), Err(Error::InvalidBackup(_))));

        // A directory which can't be created.
        let file = dir.join("a-file");
        fs::write(&file, b"").unwrap();
        assert!(matches!(
            save_backup(&backup, &file.join("sub")),
            Err(Error::BackupNotSaved(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn timestamps() {
        assert_eq!(format_timestamp(0), "19700101T000000Z");
        assert_eq!(format_timestamp(951_782_400), "20000229T000000Z");
        assert_eq!(format_timestamp(1_790_247_723), "20260924T110203Z");
        assert_eq!(format_timestamp(4_107_542_399), "21000228T235959Z");
    }

    #[test]
    fn reports() {
        let report = RestoreReport {
            language: RestoreOutcome::Restored,
            lock_screen: RestoreOutcome::Skipped("no custom picture was set".into()),
            apps: vec![
                ("Bitcoin".into(), RestoreOutcome::Restored),
                (
                    "Bitcoin Test".into(),
                    RestoreOutcome::Failed("not available".into()),
                ),
            ],
            apps_error: None,
        };
        assert_eq!(report.reinstalled_apps(), vec!["Bitcoin"]);
        assert!(!report.is_complete());
        assert_eq!(
            report.lines(),
            vec![
                "Language: restored",
                "Lock screen picture: skipped (no custom picture was set)",
                "App Bitcoin: restored",
                "App Bitcoin Test: FAILED (not available)"
            ]
        );
        assert_eq!(
            sample_backup().summary(),
            vec![
                "Apps: Bitcoin, Bitcoin Test".to_string(),
                "Language: French".to_string(),
                format!("Lock screen picture: saved ({} bytes)", 400 * 672 / 2 + 8),
            ]
        );
    }
}
