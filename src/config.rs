//! Settings (`%APPDATA%\OTHCloud Drive\config.json`) and the sign-in token
//! (`credentials.bin`, DPAPI-encrypted for the current Windows user).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_ORIGIN: &str = "https://othcloud.xyz";

/// The panel to sign in to when there's no saved one. `OTHCLOUD_ORIGIN`
/// overrides it (self-hosted / test panels).
pub fn default_origin() -> String {
    std::env::var("OTHCLOUD_ORIGIN")
        .ok()
        .and_then(|o| crate::api::normalize_origin(&o).ok())
        .unwrap_or_else(|| DEFAULT_ORIGIN.to_string())
}

/// What a backed-up folder holds: everything, or pictures only (the old
/// photo backup, which became one of these folders).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FolderKind {
    #[default]
    Folder,
    Photos,
}

impl FolderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FolderKind::Folder => "folder",
            FolderKind::Photos => "photos",
        }
    }
}

/// One local folder backed up into the backup drive (PROTOCOL.md "Folder
/// backup").
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupFolder {
    /// Random, made once when the folder is added; the panel keys the
    /// folder's remote folder and status by it. The migrated photo backup
    /// is "photos".
    pub folder_key: String,
    pub label: String,
    pub local_path: PathBuf,
    #[serde(default)]
    pub kind: FolderKind,
    #[serde(default)]
    pub enabled: bool,
    /// Glob patterns (see `backup::Excludes`).
    #[serde(default)]
    pub excludes: Vec<String>,
    /// Remote folder path override ("a/b"). Only the migrated photo backup
    /// has one, when it had a custom remote folder.
    #[serde(default)]
    pub remote_folder: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupSettings {
    /// LEGACY photo backup on/off. Read once by the migration into
    /// `folders`; afterwards kept in step with the `photos` folder so that
    /// an older build reading this file behaves the same.
    #[serde(default)]
    pub enabled: bool,
    /// LEGACY: local folder to watch; None = the user's Pictures.
    #[serde(default)]
    pub folder: Option<PathBuf>,
    /// The ONE drive every backed-up folder goes into; None = the first drive.
    #[serde(default)]
    pub storage_id: Option<String>,
    /// LEGACY: remote folder path ("a/b"); None = "Photo backup/<PC name>".
    #[serde(default)]
    pub remote_folder: Option<String>,
    /// Random id of this install, made once and kept (`PUT device`).
    #[serde(default)]
    pub device_key: Option<String>,
    #[serde(default)]
    pub folders: Vec<BackupFolder>,
    /// Backup alone is paused (the app-wide pause stops it too).
    #[serde(default)]
    pub paused: bool,
    /// The legacy photo backup has been turned into a `photos` folder.
    #[serde(default)]
    pub migrated: bool,
}

impl BackupSettings {
    /// Turn the legacy photo backup into a `photos` folder, once. Its
    /// remote folder and upload state (`backup-<storageId>.json`, see
    /// `backup::state_path`) stay what they were, so nothing is uploaded
    /// twice. `pictures` is the user's Pictures folder (the legacy default).
    /// Returns whether anything changed.
    pub fn migrate(&mut self, pictures: Option<PathBuf>) -> bool {
        if self.migrated {
            return false;
        }
        self.migrated = true;
        let used = self.enabled || self.folder.is_some() || self.remote_folder.is_some();
        if used && !self.folders.iter().any(|f| f.kind == FolderKind::Photos) {
            if let Some(path) = self.folder.clone().or(pictures) {
                self.folders.push(BackupFolder {
                    folder_key: PHOTOS_KEY.into(),
                    label: "Photos".into(),
                    local_path: path,
                    kind: FolderKind::Photos,
                    enabled: self.enabled,
                    excludes: Vec::new(),
                    remote_folder: self.remote_folder.clone(),
                });
            }
        }
        true
    }

    pub fn folder(&self, key: &str) -> Option<&BackupFolder> {
        self.folders.iter().find(|f| f.folder_key == key)
    }
}

/// The folder key of the migrated photo backup.
pub const PHOTOS_KEY: &str = "photos";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Last panel signed in to.
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub org_id: Option<String>,
    /// storageId -> sync it? (absent = yes)
    #[serde(default)]
    pub drives: HashMap<String, bool>,
    /// storageId -> its sync root folder (chosen once, then kept).
    #[serde(default)]
    pub roots: HashMap<String, PathBuf>,
    /// storageId -> preferred drive letter (absent = first free from O:).
    #[serde(default)]
    pub letters: HashMap<String, char>,
    /// storageId -> the letter it is mapped to now (chosen or automatic).
    /// Kept when the app quits — the letter itself stays for the rest of the
    /// logon session — so it can be put back at the next sign-in before the
    /// app runs (`app::restore_letters`; the Companion's `--letters`).
    /// Cleared when a drive is removed from this PC and on sign-out.
    #[serde(default)]
    pub mapped: HashMap<String, char>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub backup: BackupSettings,
}

impl Config {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    pub fn load(dir: &Path) -> Config {
        std::fs::read(Self::path(dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, Self::path(dir))
    }

    pub fn wants_mounted(&self, storage_id: &str) -> bool {
        self.drives.get(storage_id).copied().unwrap_or(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Credentials {
    pub origin: String,
    pub token: String,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub user_email: Option<String>,
}

fn cred_path(dir: &Path) -> PathBuf {
    dir.join("credentials.bin")
}

pub fn load_credentials(dir: &Path) -> Option<Credentials> {
    let enc = std::fs::read(cred_path(dir)).ok()?;
    let plain = match crate::platform::dpapi_unprotect(&enc) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("stored credentials unreadable ({e}); sign in again");
            return None;
        }
    };
    serde_json::from_slice(&plain).ok()
}

pub fn save_credentials(dir: &Path, c: &Credentials) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let plain = serde_json::to_vec(c).map_err(|e| e.to_string())?;
    let enc = crate::platform::dpapi_protect(&plain)?;
    std::fs::write(cred_path(dir), enc).map_err(|e| e.to_string())
}

pub fn delete_credentials(dir: &Path) {
    let _ = std::fs::remove_file(cred_path(dir));
}

/// Parse `othcloud-drive://auth?code=…&origin=…` → (code, normalised origin).
pub fn parse_auth_url(url: &str) -> Result<(String, String), String> {
    let u = url::Url::parse(url.trim()).map_err(|e| format!("bad link: {e}"))?;
    if u.scheme() != crate::platform::URL_SCHEME {
        return Err(format!("not an {} link", crate::platform::URL_SCHEME));
    }
    // "othcloud-drive://auth?..." parses "auth" as the host; accept a path
    // form too ("othcloud-drive:auth?..." / "othcloud-drive:///auth?...").
    let action = u
        .host_str()
        .filter(|h| !h.is_empty())
        .map(|h| h.to_string())
        .unwrap_or_else(|| u.path().trim_matches('/').to_string());
    if action != "auth" {
        return Err(format!("unknown action {action:?}"));
    }
    let mut code = None;
    let mut origin = None;
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.to_string()),
            "origin" => origin = Some(v.to_string()),
            _ => {}
        }
    }
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or("link has no code")?;
    let origin = crate::api::normalize_origin(&origin.ok_or("link has no origin")?)?;
    Ok((code, origin))
}

/// Device name sent with the token exchange (≤100 chars per PROTOCOL.md).
pub fn device_name() -> String {
    let mut n = format!("OTHCloud Drive ({})", crate::util::computer_name());
    if n.chars().count() > 100 {
        n = n.chars().take(99).collect::<String>() + ")";
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_urls() {
        let (c, o) =
            parse_auth_url("othcloud-drive://auth?code=abc&origin=https%3A%2F%2Fothcloud.xyz%2F")
                .unwrap();
        assert_eq!(c, "abc");
        assert_eq!(o, "https://othcloud.xyz");
        let (_, o) =
            parse_auth_url("othcloud-drive://auth/?code=x&origin=http%3A%2F%2F127.0.0.1%3A3900")
                .unwrap();
        assert_eq!(o, "http://127.0.0.1:3900");
        assert!(parse_auth_url("othcloud-drive://auth?origin=https%3A%2F%2Fa").is_err());
        assert!(parse_auth_url("othcloud-drive://evil?code=x&origin=https%3A%2F%2Fa").is_err());
        assert!(parse_auth_url("https://auth?code=x&origin=https%3A%2F%2Fa").is_err());
        assert!(parse_auth_url("othcloud-drive://auth?code=x&origin=javascript%3Aalert").is_err());
    }

    #[test]
    fn config_and_credentials_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        assert!(c.wants_mounted("s"));
        c.drives.insert("s".into(), false);
        c.backup.enabled = true;
        c.letters.insert("s".into(), 'P');
        c.mapped.insert("s".into(), 'P');
        c.save(dir.path()).unwrap();
        let c2 = Config::load(dir.path());
        assert_eq!(c, c2);
        assert!(!c2.wants_mounted("s"));

        let cr = Credentials {
            origin: "https://x".into(),
            token: "secret-token".into(),
            user_name: Some("A".into()),
            user_email: None,
        };
        save_credentials(dir.path(), &cr).unwrap();
        let raw = std::fs::read(dir.path().join("credentials.bin")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("secret-token"));
        assert_eq!(load_credentials(dir.path()).unwrap(), cr);
        delete_credentials(dir.path());
        assert!(load_credentials(dir.path()).is_none());
    }

    #[test]
    fn photo_backup_migrates_once_into_a_photos_folder() {
        // A config written by the photo-backup-only build.
        let old = r#"{"origin":"https://x","backup":{"enabled":true,"folder":null,"storage_id":"stor_1","remote_folder":null}}"#;
        let mut c: Config = serde_json::from_str(old).unwrap();
        assert!(!c.backup.migrated && c.backup.folders.is_empty());
        assert!(c.backup.migrate(Some(PathBuf::from(r"C:\Users\me\Pictures"))));
        let f = &c.backup.folders[0];
        assert_eq!(f.folder_key, PHOTOS_KEY);
        assert_eq!(f.kind, FolderKind::Photos);
        assert!(f.enabled);
        assert_eq!(f.local_path, PathBuf::from(r"C:\Users\me\Pictures"));
        assert_eq!(f.remote_folder, None);
        assert_eq!(c.backup.storage_id.as_deref(), Some("stor_1"));
        // Once only; nothing duplicated.
        assert!(!c.backup.migrate(Some(PathBuf::from("x"))));
        assert_eq!(c.backup.folders.len(), 1);
        // Custom folder + remote folder carry over; off stays off.
        let mut b = BackupSettings {
            folder: Some(PathBuf::from(r"D:\Camera")),
            remote_folder: Some("Phone/Camera".into()),
            ..Default::default()
        };
        b.migrate(None);
        assert_eq!(b.folders[0].local_path, PathBuf::from(r"D:\Camera"));
        assert_eq!(b.folders[0].remote_folder.as_deref(), Some("Phone/Camera"));
        assert!(!b.folders[0].enabled);
        // Never used: nothing added.
        let mut n = BackupSettings::default();
        n.migrate(Some(PathBuf::from("p")));
        assert!(n.folders.is_empty() && n.migrated);
        // And the new fields survive a round trip.
        let back: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn device_name_is_short() {
        assert!(device_name().chars().count() <= 100);
        assert!(device_name().starts_with("OTHCloud Drive ("));
    }
}
