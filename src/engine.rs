//! One drive's sync engine on top of the Cloud Files API.
//!
//! * Remote -> local: the whole `tree` is polled (If-None-Match); the
//!   namespace model (`ns.rs`) turns it into a diff, which is applied to the
//!   sync root as placeholder operations (create / update+dehydrate /
//!   rename / delete).
//! * Reads: FETCH_DATA callbacks are answered with ranged GETs. The
//!   placeholder file on disk IS the cache.
//! * Local -> remote: NOTIFY_RENAME / NOTIFY_DELETE callbacks map to
//!   `POST move` / `DELETE item`; new or modified files and folders are found
//!   by a directory watcher (+ a scan at start) and uploaded, then turned
//!   into in-sync placeholders.
//! * Office/editor save patterns keep the server fileId (see `reconcile`).

use crate::api::{Api, ApiError, ItemType, Tree};
use crate::cf::{self, DeleteNotice, FetchRequest, Meta, RenameNotice};
use crate::ns::{remote_tag, Change, Lookup, Namespace, NodeId, ROOT};
use crate::util::{conflict_name, fold, is_temp_name, iso_to_filetime, now_filetime};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE, STATUS_CLOUD_FILE_UNSUCCESSFUL,
    STATUS_SUCCESS,
};

const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = NTSTATUS(0xC0000035u32 as i32);
const STATUS_ACCESS_DENIED: NTSTATUS = NTSTATUS(0xC0000022u32 as i32);

#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub poll_interval: Duration,
    /// Quiet time after the last change to a path before it's uploaded
    /// (lets save-by-rename sequences finish).
    pub settle: Duration,
    /// How long a local delete waits before it reaches the server (an app
    /// replacing the file may still "adopt" its id).
    pub delete_grace: Duration,
    pub host_name: String,
    /// Periodic full rescan of the sync root (catches missed events).
    pub rescan_interval: Duration,
    /// Sync root provider id (`cf::PROVIDER`; tests use `cf::TEST_PROVIDER`).
    pub provider: String,
    /// How often to revalidate the drive's cover image (icon).
    pub cover_interval: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            // A tree poll that answers 304 is a few small queries on the
            // panel (driveTreeEtag hashes the rows in the database; nothing
            // is transferred) and ~25 ms here, so remote changes can show
            // up within ~5 s rather than ~15 s.
            poll_interval: Duration::from_secs(5),
            settle: Duration::from_millis(1500),
            delete_grace: Duration::from_secs(4),
            host_name: crate::util::computer_name(),
            rescan_interval: Duration::from_secs(300),
            provider: cf::PROVIDER.to_string(),
            cover_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EngineStatus {
    pub online: bool,
    pub pending: usize,
    pub busy: bool,
    pub last_error: Option<String>,
    pub last_sync: Option<chrono::DateTime<chrono::Local>>,
}

#[derive(Debug, Clone)]
struct PendingDelete {
    is_dir: bool,
    rid: String,
    remote_name: String,
    remote_parent: Option<String>,
    due: Instant,
}

#[derive(Debug, Clone)]
struct Shadow {
    remote_name: String,
    remote_parent: Option<String>,
}

/// The sync-root icon used when a drive has no cover.
pub const DEFAULT_ICON: &str = r"%SystemRoot%\System32\imageres.dll,-1043";

/// Where the drive's cover shows up, and what we last showed.
#[derive(Default)]
struct IconState {
    letter: Option<char>,
    data_dir: Option<PathBuf>,
    /// ETag of the cover the current .ico was made from.
    etag: Option<String>,
    ico: Option<PathBuf>,
    has_cover: bool,
    /// Drive colour the current no-cover icon was drawn with.
    color: Option<String>,
    last_check: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// Paths to reconcile, with due time and attempt count.
    dirty: HashMap<PathBuf, (Instant, u32)>,
    deletes: Vec<PendingDelete>,
    /// Placeholders renamed to a scratch name (Office's "move the original
    /// away" step); the server still has them under their old name.
    shadows: HashMap<String, Shadow>,
    /// Remote ids that have (or had) a local placeholder we made.
    materialized: HashSet<String>,
    /// remote id -> (size, last-write FILETIME) of the local file when it
    /// was last in sync. A placeholder that drops out of sync with the same
    /// size and time was only renamed/moved/re-attributed, not rewritten.
    synced: HashMap<String, (u64, i64)>,
    /// Uploaded but not yet converted to a placeholder (file was busy).
    unconverted: HashMap<PathBuf, (String, u64, Option<SystemTime>)>,
    /// Lower-cased paths of file operations this engine is performing
    /// itself (their callbacks must not go back to the server).
    self_ops: HashSet<String>,
    busy: bool,
}

pub struct Engine {
    pub storage_id: String,
    pub root: PathBuf,
    pub sync_root_id: String,
    label: Mutex<String>,
    api: Api,
    ns: Mutex<Namespace>,
    etag: Mutex<String>,
    tree_loaded: AtomicBool,
    epoch: AtomicU64,
    st: Mutex<State>,
    cv: Condvar,
    poll_cv: Condvar,
    poll_flag: Mutex<bool>,
    /// Serialises remote-apply and reconcile passes.
    sync_lock: Mutex<()>,
    paused: Arc<AtomicBool>,
    online: Arc<AtomicBool>,
    stopping: AtomicBool,
    volume: Mutex<(u64, u64)>,
    last_error: Mutex<Option<String>>,
    last_sync: Mutex<Option<chrono::DateTime<chrono::Local>>>,
    opts: EngineOptions,
    conn: Mutex<Option<cf::Connection>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    icons: Mutex<IconState>,
    color: Mutex<Option<String>>,
}

fn file_sig(p: &Path) -> Option<(u64, Option<SystemTime>)> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.len(), m.modified().ok()))
}

fn filetime_of(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| crate::util::unix_ms_to_filetime(d.as_millis() as i64) as i64)
        .unwrap_or_else(|| now_filetime() as i64)
}

fn key(p: &Path) -> String {
    p.display().to_string().trim_end_matches('\\').to_lowercase()
}

impl Engine {
    /// Register the sync root, connect, populate placeholders, start the
    /// watcher, poller and worker.
    pub fn start(
        api: Api,
        storage_id: &str,
        label: &str,
        root: &Path,
        icon: &str,
        paused: Arc<AtomicBool>,
        opts: EngineOptions,
    ) -> Result<Arc<Engine>, String> {
        std::fs::create_dir_all(root).map_err(|e| format!("creating {}: {e}", root.display()))?;
        let root = std::fs::canonicalize(root)
            .map(|p| PathBuf::from(p.display().to_string().trim_start_matches(r"\\?\")))
            .unwrap_or_else(|_| root.to_path_buf());
        let sync_root_id = cf::sync_root_id(&opts.provider, storage_id);
        cf::register_sync_root(&cf::RootRegistration {
            id: &sync_root_id,
            path: &root,
            display_name: label,
            icon,
        })
        .map_err(|e| format!("registering sync root {}: {e}", root.display()))?;
        let e = Arc::new(Engine {
            storage_id: storage_id.to_string(),
            root: root.clone(),
            sync_root_id,
            label: Mutex::new(label.to_string()),
            api,
            ns: Mutex::new(Namespace::new()),
            etag: Mutex::new(String::new()),
            tree_loaded: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            st: Mutex::new(State::default()),
            cv: Condvar::new(),
            poll_cv: Condvar::new(),
            poll_flag: Mutex::new(false),
            sync_lock: Mutex::new(()),
            paused,
            online: Arc::new(AtomicBool::new(true)),
            stopping: AtomicBool::new(false),
            volume: Mutex::new((0, 0)),
            last_error: Mutex::new(None),
            last_sync: Mutex::new(None),
            opts,
            conn: Mutex::new(None),
            watcher: Mutex::new(None),
            threads: Mutex::new(Vec::new()),
            icons: Mutex::new(IconState::default()),
            color: Mutex::new(None),
        });
        let conn = cf::connect(&root, e.clone() as Arc<dyn cf::Callbacks>)
            .map_err(|er| format!("connecting sync root: {er}"))?;
        *e.conn.lock() = Some(conn);
        // Explorer's thumbnails for placeholders (when the embedding app
        // registered a handler; see thumbs.rs).
        crate::thumbs::add_root(&root, e.api.clone());
        if e.opts.provider == cf::PROVIDER && cf::thumbnail_provider().is_some() {
            crate::thumbs::ensure_serving();
        }

        // First tree before anything else (offline start is tolerated).
        if let Err(er) = e.refresh() {
            tracing::warn!("{label}: initial tree failed: {er}");
        }
        e.refresh_volume();
        e.scan_all(Duration::ZERO);
        e.start_watcher()?;
        e.start_threads();
        Ok(e)
    }

    pub fn label(&self) -> String {
        self.label.lock().clone()
    }
    pub fn volume(&self) -> (u64, u64) {
        *self.volume.lock()
    }
    pub fn api(&self) -> &Api {
        &self.api
    }

    pub fn status(&self) -> EngineStatus {
        let s = self.st.lock();
        EngineStatus {
            online: self.online.load(Ordering::Relaxed),
            pending: s.dirty.len() + s.deletes.len(),
            busy: s.busy,
            last_error: self.last_error.lock().clone(),
            last_sync: *self.last_sync.lock(),
        }
    }

    fn bump(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn net<T>(&self, r: Result<T, ApiError>) -> Result<T, ApiError> {
        match &r {
            Err(e) if e.is_network() => self.online.store(false, Ordering::Relaxed),
            Ok(_) => self.online.store(true, Ordering::Relaxed),
            _ => {}
        }
        r
    }

    // ------------------------------------------------------------- paths

    /// `\a\b` (namespace path) for an absolute path under the root.
    pub fn rel(&self, p: &Path) -> Option<String> {
        let ps = p.display().to_string();
        let ps = ps.trim_start_matches(r"\\?\");
        let rs = self.root.display().to_string();
        if ps.len() < rs.len() || !ps[..rs.len()].eq_ignore_ascii_case(&rs) {
            return None;
        }
        let rest = &ps[rs.len()..];
        if !rest.is_empty() && !rest.starts_with('\\') {
            return None;
        }
        Some(if rest.is_empty() { "\\".to_string() } else { rest.to_string() })
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        let r = rel.trim_start_matches('\\');
        if r.is_empty() {
            self.root.clone()
        } else {
            self.root.join(r)
        }
    }

    fn parent_rel(rel: &str) -> String {
        match rel.trim_end_matches('\\').rfind('\\') {
            Some(0) | None => "\\".to_string(),
            Some(i) => rel[..i].to_string(),
        }
    }

    fn name_of(rel: &str) -> String {
        rel.rsplit('\\').next().unwrap_or("").to_string()
    }

    fn is_self_op(&self, p: &Path) -> bool {
        self.st.lock().self_ops.contains(&key(p))
    }

    fn self_op<T>(&self, paths: &[&Path], f: impl FnOnce() -> T) -> T {
        {
            let mut s = self.st.lock();
            for p in paths {
                s.self_ops.insert(key(p));
            }
        }
        let r = f();
        let mut s = self.st.lock();
        for p in paths {
            s.self_ops.remove(&key(p));
        }
        r
    }

    fn meta_of(&self, id: NodeId) -> Option<(Meta, String)> {
        let ns = self.ns.lock();
        let n = ns.get(id)?;
        Some((
            Meta {
                is_dir: n.is_dir,
                size: n.size,
                created: n.created as i64,
                modified: n.modified as i64,
            },
            n.remote_id.clone()?,
        ))
    }

    fn node_at(&self, rel: &str) -> Option<NodeId> {
        match self.ns.lock().lookup(rel) {
            Lookup::Found(id) => Some(id),
            _ => None,
        }
    }

    // ---------------------------------------------------- remote -> local

    /// Fetch `tree` and apply the diff to disk. Ok(true) if anything changed.
    pub fn refresh(&self) -> Result<bool, ApiError> {
        let epoch0 = self.epoch.load(Ordering::SeqCst);
        let etag = self.etag.lock().clone();
        let t = self.net(self.api.tree(&self.storage_id, Some(&etag)))?;
        *self.last_sync.lock() = Some(chrono::Local::now());
        let Some(mut t) = t else { return Ok(false) };
        let deleting: HashSet<String> = self.st.lock().deletes.iter().map(|d| d.rid.clone()).collect();
        t.files.retain(|f| !deleting.contains(&f.file_id));
        t.folders.retain(|f| !deleting.contains(&f.folder_id));
        let _g = self.sync_lock.lock();
        let changes = {
            let mut ns = self.ns.lock();
            if self.epoch.load(Ordering::SeqCst) != epoch0 {
                return Ok(false); // raced a local change; next round
            }
            let res = ns.apply_remote(&t);
            *self.etag.lock() = t.etag.clone();
            res.changes
        };
        self.tree_loaded.store(true, Ordering::SeqCst);
        let changed = !changes.is_empty();
        if changed {
            tracing::info!("{}: {} remote change(s)", self.label(), changes.len());
        }
        for c in changes {
            self.apply_change(c);
        }
        Ok(changed)
    }

    fn identity_at(p: &Path) -> Option<(cf::PlaceholderInfo, Option<(bool, String)>)> {
        let pi = cf::placeholder_info(p).ok()??;
        let id = cf::parse_identity(&pi.identity);
        Some((pi, id))
    }

    fn apply_change(&self, c: Change) {
        match c {
            Change::Added { path, dir, rid } => self.materialize(&path, dir, &rid),
            Change::Removed { path, dir, rid } => {
                let local = self.abs(&path);
                self.st.lock().materialized.remove(&rid);
                self.st.lock().shadows.remove(&rid);
                match Self::identity_at(&local) {
                    Some((pi, Some((d, r)))) if r == rid && d == dir => {
                        if dir {
                            self.remove_synced_tree(&local);
                        } else if pi.in_sync {
                            let r = self.self_op(&[&local], || std::fs::remove_file(&local));
                            if let Err(e) = r {
                                tracing::warn!("removing {}: {e}", local.display());
                            }
                        } else {
                            tracing::warn!(
                                "{} was deleted on the server but has local changes; keeping it (will upload as new)",
                                local.display()
                            );
                            self.mark_dirty(&local, Duration::ZERO);
                        }
                    }
                    _ => {}
                }
            }
            Change::Renamed { from, to, dir, rid } => {
                let lf = self.abs(&from);
                let lt = self.abs(&to);
                let from_ok = matches!(Self::identity_at(&lf), Some((_, Some((d, r)))) if r == rid && d == dir);
                if from_ok && !lt.exists() {
                    let r = self.self_op(&[&lf, &lt], || std::fs::rename(&lf, &lt));
                    if let Err(e) = r {
                        tracing::warn!("rename {} -> {}: {e}", lf.display(), lt.display());
                    }
                } else if !lt.exists() {
                    self.materialize(&to, dir, &rid);
                }
            }
            Change::Modified { path, rid } => {
                let local = self.abs(&path);
                if let Some((pi, Some((false, r)))) = Self::identity_at(&local) {
                    if r != rid {
                        return;
                    }
                    if !pi.in_sync {
                        tracing::warn!(
                            "{} changed on the server and locally; local copy wins",
                            local.display()
                        );
                        return;
                    }
                    self.update_from_remote(&path, &local, pi.pinned);
                }
            }
        }
    }

    /// Server content changed: new size/time, drop local bytes.
    fn update_from_remote(&self, rel: &str, local: &Path, pinned: bool) {
        let Some(id) = self.node_at(rel) else { return };
        let Some((meta, rid)) = self.meta_of(id) else { return };
        let r = self.self_op(&[local], || {
            cf::update_placeholder(local, &meta, &cf::identity(false, &rid), true)
        });
        match r {
            Ok(()) => {
                self.st.lock().synced.insert(rid.clone(), (meta.size, meta.modified));
                if pinned {
                    self.spawn_hydrate(local.to_path_buf());
                }
            }
            Err(e) => {
                tracing::info!("update {} deferred: {e}", local.display());
                self.mark_dirty(local, Duration::from_secs(5));
            }
        }
    }

    /// Create the placeholder for namespace path `rel` (if missing).
    fn materialize(&self, rel: &str, dir: bool, rid: &str) {
        let local = self.abs(rel);
        let Some(parent) = local.parent() else { return };
        if !parent.is_dir() {
            return;
        }
        if std::fs::symlink_metadata(&local).is_ok() {
            match Self::identity_at(&local) {
                Some((pi, Some((d, r)))) if r == rid && d == dir => {
                    self.st.lock().materialized.insert(rid.to_string());
                    // Came back from an earlier run: refresh stale metadata.
                    if !dir && pi.in_sync {
                        let want = self.node_at(rel).and_then(|i| self.meta_of(i));
                        let have = std::fs::metadata(&local).map(|m| m.len()).ok();
                        if let Some((m, _)) = want {
                            if Some(m.size) != have {
                                self.update_from_remote(rel, &local, pi.pinned);
                            }
                        }
                    }
                }
                _ => self.mark_dirty(&local, Duration::ZERO),
            }
            return;
        }
        let Some(id) = self.node_at(rel) else { return };
        let Some((meta, _)) = self.meta_of(id) else { return };
        let name = Self::name_of(rel);
        match cf::create_placeholder(parent, &name, &meta, &cf::identity(dir, rid)) {
            Ok(()) => {
                {
                    let mut s = self.st.lock();
                    s.materialized.insert(rid.to_string());
                    if !dir {
                        s.synced.insert(rid.to_string(), (meta.size, meta.modified));
                    }
                }
                if dir {
                    // Children were diffed as separate Added changes, but
                    // they may have come before this folder existed.
                    let kids: Vec<(String, bool, String)> = {
                        let ns = self.ns.lock();
                        ns.list(id)
                            .into_iter()
                            .filter_map(|c| ns.get(c))
                            .filter_map(|n| n.remote_id.clone().map(|r| (ns.path_of(n.id), n.is_dir, r)))
                            .collect()
                    };
                    for (p, d, r) in kids {
                        self.materialize(&p, d, &r);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("placeholder {}: {e}", local.display());
                self.mark_dirty(&local, Duration::ZERO);
            }
        }
    }

    /// Delete in-sync placeholders under `dir` (and `dir` if it ends up
    /// empty). Unsynced local files are kept and re-uploaded.
    fn remove_synced_tree(&self, dir: &Path) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                self.remove_synced_tree(&p);
            } else {
                match Self::identity_at(&p) {
                    Some((pi, _)) if pi.in_sync => {
                        let _ = self.self_op(&[&p], || std::fs::remove_file(&p));
                    }
                    _ => {
                        tracing::warn!("keeping unsynced {}", p.display());
                        self.mark_dirty(&p, Duration::ZERO);
                    }
                }
            }
        }
        let _ = self.self_op(&[dir], || std::fs::remove_dir(dir));
        if dir.exists() {
            // Kept files inside: the folder must be re-created remotely.
            self.mark_dirty(dir, Duration::ZERO);
        }
    }

    pub fn refresh_volume(&self) {
        if let Ok(drives) = self.net(self.api.drives()) {
            if let Some(d) = drives.iter().find(|d| d.storage_id == self.storage_id) {
                let total = d.quota_bytes.filter(|q| *q > 0).unwrap_or(0);
                *self.volume.lock() = (total, total.saturating_sub(d.size_bytes));
                *self.label.lock() = d.name.clone();
                *self.color.lock() = d.color.clone();
            }
        }
    }

    // ---------------------------------------------------- local -> remote

    pub fn mark_dirty(&self, p: &Path, delay: Duration) {
        if self.rel(p).map(|r| r == "\\").unwrap_or(true) {
            return;
        }
        let mut s = self.st.lock();
        let due = Instant::now() + delay;
        let e = s.dirty.entry(p.to_path_buf()).or_insert((due, 0));
        e.0 = due;
        drop(s);
        self.cv.notify_all();
    }

    /// Queue every entry under the root (start-up / periodic safety net).
    pub fn scan_all(&self, delay: Duration) {
        let mut stack = vec![self.root.clone()];
        let mut n = 0;
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let ft = e.file_type().ok();
                if ft.map(|t| t.is_symlink()).unwrap_or(false) {
                    continue;
                }
                if ft.map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p.clone());
                }
                // Skip clean placeholders cheaply.
                let needs = match Self::identity_at(&p) {
                    Some((pi, Some((isdir, rid)))) => {
                        let known = self.ns.lock().by_remote(isdir, &rid).is_some();
                        let attrs = cf::attributes(&p).unwrap_or(0);
                        !pi.in_sync
                            || !known
                            || (attrs & cf::ATTR_PINNED != 0 && attrs & cf::ATTR_RECALL_ON_DATA_ACCESS != 0)
                            || (attrs & cf::ATTR_UNPINNED != 0 && attrs & cf::ATTR_RECALL_ON_DATA_ACCESS == 0 && !isdir)
                    }
                    _ => true,
                };
                if needs {
                    self.mark_dirty(&p, delay);
                    n += 1;
                }
            }
        }
        if n > 0 {
            tracing::info!("{}: {n} local item(s) to check", self.label());
        }
    }

    fn start_watcher(self: &Arc<Self>) -> Result<(), String> {
        use notify::Watcher;
        let weak: Weak<Engine> = Arc::downgrade(self);
        let settle = self.opts.settle;
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Some(e) = weak.upgrade() else { return };
            if let Ok(ev) = res {
                for p in ev.paths {
                    e.mark_dirty(&p, settle);
                }
            }
        })
        .map_err(|e| format!("watcher: {e}"))?;
        w.watch(&self.root, notify::RecursiveMode::Recursive)
            .map_err(|e| format!("watching {}: {e}", self.root.display()))?;
        *self.watcher.lock() = Some(w);
        Ok(())
    }

    fn start_threads(self: &Arc<Self>) {
        let a = self.clone();
        let poller = std::thread::Builder::new()
            .name(format!("poll-{}", self.storage_id))
            .spawn(move || a.poll_loop())
            .expect("spawn poller");
        let b = self.clone();
        let worker = std::thread::Builder::new()
            .name(format!("sync-{}", self.storage_id))
            .spawn(move || b.work_loop())
            .expect("spawn worker");
        self.threads.lock().extend([poller, worker]);
    }

    pub fn poke(&self) {
        *self.poll_flag.lock() = true;
        self.poll_cv.notify_all();
    }

    fn poll_loop(&self) {
        let mut next_volume = Instant::now() + Duration::from_secs(60);
        let mut next_scan = Instant::now() + self.opts.rescan_interval;
        while !self.stopping.load(Ordering::SeqCst) {
            {
                let mut f = self.poll_flag.lock();
                if !*f {
                    self.poll_cv.wait_for(&mut f, self.opts.poll_interval);
                }
                *f = false;
            }
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            if self.paused.load(Ordering::Relaxed) {
                continue;
            }
            if let Err(e) = self.refresh() {
                tracing::warn!("{}: tree poll failed: {e}", self.label());
            }
            if Instant::now() >= next_volume {
                self.refresh_volume();
                next_volume = Instant::now() + Duration::from_secs(60);
            }
            if Instant::now() >= next_scan {
                self.scan_all(Duration::ZERO);
                next_scan = Instant::now() + self.opts.rescan_interval;
            }
            // Cover (drive icon): piggybacks on the poll, at most once per
            // cover_interval.
            let due = self
                .icons
                .lock()
                .last_check
                .map(|t| t.elapsed() >= self.opts.cover_interval)
                .unwrap_or(false);
            if due {
                if let Err(e) = self.check_cover() {
                    tracing::warn!("{}: cover check: {e}", self.label());
                }
            }
        }
    }

    // --------------------------------------------------------- drive icon

    /// Show the drive's cover as the icon of `letter` (if any) and of the
    /// sync root; fetches the cover now and then keeps it current.
    pub fn attach_icons(&self, letter: Option<char>, data_dir: &Path) {
        {
            let mut i = self.icons.lock();
            i.letter = letter;
            i.data_dir = Some(data_dir.to_path_buf());
        }
        if let Err(e) = self.check_cover() {
            tracing::warn!("{}: cover: {e}", self.label());
        }
    }

    /// Current .ico (None = no cover).
    pub fn icon_path(&self) -> Option<PathBuf> {
        self.icons.lock().ico.clone()
    }

    /// Revalidate the cover (If-None-Match) and the drive colour, and when
    /// either changed re-render the icon under a NEW file name (Explorer
    /// caches icons by path), point the letter's DefaultIcon and the sync
    /// root's icon at it, and delete the old file. Ok(true) if it changed.
    pub fn check_cover(&self) -> Result<bool, String> {
        let (etag, old, had_cover, used_color, data_dir) = {
            let mut i = self.icons.lock();
            i.last_check = Some(Instant::now());
            (i.etag.clone(), i.ico.clone(), i.has_cover, i.color.clone(), i.data_dir.clone())
        };
        let Some(data_dir) = data_dir else { return Ok(false) };
        self.refresh_volume();
        let color = self.color.lock().clone();
        let r = self
            .net(self.api.cover_if(&self.storage_id, if had_cover { etag.as_deref() } else { None }))
            .map_err(|e| e.to_string())?;
        let fnv = |parts: &[&[u8]]| {
            let mut h: u32 = 0x811c9dc5;
            for p in parts {
                for b in *p {
                    h ^= *b as u32;
                    h = h.wrapping_mul(0x01000193);
                }
            }
            h
        };
        let (ico_bytes, key, new_etag, has_cover) = match r {
            // Same cover; the colour doesn't show on a cover icon.
            crate::api::Cover::NotModified => return Ok(false),
            crate::api::Cover::Image { bytes, etag: new_etag } => {
                let tag = new_etag.clone().unwrap_or_default();
                let key = fnv(&[b"cover", tag.as_bytes(), &bytes[..bytes.len().min(65536)]]);
                (crate::icons::drive_ico(Some(&bytes), None)?, key, new_etag, true)
            }
            crate::api::Cover::None => {
                if old.is_some() && !had_cover && used_color == color {
                    return Ok(false);
                }
                let c = color.clone().unwrap_or_default();
                let key = fnv(&[b"nocover", c.as_bytes()]);
                (crate::icons::drive_ico(None, color.as_deref())?, key, None, false)
            }
        };
        let path = data_dir.join("icons").join(format!("{}-{key:08x}.ico", self.storage_id));
        if Some(&path) == old.as_ref() {
            let mut i = self.icons.lock();
            i.etag = new_etag;
            i.has_cover = has_cover;
            i.color = color;
            return Ok(false);
        }
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&path, ico_bytes).map_err(|e| e.to_string())?;
        self.apply_icon(&path);
        {
            let mut i = self.icons.lock();
            i.etag = new_etag;
            i.has_cover = has_cover;
            i.color = color.clone();
            i.ico = Some(path.clone());
        }
        if let Some(o) = old {
            let _ = std::fs::remove_file(&o);
        }
        tracing::info!(
            "{}: drive icon -> {} ({})",
            self.label(),
            path.display(),
            if has_cover { "cover".to_string() } else { format!("no cover, colour {}", color.as_deref().unwrap_or("default")) }
        );
        Ok(true)
    }

    fn apply_icon(&self, ico: &Path) {
        let label = self.label();
        let res = format!("{},0", ico.display());
        // Re-registering the same id updates the shell's sync root entry
        // (navigation pane icon) in place.
        if let Err(e) = cf::register_sync_root(&cf::RootRegistration {
            id: &self.sync_root_id,
            path: &self.root,
            display_name: &label,
            icon: &res,
        }) {
            tracing::warn!("{label}: updating sync root icon: {e}");
        }
        let letter = self.icons.lock().letter;
        if let Some(l) = letter {
            if let Err(e) = crate::platform::set_drive_icon(l, Some(ico), &label) {
                tracing::warn!("drive icon for {l}: {e}");
            }
        }
        crate::platform::shell_refresh_path(&self.root);
    }

    fn work_loop(&self) {
        loop {
            enum Job {
                Delete(PendingDelete),
                Path(PathBuf, u32),
            }
            let job = {
                let mut s = self.st.lock();
                loop {
                    let stopping = self.stopping.load(Ordering::SeqCst);
                    // Quitting: drain the queue, unless sync is paused (then
                    // nothing more goes to the server; it resumes next start).
                    if stopping
                        && ((s.dirty.is_empty() && s.deletes.is_empty())
                            || self.paused.load(Ordering::Relaxed))
                    {
                        return;
                    }
                    if self.paused.load(Ordering::Relaxed) && !stopping {
                        self.cv.wait_for(&mut s, Duration::from_millis(500));
                        continue;
                    }
                    let now = Instant::now();
                    if let Some(i) = s.deletes.iter().position(|d| d.due <= now || stopping) {
                        let d = s.deletes.remove(i);
                        s.busy = true;
                        break Job::Delete(d);
                    }
                    let next = s
                        .dirty
                        .iter()
                        .min_by_key(|(_, (t, _))| *t)
                        .map(|(p, (t, a))| (p.clone(), *t, *a));
                    let next_del = s.deletes.iter().map(|d| d.due).min();
                    match next {
                        Some((p, t, a)) if t <= now || stopping => {
                            s.dirty.remove(&p);
                            s.busy = true;
                            break Job::Path(p, a);
                        }
                        other => {
                            let wake = [other.map(|o| o.1), next_del]
                                .into_iter()
                                .flatten()
                                .min()
                                .map(|t| t.saturating_duration_since(now))
                                .unwrap_or(Duration::from_secs(5));
                            self.cv.wait_for(&mut s, wake.max(Duration::from_millis(20)));
                        }
                    }
                }
            };
            match job {
                Job::Delete(d) => {
                    let r = self.net(self.api.delete_item(
                        if d.is_dir { ItemType::Folder } else { ItemType::File },
                        &d.rid,
                    ));
                    match r {
                        Ok(()) => {
                            tracing::info!("deleted {} on the server", d.remote_name);
                            let mut ns = self.ns.lock();
                            if let Some(id) = ns.by_remote(d.is_dir, &d.rid) {
                                ns.remove(id);
                            }
                            drop(ns);
                            self.bump();
                            let mut s = self.st.lock();
                            s.materialized.remove(&d.rid);
                            s.shadows.remove(&d.rid);
                        }
                        Err(e) if (e.is_network() || e.is_auth()) && !self.stopping.load(Ordering::SeqCst) => {
                            let mut d = d;
                            d.due = Instant::now() + Duration::from_secs(10);
                            self.st.lock().deletes.push(d);
                        }
                        Err(e) => tracing::warn!("delete {}: {e}", d.rid),
                    }
                }
                Job::Path(p, attempts) => {
                    let r = {
                        let _g = self.sync_lock.lock();
                        self.reconcile(&p)
                    };
                    match r {
                        Ok(()) => {
                            *self.last_error.lock() = None;
                        }
                        Err(e) => {
                            *self.last_error.lock() = Some(e.clone());
                            if !self.stopping.load(Ordering::SeqCst) {
                                let backoff = Duration::from_secs(2u64.pow(attempts.min(8)))
                                    .min(Duration::from_secs(300));
                                tracing::warn!("{}: {e}; retry in {backoff:?}", p.display());
                                let mut s = self.st.lock();
                                let ent = s.dirty.entry(p).or_insert((Instant::now() + backoff, attempts + 1));
                                ent.1 = attempts + 1;
                            }
                        }
                    }
                }
            }
            self.st.lock().busy = false;
        }
    }

    /// Remote folder id for namespace dir `rel` (None = drive root),
    /// creating missing folders on the server (namespace only; the local
    /// folders are adopted as placeholders when they're reconciled).
    fn remote_dir(&self, rel: &str) -> Result<Option<String>, String> {
        if rel == "\\" || rel.is_empty() {
            return Ok(None);
        }
        if let Some(id) = self.node_at(rel) {
            let ns = self.ns.lock();
            let n = ns.get(id).unwrap();
            if n.is_dir {
                if let Some(r) = &n.remote_id {
                    return Ok(Some(r.clone()));
                }
            } else {
                return Err(format!("{rel}: a file is in the way on the server"));
            }
        }
        let parent = self.remote_dir(&Self::parent_rel(rel))?;
        let name = Self::name_of(rel);
        let f = match self.net(self.api.create_folder(&self.storage_id, parent.as_deref(), &name)) {
            Ok(f) => f,
            Err(e) if e.is_name_taken() => {
                // Exists on the server but not in our tree yet.
                let t = self.net(self.api.tree(&self.storage_id, None)).map_err(|e| e.to_string())?;
                let t = t.unwrap_or(Tree { storage_id: String::new(), etag: String::new(), folders: vec![], files: vec![] });
                match t.folders.iter().find(|f| f.parent_id == parent && fold(&f.name) == fold(&name)) {
                    Some(f) => f.clone(),
                    None => return Err(format!("{name}: a file with that name exists on the server")),
                }
            }
            Err(e) => return Err(e.to_string()),
        };
        let mut ns = self.ns.lock();
        let pnode = match &parent {
            None => ROOT,
            Some(p) => ns.by_remote(true, p).unwrap_or(ROOT),
        };
        if ns.by_remote(true, &f.folder_id).is_none() {
            let t = f.created_at.as_deref().and_then(iso_to_filetime).unwrap_or_else(now_filetime);
            let id = ns.insert(pnode, &name, true, Some(f.folder_id.clone()), 0, t, t, String::new(), String::new());
            if let Some(n) = ns.get_mut(id) {
                n.remote_name = f.name.clone();
                n.remote_parent = parent.clone();
            }
        }
        drop(ns);
        self.bump();
        Ok(Some(f.folder_id))
    }

    /// Bring one local path in line with the server.
    fn reconcile(&self, p: &Path) -> Result<(), String> {
        let Some(rel) = self.rel(p) else { return Ok(()) };
        if rel == "\\" {
            return Ok(());
        }
        let Ok(md) = std::fs::symlink_metadata(p) else { return Ok(()) };
        if md.file_type().is_symlink() {
            return Ok(());
        }
        let name = Self::name_of(&rel);
        let is_dir = md.is_dir();
        if !is_dir && is_temp_name(&name) {
            return Ok(()); // scratch file: stays local
        }
        let info = cf::placeholder_info(p).map_err(|e| format!("placeholder info: {e}"))?;
        let ident = info.as_ref().and_then(|i| cf::parse_identity(&i.identity));
        match (is_dir, info, ident) {
            (true, Some(pi), Some((true, rid))) => self.reconcile_dir_placeholder(p, &rel, &rid, pi.in_sync),
            (true, _, _) => self.adopt_or_create_dir(p, &rel, &name),
            (false, Some(pi), Some((false, rid))) => self.reconcile_file_placeholder(p, &rel, &rid, &pi),
            (false, _, _) => self.upload_new_file(p, &rel, &name),
        }
    }

    fn reconcile_dir_placeholder(&self, p: &Path, rel: &str, rid: &str, in_sync: bool) -> Result<(), String> {
        let known = self.ns.lock().by_remote(true, rid).is_some();
        if !known && self.tree_loaded.load(Ordering::SeqCst) {
            // Its folder is gone on the server (deleted while we were away,
            // or it held unsynced files when it was deleted): re-create it.
            let parent = self.remote_dir(&Self::parent_rel(rel))?;
            let name = Self::name_of(rel);
            let f = self
                .net(self.api.create_folder(&self.storage_id, parent.as_deref(), &name))
                .map_err(|e| e.to_string())?;
            let meta = Meta { is_dir: true, size: 0, created: now_filetime() as i64, modified: now_filetime() as i64 };
            self.insert_node(rel, true, &f.folder_id, 0, None);
            let _ = self.self_op(&[p], || cf::update_placeholder(p, &meta, &cf::identity(true, &f.folder_id), false));
            let _ = cf::set_in_sync(p);
            self.st.lock().materialized.insert(f.folder_id);
            self.queue_children(p);
        } else if !in_sync {
            let _ = cf::set_in_sync(p);
        }
        Ok(())
    }

    fn queue_children(&self, dir: &Path) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                self.mark_dirty(&e.path(), Duration::ZERO);
            }
        }
    }

    fn insert_node(&self, rel: &str, is_dir: bool, rid: &str, size: u64, tag: Option<String>) {
        let mut ns = self.ns.lock();
        if ns.by_remote(is_dir, rid).is_some() {
            return;
        }
        let Some((parent, name)) = ns.parent_of_path(rel).map(|(p, n)| (p, n.to_string())) else {
            return;
        };
        let now = now_filetime();
        let id = ns.insert(parent, &name, is_dir, Some(rid.to_string()), size, now, now, tag.unwrap_or_default(), rid.to_string());
        let prid = ns.get(parent).and_then(|n| n.remote_id.clone());
        if let Some(n) = ns.get_mut(id) {
            n.remote_parent = prid;
        }
        drop(ns);
        self.bump();
    }

    fn adopt_or_create_dir(&self, p: &Path, rel: &str, name: &str) -> Result<(), String> {
        // A folder of that name already on the server (made by `remote_dir`,
        // or another client) is adopted instead of duplicated.
        let rid = self.remote_dir(rel)?.ok_or("root")?;
        let _ = name;
        let r = self.self_op(&[p], || cf::convert_to_placeholder(p, &cf::identity(true, &rid)));
        if let Err(e) = r {
            return Err(format!("converting folder: {e}"));
        }
        self.st.lock().materialized.insert(rid);
        self.queue_children(p);
        Ok(())
    }

    fn reconcile_file_placeholder(&self, p: &Path, rel: &str, rid: &str, pi: &cf::PlaceholderInfo) -> Result<(), String> {
        let node = self.ns.lock().by_remote(false, rid);
        if pi.in_sync {
            let attrs = cf::attributes(p).unwrap_or(0);
            let partial = attrs & cf::ATTR_RECALL_ON_DATA_ACCESS != 0;
            if node.is_none() && self.tree_loaded.load(Ordering::SeqCst) && !self.st.lock().shadows.contains_key(rid) {
                // Deleted on the server while we weren't looking.
                let _ = self.self_op(&[p], || std::fs::remove_file(p));
                return Ok(());
            }
            if attrs & cf::ATTR_PINNED != 0 && partial {
                self.spawn_hydrate(p.to_path_buf());
            } else if attrs & cf::ATTR_UNPINNED != 0 && !partial {
                if let Err(e) = cf::dehydrate(p) {
                    return Err(format!("free up space: {e}"));
                }
                tracing::info!("freed up space: {}", p.display());
            } else if let Some(id) = node {
                // Stale placeholder (server changed while the update was busy).
                let size = self.ns.lock().get(id).map(|n| n.size);
                let have = std::fs::metadata(p).map(|m| m.len()).ok();
                if size.is_some() && size != have {
                    self.update_from_remote(rel, p, pi.pinned);
                }
            }
            return Ok(());
        }
        // Out of sync. A rename/move (or attribute change) also clears the
        // in-sync bit, so first check whether the bytes actually changed:
        // same size and last-write time as when it was last in sync means
        // "no", and re-uploading would needlessly hydrate the whole file.
        let before = file_sig(p);
        if let Some((len, mt)) = before {
            let now = (len, filetime_of(mt));
            let known = self.st.lock().synced.get(rid).copied().or_else(|| {
                let ns = self.ns.lock();
                node.and_then(|i| ns.get(i)).map(|n| (n.size, n.modified as i64))
            });
            let same = known
                .map(|(s, t)| s == now.0 && (t - now.1).abs() <= 10_000_000)
                .unwrap_or(false);
            if same {
                return self
                    .self_op(&[p], || cf::set_in_sync(p))
                    .map_err(|e| format!("mark in sync: {e}"));
            }
        }
        let r = self.net(self.api.upload_replace(rid, p));
        let pf = match r {
            Ok(pf) => pf,
            Err(e) if e.is_not_found() => {
                // Gone on the server: upload as a new file and re-identify.
                return self.upload_new_file(p, rel, &Self::name_of(rel));
            }
            Err(e) => return Err(e.to_string()),
        };
        tracing::info!("uploaded {rel}");
        let tag = remote_tag(pf.size_bytes.unwrap_or(before.map(|b| b.0).unwrap_or(0)), pf.updated_at.as_deref());
        if let Some(id) = node {
            let mut ns = self.ns.lock();
            if let Some(n) = ns.get_mut(id) {
                n.size = pf.size_bytes.unwrap_or(n.size);
                n.remote_tag = tag;
                if let Some(t) = pf.updated_at.as_deref().and_then(iso_to_filetime) {
                    n.modified = t;
                }
            }
            drop(ns);
            self.bump();
        }
        if file_sig(p) != before {
            self.mark_dirty(p, self.opts.settle);
            return Ok(());
        }
        self.self_op(&[p], || cf::set_in_sync(p))
            .map_err(|e| format!("mark in sync: {e}"))?;
        if let Some((len, mt)) = before {
            self.st.lock().synced.insert(rid.to_string(), (len, filetime_of(mt)));
        }
        Ok(())
    }

    fn upload_new_file(&self, p: &Path, rel: &str, name: &str) -> Result<(), String> {
        let before = file_sig(p).ok_or("vanished")?;
        // Uploaded earlier but the conversion failed (file was busy)?
        let prior = self.st.lock().unconverted.get(p).cloned();
        let rid = if let Some((rid, len, mt)) = prior.filter(|(_, l, m)| (*l, *m) == before) {
            let _ = (len, mt);
            rid
        } else {
            let parent_rel = Self::parent_rel(rel);
            let parent = self.remote_dir(&parent_rel)?;
            let target = self.adoption_target(&parent_rel, parent.as_deref(), name, before);
            let (rid, pf) = match target {
                Target::Replace(rid) => {
                    tracing::info!("{rel} replaces server file {rid} (keeps its id)");
                    match self.net(self.api.upload_replace(&rid, p)) {
                        Ok(pf) => (rid, pf),
                        Err(e) if e.is_not_found() => self.create_remote(p, parent.as_deref(), name, false)?,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Target::Adopt(rid, size, tag) => {
                    tracing::info!("{rel} is already on the server as {rid}");
                    // Keep the known server version tag (no content change).
                    let updated = tag.as_deref().and_then(|t| t.split_once('|')).map(|(_, u)| u.to_string());
                    let pf = crate::api::PartialFile {
                        file_id: Some(rid.clone()),
                        size_bytes: Some(size),
                        name: Some(name.to_string()),
                        updated_at: updated.filter(|u| !u.is_empty()),
                        ..Default::default()
                    };
                    (rid, pf)
                }
                Target::Conflict => self.create_remote(p, parent.as_deref(), name, true)?,
                Target::New => self.create_remote(p, parent.as_deref(), name, false)?,
            };
            let tag = remote_tag(pf.size_bytes.unwrap_or(before.0), pf.updated_at.as_deref());
            // Record the server file in the namespace (under the name the
            // server has; a conflicted copy is renamed by the next poll).
            let server_name = pf.name.clone().unwrap_or_else(|| name.to_string());
            let server_rel = if Self::parent_rel(rel) == "\\" {
                format!("\\{}", crate::util::display_name(&server_name))
            } else {
                format!("{}\\{}", Self::parent_rel(rel), crate::util::display_name(&server_name))
            };
            {
                let mut ns = self.ns.lock();
                if let Some(id) = ns.by_remote(false, &rid) {
                    if let Some(n) = ns.get_mut(id) {
                        n.size = pf.size_bytes.unwrap_or(before.0);
                        n.remote_tag = tag.clone();
                    }
                    drop(ns);
                    self.bump();
                } else {
                    drop(ns);
                    self.insert_node(&server_rel, false, &rid, pf.size_bytes.unwrap_or(before.0), Some(tag));
                }
            }
            {
                let mut s = self.st.lock();
                s.materialized.insert(rid.clone());
                s.unconverted.insert(p.to_path_buf(), (rid.clone(), before.0, before.1));
            }
            tracing::info!("uploaded {rel}");
            rid
        };
        if file_sig(p) != Some(before) {
            self.st.lock().unconverted.remove(p);
            self.mark_dirty(p, self.opts.settle);
            return Ok(());
        }
        let already = cf::placeholder_info(p).ok().flatten().is_some();
        let r = self.self_op(&[p], || {
            if already {
                let meta = Meta { is_dir: false, size: before.0, created: filetime_of(before.1), modified: filetime_of(before.1) };
                cf::update_placeholder_unverified(p, &meta, &cf::identity(false, &rid))
            } else {
                cf::convert_to_placeholder(p, &cf::identity(false, &rid))
            }
        });
        match r {
            Ok(()) => {
                let mut s = self.st.lock();
                s.unconverted.remove(p);
                s.synced.insert(rid, (before.0, filetime_of(before.1)));
                Ok(())
            }
            Err(e) => Err(format!("converting to placeholder: {e}")),
        }
    }

    fn create_remote(
        &self,
        p: &Path,
        parent: Option<&str>,
        name: &str,
        conflict: bool,
    ) -> Result<(String, crate::api::PartialFile), String> {
        let mut upload_name = if conflict {
            conflict_name(name, &self.opts.host_name)
        } else {
            name.to_string()
        };
        for attempt in 0..2 {
            match self.net(self.api.upload_new(&self.storage_id, parent, &upload_name, p)) {
                Ok(pf) => {
                    let rid = match pf.file_id.clone() {
                        Some(r) => r,
                        None => {
                            let t = self.net(self.api.tree(&self.storage_id, None)).map_err(|e| e.to_string())?;
                            t.and_then(|t| {
                                t.files
                                    .into_iter()
                                    .find(|f| f.folder_id.as_deref() == parent && f.name == upload_name)
                                    .map(|f| f.file_id)
                            })
                            .ok_or("server didn't return the new file's id")?
                        }
                    };
                    let mut pf = pf;
                    pf.name.get_or_insert(upload_name.clone());
                    return Ok((rid, pf));
                }
                Err(e) if e.is_name_taken() && attempt == 0 => {
                    tracing::warn!("{name} exists on the server; uploading as a conflicted copy");
                    upload_name = conflict_name(name, &self.opts.host_name);
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("name conflict".into())
    }

    /// Which server file (if any) a NEW local file should be written into.
    fn adoption_target(
        &self,
        parent_rel: &str,
        parent: Option<&str>,
        name: &str,
        local: (u64, Option<SystemTime>),
    ) -> Target {
        let fname = fold(name);
        let mut s = self.st.lock();
        // Office: the original was renamed to a scratch name (shadow).
        let shadow = s
            .shadows
            .iter()
            .find(|(_, sh)| sh.remote_parent.as_deref() == parent && fold(&sh.remote_name) == fname)
            .map(|(r, _)| r.clone());
        if let Some(rid) = shadow {
            s.shadows.remove(&rid);
            s.deletes.retain(|d| d.rid != rid);
            return Target::Replace(rid);
        }
        // The original was deleted moments ago (delete + create, or
        // MoveFileEx(REPLACE_EXISTING) over it).
        if let Some(i) = s
            .deletes
            .iter()
            .position(|d| !d.is_dir && d.remote_parent.as_deref() == parent && fold(&d.remote_name) == fname)
        {
            let d = s.deletes.remove(i);
            return Target::Replace(d.rid);
        }
        let materialized = s.materialized.clone();
        drop(s);
        // The server file at this path was replaced in place locally.
        let ns = self.ns.lock();
        let pnode = match ns.lookup(parent_rel) {
            Lookup::Found(id) => id,
            _ => return Target::New,
        };
        if let Some(c) = ns.child(pnode, name) {
            if let Some(n) = ns.get(c) {
                if !n.is_dir {
                    if let Some(rid) = &n.remote_id {
                        if materialized.contains(rid) {
                            return Target::Replace(rid.clone());
                        }
                        // Same size and the server copy is at least as new as
                        // the local write: it's this file, uploaded before (e.g.
                        // by a run that stopped before converting it).
                        if n.size == local.0 && n.modified as i64 >= filetime_of(local.1) - 20_000_000 {
                            return Target::Adopt(rid.clone(), n.size, Some(n.remote_tag.clone()));
                        }
                        return Target::Conflict;
                    }
                }
            }
        }
        Target::New
    }

    fn spawn_hydrate(&self, p: PathBuf) {
        std::thread::spawn(move || match cf::hydrate(&p) {
            Ok(()) => tracing::info!("kept on this device: {}", p.display()),
            Err(e) => tracing::warn!("hydrate {}: {e}", p.display()),
        });
    }

    // ------------------------------------------------------------ control

    pub fn is_idle(&self) -> bool {
        let s = self.st.lock();
        s.dirty.is_empty() && s.deletes.is_empty() && !s.busy
    }

    /// Wait until nothing is queued (or timeout). True if idle.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.is_idle() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.is_idle()
    }

    /// Make everything queued due now.
    pub fn expedite(&self) {
        let mut s = self.st.lock();
        let now = Instant::now();
        for v in s.dirty.values_mut() {
            v.0 = now;
        }
        for d in s.deletes.iter_mut() {
            d.due = now;
        }
        drop(s);
        self.cv.notify_all();
    }

    /// Flush queued work (bounded), stop threads, disconnect.
    pub fn shutdown(&self, wait: Duration) {
        self.watcher.lock().take();
        self.expedite();
        if !self.paused.load(Ordering::Relaxed) && !self.wait_idle(wait) {
            tracing::warn!("{}: stopping with work still queued", self.label());
        }
        self.stopping.store(true, Ordering::SeqCst);
        self.cv.notify_all();
        self.poke();
        for t in self.threads.lock().drain(..) {
            let _ = t.join();
        }
        self.conn.lock().take();
        crate::thumbs::remove_root(&self.root);
        tracing::info!("{}: disconnected", self.label());
    }

    /// Sign-out / removal: after `shutdown`, move anything not uploaded
    /// aside, unregister the sync root and delete the placeholders.
    /// Returns where unsynced files were moved (if any).
    pub fn remove_local(&self, aside_base: &Path) -> Result<Option<PathBuf>, String> {
        remove_root(&self.sync_root_id, &self.root, &self.label(), aside_base)
    }
}

/// Unregister a (disconnected) sync root and delete its placeholders, first
/// copying every file that isn't an in-sync placeholder to
/// `<aside_base>\Not uploaded - <label> - <time>`. Nothing unsynced is ever
/// deleted: if a copy fails, nothing is removed.
pub fn remove_root(
    sync_root_id: &str,
    root: &Path,
    label: &str,
    aside_base: &Path,
) -> Result<Option<PathBuf>, String> {
    {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H%M%S");
        let aside = aside_base.join(format!("Not uploaded - {} - {stamp}", crate::util::display_name(label)));
        let mut moved = 0;
        let mut stack = vec![root.to_path_buf()];
        let mut files = Vec::new();
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p);
                } else {
                    files.push(p);
                }
            }
        }
        for p in files {
            let synced = matches!(cf::placeholder_info(&p), Ok(Some(pi)) if pi.in_sync);
            if synced {
                continue;
            }
            let rel = p
                .strip_prefix(root)
                .map(|r| r.to_path_buf())
                .unwrap_or_else(|_| PathBuf::from(p.file_name().unwrap_or_default()));
            let dest = aside.join(rel);
            if let Some(d) = dest.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            // Copy (not move): a placeholder moved out keeps its cloud
            // reparse data; a copy is a plain file.
            match std::fs::copy(&p, &dest) {
                Ok(_) => {
                    moved += 1;
                    tracing::warn!("not uploaded, moved aside: {} -> {}", p.display(), dest.display());
                }
                Err(e) => return Err(format!("couldn't move unsynced {} aside ({e}); nothing deleted", p.display())),
            }
        }
        if let Err(e) = cf::unregister_sync_root(sync_root_id) {
            tracing::warn!("unregistering {sync_root_id}: {e}");
        }
        if let Err(e) = std::fs::remove_dir_all(root) {
            tracing::warn!("removing {}: {e}", root.display());
        }
        Ok((moved > 0).then_some(aside))
    }
}

enum Target {
    Replace(String),
    /// Already on the server with this content: just convert.
    Adopt(String, u64, Option<String>),
    Conflict,
    New,
}

// ------------------------------------------------------------- callbacks

fn status_for(e: &ApiError) -> NTSTATUS {
    if e.is_network() {
        STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE
    } else if e.is_name_taken() {
        STATUS_OBJECT_NAME_COLLISION
    } else if e.is_auth() || e.status() == Some(403) {
        STATUS_ACCESS_DENIED
    } else {
        STATUS_CLOUD_FILE_UNSUCCESSFUL
    }
}

impl cf::Callbacks for Engine {
    fn fetch_data(&self, req: FetchRequest) {
        tracing::debug!(
            "FETCH_DATA {} required {}+{} optional {}+{} size {}",
            req.path.display(),
            req.offset,
            req.length,
            req.optional_offset,
            req.optional_length,
            req.file_size
        );
        let api = self.api.clone();
        let online = self.online.clone();
        let rid = match cf::parse_identity(&req.identity) {
            Some((false, r)) => r,
            _ => {
                req.fail(STATUS_CLOUD_FILE_UNSUCCESSFUL);
                return;
            }
        };
        let size = req.file_size;
        std::thread::spawn(move || {
            const ALIGN: u64 = 4096;
            const CHUNK: u64 = 4 << 20;
            let start = req.offset & !(ALIGN - 1);
            // Exactly the required range (4 KiB aligned). The platform may run
            // several requests for one file at once (CopyFile does); going
            // beyond the required range makes them overlap and the platform
            // cancels the copy. It asks again for whatever else it needs.
            let end = ((req.offset + req.length).div_ceil(ALIGN) * ALIGN).min(size);
            let mut pos = start;
            while pos < end {
                let n = CHUNK.min(end - pos);
                match api.read_range(&rid, pos, n) {
                    Ok(bytes) if bytes.len() as u64 == n => {
                        online.store(true, Ordering::Relaxed);
                        if let Err(e) = req.transfer(pos, &bytes) {
                            tracing::warn!("transfer {}: {e}", req.path.display());
                            return;
                        }
                    }
                    Ok(bytes) => {
                        tracing::warn!("short read of {} ({} of {n} bytes): server copy changed?", req.path.display(), bytes.len());
                        req.fail(STATUS_CLOUD_FILE_UNSUCCESSFUL);
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("fetch {}: {e}", req.path.display());
                        if e.is_network() {
                            online.store(false, Ordering::Relaxed);
                        }
                        req.fail(status_for(&e));
                        return;
                    }
                }
                pos += n;
            }
        });
    }

    fn notify_delete(&self, n: DeleteNotice) -> NTSTATUS {
        if self.is_self_op(&n.path) || self.stopping.load(Ordering::SeqCst) {
            return STATUS_SUCCESS;
        }
        let Some((is_dir, rid)) = cf::parse_identity(&n.identity) else {
            return STATUS_SUCCESS;
        };
        let (remote_name, remote_parent) = {
            let ns = self.ns.lock();
            match ns.by_remote(is_dir, &rid).and_then(|i| ns.get(i)) {
                Some(node) => (node.remote_name.clone(), node.remote_parent.clone()),
                None => (String::new(), None),
            }
        };
        // A shadow (Office's renamed-away original) keeps its original
        // place for adoption.
        let (remote_name, remote_parent) = match self.st.lock().shadows.get(&rid) {
            Some(sh) => (sh.remote_name.clone(), sh.remote_parent.clone()),
            None => (remote_name, remote_parent),
        };
        tracing::info!("local delete of {} ({rid})", n.path.display());
        let mut s = self.st.lock();
        s.deletes.retain(|d| d.rid != rid);
        s.deletes.push(PendingDelete {
            is_dir,
            rid,
            remote_name,
            remote_parent,
            due: Instant::now() + self.opts.delete_grace,
        });
        drop(s);
        self.cv.notify_all();
        STATUS_SUCCESS
    }

    fn notify_rename(&self, n: RenameNotice) -> NTSTATUS {
        if self.is_self_op(&n.from) || self.stopping.load(Ordering::SeqCst) {
            return STATUS_SUCCESS;
        }
        let Some((is_dir, rid)) = cf::parse_identity(&n.identity) else {
            return STATUS_SUCCESS;
        };
        let to_rel = if n.target_in_scope { self.rel(&n.to) } else { None };
        let Some(to_rel) = to_rel else {
            // Moved out of the drive: same as a delete.
            return self.notify_delete(DeleteNotice { path: n.from, is_dir, identity: n.identity, pid: n.pid });
        };
        let to_name = Self::name_of(&to_rel);
        let to_parent_rel = Self::parent_rel(&to_rel);
        let (node, remote_name, remote_parent) = {
            let ns = self.ns.lock();
            match ns.by_remote(is_dir, &rid).and_then(|i| ns.get(i)) {
                Some(x) => (Some(x.id), x.remote_name.clone(), x.remote_parent.clone()),
                None => (None, String::new(), None),
            }
        };
        let Some(node) = node else { return STATUS_SUCCESS };
        let from_name = self.rel(&n.from).map(|r| Self::name_of(&r)).unwrap_or_default();
        let shadowed = self.st.lock().shadows.contains_key(&rid);

        // Renamed to a scratch name: keep it where it is on the server.
        if !is_dir && is_temp_name(&to_name) && (!is_temp_name(&from_name) || shadowed) {
            tracing::info!("{} -> {to_name}: local-only (shadow) rename", n.from.display());
            self.st.lock().shadows.entry(rid).or_insert(Shadow { remote_name, remote_parent });
            return STATUS_SUCCESS;
        }
        let parent_rid = match self.remote_dir(&to_parent_rel) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("rename {} -> {to_rel}: {e}", n.from.display());
                return if self.online.load(Ordering::Relaxed) { STATUS_CLOUD_FILE_UNSUCCESSFUL } else { STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE };
            }
        };
        // Replacing an existing synced file: that one goes away on the server.
        if n.to.exists() && !is_dir {
            if let Some((_, Some((false, trid)))) = Self::identity_at(&n.to) {
                if trid != rid {
                    if let Err(e) = self.net(self.api.delete_item(ItemType::File, &trid)) {
                        return status_for(&e);
                    }
                    let mut ns = self.ns.lock();
                    if let Some(i) = ns.by_remote(false, &trid) {
                        ns.remove(i);
                    }
                    drop(ns);
                    let mut s = self.st.lock();
                    s.deletes.retain(|d| d.rid != trid);
                    s.materialized.remove(&trid);
                }
            }
        }
        self.st.lock().shadows.remove(&rid);
        let name_change = (remote_name != to_name).then_some(to_name.as_str());
        let parent_change = (remote_parent != parent_rid).then_some(parent_rid.as_deref());
        if name_change.is_some() || parent_change.is_some() {
            let kind = if is_dir { ItemType::Folder } else { ItemType::File };
            match self.net(self.api.move_item(kind, &rid, name_change, parent_change)) {
                Ok(row) => {
                    let mut ns = self.ns.lock();
                    let pnode = match &parent_rid {
                        None => Some(ROOT),
                        Some(p) => ns.by_remote(true, p),
                    };
                    if let Some(pnode) = pnode {
                        ns.move_node(node, pnode, &to_name);
                    }
                    if let Some(x) = ns.get_mut(node) {
                        x.remote_name = to_name.clone();
                        x.remote_parent = parent_rid.clone();
                        // A move bumps updatedAt on the panel; keep the tag
                        // current so the next poll isn't a "content change".
                        if !is_dir {
                            if let Some(u) = row.updated_at.as_deref() {
                                x.remote_tag = remote_tag(row.size_bytes.unwrap_or(x.size), Some(u));
                            }
                        }
                    }
                    drop(ns);
                    self.bump();
                    tracing::info!("moved {} -> {to_rel} on the server", n.from.display());
                }
                Err(e) => {
                    tracing::warn!("move {} -> {to_rel}: {e}", n.from.display());
                    return status_for(&e);
                }
            }
        } else {
            // Same server place (e.g. back from a scratch name).
            let mut ns = self.ns.lock();
            let pnode = match &parent_rid {
                None => Some(ROOT),
                Some(p) => ns.by_remote(true, p),
            };
            if let Some(pnode) = pnode {
                ns.move_node(node, pnode, &to_name);
            }
            drop(ns);
            self.bump();
        }
        STATUS_SUCCESS
    }
}
