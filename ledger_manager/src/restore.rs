//! Backing up the settings of the device before a firmware update, and restoring them after.
//!
//! A firmware update uninstalls all the apps, and may reset the language and the custom lock
//! screen picture of the device. Like Ledger Live, before the update we back up:
//! - the list of installed apps;
//! - the language of the device;
//! - the custom lock screen picture (Stax, Flex, Nano Gen5);
//!
//! and after the update we restore, in this order: the language (by installing the language pack
//! for the new firmware), the lock screen picture, and the apps (from the catalog for the new
//! firmware, along with their dependencies). This follows
//! https://github.com/LedgerHQ/ledger-live/blob/develop/apps/ledger-live-mobile/src/screens/FirmwareUpdate/useUpdateFirmwareAndRestoreSettings.ts
//! and the desktop firmware update modal
//! (https://github.com/LedgerHQ/ledger-live/tree/develop/apps/ledger-live-desktop/src/renderer/modals/UpdateFirmwareModal).
//!
//! Each part is independent: a failure of one does not prevent the others. As in Ledger Live, a
//! failure to back up something never prevents the firmware update.
//!
//! The data stored inside the apps (for instance the wallet policies registered in the Bitcoin
//! app) is not backed up: it is lost when the apps are uninstalled.
//!
//! Unlike Ledger Live, the backup is saved to a file before starting the update (see
//! `save_backup`), so it can still be restored (`restore_device_settings`) if the update gets
//! interrupted.

use crate::{
    api::{apps_catalog, bitcoin_apps_by_hashes, AppInfo, FirmwareUpdateInfo},
    apps::{install_app, list_installed_apps_raw, AppInstallStep, MANAGER_INSTALL_DELAY},
    device::{connect, is_device_localization_supported, quit_app, DeviceInfo},
    error::Error,
    firmware::{
        check_firmware_update_supported, update_firmware_with_options, FirmwareUpdateOptions,
        FirmwareUpdateStep,
    },
    language::ENGLISH_LANGUAGE_ID,
    language::{install_language, language_display_name, language_name, LanguageInstallStep},
    lock_screen::{
        check_image_for_model, fetch_image, fetch_image_hash, load_image, LoadImageStep,
    },
    model::DeviceModel,
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
pub const BACKUP_FORMAT_VERSION: u32 = 1;

/// The prefix of the name of the backup files.
const BACKUP_FILE_PREFIX: &str = "ledger-backup-";

/// When resuming an interrupted update (device in updater mode), the most recent backup of the
/// device is used if it is not older than this.
pub const RESUME_BACKUP_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// (De)serialize bytes as a hex string.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(s).map_err(serde::de::Error::custom)
    }
}

/// An app installed on the device when it was backed up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackedUpApp {
    /// The name of the app, e.g. "Bitcoin".
    pub name: String,
    /// Its version, if known by the Ledger API.
    #[serde(default)]
    pub version: Option<String>,
    /// Its hash (hex-encoded), as listed by the device.
    #[serde(default)]
    pub hash: Option<String>,
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
    /// The backup failed.
    Failed { error: String },
}

/// The settings of a device backed up before a firmware update. It can be serialized (see
/// `save_backup`) to be restored later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceBackup {
    /// The version of the format, `BACKUP_FORMAT_VERSION`.
    pub format_version: u32,
    /// When the backup was made (seconds since the Unix epoch).
    pub created_at: u64,
    /// The target id of the device.
    pub target_id: u32,
    /// The model of the device (Ledger Live's model id, e.g. "stax"), if known.
    #[serde(default)]
    pub model: Option<String>,
    /// The firmware version of the device when it was backed up.
    pub firmware_version: String,
    /// The installed apps.
    #[serde(default)]
    pub apps: Vec<BackedUpApp>,
    /// Set if the installed apps could not be listed (then `apps` is empty).
    #[serde(default)]
    pub apps_error: Option<String>,
    /// The language id (see `language::LANGUAGES`), `None` if the firmware doesn't support
    /// changing the language.
    #[serde(default)]
    pub language_id: Option<u8>,
    /// The custom lock screen picture.
    pub lock_screen: LockScreenBackup,
}

impl DeviceBackup {
    /// The model of the device, if known.
    pub fn device_model(&self) -> Option<DeviceModel> {
        self.model
            .as_deref()
            .and_then(DeviceModel::from_id)
            .or_else(|| DeviceModel::from_target_id(self.target_id))
    }

    /// Whether there is something to restore after the update.
    pub fn has_something_to_restore(&self) -> bool {
        !self.apps.is_empty()
            || self.language_id.map(|l| l != ENGLISH_LANGUAGE_ID) == Some(true)
            || matches!(self.lock_screen, LockScreenBackup::Saved { .. })
    }

    /// A human-readable summary of what was backed up, one line per item.
    pub fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(match (&self.apps_error, self.apps.is_empty()) {
            (Some(e), _) => format!("Apps: could not be listed ({})", e),
            (None, true) => "Apps: none installed".to_string(),
            (None, false) => format!(
                "Apps: {}",
                self.apps
                    .iter()
                    .map(|a| match &a.version {
                        Some(v) => format!("{} {}", a.name, v),
                        None => a.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
        lines.push(match self.language_id {
            None => "Language: not supported by the firmware".to_string(),
            Some(id) => format!("Language: {}", language_display_name(id)),
        });
        lines.push(match &self.lock_screen {
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
        });
        lines
    }
}

/// A step of the backup.
#[derive(Debug, Clone, PartialEq)]
pub enum BackupStep {
    /// Listing the installed apps. The user may have to allow the Ledger manager on the device.
    ListingApps,
    /// Querying the Ledger API about the installed apps.
    QueryingApi,
    /// Backing up the lock screen picture. The user may have to approve it on the device.
    FetchingLockScreen,
    /// Fetching the lock screen picture. `progress` is between 0 and 1.
    FetchingLockScreenProgress { progress: f32 },
    /// The backup is done.
    Done,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The installed apps to back up, from the apps listed by the device and their matches (by hash)
/// in the Ledger API, in the same order (`None` if unknown or not queried).
///
/// Like Ledger Live (apps/listApps.ts), the entries with an empty `hash_code_data` (language
/// packs, sideloaded apps) are not apps and are ignored. The name known by the Ledger API is used
/// if its hash matches, otherwise the name listed by the device.
pub(crate) fn apps_to_back_up(
    listed: &[crate::apps::InstalledApp],
    matches: &[Option<AppInfo>],
) -> Vec<BackedUpApp> {
    listed
        .iter()
        .filter(|a| a.hash_code_data.iter().any(|b| *b != 0))
        .enumerate()
        .map(|(i, a)| {
            let hash = hex::encode(&a.hash);
            let m = matches
                .get(i)
                .and_then(|m| m.as_ref())
                .filter(|m| m.hash.eq_ignore_ascii_case(&hash));
            BackedUpApp {
                name: m.map(|m| m.version_name.clone()).unwrap_or(a.name.clone()),
                version: m.map(|m| m.version.clone()),
                hash: Some(hash),
            }
        })
        .collect()
}

/// Back up the settings of the device before a firmware update: the installed apps, the language
/// and the custom lock screen picture (on the models supporting it). `device_info` is the
/// information of the device, which must run its firmware normally and be on its dashboard.
///
/// The user may have to allow the Ledger manager (to list the apps) and to approve the backup of
/// the lock screen picture on the device. A failure (or refusal) to back up the apps or the
/// picture doesn't make the backup fail: it is recorded in the backup.
pub fn backup_device_settings<P: FnMut(BackupStep)>(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    mut progress: P,
) -> Result<DeviceBackup, Error> {
    device_info.check_normal_mode()?;
    log::info!("Backing up the device settings.");

    // The installed apps.
    progress(BackupStep::ListingApps);
    let (apps, apps_error) = match list_installed_apps_raw(transport) {
        Ok(listed) => {
            let hashes: Vec<Vec<u8>> = listed
                .iter()
                .filter(|a| a.hash_code_data.iter().any(|b| *b != 0))
                .map(|a| a.hash.clone())
                .collect();
            progress(BackupStep::QueryingApi);
            let matches = if hashes.is_empty() {
                Vec::new()
            } else {
                bitcoin_apps_by_hashes(hashes).unwrap_or_else(|e| {
                    log::warn!("Could not query the installed apps versions: {}", e);
                    Vec::new()
                })
            };
            (apps_to_back_up(&listed, &matches), None)
        }
        Err(e) => {
            log::warn!("Could not list the installed apps: {}", e);
            (Vec::new(), Some(e.to_string()))
        }
    };

    // The custom lock screen picture. Like Ledger Live Desktop, only on a set up device.
    let model = device_info.model;
    let lock_screen = if !model
        .map(|m| m.is_custom_lock_screen_supported())
        .unwrap_or(false)
    {
        LockScreenBackup::NotSupported
    } else if !device_info.onboarded {
        LockScreenBackup::NotSet
    } else {
        progress(BackupStep::FetchingLockScreen);
        match fetch_image(transport, |p| {
            progress(BackupStep::FetchingLockScreenProgress { progress: p })
        }) {
            Ok(None) => LockScreenBackup::NotSet,
            Ok(Some(image)) => {
                if let Some(model) = model {
                    if let Err(e) = check_image_for_model(&image.data, model) {
                        log::warn!("The lock screen picture has an unexpected format: {}", e);
                    }
                }
                LockScreenBackup::Saved {
                    image: image.data,
                    hash: image.hash,
                }
            }
            Err(Error::UserRefusedOnDevice) => LockScreenBackup::Refused,
            Err(e) => {
                log::warn!("Could not back up the lock screen picture: {}", e);
                LockScreenBackup::Failed {
                    error: e.to_string(),
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
pub(crate) fn format_timestamp(secs: u64) -> String {
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

/// The name of the file of this backup, e.g.
/// "ledger-backup-stax-33200004-20260924T110203Z.json".
pub fn backup_file_name(backup: &DeviceBackup) -> String {
    format!(
        "{}{}-{:08x}-{}.json",
        BACKUP_FILE_PREFIX,
        backup.model.as_deref().unwrap_or("unknown"),
        backup.target_id,
        format_timestamp(backup.created_at)
    )
}

/// Save the backup in this directory (created if needed), in a new file named after the device
/// and the date of the backup (see `backup_file_name`). Returns the path of the file.
///
/// The file is written to a temporary file which is then renamed, so a backup file is always
/// complete. On Unix it is only readable by the user.
pub fn save_backup(backup: &DeviceBackup, dir: &Path) -> Result<PathBuf, Error> {
    let err = |path: &Path, e: &dyn fmt::Display| {
        Error::BackupNotSaved(format!("{}: {}", path.display(), e))
    };
    fs::create_dir_all(dir).map_err(|e| err(dir, &e))?;
    let path = dir.join(backup_file_name(backup));
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
    let read = load_backup(&path).map_err(|e| err(&path, &e))?;
    if read != *backup {
        return Err(err(&path, &"the file doesn't contain the backup"));
    }
    log::info!("Device backup saved to {}.", path.display());
    Ok(path)
}

/// Load a backup saved with `save_backup`.
pub fn load_backup(path: &Path) -> Result<DeviceBackup, Error> {
    let data = fs::read(path)?;
    let backup: DeviceBackup = serde_json::from_slice(&data)
        .map_err(|e| Error::InvalidBackup(format!("{}: {}", path.display(), e)))?;
    if backup.format_version > BACKUP_FORMAT_VERSION {
        return Err(Error::InvalidBackup(format!(
            "{}: unsupported format version {}",
            path.display(),
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
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(BACKUP_FILE_PREFIX) && n.ends_with(".json"))
                .unwrap_or(false)
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
    /// It was restored.
    Restored,
    /// There was nothing to restore, with the reason.
    Skipped(String),
    /// It could not be restored, with the reason.
    Failed(String),
}

impl RestoreOutcome {
    pub fn is_failed(&self) -> bool {
        matches!(self, RestoreOutcome::Failed(_))
    }
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

/// The outcome of the reinstallation of an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRestoreOutcome {
    pub name: String,
    pub outcome: RestoreOutcome,
    /// Set if the app was not installed before the update, but is installed as a dependency of
    /// this app.
    pub dependency_of: Option<String>,
}

/// What was restored after a firmware update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub language: RestoreOutcome,
    pub lock_screen: RestoreOutcome,
    /// The outcome for each app to reinstall.
    pub apps: Vec<AppRestoreOutcome>,
    /// Set if the apps could not be reinstalled at all (or not listed before the update).
    pub apps_error: Option<String>,
}

impl RestoreReport {
    /// The names of the apps which were reinstalled.
    pub fn reinstalled_apps(&self) -> Vec<&str> {
        self.apps
            .iter()
            .filter(|a| a.outcome == RestoreOutcome::Restored)
            .map(|a| a.name.as_str())
            .collect()
    }

    /// The apps which could not be reinstalled, with the reason.
    pub fn failed_apps(&self) -> Vec<(&str, &str)> {
        self.apps
            .iter()
            .filter_map(|a| match &a.outcome {
                RestoreOutcome::Failed(r) => Some((a.name.as_str(), r.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Whether everything was restored (nothing failed).
    pub fn is_complete(&self) -> bool {
        self.apps_error.is_none()
            && !self.language.is_failed()
            && !self.lock_screen.is_failed()
            && self.apps.iter().all(|a| !a.outcome.is_failed())
    }

    /// A human-readable report, one line per item.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Language: {}", self.language),
            format!("Lock screen picture: {}", self.lock_screen),
        ];
        if let Some(e) = &self.apps_error {
            lines.push(format!("Apps: FAILED ({})", e));
        }
        if self.apps.is_empty() && self.apps_error.is_none() {
            lines.push("Apps: none to reinstall".to_string());
        }
        for app in &self.apps {
            let dep = app
                .dependency_of
                .as_ref()
                .map(|d| format!(" (dependency of {})", d))
                .unwrap_or_default();
            lines.push(format!("App {}{}: {}", app.name, dep, app.outcome));
        }
        lines
    }
}

impl fmt::Display for RestoreReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.lines().join("\n"))
    }
}

/// A step of the restoration.
#[derive(Debug, Clone, PartialEq)]
pub enum RestoreStep {
    /// Installing the language pack of this language (display name, e.g. "French").
    InstallingLanguage { language: String },
    /// A step of the language pack installation (the user must allow it on the device).
    Language(LanguageInstallStep),
    /// Restoring the lock screen picture.
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
    /// A step of the app installation.
    App(AppInstallStep),
    /// The restoration is done.
    Done { report: Box<RestoreReport> },
}

/// The apps to reinstall, in order.
#[derive(Debug, Clone)]
pub(crate) struct ReinstallPlan {
    /// The apps to install, dependencies first, with the app they are a dependency of if they
    /// were not in the list of apps to reinstall.
    pub queue: Vec<(AppInfo, Option<String>)>,
    /// The apps which won't be installed, with the reason.
    pub skipped: Vec<(String, RestoreOutcome)>,
}

/// Plan the reinstallation of these apps (by name), given the names of the apps currently
/// installed and the catalog of apps for the firmware of the device.
///
/// Like the `install` action of Ledger Live's apps logic (apps/logic.ts) with
/// `allowPartialDependencies`: apps already installed are skipped, apps missing from the catalog
/// are skipped (and reported), and the dependencies (`parentName`) not installed are installed
/// first.
pub(crate) fn plan_reinstall(
    to_restore: &[String],
    installed: &[String],
    catalog: &[AppInfo],
) -> ReinstallPlan {
    let mut queue: Vec<(AppInfo, Option<String>)> = Vec::new();
    let mut skipped = Vec::new();
    let find = |name: &str| catalog.iter().find(|a| a.version_name == name);
    let queued =
        |q: &[(AppInfo, Option<String>)], name: &str| q.iter().any(|(a, _)| a.version_name == name);
    let mut seen: Vec<&str> = Vec::new();

    for name in to_restore {
        if seen.contains(&name.as_str()) {
            continue;
        }
        seen.push(name);
        if queued(&queue, name) {
            // Already queued as a dependency of a previous app.
            continue;
        }
        if installed.contains(name) {
            skipped.push((
                name.clone(),
                RestoreOutcome::Skipped("already installed".into()),
            ));
            continue;
        }
        let app = match find(name) {
            Some(a) => a,
            None => {
                skipped.push((
                    name.clone(),
                    RestoreOutcome::Failed(
                        "not available for the new firmware in the Ledger catalog".into(),
                    ),
                ));
                continue;
            }
        };
        let deps: Vec<&str> = app
            .parent_name
            .as_deref()
            .filter(|p| !p.is_empty())
            .into_iter()
            .collect();
        let mut deps_to_install = Vec::new();
        let mut missing_dep = None;
        for dep in deps {
            if installed.iter().any(|i| i == dep) || queued(&queue, dep) {
                continue;
            }
            match find(dep) {
                Some(d) => deps_to_install.push(d.clone()),
                None => missing_dep = Some(dep),
            }
        }
        if let Some(dep) = missing_dep {
            skipped.push((
                name.clone(),
                RestoreOutcome::Failed(format!(
                    "the app it depends on ({}) is not available in the Ledger catalog",
                    dep
                )),
            ));
            continue;
        }
        for d in deps_to_install {
            let dependency_of = (!to_restore.contains(&d.version_name)).then(|| name.clone());
            queue.push((d, dependency_of));
        }
        queue.push((app.clone(), None));
    }
    ReinstallPlan { queue, skipped }
}

fn restore_language<P: FnMut(RestoreStep)>(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    backup: &DeviceBackup,
    progress: &mut P,
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
    let name = match language_name(id) {
        Some(n) => n,
        None => return RestoreOutcome::Failed(format!("unknown language id {:#04x}", id)),
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

fn restore_lock_screen<P: FnMut(RestoreStep)>(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    backup: &DeviceBackup,
    progress: &mut P,
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
    let model = match device_info.model {
        Some(m) if m.is_custom_lock_screen_supported() => m,
        _ => return RestoreOutcome::Failed("not supported by this device".into()),
    };
    if let Err(e) = check_image_for_model(image, model) {
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

fn restore_apps<P: FnMut(RestoreStep)>(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    backup: &DeviceBackup,
    progress: &mut P,
) -> (Vec<AppRestoreOutcome>, Option<String>) {
    if let Some(e) = &backup.apps_error {
        return (
            Vec::new(),
            Some(format!(
                "the apps could not be listed before the update: {}",
                e
            )),
        );
    }
    let names: Vec<String> = backup.apps.iter().map(|a| a.name.clone()).collect();
    if names.is_empty() {
        return (Vec::new(), None);
    }
    let fail_all = |reason: String| {
        names
            .iter()
            .map(|n| AppRestoreOutcome {
                name: n.clone(),
                outcome: RestoreOutcome::Failed(reason.clone()),
                dependency_of: None,
            })
            .collect::<Vec<_>>()
    };

    progress(RestoreStep::ListingApps);
    let installed: Vec<String> = match list_installed_apps_raw(transport) {
        Ok(apps) => apps.into_iter().map(|a| a.name).collect(),
        Err(e) => {
            let reason = format!("could not list the installed apps: {}", e);
            return (fail_all(reason.clone()), Some(reason));
        }
    };
    let catalog = match apps_catalog(device_info) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("could not get the catalog of apps: {}", e);
            return (fail_all(reason.clone()), Some(reason));
        }
    };

    let plan = plan_reinstall(&names, &installed, &catalog);
    log::info!(
        "Reinstalling apps: {}.",
        plan.queue
            .iter()
            .map(|(a, _)| a.version_name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut outcomes: Vec<AppRestoreOutcome> = Vec::new();
    let total = plan.queue.len();
    for (i, (app, dependency_of)) in plan.queue.iter().enumerate() {
        let failed_dep = app.parent_name.as_deref().filter(|p| {
            outcomes
                .iter()
                .any(|o| o.name == *p && o.outcome.is_failed())
        });
        let outcome = if let Some(dep) = failed_dep {
            RestoreOutcome::Failed(format!(
                "the app it depends on ({}) could not be installed",
                dep
            ))
        } else {
            if i > 0 {
                thread::sleep(MANAGER_INSTALL_DELAY);
            }
            progress(RestoreStep::InstallingApp {
                name: app.version_name.clone(),
                index: i + 1,
                total,
            });
            match install_app(transport, device_info.target_id, app, |s| {
                progress(RestoreStep::App(s))
            }) {
                Ok(()) | Err(Error::AppAlreadyInstalled) => RestoreOutcome::Restored,
                Err(e) => {
                    log::warn!("Could not reinstall {}: {}", app.version_name, e);
                    RestoreOutcome::Failed(e.to_string())
                }
            }
        };
        outcomes.push(AppRestoreOutcome {
            name: app.version_name.clone(),
            outcome,
            dependency_of: dependency_of.clone(),
        });
    }
    outcomes.extend(
        plan.skipped
            .into_iter()
            .map(|(name, outcome)| AppRestoreOutcome {
                name,
                outcome,
                dependency_of: None,
            }),
    );
    (outcomes, None)
}

/// Restore the settings of a device from a backup made before a firmware update: the language
/// (installing the language pack for the new firmware, if it wasn't English), the custom lock
/// screen picture, and the apps (from the catalog for the new firmware, with their dependencies).
///
/// The user will have to approve the installation of the language, the loading of the picture,
/// and to allow the Ledger manager on the device.
///
/// Each part is independent: a failure of one doesn't prevent the others, the report tells what
/// was restored. An error is returned only if the device can't be used (not on its dashboard,
/// locked...) or if the backup was made for another device model.
pub fn restore_device_settings<P: FnMut(RestoreStep)>(
    transport: &TransportNativeHID,
    backup: &DeviceBackup,
    mut progress: P,
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
    log::info!("Restore report:\n{}", report);
    progress(RestoreStep::Done {
        report: Box::new(report.clone()),
    });
    Ok(report)
}

/// Where to keep the backup made before the firmware update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupLocation {
    /// Save it to a file in this directory before starting the update. The update is not started
    /// if the file can't be saved (`Error::BackupNotSaved`).
    Directory(PathBuf),
    /// Only keep it in memory: it is lost if the update is interrupted.
    MemoryOnly,
}

/// Options of `update_firmware_and_restore`.
#[derive(Debug, Clone)]
pub struct UpdateAndRestoreOptions {
    pub backup_location: BackupLocation,
    pub firmware: FirmwareUpdateOptions,
}

impl UpdateAndRestoreOptions {
    /// Save the backup in this directory, with the default firmware update options.
    pub fn new(backup_dir: PathBuf) -> Self {
        Self {
            backup_location: BackupLocation::Directory(backup_dir),
            firmware: FirmwareUpdateOptions::default(),
        }
    }

    /// Only keep the backup in memory, with the default firmware update options.
    pub fn memory_only() -> Self {
        Self {
            backup_location: BackupLocation::MemoryOnly,
            firmware: FirmwareUpdateOptions::default(),
        }
    }
}

/// A step of `update_firmware_and_restore`.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateAndRestoreStep {
    /// A step of the backup.
    Backup(BackupStep),
    /// The backup was saved to this file.
    BackupSaved { path: PathBuf },
    /// The device is in updater mode (an update was interrupted): this backup, made before the
    /// interrupted update, will be restored.
    BackupLoaded { path: PathBuf },
    /// The settings could not be backed up (the update continues), with the reason.
    BackupSkipped { reason: String },
    /// A step of the firmware update.
    Firmware(FirmwareUpdateStep),
    /// A step of the restoration.
    Restore(RestoreStep),
}

/// The result of `update_firmware_and_restore`.
#[derive(Debug, Clone)]
pub struct UpdateAndRestoreResult {
    /// The information of the device after the update (before the restoration).
    pub device_info: DeviceInfo,
    /// The backup made before the update, if any.
    pub backup: Option<DeviceBackup>,
    /// The file the backup was saved to (or loaded from), if any.
    pub backup_path: Option<PathBuf>,
    /// What was restored, if there was a backup and the device could be used.
    pub report: Option<RestoreReport>,
    /// Set if the restoration could not be performed at all.
    pub restore_error: Option<String>,
}

/// Back up the settings of the device, update its firmware and restore the settings. See
/// `backup_device_settings`, `firmware::update_firmware` and `restore_device_settings`.
///
/// With `BackupLocation::Directory`, the backup is saved to a file before the update starts. If
/// the file can't be saved the update is not started and `Error::BackupNotSaved` is returned: the
/// caller may retry with `BackupLocation::MemoryOnly`. A failure to back up (part of) the
/// settings otherwise never prevents the update.
///
/// If the device is in updater mode (a previous update was interrupted), the most recent backup of
/// the device in the backup directory (not older than `RESUME_BACKUP_MAX_AGE`) is restored after
/// the update.
///
/// Returns an error if the update fails. The backup file is kept, and can be restored with
/// `load_backup` and `restore_device_settings` once the update is completed.
pub fn update_firmware_and_restore<P: FnMut(UpdateAndRestoreStep)>(
    hid_api: &mut HidApi,
    update: &FirmwareUpdateInfo,
    options: &UpdateAndRestoreOptions,
    mut progress: P,
) -> Result<UpdateAndRestoreResult, Error> {
    progress(UpdateAndRestoreStep::Firmware(
        FirmwareUpdateStep::Preparing,
    ));
    let (backup, backup_path) = {
        let transport = connect(hid_api)?;
        quit_app(&transport)?;
        let device_info = DeviceInfo::new(&transport)?;
        // Don't bother the user with the backup if the update can't be performed.
        check_firmware_update_supported(&device_info)?;

        if device_info.is_osu {
            match &options.backup_location {
                BackupLocation::Directory(dir) => {
                    match find_latest_backup(dir, device_info.target_id, RESUME_BACKUP_MAX_AGE) {
                        Some((path, backup)) => {
                            log::info!("Resuming the update, using the backup {}.", path.display());
                            progress(UpdateAndRestoreStep::BackupLoaded { path: path.clone() });
                            (Some(backup), Some(path))
                        }
                        None => {
                            progress(UpdateAndRestoreStep::BackupSkipped {
                                reason: "the device is in updater mode and no recent backup of it was found".into(),
                            });
                            (None, None)
                        }
                    }
                }
                BackupLocation::MemoryOnly => {
                    progress(UpdateAndRestoreStep::BackupSkipped {
                        reason: "the device is in updater mode, its settings can't be backed up"
                            .into(),
                    });
                    (None, None)
                }
            }
        } else {
            match backup_device_settings(&transport, &device_info, |s| {
                progress(UpdateAndRestoreStep::Backup(s))
            }) {
                Ok(backup) => {
                    let path = match &options.backup_location {
                        BackupLocation::Directory(dir) => {
                            // Never start the update if the backup can't be saved.
                            let path = save_backup(&backup, dir)?;
                            progress(UpdateAndRestoreStep::BackupSaved { path: path.clone() });
                            Some(path)
                        }
                        BackupLocation::MemoryOnly => None,
                    };
                    (Some(backup), path)
                }
                Err(e) => {
                    log::warn!("Could not back up the device settings: {}", e);
                    progress(UpdateAndRestoreStep::BackupSkipped {
                        reason: e.to_string(),
                    });
                    (None, None)
                }
            }
        }
        // The transport is dropped here: the update opens its own connections to the device.
    };

    let device_info = update_firmware_with_options(hid_api, update, &options.firmware, |s| {
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
        backup,
        backup_path,
        report,
        restore_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::parse_catalog, apps::InstalledApp};

    fn catalog() -> Vec<AppInfo> {
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/data/apps_catalog_stax.json")).unwrap();
        parse_catalog(entries)
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

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
                    version: Some("2.4.5".into()),
                    hash: Some("aa".repeat(32)),
                },
                BackedUpApp {
                    name: "Some sideloaded app".into(),
                    version: None,
                    hash: None,
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
    fn reinstall_plan() {
        let catalog = catalog();
        assert_eq!(catalog.len(), 6);

        // Nothing installed after the update: everything available is reinstalled, in order.
        let plan = plan_reinstall(
            &names(&["Bitcoin", "Solana", "Bitcoin Test", "Unknown app"]),
            &[],
            &catalog,
        );
        let queue: Vec<&str> = plan
            .queue
            .iter()
            .map(|(a, _)| a.version_name.as_str())
            .collect();
        assert_eq!(queue, vec!["Bitcoin", "Solana", "Bitcoin Test"]);
        assert!(plan.queue.iter().all(|(_, d)| d.is_none()));
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].0, "Unknown app");
        assert!(plan.skipped[0].1.is_failed());

        // Dependencies are installed first, even if they weren't installed before.
        let plan = plan_reinstall(&names(&["Paraswap"]), &[], &catalog);
        let queue: Vec<(&str, Option<&str>)> = plan
            .queue
            .iter()
            .map(|(a, d)| (a.version_name.as_str(), d.as_deref()))
            .collect();
        assert_eq!(
            queue,
            vec![("Ethereum", Some("Paraswap")), ("Paraswap", None)]
        );

        // A dependency which was installed is not reported as such, nor installed twice.
        let plan = plan_reinstall(&names(&["Paraswap", "Ethereum", "Bitcoin"]), &[], &catalog);
        let queue: Vec<(&str, Option<&str>)> = plan
            .queue
            .iter()
            .map(|(a, d)| (a.version_name.as_str(), d.as_deref()))
            .collect();
        assert_eq!(
            queue,
            vec![("Ethereum", None), ("Paraswap", None), ("Bitcoin", None)]
        );

        // Already installed apps (and dependencies) are skipped.
        let plan = plan_reinstall(
            &names(&["Bitcoin", "Paraswap", "Bitcoin"]),
            &names(&["Bitcoin", "Ethereum"]),
            &catalog,
        );
        let queue: Vec<&str> = plan
            .queue
            .iter()
            .map(|(a, _)| a.version_name.as_str())
            .collect();
        assert_eq!(queue, vec!["Paraswap"]);
        assert_eq!(
            plan.skipped,
            vec![(
                "Bitcoin".to_string(),
                RestoreOutcome::Skipped("already installed".into())
            )]
        );

        // Missing dependency.
        let no_eth: Vec<AppInfo> = catalog
            .iter()
            .filter(|a| a.version_name != "Ethereum")
            .cloned()
            .collect();
        let plan = plan_reinstall(&names(&["Paraswap", "Bitcoin"]), &[], &no_eth);
        assert_eq!(plan.queue.len(), 1);
        assert_eq!(plan.skipped[0].0, "Paraswap");
        assert!(plan.skipped[0].1.is_failed());
    }

    #[test]
    fn apps_backup_list() {
        let app = |name: &str, code: u8, hash: u8| InstalledApp {
            name: name.into(),
            hash: vec![hash; 32],
            hash_code_data: vec![code; 32],
            blocks: 1,
            flags: 0,
        };
        let listed = vec![
            app("Bitcoin", 1, 0xaa),
            app("French language pack", 0, 0xbb),
            app("Local name", 1, 0xcc),
            app("Custom", 1, 0xdd),
        ];
        let mut btc = catalog()
            .into_iter()
            .find(|a| a.version_name == "Bitcoin")
            .unwrap();
        btc.hash = "aa".repeat(32);
        let mut eth = catalog()
            .into_iter()
            .find(|a| a.version_name == "Ethereum")
            .unwrap();
        eth.hash = "cc".repeat(32);
        let apps = apps_to_back_up(&listed, &[Some(btc), Some(eth), None]);
        assert_eq!(
            apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["Bitcoin", "Ethereum", "Custom"]
        );
        assert_eq!(apps[0].version.as_deref(), Some("2.4.6"));
        assert_eq!(apps[2].version, None);
        assert_eq!(apps[2].hash, Some("dd".repeat(32)));
        // Without API matches, the local names are used.
        let apps = apps_to_back_up(&listed, &[]);
        assert_eq!(
            apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["Bitcoin", "Local name", "Custom"]
        );
        // A match with another hash is not used.
        let mut wrong = catalog()
            .into_iter()
            .find(|a| a.version_name == "Solana")
            .unwrap();
        wrong.hash = "00".repeat(32);
        let apps = apps_to_back_up(&listed[..1], &[Some(wrong)]);
        assert_eq!(apps[0].name, "Bitcoin");
        assert_eq!(apps[0].version, None);
    }

    #[test]
    fn serialization_roundtrip() {
        let backup = sample_backup();
        let json = serde_json::to_string_pretty(&backup).unwrap();
        let back: DeviceBackup = serde_json::from_str(&json).unwrap();
        assert_eq!(back, backup);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["lock_screen"]["status"], "saved");
        assert!(value["lock_screen"]["image"]
            .as_str()
            .unwrap()
            .starts_with("9001a002"));

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
        let v: serde_json::Value = serde_json::to_value(LockScreenBackup::NotSupported).unwrap();
        assert_eq!(v, serde_json::json!({"status": "not_supported"}));

        // Minimal file.
        let b: DeviceBackup = serde_json::from_value(serde_json::json!({
            "format_version": 1,
            "created_at": 0,
            "target_id": 0x33000004u32,
            "firmware_version": "2.2.3",
            "lock_screen": {"status": "not_supported"},
        }))
        .unwrap();
        assert!(b.apps.is_empty());
        assert_eq!(b.device_model(), Some(DeviceModel::NanoX));
        assert!(!b.has_something_to_restore());
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
        let mut backup = sample_backup();
        backup.created_at = now();
        let path = save_backup(&backup, &dir.join("sub")).unwrap();
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
        save_backup(&older, &dir.join("sub")).unwrap();
        let mut other = backup.clone();
        other.target_id = 0x3330_0004;
        other.created_at += 10;
        save_backup(&other, &dir.join("sub")).unwrap();
        let (found, b) =
            find_latest_backup(&dir.join("sub"), 0x3320_0004, RESUME_BACKUP_MAX_AGE).unwrap();
        assert_eq!(found, path);
        assert_eq!(b, backup);
        assert!(find_latest_backup(&dir.join("sub"), 0x3310_0004, RESUME_BACKUP_MAX_AGE).is_none());
        assert!(
            find_latest_backup(&dir.join("nothing"), 0x3320_0004, RESUME_BACKUP_MAX_AGE).is_none()
        );
        // Too old.
        let mut old = backup.clone();
        old.target_id = 0x3340_0004;
        old.created_at -= RESUME_BACKUP_MAX_AGE.as_secs() + 10;
        save_backup(&old, &dir.join("sub")).unwrap();
        assert!(find_latest_backup(&dir.join("sub"), 0x3340_0004, RESUME_BACKUP_MAX_AGE).is_none());

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
                AppRestoreOutcome {
                    name: "Ethereum".into(),
                    outcome: RestoreOutcome::Restored,
                    dependency_of: Some("Paraswap".into()),
                },
                AppRestoreOutcome {
                    name: "Paraswap".into(),
                    outcome: RestoreOutcome::Restored,
                    dependency_of: None,
                },
                AppRestoreOutcome {
                    name: "Unknown".into(),
                    outcome: RestoreOutcome::Failed("not available".into()),
                    dependency_of: None,
                },
            ],
            apps_error: None,
        };
        assert_eq!(report.reinstalled_apps(), vec!["Ethereum", "Paraswap"]);
        assert_eq!(report.failed_apps(), vec![("Unknown", "not available")]);
        assert!(!report.is_complete());
        assert_eq!(
            report.to_string(),
            "Language: restored\nLock screen picture: skipped (no custom picture was set)\nApp Ethereum (dependency of Paraswap): restored\nApp Paraswap: restored\nApp Unknown: FAILED (not available)"
        );

        let backup = sample_backup();
        assert!(backup.has_something_to_restore());
        assert_eq!(
            backup.summary(),
            vec![
                "Apps: Bitcoin 2.4.5, Some sideloaded app".to_string(),
                "Language: French".to_string(),
                format!("Lock screen picture: saved ({} bytes)", 400 * 672 / 2 + 8),
            ]
        );
    }
}
