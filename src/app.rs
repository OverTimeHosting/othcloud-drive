//! The app's state and actions (sign in/out, org, sync/unsync drives,
//! pause, photo backup), shared by the tray UI and the headless `mount`.

use crate::api::{Api, DriveInfo, Org};
use crate::backup::{BackupHandle, BackupJob};
use crate::config::{self, Config, Credentials};
use crate::engine::{Engine, EngineOptions};
use crate::{letters, platform};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// One synced drive: its engine, folder and (optional) drive letter.
pub struct Mounted {
    pub engine: Arc<Engine>,
    pub root: PathBuf,
    pub letter: Option<char>,
}

pub struct App {
    pub cfg: Config,
    pub cfg_dir: PathBuf,
    pub data_dir: PathBuf,
    pub creds: Option<Credentials>,
    pub api: Option<Api>,
    pub orgs: Vec<Org>,
    pub drives: Vec<DriveInfo>,
    pub mounts: HashMap<String, Mounted>,
    pub paused: Arc<AtomicBool>,
    pub backup: Option<BackupHandle>,
    pub last_error: Option<String>,
}

/// `%USERPROFILE%\OTHCloud` (override: OTHCLOUD_DRIVE_ROOT_BASE).
pub fn roots_base() -> PathBuf {
    if let Ok(b) = std::env::var("OTHCLOUD_DRIVE_ROOT_BASE") {
        return PathBuf::from(b);
    }
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("C:\\"))
        .join("OTHCloud")
}

/// A folder name for a drive: Windows-safe, no trailing dots/spaces.
pub fn folder_name(drive_name: &str) -> String {
    if drive_name.trim().is_empty() {
        return "Drive".into();
    }
    let mut s = crate::util::display_name(drive_name.trim());
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    let upper = s.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if s.is_empty() || RESERVED.contains(&base) {
        s = format!("Drive {s}");
    }
    s
}

/// The sync root folder to use for a drive (kept stable in the settings).
pub fn root_for(cfg: &mut Config, d: &DriveInfo) -> PathBuf {
    if let Some(r) = cfg.roots.get(&d.storage_id) {
        return r.clone();
    }
    let base = roots_base();
    let name = folder_name(&d.name);
    let mut cand = base.join(&name);
    let mut n = 2;
    while cfg.roots.values().any(|r| r.eq(&cand)) {
        cand = base.join(format!("{name} ({n})"));
        n += 1;
    }
    cfg.roots.insert(d.storage_id.clone(), cand.clone());
    cand
}

/// Start syncing drive `d` into `root`, and alias `root` as a drive letter
/// (`letter` = None: first free from O:, or none if `want_letter` false).
#[allow(clippy::too_many_arguments)]
pub fn mount_drive(
    api: Api,
    d: &DriveInfo,
    root: &Path,
    letter: Option<char>,
    want_letter: bool,
    taken: &[char],
    data_dir: &Path,
    paused: Arc<AtomicBool>,
    opts: EngineOptions,
) -> Result<Mounted, String> {
    let engine = Engine::start(api, &d.storage_id, &d.name, root, crate::engine::DEFAULT_ICON, paused, opts)?;
    let mut m = Mounted {
        root: engine.root.clone(),
        engine,
        letter: None,
    };
    if want_letter {
        let l = letter.or_else(|| letters::choose(&m.root, taken));
        match l {
            Some(l) => match letters::map(l, &m.root) {
                Ok(()) => {
                    m.letter = Some(l);
                    if let Err(e) = platform::set_drive_icon(l, None, &d.name) {
                        tracing::warn!("drive icon for {l}: {e}");
                    }
                }
                Err(e) => tracing::warn!("{}: no drive letter: {e}", d.name),
            },
            None => tracing::warn!("{}: no free drive letter", d.name),
        }
    }
    // The cover becomes the letter's and the sync root's icon, and is
    // kept current by the poll loop.
    m.engine.attach_icons(m.letter, data_dir);
    tracing::info!(
        "{} synced at {}{}",
        d.name,
        m.root.display(),
        m.letter.map(|l| format!(" ({l}:)")).unwrap_or_default()
    );
    Ok(m)
}

/// Stop syncing: flush, disconnect, remove the letter. With `remove`, also
/// unregister the sync root and delete the placeholders (unsynced files
/// are moved to `aside_base` first).
pub fn unmount_drive(m: Mounted, remove: bool, aside_base: &Path) -> Result<Option<PathBuf>, String> {
    if let Some(l) = m.letter {
        letters::unmap(l, &m.root);
        platform::remove_drive_icon(l);
    }
    m.engine.shutdown(Duration::from_secs(30));
    if remove {
        m.engine.remove_local(aside_base)
    } else {
        Ok(None)
    }
}

impl App {
    pub fn load() -> App {
        let cfg_dir = platform::config_dir();
        let data_dir = platform::data_dir();
        let cfg = Config::load(&cfg_dir);
        let creds = config::load_credentials(&cfg_dir);
        let paused = Arc::new(AtomicBool::new(cfg.paused));
        App {
            cfg,
            cfg_dir,
            data_dir,
            creds,
            api: None,
            orgs: Vec::new(),
            drives: Vec::new(),
            mounts: HashMap::new(),
            paused,
            backup: None,
            last_error: None,
        }
    }

    pub fn signed_in(&self) -> bool {
        self.creds.is_some()
    }

    pub fn save_cfg(&self) {
        if let Err(e) = self.cfg.save(&self.cfg_dir) {
            tracing::warn!("saving settings: {e}");
        }
    }

    pub fn sign_in_origin(&self) -> String {
        self.cfg
            .origin
            .clone()
            .or_else(|| self.creds.as_ref().map(|c| c.origin.clone()))
            .unwrap_or_else(config::default_origin)
    }

    pub fn trusted_origin(&self, origin: &str) -> bool {
        origin == self.sign_in_origin() || origin == config::default_origin()
    }

    pub fn sign_in_with_code(&mut self, code: &str, origin: &str) -> Result<(), String> {
        let r = Api::exchange_code(origin, code, &config::device_name())
            .map_err(|e| format!("sign-in failed: {e}"))?;
        let user = &r.user;
        let creds = Credentials {
            origin: origin.to_string(),
            token: r.token,
            user_name: user.get("name").and_then(|v| v.as_str()).map(String::from),
            user_email: user.get("email").and_then(|v| v.as_str()).map(String::from),
        };
        config::save_credentials(&self.cfg_dir, &creds)?;
        if self.cfg.origin.as_deref() != Some(origin) {
            // Another panel/account: remove the old account's folders first.
            self.remove_all_local();
            self.cfg.org_id = None;
            self.cfg.drives.clear();
            self.cfg.backup.storage_id = None;
        }
        self.cfg.origin = Some(origin.to_string());
        self.save_cfg();
        self.creds = Some(creds);
        tracing::info!("signed in to {origin}");
        self.connect()
    }

    /// Sign out: stop everything, unregister every sync root and delete the
    /// placeholders (anything not uploaded is moved aside first).
    pub fn sign_out(&mut self) {
        self.stop_backup();
        self.remove_all_local();
        config::delete_credentials(&self.cfg_dir);
        self.creds = None;
        self.api = None;
        self.orgs.clear();
        self.drives.clear();
        tracing::info!("signed out");
    }

    fn remove_all_local(&mut self) {
        let base = roots_base();
        let mounts: Vec<Mounted> = self.mounts.drain().map(|(_, m)| m).collect();
        for m in mounts {
            match unmount_drive(m, true, &base) {
                Ok(Some(aside)) => {
                    self.last_error = Some(format!("Files that weren't uploaded were moved to {}", aside.display()));
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("{e}");
                    self.last_error = Some(e);
                }
            }
        }
        // Sync roots registered earlier but not running now.
        for (id, root) in crate::cf::our_sync_roots() {
            if root.as_os_str().is_empty() {
                let _ = crate::cf::unregister_sync_root(&id);
                continue;
            }
            let label = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if let Err(e) = crate::engine::remove_root(&id, &root, &label, &base) {
                tracing::error!("{e}");
            }
        }
        self.cfg.roots.clear();
        self.save_cfg();
    }

    pub fn connect(&mut self) -> Result<(), String> {
        let Some(c) = self.creds.clone() else {
            return Err("not signed in".into());
        };
        let base = Api::new(&c.origin, &c.token, None);
        let orgs = base.orgs().map_err(|e| {
            if e.is_auth() {
                "your sign-in has expired or was revoked — sign in again".to_string()
            } else {
                format!("can't reach {}: {e}", c.origin)
            }
        })?;
        if orgs.is_empty() {
            return Err("your account isn't in any organization".into());
        }
        let org = self
            .cfg
            .org_id
            .clone()
            .filter(|o| orgs.iter().any(|x| &x.id == o))
            .unwrap_or_else(|| orgs[0].id.clone());
        self.cfg.org_id = Some(org.clone());
        self.save_cfg();
        self.orgs = orgs;
        self.api = Some(base.with_org(&org));
        self.refresh_drives()?;
        self.apply_mounts();
        self.restart_backup();
        Ok(())
    }

    pub fn refresh_drives(&mut self) -> Result<(), String> {
        let api = self.api.clone().ok_or("not signed in")?;
        self.drives = api.drives().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_org(&mut self, org: &str) -> Result<(), String> {
        if self.cfg.org_id.as_deref() == Some(org) {
            return Ok(());
        }
        self.stop_backup();
        self.remove_all_local();
        self.cfg.org_id = Some(org.to_string());
        self.cfg.drives.clear();
        self.cfg.backup.storage_id = None;
        self.save_cfg();
        self.connect()
    }

    pub fn apply_mounts(&mut self) {
        let wanted: Vec<DriveInfo> = self
            .drives
            .iter()
            .filter(|d| self.cfg.wants_mounted(&d.storage_id))
            .filter(|d| !self.mounts.contains_key(&d.storage_id))
            .cloned()
            .collect();
        for d in wanted {
            if let Err(e) = self.mount(&d.storage_id) {
                tracing::error!("sync {}: {e}", d.name);
                self.last_error = Some(format!("{}: {e}", d.name));
            }
        }
        let gone: Vec<String> = self
            .mounts
            .keys()
            .filter(|s| !self.drives.iter().any(|d| &d.storage_id == *s))
            .cloned()
            .collect();
        for s in gone {
            self.disable(&s);
        }
    }

    pub fn mount(&mut self, storage_id: &str) -> Result<char, String> {
        if let Some(m) = self.mounts.get(storage_id) {
            return m.letter.ok_or_else(|| "synced (no drive letter)".into());
        }
        let api = self.api.clone().ok_or("not signed in")?;
        let d = self
            .drives
            .iter()
            .find(|d| d.storage_id == storage_id)
            .cloned()
            .ok_or("no such drive")?;
        let root = root_for(&mut self.cfg, &d);
        let taken: Vec<char> = self.mounts.values().filter_map(|m| m.letter).collect();
        let m = mount_drive(
            api,
            &d,
            &root,
            None,
            true,
            &taken,
            &self.data_dir,
            self.paused.clone(),
            EngineOptions::default(),
        )?;
        let letter = m.letter;
        self.cfg.drives.insert(storage_id.to_string(), true);
        self.save_cfg();
        self.mounts.insert(storage_id.to_string(), m);
        letter.ok_or_else(|| "synced (no free drive letter)".into())
    }

    /// Stop syncing a drive and remove its local folder (unsynced files are
    /// moved aside), and remember the choice.
    pub fn disable(&mut self, storage_id: &str) {
        if let Some(m) = self.mounts.remove(storage_id) {
            if let Err(e) = unmount_drive(m, true, &roots_base()) {
                self.last_error = Some(e);
            }
        }
        self.cfg.roots.remove(storage_id);
        self.cfg.drives.insert(storage_id.to_string(), false);
        self.save_cfg();
    }

    /// Quit: flush and disconnect every drive, remove the letters (the sync
    /// roots stay registered and the files stay where they are).
    pub fn unmount_all(&mut self) {
        let mounts: Vec<Mounted> = self.mounts.drain().map(|(_, m)| m).collect();
        let base = roots_base();
        let handles: Vec<_> = mounts
            .into_iter()
            .map(|m| {
                let b = base.clone();
                std::thread::spawn(move || unmount_drive(m, false, &b))
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
    }

    pub fn set_paused(&mut self, p: bool) {
        self.paused.store(p, Ordering::SeqCst);
        self.cfg.paused = p;
        self.save_cfg();
        if !p {
            for m in self.mounts.values() {
                m.engine.poke();
                m.engine.expedite();
            }
            if let Some(b) = &self.backup {
                b.kick();
            }
        }
    }

    // ------------------------------------------------------------- backup

    pub fn backup_folder(&self) -> Option<PathBuf> {
        self.cfg.backup.folder.clone().or_else(platform::pictures_dir)
    }

    pub fn backup_drive(&self) -> Option<&DriveInfo> {
        match &self.cfg.backup.storage_id {
            Some(s) => self.drives.iter().find(|d| &d.storage_id == s),
            None => self.drives.first(),
        }
    }

    pub fn stop_backup(&mut self) {
        if let Some(mut b) = self.backup.take() {
            b.stop();
        }
    }

    pub fn restart_backup(&mut self) {
        self.stop_backup();
        if !self.cfg.backup.enabled {
            return;
        }
        let (Some(api), Some(folder), Some(drive)) =
            (self.api.clone(), self.backup_folder(), self.backup_drive().cloned())
        else {
            return;
        };
        let remote: Vec<String> = match &self.cfg.backup.remote_folder {
            Some(r) => r.split('/').filter(|s| !s.is_empty()).map(String::from).collect(),
            None => vec!["Photo backup".into(), crate::util::computer_name()],
        };
        let job = BackupJob {
            folder,
            storage_id: drive.storage_id.clone(),
            remote,
            state_path: self.data_dir.join(format!("backup-{}.json", drive.storage_id)),
            settle: Duration::from_secs(3),
        };
        match BackupHandle::start(api, job, self.paused.clone()) {
            Ok(h) => self.backup = Some(h),
            Err(e) => {
                tracing::error!("photo backup: {e}");
                self.last_error = Some(format!("Photo backup: {e}"));
            }
        }
    }

    pub fn set_backup_enabled(&mut self, on: bool) {
        self.cfg.backup.enabled = on;
        self.save_cfg();
        self.restart_backup();
    }

    pub fn set_backup_folder(&mut self, f: PathBuf) {
        self.cfg.backup.folder = Some(f);
        self.save_cfg();
        self.restart_backup();
    }

    pub fn set_backup_drive(&mut self, storage_id: &str) {
        self.cfg.backup.storage_id = Some(storage_id.to_string());
        self.save_cfg();
        self.restart_backup();
    }

    pub fn status_line(&self) -> String {
        if !self.signed_in() {
            return "Not signed in".into();
        }
        if self.api.is_none() {
            return match &self.last_error {
                Some(e) => format!("Offline: {e}"),
                None => "Connecting…".into(),
            };
        }
        if self.paused.load(Ordering::Relaxed) {
            return "Sync paused".into();
        }
        let mut pending = 0;
        let mut offline = false;
        let mut busy = false;
        for m in self.mounts.values() {
            let s = m.engine.status();
            pending += s.pending;
            offline |= !s.online;
            busy |= s.busy;
        }
        if offline {
            "Offline — changes will upload when back online".into()
        } else if busy || pending > 0 {
            format!("Syncing ({} waiting)", pending + usize::from(busy))
        } else {
            "Up to date".into()
        }
    }

    pub fn shutdown(&mut self) {
        self.stop_backup();
        self.unmount_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names() {
        assert_eq!(folder_name("Alice Files"), "Alice Files");
        assert_eq!(folder_name("a:b?"), "a\u{FF1A}b\u{FF1F}");
        assert_eq!(folder_name("trailing. "), "trailing");
        assert_eq!(folder_name("CON"), "Drive CON");
        assert_eq!(folder_name(""), "Drive");
    }
}
