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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Local folder to watch; None = the user's Pictures.
    #[serde(default)]
    pub folder: Option<PathBuf>,
    /// Drive to upload into; None = the first drive.
    #[serde(default)]
    pub storage_id: Option<String>,
    /// Remote folder path ("a/b"); None = "Photo backup/<PC name>".
    #[serde(default)]
    pub remote_folder: Option<String>,
}

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
    fn device_name_is_short() {
        assert!(device_name().chars().count() <= 100);
        assert!(device_name().starts_with("OTHCloud Drive ("));
    }
}
