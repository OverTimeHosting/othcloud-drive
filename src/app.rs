//! The app's state and actions (sign in/out, org, sync/unsync drives,
//! pause, photo backup), shared by the tray UI and the headless `mount`.

use crate::api::{Api, DriveInfo, Org};
use crate::backup::{self, BackupService, ServiceOptions};
use crate::config::{self, BackupFolder, Config, Credentials, FolderKind};
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
    pub backup: Option<BackupService>,
    pub last_error: Option<String>,
    /// Sent as `appVersion` when the backup registers this device (an
    /// embedding app puts its own here).
    pub client_version: String,
    /// Timings of the folder backup (tests shorten them).
    pub backup_options: ServiceOptions,
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
    // Register with the icon this drive already has, if any, so Explorer
    // doesn't flash the generic icon until the first check (attach_icons).
    let icon = newest_icon(data_dir, &d.storage_id)
        .map(|p| format!("{},0", p.display()))
        .unwrap_or_else(|| crate::engine::DEFAULT_ICON.to_string());
    let engine = Engine::start(api, &d.storage_id, &d.name, root, &icon, paused, opts)?;
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

/// The app is quitting (or was signed out by the panel): flush and
/// disconnect, and leave EVERYTHING Explorer shows in place — the sync root
/// stays registered (sidebar entry, placeholders, downloaded files openable
/// offline) and so do the drive letter and its label/icon, which live until
/// logoff whatever happens to this process.
pub fn stop_drive(m: Mounted) {
    m.engine.shutdown(Duration::from_secs(30));
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
        App::load_in(platform::config_dir(), platform::data_dir())
    }

    /// Like [`App::load`], with the settings and data directories chosen by
    /// the caller (an app that embeds this engine keeps its own folders).
    pub fn load_in(cfg_dir: PathBuf, data_dir: PathBuf) -> App {
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
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            backup_options: ServiceOptions::default(),
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
        self.use_credentials(creds, true)
    }

    /// Sign in with a token obtained elsewhere (an app that embeds this
    /// engine and owns sign-in, e.g. OTHCloud Companion). The token is kept
    /// in memory only — nothing is written to `credentials.bin`; the
    /// embedding app stores it. Unlike a code sign-in, a first sign-in here
    /// (no panel remembered yet) leaves existing sync roots alone; only a
    /// switch to another panel removes the old one's folders.
    pub fn sign_in_with_token(&mut self, creds: Credentials) -> Result<(), String> {
        self.use_credentials(creds, false)
    }

    fn use_credentials(&mut self, creds: Credentials, clean_first: bool) -> Result<(), String> {
        let origin = creds.origin.clone();
        let origin = origin.as_str();
        let switching = match self.cfg.origin.as_deref() {
            Some(o) => o != origin,
            None => clean_first,
        };
        if switching {
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
        // Letters of drives that weren't running (put back at sign-in, or
        // left by a previous run).
        let ids: Vec<String> = self.cfg.mapped.keys().cloned().collect();
        for id in ids {
            self.drop_letter(&id);
        }
        crate::thumbs::clear_roots();
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
        self.cfg.mapped.clear();
        self.save_cfg();
    }

    /// Forget a drive's letter and take it off this PC (only if it still
    /// aliases the drive's folder: a letter something else took is left be).
    fn drop_letter(&mut self, storage_id: &str) {
        let Some(l) = self.cfg.mapped.remove(storage_id) else { return };
        if let Some(root) = self.cfg.roots.get(storage_id) {
            if letters::points_to(l, root) {
                letters::unmap(l, root);
                platform::remove_drive_icon(l);
            }
        }
    }

    /// Serve Explorer's thumbnails for the drives that were synced last time
    /// straight away, before any network: the pipe's handler needs only the
    /// token, the organization and the placeholders on disk. This is what
    /// lets a thumbnail request that STARTED the app (OTHCloud Companion's
    /// handler launches it when nothing answers) be answered within the
    /// handler's wait, long before the drives are connected. Each `mount`
    /// later replaces its entry with the live engine's.
    pub fn prime_thumbnails(&self, creds: &Credentials) {
        if crate::cf::thumbnail_provider().is_none() {
            return;
        }
        // Folders of another panel/account are never answered for.
        let (Some(org), Some(origin)) = (self.cfg.org_id.as_deref(), self.cfg.origin.as_deref()) else {
            return;
        };
        if origin != creds.origin {
            return;
        }
        let api = Api::new(&creds.origin, &creds.token, None).with_org(org);
        let registered = crate::cf::our_sync_roots();
        let mut n = 0;
        for sid in self.cfg.roots.keys() {
            if !self.cfg.wants_mounted(sid) || self.mounts.contains_key(sid) {
                continue;
            }
            let id = crate::cf::sync_root_id(crate::cf::PROVIDER, sid);
            if let Some((_, path)) = registered.iter().find(|(i, p)| *i == id && !p.as_os_str().is_empty()) {
                crate::thumbs::add_root(path, api.clone());
                n += 1;
            }
        }
        if n > 0 {
            crate::thumbs::ensure_serving();
            tracing::info!("thumbnails: answering for {n} drive(s) before they connect");
        }
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
        self.apply_backup();
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
        // A chosen letter, if it is still free (or already ours); else auto.
        let preferred = self.cfg.letters.get(storage_id).copied().filter(|l| {
            !taken.contains(l) && (!platform::letter_in_use(*l) || letters::points_to(*l, &root))
        });
        let m = mount_drive(
            api,
            &d,
            &root,
            preferred,
            true,
            &taken,
            &self.data_dir,
            self.paused.clone(),
            EngineOptions::default(),
        )?;
        let letter = m.letter;
        self.cfg.drives.insert(storage_id.to_string(), true);
        match letter {
            Some(l) => self.cfg.mapped.insert(storage_id.to_string(), l),
            None => self.cfg.mapped.remove(storage_id),
        };
        self.save_cfg();
        self.mounts.insert(storage_id.to_string(), m);
        letter.ok_or_else(|| "synced (no free drive letter)".into())
    }

    /// Choose the drive letter for a drive (None = automatic). If the drive
    /// is synced under another letter it is remounted (files stay put).
    pub fn set_letter(&mut self, storage_id: &str, letter: Option<char>) -> Result<(), String> {
        let letter = letter.map(|c| c.to_ascii_uppercase());
        if let Some(l) = letter {
            if !('D'..='Z').contains(&l) {
                return Err(format!("{l}: is not a drive letter that can be used"));
            }
        }
        match letter {
            Some(l) => self.cfg.letters.insert(storage_id.to_string(), l),
            None => self.cfg.letters.remove(storage_id),
        };
        self.save_cfg();
        let remount = match (self.mounts.get(storage_id), letter) {
            (Some(m), Some(l)) => m.letter != Some(l),
            _ => false,
        };
        if remount {
            if let Some(m) = self.mounts.remove(storage_id) {
                unmount_drive(m, false, &roots_base())?;
            }
            self.mount(storage_id)?;
        }
        Ok(())
    }

    /// Stop syncing a drive and remove its local folder (unsynced files are
    /// moved aside), and remember the choice.
    pub fn disable(&mut self, storage_id: &str) {
        if let Some(m) = self.mounts.remove(storage_id) {
            if let Err(e) = unmount_drive(m, true, &roots_base()) {
                self.last_error = Some(e);
            }
        }
        self.drop_letter(storage_id);
        self.cfg.roots.remove(storage_id);
        self.cfg.drives.insert(storage_id.to_string(), false);
        self.save_cfg();
    }

    /// Quit: flush and disconnect every drive. Nothing Explorer shows goes
    /// away: the sync roots stay registered, the files stay where they are
    /// and the drive letters stay mapped (see [`stop_drive`]).
    pub fn unmount_all(&mut self) {
        let mounts: Vec<Mounted> = self.mounts.drain().map(|(_, m)| m).collect();
        let handles: Vec<_> = mounts.into_iter().map(|m| std::thread::spawn(move || stop_drive(m))).collect();
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

    /// The photos folder's local folder (the legacy photo backup's), or the
    /// user's Pictures.
    pub fn backup_folder(&self) -> Option<PathBuf> {
        self.cfg
            .backup
            .folders
            .iter()
            .find(|f| f.kind == FolderKind::Photos)
            .map(|f| f.local_path.clone())
            .or_else(|| self.cfg.backup.folder.clone())
            .or_else(platform::pictures_dir)
    }

    /// The drive backups go into (chosen explicitly; none = nothing runs).
    pub fn backup_drive(&self) -> Option<&DriveInfo> {
        let s = self.cfg.backup.storage_id.as_deref()?;
        self.drives.iter().find(|d| d.storage_id == s)
    }

    /// The photo backup is on.
    pub fn photo_backup_enabled(&self) -> bool {
        self.cfg.backup.folders.iter().any(|f| f.kind == FolderKind::Photos && f.enabled)
    }

    pub fn stop_backup(&mut self) {
        if let Some(mut b) = self.backup.take() {
            b.stop(Duration::from_secs(3));
        }
    }

    /// Folders a backup may never be inside of: every drive's folder of
    /// ours (registered or remembered) and the folder they live in.
    pub fn backup_forbidden_roots(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.cfg.roots.values().cloned().collect();
        v.extend(self.mounts.values().map(|m| m.root.clone()));
        v.extend(crate::cf::our_sync_roots().into_iter().map(|(_, p)| p).filter(|p| !p.as_os_str().is_empty()));
        v.push(roots_base());
        v
    }

    /// Why `path` can't be backed up (PROTOCOL.md "Folder backup"), or the
    /// path to keep: an existing local folder, not one of our drives or
    /// inside one, not Windows' own, not inside or around a folder that is
    /// already backed up (`except`: the folder being changed).
    pub fn backup_check_path(&self, path: &Path, except: Option<&str>) -> Result<PathBuf, String> {
        let p = backup::check_backup_root(path, &self.backup_forbidden_roots(), &platform::system_dirs())?;
        for f in self.cfg.backup.folders.iter().filter(|f| Some(f.folder_key.as_str()) != except) {
            if backup::is_within(&p, &f.local_path) {
                return Err(format!("That folder is already backed up as part of \"{}\"", f.label));
            }
            if backup::is_within(&f.local_path, &p) {
                return Err(format!("\"{}\" is inside that folder — remove it first, then add this one", f.label));
            }
        }
        Ok(p)
    }

    fn unique_label(&self, want: &str, except: Option<&str>) -> String {
        let taken: Vec<String> = self
            .cfg
            .backup
            .folders
            .iter()
            .filter(|f| Some(f.folder_key.as_str()) != except)
            .map(|f| f.label.to_lowercase())
            .collect();
        if !taken.contains(&want.to_lowercase()) {
            return want.to_string();
        }
        (2..).map(|n| format!("{want} ({n})")).find(|c| !taken.contains(&c.to_lowercase())).unwrap()
    }

    /// The settings the backup runs with, or None when it can't run (not
    /// signed in, no drive chosen, nothing configured).
    fn backup_service_config(&mut self) -> Option<backup::ServiceConfig> {
        let sid = self.backup_drive()?.storage_id.clone();
        if self.cfg.backup.folders.is_empty() && self.backup.is_none() {
            return None;
        }
        let device_key = match &self.cfg.backup.device_key {
            Some(k) => k.clone(),
            None => {
                let k = platform::random_hex(16);
                self.cfg.backup.device_key = Some(k.clone());
                self.save_cfg();
                k
            }
        };
        Some(backup::ServiceConfig {
            storage_id: sid,
            device_key,
            device_name: crate::util::computer_name(),
            app_version: self.client_version.clone(),
            os_version: platform::os_version(),
            folders: self.cfg.backup.folders.clone(),
            paused: self.cfg.backup.paused,
            data_dir: self.data_dir.clone(),
            skip_inside: self.backup_forbidden_roots(),
        })
    }

    /// Start, reconfigure (between files, no restart) or stop the backup to
    /// match the settings. Also runs the one-time photo backup migration.
    pub fn apply_backup(&mut self) {
        if !self.cfg.backup.migrated {
            let legacy_on = self.cfg.backup.enabled;
            self.cfg.backup.migrate(platform::pictures_dir());
            // The photo backup used the first drive when none was chosen.
            if legacy_on && self.cfg.backup.storage_id.is_none() {
                self.cfg.backup.storage_id = self.drives.first().map(|d| d.storage_id.clone());
            }
            self.save_cfg();
            if let Some(f) = self.cfg.backup.folder(crate::config::PHOTOS_KEY) {
                tracing::info!(
                    "photo backup is now the folder backup \"{}\" ({}, {})",
                    f.label,
                    f.local_path.display(),
                    if f.enabled { "on" } else { "off" }
                );
            }
        }
        let Some(api) = self.api.clone() else {
            self.stop_backup();
            return;
        };
        let Some(cfg) = self.backup_service_config() else {
            self.stop_backup();
            return;
        };
        match &self.backup {
            Some(b) if b.storage_id == cfg.storage_id => b.reconfigure(cfg),
            _ => {
                self.stop_backup();
                self.backup = Some(BackupService::start(api, cfg, self.paused.clone(), self.backup_options.clone()));
            }
        }
    }

    /// Kept for callers of the photo-backup era (the standalone tray).
    pub fn restart_backup(&mut self) {
        self.apply_backup();
    }

    /// Choose the drive backups go into.
    pub fn set_backup_drive(&mut self, storage_id: &str) {
        self.cfg.backup.storage_id = Some(storage_id.to_string());
        self.save_cfg();
        self.apply_backup();
    }

    /// Add a folder to back up. Returns its key.
    pub fn backup_add_folder(&mut self, path: &Path, label: Option<&str>, kind: FolderKind) -> Result<String, String> {
        let path = self.backup_check_path(path, None)?;
        if kind == FolderKind::Photos && self.cfg.backup.folders.iter().any(|f| f.kind == FolderKind::Photos) {
            return Err("Photo backup is already set up — change its folder instead".into());
        }
        let want = match label {
            Some(l) => backup::check_label(l)?,
            None => backup::check_label(
                &path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Folder".into()),
            )
            .unwrap_or_else(|_| "Folder".into()),
        };
        let label = self.unique_label(&want, None);
        let key = if kind == FolderKind::Photos {
            crate::config::PHOTOS_KEY.to_string()
        } else {
            platform::random_hex(8)
        };
        tracing::info!("backup: added {} as \"{label}\"", path.display());
        self.cfg.backup.folders.push(BackupFolder {
            folder_key: key.clone(),
            label,
            local_path: path,
            kind,
            enabled: true,
            excludes: Vec::new(),
            remote_folder: None,
        });
        self.save_cfg();
        self.apply_backup();
        Ok(key)
    }

    /// Change a folder: label, on/off, exclude patterns, local folder.
    pub fn backup_update_folder(
        &mut self,
        key: &str,
        label: Option<&str>,
        enabled: Option<bool>,
        excludes: Option<Vec<String>>,
        path: Option<&Path>,
    ) -> Result<(), String> {
        let label = match label {
            Some(l) => Some(self.unique_label(&backup::check_label(l)?, Some(key))),
            None => None,
        };
        let excludes = excludes.map(|e| backup::clean_excludes(&e)).transpose()?;
        let path = match path {
            Some(p) => Some(self.backup_check_path(p, Some(key))?),
            None => None,
        };
        let f = self
            .cfg
            .backup
            .folders
            .iter_mut()
            .find(|f| f.folder_key == key)
            .ok_or("No such backup folder")?;
        if let Some(l) = label {
            f.label = l;
        }
        if let Some(e) = enabled {
            f.enabled = e;
        }
        if let Some(x) = excludes {
            f.excludes = x;
        }
        if let Some(p) = path {
            f.local_path = p;
        }
        let photos = f.kind == FolderKind::Photos;
        let (on, folder) = (f.enabled, f.local_path.clone());
        if photos {
            // An older build reading this file keeps doing the same.
            self.cfg.backup.enabled = on;
            self.cfg.backup.folder = Some(folder);
        }
        self.save_cfg();
        self.apply_backup();
        Ok(())
    }

    /// Stop backing up a folder. What was uploaded stays in the drive; the
    /// folder's upload state on this PC is deleted.
    pub fn backup_remove_folder(&mut self, key: &str) -> Result<(), String> {
        let i = self
            .cfg
            .backup
            .folders
            .iter()
            .position(|f| f.folder_key == key)
            .ok_or("No such backup folder")?;
        let f = self.cfg.backup.folders.remove(i);
        if f.kind == FolderKind::Photos {
            self.cfg.backup.enabled = false;
        }
        self.save_cfg();
        self.apply_backup();
        if let Some(sid) = &self.cfg.backup.storage_id {
            let _ = std::fs::remove_file(backup::state_path(&self.data_dir, sid, key));
        }
        tracing::info!("backup: removed \"{}\" ({})", f.label, f.local_path.display());
        Ok(())
    }

    /// Pause / resume the backup alone.
    pub fn set_backup_paused(&mut self, p: bool) {
        self.cfg.backup.paused = p;
        self.save_cfg();
        self.apply_backup();
        if let Some(b) = &self.backup {
            b.kick();
        }
    }

    /// The photo backup on/off (the standalone tray's toggle). Turning it on
    /// with no photos folder yet adds one for Pictures.
    pub fn set_backup_enabled(&mut self, on: bool) {
        if self.cfg.backup.storage_id.is_none() {
            self.cfg.backup.storage_id = self.drives.first().map(|d| d.storage_id.clone());
        }
        let key = self
            .cfg
            .backup
            .folders
            .iter()
            .find(|f| f.kind == FolderKind::Photos)
            .map(|f| f.folder_key.clone());
        let r = match (key, on) {
            (Some(k), _) => self.backup_update_folder(&k, None, Some(on), None, None),
            (None, true) => match self.backup_folder() {
                Some(p) => self.backup_add_folder(&p, Some("Photos"), FolderKind::Photos).map(|_| ()),
                None => Err("no Pictures folder".into()),
            },
            (None, false) => Ok(()),
        };
        if let Err(e) = r {
            self.last_error = Some(format!("Photo backup: {e}"));
        }
    }

    /// The photo backup's folder (the standalone tray).
    pub fn set_backup_folder(&mut self, f: PathBuf) {
        let key = self
            .cfg
            .backup
            .folders
            .iter()
            .find(|x| x.kind == FolderKind::Photos)
            .map(|x| x.folder_key.clone());
        let r = match key {
            Some(k) => self.backup_update_folder(&k, None, None, None, Some(&f)),
            None => {
                self.cfg.backup.folder = Some(f);
                self.save_cfg();
                Ok(())
            }
        };
        if let Err(e) = r {
            self.last_error = Some(format!("Photo backup: {e}"));
        }
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
        crate::thumbs::clear_roots();
    }
}

/// What [`restore_letters`] did for one drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LetterRestore {
    /// Mapped now.
    Mapped(char, PathBuf),
    /// Already mapped there (the app, or an earlier run, did it).
    Already(char, PathBuf),
    /// Something else has that letter now; the app picks another when it runs.
    Taken(char),
    /// Not a drive to restore (removed from this PC, not one of our sync
    /// roots any more, or its folder is gone).
    Skipped(String),
    Failed(char, String),
}

/// Put the drive letters back as they were when the app last ran: at logon,
/// before (or without) the app, and again at the app's own start. Only for a
/// drive the settings still want on this PC whose folder is a sync root of
/// ours that exists; only onto a letter that is free. Never creates a sync
/// root, a folder or a letter of its own choosing, and makes no network call
/// — this is the whole of the Companion's `--letters` launch.
pub fn restore_letters(cfg_dir: &Path, data_dir: &Path) -> Vec<LetterRestore> {
    let cfg = Config::load(cfg_dir);
    let r = restore_letters_from(&cfg, &crate::cf::our_sync_roots(), crate::cf::PROVIDER, data_dir);
    // `--letters` exits right after this: send the batched shell events now.
    platform::flush_shell_notifications();
    r
}

pub fn restore_letters_from(
    cfg: &Config,
    registered: &[(String, PathBuf)],
    provider: &str,
    data_dir: &Path,
) -> Vec<LetterRestore> {
    let mut ids: Vec<(&String, &char)> = cfg.mapped.iter().collect();
    ids.sort();
    let mut out = Vec::new();
    for (sid, &l) in ids {
        let l = l.to_ascii_uppercase();
        if !('D'..='Z').contains(&l) {
            out.push(LetterRestore::Skipped(format!("{sid}: {l}: is not a usable letter")));
            continue;
        }
        let Some(root) = cfg.roots.get(sid) else {
            out.push(LetterRestore::Skipped(format!("{sid}: no folder")));
            continue;
        };
        if !cfg.wants_mounted(sid) {
            out.push(LetterRestore::Skipped(format!("{sid}: not on this PC")));
            continue;
        }
        let id = crate::cf::sync_root_id(provider, sid);
        let same = |p: &Path| {
            p.display().to_string().trim_end_matches('\\').eq_ignore_ascii_case(root.display().to_string().trim_end_matches('\\'))
        };
        if !registered.iter().any(|(i, p)| *i == id && same(p)) || !root.is_dir() {
            out.push(LetterRestore::Skipped(format!("{sid}: {} is not a sync root of ours", root.display())));
            continue;
        }
        if letters::points_to(l, root) {
            out.push(LetterRestore::Already(l, root.clone()));
            continue;
        }
        if letters::target(l).is_some() || platform::letter_in_use(l) {
            // Our label/icon must not dress up whatever has it now — unless
            // that is another of our drives.
            let ours = letters::root_of(l)
                .is_some_and(|t| cfg.roots.values().any(|r| letters::points_to(l, r)) || t.starts_with(roots_base()));
            if !ours {
                platform::remove_drive_icon(l);
            }
            out.push(LetterRestore::Taken(l));
            continue;
        }
        match letters::map(l, root) {
            Ok(()) => {
                // Quit leaves the label/icon keys in place; put them back if
                // something (an older version's quit) removed them.
                if platform::drive_icon_registration(l).is_none() {
                    let label = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let ico = newest_icon(data_dir, sid);
                    if let Err(e) = platform::set_drive_icon(l, ico.as_deref(), &label) {
                        tracing::warn!("drive icon for {l}: {e}");
                    }
                }
                out.push(LetterRestore::Mapped(l, root.clone()));
            }
            Err(e) => out.push(LetterRestore::Failed(l, e)),
        }
    }
    out
}

/// The drive's current icon file (the engine names them `<storageId>-<hash>.ico`
/// and deletes the old one when it makes a new one).
fn newest_icon(data_dir: &Path, storage_id: &str) -> Option<PathBuf> {
    let prefix = format!("{storage_id}-");
    std::fs::read_dir(data_dir.join("icons"))
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with(&prefix) && n.ends_with(".ico")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_only_our_drives_onto_free_letters() {
        let base = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(base.path()).unwrap();
        let root = PathBuf::from(root.display().to_string().trim_start_matches(r"\\?\"));
        let a = root.join("A");
        let b = root.join("B");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.txt"), b"x").unwrap();
        // From the far end of the order, so the letters test (first free
        // one) running beside this can't pick the same letter.
        let mut free = platform::free_letters(&[]);
        free.reverse();
        let (la, lb) = (free[0], free[1]);
        let mut cfg = Config::default();
        cfg.roots.insert("sa".into(), a.clone());
        cfg.roots.insert("sb".into(), b.clone());
        cfg.roots.insert("sc".into(), root.join("C-missing"));
        cfg.mapped.insert("sa".into(), la);
        cfg.mapped.insert("sb".into(), lb);
        cfg.mapped.insert("sc".into(), free[2]);
        let p = crate::cf::TEST_PROVIDER;
        // B is registered for a DIFFERENT folder: not ours to alias.
        let registered = vec![
            (crate::cf::sync_root_id(p, "sa"), a.clone()),
            (crate::cf::sync_root_id(p, "sb"), root.join("elsewhere")),
            (crate::cf::sync_root_id(p, "sc"), root.join("C-missing")),
        ];
        let r = restore_letters_from(&cfg, &registered, p, data.path());
        assert!(r.contains(&LetterRestore::Mapped(la, a.clone())), "{r:?}");
        assert!(letters::points_to(la, &a));
        assert_eq!(std::fs::read(format!("{la}:\\x.txt")).unwrap(), b"x");
        assert!(letters::target(lb).is_none(), "not registered there: no letter");
        assert!(letters::target(free[2]).is_none(), "missing folder: no letter");
        // Twice (the logon helper, then the app): no second definition.
        let r2 = restore_letters_from(&cfg, &registered, p, data.path());
        assert!(r2.contains(&LetterRestore::Already(la, a.clone())), "{r2:?}");
        // A drive removed from this PC isn't restored; a taken letter isn't touched.
        letters::unmap(la, &a);
        assert!(letters::target(la).is_none(), "one unmap removes it (nothing stacked)");
        letters::map(la, &b).unwrap();
        let r3 = restore_letters_from(&cfg, &registered, p, data.path());
        assert!(r3.contains(&LetterRestore::Taken(la)), "{r3:?}");
        assert!(letters::points_to(la, &b));
        letters::unmap(la, &b);
        cfg.drives.insert("sa".into(), false);
        let r4 = restore_letters_from(&cfg, &registered, p, data.path());
        assert!(letters::target(la).is_none(), "{r4:?}");
        platform::remove_drive_icon(la);
    }

    #[test]
    fn folder_names() {
        assert_eq!(folder_name("Alice Files"), "Alice Files");
        assert_eq!(folder_name("a:b?"), "a\u{FF1A}b\u{FF1F}");
        assert_eq!(folder_name("trailing. "), "trailing");
        assert_eq!(folder_name("CON"), "Drive CON");
        assert_eq!(folder_name(""), "Drive");
    }
}
