//! The app's state and actions (sign in/out, org, mount/unmount, pause,
//! photo backup), shared by the tray UI and the headless `mount` command.

use crate::api::{Api, DriveInfo, Org};
use crate::backup::{BackupHandle, BackupJob};
use crate::config::{self, Config, Credentials};
use crate::drive::{DriveCore, DriveOptions};
use crate::fs::Mounted;
use crate::platform;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    pub winfsp: Result<(), String>,
    pub last_error: Option<String>,
}

pub fn drive_label(d: &DriveInfo) -> String {
    crate::fs::volume_label(&d.name)
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
            winfsp: crate::winfsp_loader::load(),
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

    /// The origin to open for "Sign in".
    pub fn sign_in_origin(&self) -> String {
        self.cfg
            .origin
            .clone()
            .or_else(|| self.creds.as_ref().map(|c| c.origin.clone()))
            .unwrap_or_else(config::default_origin)
    }

    /// Whether an auth link for `origin` may be accepted without asking.
    pub fn trusted_origin(&self, origin: &str) -> bool {
        origin == self.sign_in_origin() || origin == config::default_origin()
    }

    /// Exchange a pairing code and store the token.
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
        // A different account/panel: forget per-drive choices of the old one.
        if self.cfg.origin.as_deref() != Some(origin) {
            self.cfg.org_id = None;
            self.cfg.drives.clear();
            self.cfg.backup.storage_id = None;
        }
        self.cfg.origin = Some(origin.to_string());
        self.save_cfg();
        self.unmount_all();
        self.creds = Some(creds);
        tracing::info!("signed in to {origin}");
        self.connect()
    }

    pub fn sign_out(&mut self) {
        self.stop_backup();
        self.unmount_all();
        config::delete_credentials(&self.cfg_dir);
        self.creds = None;
        self.api = None;
        self.orgs.clear();
        self.drives.clear();
        tracing::info!("signed out");
    }

    /// Load orgs + drives for the stored credentials and mount what's wanted.
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
        self.unmount_all();
        self.cfg.org_id = Some(org.to_string());
        self.cfg.drives.clear();
        self.cfg.backup.storage_id = None;
        self.save_cfg();
        self.connect()
    }

    /// Mount every drive the settings want and that isn't mounted yet.
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
                tracing::error!("mount {}: {e}", d.name);
                self.last_error = Some(format!("{}: {e}", d.name));
            }
        }
        // Drives that disappeared from the account.
        let gone: Vec<String> = self
            .mounts
            .keys()
            .filter(|s| !self.drives.iter().any(|d| &d.storage_id == *s))
            .cloned()
            .collect();
        for s in gone {
            self.unmount(&s);
        }
    }

    pub fn mount(&mut self, storage_id: &str) -> Result<char, String> {
        if let Some(m) = self.mounts.get(storage_id) {
            return Ok(m.letter);
        }
        self.winfsp.clone()?;
        let api = self.api.clone().ok_or("not signed in")?;
        let d = self
            .drives
            .iter()
            .find(|d| d.storage_id == storage_id)
            .cloned()
            .ok_or("no such drive")?;
        let taken: Vec<char> = self.mounts.values().map(|m| m.letter).collect();
        let letter = *platform::free_letters(&taken)
            .first()
            .ok_or("no free drive letter")?;
        let m = mount_drive(
            api,
            &d,
            letter,
            &self.data_dir,
            self.paused.clone(),
            DriveOptions::default(),
        )?;
        self.cfg.drives.insert(storage_id.to_string(), true);
        self.save_cfg();
        self.mounts.insert(storage_id.to_string(), m);
        Ok(letter)
    }

    pub fn unmount(&mut self, storage_id: &str) {
        if let Some(m) = self.mounts.remove(storage_id) {
            unmount_drive(m);
        }
    }

    /// User toggled a drive off: unmount and remember.
    pub fn disable(&mut self, storage_id: &str) {
        self.unmount(storage_id);
        self.cfg.drives.insert(storage_id.to_string(), false);
        self.save_cfg();
    }

    pub fn unmount_all(&mut self) {
        // Unmount every letter, then flush all drives' uploads in parallel.
        let mounts: Vec<Mounted> = self.mounts.drain().map(|(_, m)| m).collect();
        let handles: Vec<_> = mounts
            .into_iter()
            .map(|m| std::thread::spawn(move || unmount_drive(m)))
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
                m.core.poke();
                m.core.expedite();
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

    /// (Re)start the watcher according to the settings.
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
            state_path: self
                .data_dir
                .join(format!("backup-{}.json", drive.storage_id)),
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

    /// One line for the tray.
    pub fn status_line(&self) -> String {
        if let Err(e) = &self.winfsp {
            return format!("WinFsp missing: {e}");
        }
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
        let mut uploading = false;
        for m in self.mounts.values() {
            let s = m.core.status();
            pending += s.pending_uploads;
            offline |= !s.online;
            uploading |= s.uploading;
        }
        if offline {
            "Offline — changes will upload when back online".into()
        } else if uploading || pending > 0 {
            format!("Syncing ({} waiting)", pending + usize::from(uploading))
        } else {
            "Up to date".into()
        }
    }

    pub fn shutdown(&mut self) {
        self.stop_backup();
        self.unmount_all();
    }
}

/// Mount one drive at `letter`: namespace + workers + WinFsp + Explorer icon.
pub fn mount_drive(
    api: Api,
    d: &DriveInfo,
    letter: char,
    data_dir: &std::path::Path,
    paused: Arc<AtomicBool>,
    opts: DriveOptions,
) -> Result<Mounted, String> {
    let core = DriveCore::new(
        api.clone(),
        &d.storage_id,
        &d.name,
        data_dir.join("cache").join(&d.storage_id),
        paused,
        opts,
    )
    .map_err(|e| format!("cache: {e}"))?;
    // First tree before Explorer sees the letter; offline is tolerated (the
    // poller fills it in when the network is back).
    if let Err(e) = core.refresh() {
        tracing::warn!("{}: initial tree failed: {e}", d.name);
    }
    core.refresh_volume();
    core.start_workers();
    let m = match crate::fs::mount(core.clone(), letter) {
        Ok(m) => m,
        Err(e) => {
            core.stop();
            return Err(e);
        }
    };
    // Icon: best effort.
    let ico = if d.has_cover {
        match api.cover(&d.storage_id) {
            Ok(Some(bytes)) => match crate::icons::cover_to_ico(&bytes) {
                Ok(ico) => {
                    let p = data_dir.join("icons").join(format!("{}.ico", d.storage_id));
                    let _ = std::fs::create_dir_all(p.parent().unwrap());
                    std::fs::write(&p, ico).ok().map(|_| p)
                }
                Err(e) => {
                    tracing::warn!("{}: cover: {e}", d.name);
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("{}: cover download: {e}", d.name);
                None
            }
        }
    } else {
        None
    };
    if let Err(e) = platform::set_drive_icon(letter, ico.as_deref(), &d.name) {
        tracing::warn!("drive icon for {letter}: {e}");
    }
    Ok(m)
}

pub fn unmount_drive(mut m: Mounted) {
    let letter = m.letter;
    // Take the letter away first (no new writes), then flush uploads.
    m.unmount();
    m.core.shutdown(Duration::from_secs(30));
    platform::remove_drive_icon(letter);
}
