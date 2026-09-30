//! One mounted drive's logic, independent of WinFsp: path operations mapped
//! onto the protocol, the upload queue and the `tree` poller.
//!
//! Locking rule: a cache `Entry` lock may be held while taking the namespace
//! lock, never the other way round.

use crate::api::{Api, ApiError, ItemType};
use crate::cache::{Cache, CacheError};
use crate::ns::{remote_tag, Change, Lookup, Namespace, NodeId, ROOT};
use crate::util::{conflict_name, is_temp_name, is_valid_component, now_filetime};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// NTSTATUS values we return. Kept as plain i32 so this module doesn't
/// depend on WinFsp.
pub mod status {
    pub const INVALID_PARAMETER: i32 = 0xC000000Du32 as i32;
    pub const OBJECT_NAME_INVALID: i32 = 0xC0000033u32 as i32;
    pub const OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034u32 as i32;
    pub const OBJECT_NAME_COLLISION: i32 = 0xC0000035u32 as i32;
    pub const OBJECT_PATH_NOT_FOUND: i32 = 0xC000003Au32 as i32;
    pub const ACCESS_DENIED: i32 = 0xC0000022u32 as i32;
    pub const DISK_FULL: i32 = 0xC000007Fu32 as i32;
    pub const FILE_IS_A_DIRECTORY: i32 = 0xC00000BAu32 as i32;
    pub const NOT_A_DIRECTORY: i32 = 0xC0000103u32 as i32;
    pub const DIRECTORY_NOT_EMPTY: i32 = 0xC0000101u32 as i32;
    pub const FILE_DELETED: i32 = 0xC0000123u32 as i32;
    pub const UNEXPECTED_NETWORK_ERROR: i32 = 0xC00000C4u32 as i32;
    pub const NETWORK_UNREACHABLE: i32 = 0xC000023Cu32 as i32;
    pub const IO_DEVICE_ERROR: i32 = 0xC0000185u32 as i32;
    pub const FILE_TOO_LARGE: i32 = 0xC0000904u32 as i32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsError(pub i32);

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NTSTATUS {:#010x}", self.0 as u32)
    }
}

pub type FsResult<T> = Result<T, FsError>;

impl From<&ApiError> for FsError {
    fn from(e: &ApiError) -> Self {
        FsError(match e {
            ApiError::Network(_) => status::NETWORK_UNREACHABLE,
            ApiError::Io(_) => status::IO_DEVICE_ERROR,
            ApiError::Decode(_) => status::UNEXPECTED_NETWORK_ERROR,
            ApiError::Status { status: s, .. } => match *s {
                400 => status::INVALID_PARAMETER,
                401 | 403 => status::ACCESS_DENIED,
                404 => status::OBJECT_NAME_NOT_FOUND,
                409 if e.is_quota() => status::DISK_FULL,
                409 => status::OBJECT_NAME_COLLISION,
                413 => status::FILE_TOO_LARGE,
                _ => status::UNEXPECTED_NETWORK_ERROR,
            },
        })
    }
}
impl From<ApiError> for FsError {
    fn from(e: ApiError) -> Self {
        FsError::from(&e)
    }
}
impl From<CacheError> for FsError {
    fn from(e: CacheError) -> Self {
        match e {
            CacheError::Api(a) => a.into(),
            CacheError::Io(_) => FsError(status::IO_DEVICE_ERROR),
        }
    }
}
impl From<std::io::Error> for FsError {
    fn from(_: std::io::Error) -> Self {
        FsError(status::IO_DEVICE_ERROR)
    }
}

/// What the filesystem layer needs to fill a FileInfo.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub id: NodeId,
    pub is_dir: bool,
    pub name: String,
    pub size: u64,
    pub created: u64,
    pub modified: u64,
}

#[derive(Debug, Clone)]
pub struct DriveOptions {
    pub poll_interval: Duration,
    /// Wait this long after the last close before uploading (lets the
    /// "write temp + rename over original" save pattern settle).
    pub upload_delay: Duration,
    pub host_name: String,
}

impl Default for DriveOptions {
    fn default() -> Self {
        DriveOptions {
            poll_interval: Duration::from_secs(15),
            upload_delay: Duration::from_secs(2),
            host_name: crate::util::computer_name(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DriveStatus {
    pub online: bool,
    pub pending_uploads: usize,
    pub uploading: bool,
    pub last_error: Option<String>,
    pub last_sync: Option<chrono::DateTime<chrono::Local>>,
}

#[derive(Default)]
struct Queue {
    due: HashMap<NodeId, (Instant, u32)>,
    /// Remote deletes that failed (offline); retried, and hidden from polls.
    deletes: Vec<(ItemType, String)>,
    uploading: bool,
}

pub struct DriveCore {
    pub storage_id: String,
    label: Mutex<String>,
    api: Api,
    ns: Mutex<Namespace>,
    cache: Cache,
    etag: Mutex<String>,
    epoch: AtomicU64,
    changes: Mutex<Vec<Change>>,
    queue: Mutex<Queue>,
    queue_cv: Condvar,
    poll_cv: Condvar,
    poll_lock: Mutex<bool>,
    paused: Arc<AtomicBool>,
    online: AtomicBool,
    stopping: AtomicBool,
    volume: Mutex<(u64, u64)>,
    last_error: Mutex<Option<String>>,
    last_sync: Mutex<Option<chrono::DateTime<chrono::Local>>>,
    opts: DriveOptions,
}

const DEFAULT_TOTAL: u64 = 1 << 40;

impl DriveCore {
    pub fn new(
        api: Api,
        storage_id: &str,
        label: &str,
        cache_dir: PathBuf,
        paused: Arc<AtomicBool>,
        opts: DriveOptions,
    ) -> std::io::Result<Arc<DriveCore>> {
        Ok(Arc::new(DriveCore {
            storage_id: storage_id.to_string(),
            label: Mutex::new(label.to_string()),
            api,
            ns: Mutex::new(Namespace::new()),
            cache: Cache::new(cache_dir)?,
            etag: Mutex::new(String::new()),
            epoch: AtomicU64::new(0),
            changes: Mutex::new(Vec::new()),
            queue: Mutex::new(Queue::default()),
            queue_cv: Condvar::new(),
            poll_cv: Condvar::new(),
            poll_lock: Mutex::new(false),
            paused,
            online: AtomicBool::new(true),
            stopping: AtomicBool::new(false),
            volume: Mutex::new((DEFAULT_TOTAL, DEFAULT_TOTAL)),
            last_error: Mutex::new(None),
            last_sync: Mutex::new(None),
            opts,
        }))
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

    pub fn status(&self) -> DriveStatus {
        let q = self.queue.lock();
        DriveStatus {
            online: self.online.load(Ordering::Relaxed),
            pending_uploads: q.due.len(),
            uploading: q.uploading,
            last_error: self.last_error.lock().clone(),
            last_sync: *self.last_sync.lock(),
        }
    }

    fn bump(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn net_result<T>(&self, r: Result<T, ApiError>) -> Result<T, ApiError> {
        match &r {
            Err(e) if e.is_network() => self.online.store(false, Ordering::Relaxed),
            Ok(_) => self.online.store(true, Ordering::Relaxed),
            _ => {}
        }
        r
    }

    // ---------------------------------------------------------------- sync

    /// Fetch `tree` and merge it. Returns true if anything changed.
    pub fn refresh(&self) -> Result<bool, ApiError> {
        let epoch0 = self.epoch.load(Ordering::SeqCst);
        let etag = self.etag.lock().clone();
        let t = self.net_result(self.api.tree(&self.storage_id, Some(&etag)))?;
        *self.last_sync.lock() = Some(chrono::Local::now());
        let Some(mut t) = t else { return Ok(false) };
        let deletes: Vec<String> = self.queue.lock().deletes.iter().map(|d| d.1.clone()).collect();
        if !deletes.is_empty() {
            t.files.retain(|f| !deletes.contains(&f.file_id));
            t.folders.retain(|f| !deletes.contains(&f.folder_id));
        }
        let res = {
            let mut ns = self.ns.lock();
            if self.epoch.load(Ordering::SeqCst) != epoch0 {
                // A local change raced this fetch: the tree may predate it.
                // Try again on the next round rather than undo it.
                return Ok(false);
            }
            let res = ns.apply_remote(&t);
            *self.etag.lock() = t.etag.clone();
            res
        };
        for k in &res.invalidated {
            self.cache.invalidate(k);
        }
        for k in &res.removed {
            self.cache.remove(k);
        }
        let changed = !res.changes.is_empty();
        if changed {
            tracing::info!("{}: {} remote change(s)", self.label(), res.changes.len());
            self.changes.lock().extend(res.changes);
        }
        Ok(changed)
    }

    pub fn refresh_volume(&self) {
        if let Ok(drives) = self.net_result(self.api.drives()) {
            if let Some(d) = drives.iter().find(|d| d.storage_id == self.storage_id) {
                let total = d.quota_bytes.filter(|q| *q > 0).unwrap_or(DEFAULT_TOTAL);
                *self.volume.lock() = (total, total.saturating_sub(d.size_bytes));
                *self.label.lock() = d.name.clone();
            }
        }
    }

    /// Changes for Explorer since the last call.
    pub fn take_changes(&self) -> Vec<Change> {
        std::mem::take(&mut *self.changes.lock())
    }

    pub fn has_changes(&self) -> bool {
        !self.changes.lock().is_empty()
    }

    /// Wake the poller now.
    pub fn poke(&self) {
        *self.poll_lock.lock() = true;
        self.poll_cv.notify_all();
    }

    pub fn start_workers(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let a = self.clone();
        let poller = std::thread::Builder::new()
            .name(format!("poll-{}", self.storage_id))
            .spawn(move || a.poll_loop())
            .expect("spawn poller");
        let b = self.clone();
        let uploader = std::thread::Builder::new()
            .name(format!("upload-{}", self.storage_id))
            .spawn(move || b.upload_loop())
            .expect("spawn uploader");
        vec![poller, uploader]
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.queue_cv.notify_all();
        self.poke();
    }

    fn poll_loop(&self) {
        let mut next_volume = Instant::now();
        while !self.stopping.load(Ordering::SeqCst) {
            if !self.paused.load(Ordering::Relaxed) {
                if Instant::now() >= next_volume {
                    self.refresh_volume();
                    next_volume = Instant::now() + Duration::from_secs(60);
                }
                if let Err(e) = self.refresh() {
                    tracing::warn!("{}: tree poll failed: {e}", self.label());
                }
            }
            let mut poked = self.poll_lock.lock();
            if !*poked {
                self.poll_cv.wait_for(&mut poked, self.opts.poll_interval);
            }
            *poked = false;
        }
    }

    // --------------------------------------------------------------- queries

    pub fn info(&self, id: NodeId) -> Option<NodeInfo> {
        let ns = self.ns.lock();
        ns.get(id).map(|n| NodeInfo {
            id,
            is_dir: n.is_dir,
            name: n.name.clone(),
            size: n.size,
            created: n.created,
            modified: n.modified,
        })
    }

    pub fn lookup(&self, path: &str) -> FsResult<NodeInfo> {
        let id = {
            let ns = self.ns.lock();
            match ns.lookup(path) {
                Lookup::Found(id) => id,
                Lookup::NotFound => return Err(FsError(status::OBJECT_NAME_NOT_FOUND)),
                Lookup::PathNotFound => return Err(FsError(status::OBJECT_PATH_NOT_FOUND)),
            }
        };
        self.info(id).ok_or(FsError(status::OBJECT_NAME_NOT_FOUND))
    }

    pub fn path_of(&self, id: NodeId) -> String {
        self.ns.lock().path_of(id)
    }

    /// Children of a directory (visible names only).
    pub fn list(&self, dir: NodeId) -> FsResult<Vec<NodeInfo>> {
        let ns = self.ns.lock();
        let d = ns.get(dir).ok_or(FsError(status::FILE_DELETED))?;
        if !d.is_dir {
            return Err(FsError(status::NOT_A_DIRECTORY));
        }
        Ok(ns
            .list(dir)
            .into_iter()
            .filter_map(|c| ns.get(c))
            .map(|n| NodeInfo {
                id: n.id,
                is_dir: n.is_dir,
                name: n.name.clone(),
                size: n.size,
                created: n.created,
                modified: n.modified,
            })
            .collect())
    }

    /// Parent directory's info, for "..".
    pub fn parent_info(&self, id: NodeId) -> Option<NodeInfo> {
        let p = self.ns.lock().get(id)?.parent;
        self.info(p)
    }

    // ----------------------------------------------------------- open/close

    pub fn open(&self, path: &str) -> FsResult<NodeInfo> {
        let info = self.lookup(path)?;
        if let Some(n) = self.ns.lock().get_mut(info.id) {
            n.open_count += 1;
        }
        Ok(info)
    }

    pub fn close(&self, id: NodeId) {
        let key = {
            let mut ns = self.ns.lock();
            let Some(n) = ns.get_mut(id) else { return };
            n.open_count = n.open_count.saturating_sub(1);
            if n.open_count > 0 || n.is_dir {
                return;
            }
            n.cache_key.clone()
        };
        if !key.is_empty() {
            self.cache.entry(&key).lock().close_file();
        }
    }

    /// Create a file or directory at `path`; returns it opened.
    pub fn create(&self, path: &str, is_dir: bool) -> FsResult<NodeInfo> {
        let mut ns = self.ns.lock();
        let (parent, name) = ns
            .parent_of_path(path)
            .ok_or(FsError(status::OBJECT_PATH_NOT_FOUND))?;
        if !is_valid_component(name) {
            return Err(FsError(status::OBJECT_NAME_INVALID));
        }
        if ns.child(parent, name).is_some() {
            return Err(FsError(status::OBJECT_NAME_COLLISION));
        }
        let now = now_filetime();
        let id = if is_dir {
            match self.ensure_remote_dir(&mut ns, parent) {
                Ok(parent_rid) => {
                    let r = self.net_result(self.api.create_folder(
                        &self.storage_id,
                        parent_rid.as_deref(),
                        name,
                    ));
                    match r {
                        Ok(f) => {
                            let id = ns.insert(
                                parent,
                                name,
                                true,
                                Some(f.folder_id.clone()),
                                0,
                                now,
                                now,
                                String::new(),
                                String::new(),
                            );
                            if let Some(n) = ns.get_mut(id) {
                                n.remote_name = f.name.clone();
                                n.remote_parent = f.parent_id.clone();
                            }
                            self.bump();
                            id
                        }
                        Err(e) if e.is_network() => self.pending_dir(&mut ns, parent, name, now),
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) if e.0 == status::NETWORK_UNREACHABLE => {
                    self.pending_dir(&mut ns, parent, name, now)
                }
                Err(e) => return Err(e),
            }
        } else {
            let key = ns.new_local_cache_key();
            let id = ns.insert(parent, name, false, None, 0, now, now, String::new(), key.clone());
            if let Some(n) = ns.get_mut(id) {
                n.dirty = true;
                n.gen = 1;
            }
            drop(ns);
            self.cache.entry(&key).lock().init_new()?;
            ns = self.ns.lock();
            id
        };
        if let Some(n) = ns.get_mut(id) {
            n.open_count += 1;
        }
        drop(ns);
        self.info(id).ok_or(FsError(status::FILE_DELETED))
    }

    fn pending_dir(&self, ns: &mut Namespace, parent: NodeId, name: &str, now: u64) -> NodeId {
        tracing::info!("offline: folder {name} will be created on the server later");
        let id = ns.insert(parent, name, true, None, 0, now, now, String::new(), String::new());
        self.enqueue_at(id, Instant::now());
        id
    }

    /// Make sure directory `dir` exists on the server; returns its folder id
    /// (None for the drive root). Creates missing ancestors.
    fn ensure_remote_dir(&self, ns: &mut Namespace, dir: NodeId) -> FsResult<Option<String>> {
        if dir == ROOT {
            return Ok(None);
        }
        let mut chain = Vec::new();
        let mut cur = dir;
        while cur != ROOT {
            let n = ns.get(cur).ok_or(FsError(status::OBJECT_PATH_NOT_FOUND))?;
            if n.remote_id.is_some() {
                break;
            }
            chain.push(cur);
            cur = n.parent;
        }
        let mut parent_rid = if cur == ROOT {
            None
        } else {
            ns.get(cur).and_then(|n| n.remote_id.clone())
        };
        for d in chain.into_iter().rev() {
            let name = ns.get(d).map(|n| n.name.clone()).unwrap_or_default();
            let f = self.net_result(self.api.create_folder(
                &self.storage_id,
                parent_rid.as_deref(),
                &name,
            ))?;
            ns.set_remote_id(d, Some(f.folder_id.clone()));
            if let Some(n) = ns.get_mut(d) {
                n.remote_name = f.name.clone();
                n.remote_parent = f.parent_id.clone();
            }
            self.bump();
            parent_rid = Some(f.folder_id);
        }
        Ok(ns.get(dir).and_then(|n| n.remote_id.clone()))
    }

    // ------------------------------------------------------------------ I/O

    fn snap(&self, id: NodeId) -> FsResult<Snap> {
        let ns = self.ns.lock();
        let n = ns.get(id).ok_or(FsError(status::FILE_DELETED))?;
        Ok(Snap {
            is_dir: n.is_dir,
            name: n.name.clone(),
            remote_id: n.remote_id.clone(),
            key: n.cache_key.clone(),
            tag: n.remote_tag.clone(),
            size: n.size,
            dirty: n.dirty,
            local_only: n.remote_id.is_none(),
        })
    }

    pub fn read(&self, id: NodeId, buf: &mut [u8], offset: u64) -> FsResult<usize> {
        let s = self.snap(id)?;
        if s.is_dir {
            return Err(FsError(status::FILE_IS_A_DIRECTORY));
        }
        let e = self.cache.entry(&s.key);
        let mut g = e.lock();
        if !s.dirty && !s.local_only {
            if let Some(rid) = &s.remote_id {
                g.prepare(&s.tag, s.size)?;
                let r = g.ensure(&self.api, rid, offset, buf.len() as u64);
                if let Err(CacheError::Api(ae)) = &r {
                    if ae.is_network() {
                        self.online.store(false, Ordering::Relaxed);
                    }
                    tracing::warn!("read {}: {ae}", s.name);
                }
                r?;
            }
        } else if !g.is_complete() {
            // Local-only bytes that were never fully fetched (an orphaned
            // shadow): refuse rather than hand out zeros.
            return Err(FsError(status::IO_DEVICE_ERROR));
        }
        Ok(g.read_at(buf, offset)?)
    }

    /// Turn the cache entry into a complete, writable local copy.
    fn prepare_write<'a>(
        &self,
        s: &Snap,
        g: &mut parking_lot::MutexGuard<'a, crate::cache::Entry>,
    ) -> FsResult<()> {
        if g.is_complete() && (s.dirty || s.local_only || g.tag() == s.tag) {
            return Ok(());
        }
        match &s.remote_id {
            Some(rid) if !s.dirty => {
                g.prepare(&s.tag, s.size)?;
                g.ensure_complete(&self.api, rid).map_err(|e| {
                    tracing::warn!("fetching {} before a write: {e}", s.name);
                    FsError::from(e)
                })?;
                Ok(())
            }
            _ => {
                if g.is_complete() {
                    Ok(())
                } else {
                    Err(FsError(status::IO_DEVICE_ERROR))
                }
            }
        }
    }

    fn mark_modified(&self, id: NodeId, new_size: u64) -> Option<NodeInfo> {
        let mut ns = self.ns.lock();
        let n = ns.get_mut(id)?;
        n.size = new_size;
        n.dirty = true;
        n.gen += 1;
        n.modified = now_filetime();
        Some(NodeInfo {
            id,
            is_dir: false,
            name: n.name.clone(),
            size: n.size,
            created: n.created,
            modified: n.modified,
        })
    }

    pub fn write(
        &self,
        id: NodeId,
        buf: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained: bool,
    ) -> FsResult<(usize, NodeInfo)> {
        let s = self.snap(id)?;
        if s.is_dir {
            return Err(FsError(status::FILE_IS_A_DIRECTORY));
        }
        let e = self.cache.entry(&s.key);
        let mut g = e.lock();
        self.prepare_write(&s, &mut g)?;
        let size = g.size();
        let off = if write_to_eof { size } else { offset };
        let data = if constrained {
            if off >= size {
                &buf[..0]
            } else {
                &buf[..buf.len().min((size - off) as usize)]
            }
        } else {
            buf
        };
        let n = if data.is_empty() { 0 } else { g.write_at(data, off)? };
        let new_size = g.size();
        let info = self
            .mark_modified(id, new_size)
            .ok_or(FsError(status::FILE_DELETED))?;
        Ok((n, info))
    }

    pub fn set_size(&self, id: NodeId, new_size: u64) -> FsResult<NodeInfo> {
        let s = self.snap(id)?;
        if s.is_dir {
            return Err(FsError(status::FILE_IS_A_DIRECTORY));
        }
        let e = self.cache.entry(&s.key);
        let mut g = e.lock();
        if g.size() == new_size && (s.dirty || s.local_only || g.is_complete()) {
            return self.info(id).ok_or(FsError(status::FILE_DELETED));
        }
        if new_size == 0 {
            g.init_new()?;
        } else {
            self.prepare_write(&s, &mut g)?;
            g.set_len(new_size)?;
        }
        self.mark_modified(id, new_size)
            .ok_or(FsError(status::FILE_DELETED))
    }

    /// Truncate for CREATE_ALWAYS / TRUNCATE_EXISTING.
    pub fn overwrite(&self, id: NodeId) -> FsResult<NodeInfo> {
        let s = self.snap(id)?;
        if s.is_dir {
            return Err(FsError(status::FILE_IS_A_DIRECTORY));
        }
        let e = self.cache.entry(&s.key);
        e.lock().init_new()?;
        self.mark_modified(id, 0).ok_or(FsError(status::FILE_DELETED))
    }

    pub fn set_times(&self, id: NodeId, created: u64, modified: u64) -> Option<NodeInfo> {
        {
            let mut ns = self.ns.lock();
            let n = ns.get_mut(id)?;
            // Kept locally only: the protocol has no way to set times.
            if created != 0 {
                n.created = created;
            }
            if modified != 0 {
                n.modified = modified;
            }
        }
        self.info(id)
    }

    /// FlushFileBuffers / close-after-write: persist and schedule the upload.
    pub fn flush(&self, id: NodeId) {
        let Ok(s) = self.snap(id) else { return };
        if s.is_dir || s.key.is_empty() {
            return;
        }
        self.cache.entry(&s.key).lock().flush();
        if s.dirty || s.remote_id.is_none() {
            self.enqueue(id);
        }
    }

    // ------------------------------------------------------------- deletion

    pub fn can_delete(&self, id: NodeId) -> FsResult<()> {
        if id == ROOT {
            return Err(FsError(status::ACCESS_DENIED));
        }
        let ns = self.ns.lock();
        let n = ns.get(id).ok_or(FsError(status::FILE_DELETED))?;
        if n.is_dir && !ns.is_empty_dir(id) {
            return Err(FsError(status::DIRECTORY_NOT_EMPTY));
        }
        Ok(())
    }

    /// Delete now (cleanup with the delete flag). Remote first; if offline,
    /// the remote delete is queued.
    pub fn delete(&self, id: NodeId) -> FsResult<()> {
        if id == ROOT {
            return Err(FsError(status::ACCESS_DENIED));
        }
        let mut ns = self.ns.lock();
        let Some(n) = ns.get(id) else { return Ok(()) };
        let kind = if n.is_dir { ItemType::Folder } else { ItemType::File };
        if let Some(rid) = n.remote_id.clone() {
            match self.net_result(self.api.delete_item(kind, &rid)) {
                Ok(()) => {}
                Err(e) if e.is_network() => {
                    tracing::info!("offline: delete of {rid} queued");
                    self.queue.lock().deletes.push((kind, rid));
                    self.queue_cv.notify_all();
                }
                Err(e) => {
                    tracing::warn!("delete {rid}: {e}");
                    return Err(e.into());
                }
            }
            self.bump();
        }
        let removed = ns.remove(id);
        drop(ns);
        let mut q = self.queue.lock();
        for r in &removed {
            q.due.remove(&r.id);
        }
        drop(q);
        for r in removed {
            if r.is_file() && !r.cache_key.is_empty() {
                self.cache.remove(&r.cache_key);
            }
        }
        Ok(())
    }

    // --------------------------------------------------------------- rename

    pub fn rename(&self, id: NodeId, new_path: &str, replace: bool) -> FsResult<()> {
        if id == ROOT {
            return Err(FsError(status::ACCESS_DENIED));
        }
        let mut ns = self.ns.lock();
        let (np, nname) = ns
            .parent_of_path(new_path)
            .ok_or(FsError(status::OBJECT_PATH_NOT_FOUND))?;
        let nname = nname.to_string();
        if !is_valid_component(&nname) {
            return Err(FsError(status::OBJECT_NAME_INVALID));
        }
        let src = ns.get(id).cloned().ok_or(FsError(status::FILE_DELETED))?;
        if src.is_dir && ns.is_ancestor(id, np) {
            return Err(FsError(status::INVALID_PARAMETER));
        }
        let target = ns.child(np, &nname).filter(|t| *t != id);
        let mut adopted = false;
        let mut retag: Option<(String, String, String)> = None;
        let mut drop_keys = Vec::new();

        if let Some(t) = target {
            if !replace {
                return Err(FsError(status::OBJECT_NAME_COLLISION));
            }
            let tn = ns.get(t).cloned().unwrap();
            if tn.is_dir || src.is_dir {
                return Err(FsError(status::ACCESS_DENIED));
            }
            if src.remote_id.is_none() && tn.remote_id.is_some() {
                // "Save to temp, rename over the original": the new bytes
                // take over the original's server id (keeps shares/history).
                self.adopt(&mut ns, id, &tn);
                adopted = true;
            } else if let Some(trid) = &tn.remote_id {
                self.net_result(self.api.delete_item(ItemType::File, trid))?;
                self.bump();
            }
            for r in ns.remove(t) {
                if !r.cache_key.is_empty() {
                    drop_keys.push(r.cache_key);
                }
                self.queue.lock().due.remove(&r.id);
            }
        } else if src.is_file() && src.remote_id.is_none() && !is_temp_name(&nname) {
            // Excel/Word: the original was first renamed away to a scratch
            // name (a shadow). Put the new bytes back under its server id.
            let np_rid = ns.get(np).and_then(|n| n.remote_id.clone());
            let shadow = ns
                .node_ids()
                .into_iter()
                .filter_map(|x| ns.get(x))
                .find(|n| {
                    n.shadow
                        && n.is_file()
                        && n.remote_id.is_some()
                        && n.remote_parent == np_rid
                        && crate::util::fold(&n.remote_name) == crate::util::fold(&nname)
                })
                .cloned();
            if let Some(sh) = shadow {
                self.adopt(&mut ns, id, &sh);
                ns.set_remote_id(sh.id, None);
                if let Some(s) = ns.get_mut(sh.id) {
                    s.shadow = false;
                    s.dirty = false;
                    s.remote_tag.clear();
                }
                adopted = true;
            }
        }

        let src = ns.get(id).cloned().unwrap();
        let np_rid = if np == ROOT {
            None
        } else {
            ns.get(np).and_then(|n| n.remote_id.clone())
        };

        match &src.remote_id {
            Some(rid) if src.is_file() && is_temp_name(&nname) && !adopted => {
                // Renamed to a scratch name: keep it where it is on the
                // server until we see what the app does next.
                tracing::debug!("{} -> {nname}: local-only (shadow) rename", src.name);
                let _ = rid;
                ns.move_node(id, np, &nname);
                if let Some(n) = ns.get_mut(id) {
                    n.shadow = true;
                }
            }
            Some(rid) => {
                let parent_rid = if np == ROOT {
                    None
                } else if np_rid.is_some() {
                    np_rid.clone()
                } else {
                    self.ensure_remote_dir(&mut ns, np)?
                };
                let name_change = (src.remote_name != nname).then_some(nname.as_str());
                let parent_change =
                    (src.remote_parent != parent_rid).then_some(parent_rid.as_deref());
                if name_change.is_some() || parent_change.is_some() {
                    let kind = if src.is_dir { ItemType::Folder } else { ItemType::File };
                    let row = self
                        .net_result(self.api.move_item(kind, rid, name_change, parent_change))
                        .map_err(|e| {
                            tracing::warn!("move {} -> {new_path}: {e}", src.name);
                            FsError::from(e)
                        })?;
                    self.bump();
                    if src.is_file() && !src.dirty {
                        if let Some(u) = row.updated_at.as_deref() {
                            let tag = remote_tag(row.size_bytes.unwrap_or(src.size), Some(u));
                            retag = Some((src.cache_key.clone(), src.remote_tag.clone(), tag.clone()));
                            if let Some(n) = ns.get_mut(id) {
                                n.remote_tag = tag;
                            }
                        }
                    }
                }
                ns.move_node(id, np, &nname);
                if let Some(n) = ns.get_mut(id) {
                    n.shadow = false;
                    n.remote_name = nname.clone();
                    n.remote_parent = parent_rid;
                }
            }
            None => {
                ns.move_node(id, np, &nname);
            }
        }
        let needs_upload = {
            let n = ns.get(id).unwrap();
            n.is_file() && (n.dirty || n.remote_id.is_none()) && !is_temp_name(&n.name)
        };
        let pending_dir = src.is_dir && src.remote_id.is_none();
        drop(ns);
        for k in drop_keys {
            self.cache.remove(&k);
        }
        if let Some((key, old, new)) = retag {
            let e = self.cache.entry(&key);
            let mut g = e.lock();
            if g.tag() == old {
                g.set_tag(&new);
            }
        }
        if needs_upload || pending_dir {
            self.enqueue(id);
        }
        Ok(())
    }

    /// Node `id` (local bytes) takes over `from`'s server identity.
    fn adopt(&self, ns: &mut Namespace, id: NodeId, from: &crate::ns::Node) {
        tracing::info!(
            "{} replaces {} (keeps server id {:?})",
            ns.get(id).map(|n| n.name.clone()).unwrap_or_default(),
            from.remote_name,
            from.remote_id
        );
        let rid = from.remote_id.clone();
        if from.id != id {
            ns.set_remote_id(from.id, None);
        }
        ns.set_remote_id(id, rid);
        if let Some(n) = ns.get_mut(id) {
            n.remote_name = from.remote_name.clone();
            n.remote_parent = from.remote_parent.clone();
            n.remote_tag = from.remote_tag.clone();
            n.shadow = false;
            n.dirty = true;
            n.gen += 1;
        }
    }

    // --------------------------------------------------------------- upload

    pub fn enqueue(&self, id: NodeId) {
        self.enqueue_at(id, Instant::now() + self.opts.upload_delay);
    }

    fn enqueue_at(&self, id: NodeId, when: Instant) {
        let mut q = self.queue.lock();
        let e = q.due.entry(id).or_insert((when, 0));
        e.0 = when;
        drop(q);
        self.queue_cv.notify_all();
    }

    /// True when nothing is waiting or in flight.
    pub fn idle(&self) -> bool {
        let q = self.queue.lock();
        q.due.is_empty() && q.deletes.is_empty() && !q.uploading
    }

    /// Block until the upload queue drains (or the timeout passes).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.idle() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.idle()
    }

    /// Make every queued item due now (used on quit).
    pub fn expedite(&self) {
        let mut q = self.queue.lock();
        let now = Instant::now();
        for v in q.due.values_mut() {
            v.0 = now;
        }
        drop(q);
        self.queue_cv.notify_all();
    }

    fn upload_loop(&self) {
        loop {
            let next = {
                let mut q = self.queue.lock();
                loop {
                    if self.stopping.load(Ordering::SeqCst) && q.due.is_empty() {
                        return;
                    }
                    if self.stopping.load(Ordering::SeqCst) {
                        // Quit: drain what's queued, no more waiting.
                        let now = Instant::now();
                        for v in q.due.values_mut() {
                            v.0 = v.0.min(now);
                        }
                    }
                    if self.paused.load(Ordering::Relaxed) && !self.stopping.load(Ordering::SeqCst) {
                        self.queue_cv.wait_for(&mut q, Duration::from_secs(1));
                        continue;
                    }
                    if !q.deletes.is_empty() {
                        break None;
                    }
                    let now = Instant::now();
                    let soonest = q.due.iter().min_by_key(|(_, (t, _))| *t).map(|(k, v)| (*k, *v));
                    match soonest {
                        Some((id, (t, attempts))) if t <= now => {
                            q.due.remove(&id);
                            q.uploading = true;
                            break Some((id, attempts));
                        }
                        Some((_, (t, _))) => {
                            let d = t - now;
                            self.queue_cv.wait_for(&mut q, d);
                        }
                        None => {
                            self.queue_cv.wait_for(&mut q, Duration::from_secs(5));
                        }
                    }
                }
            };
            match next {
                None => self.retry_deletes(),
                Some((id, attempts)) => {
                    let r = self.upload_one(id);
                    self.queue.lock().uploading = false;
                    if let Err((e, retry)) = r {
                        *self.last_error.lock() = Some(e.to_string());
                        if retry && !self.stopping.load(Ordering::SeqCst) {
                            let backoff = if e.is_quota() || e.is_auth() {
                                Duration::from_secs(300)
                            } else {
                                Duration::from_secs(2u64.pow(attempts.min(8))).min(Duration::from_secs(300))
                            };
                            tracing::warn!("upload of node {id} failed ({e}); retry in {backoff:?}");
                            let mut q = self.queue.lock();
                            let ent = q.due.entry(id).or_insert((Instant::now() + backoff, attempts + 1));
                            ent.1 = attempts + 1;
                        } else {
                            tracing::error!("upload of node {id} failed permanently: {e}");
                        }
                    } else {
                        *self.last_error.lock() = None;
                    }
                }
            }
        }
    }

    fn retry_deletes(&self) {
        let items = std::mem::take(&mut self.queue.lock().deletes);
        let mut failed = Vec::new();
        for (k, rid) in items {
            match self.net_result(self.api.delete_item(k, &rid)) {
                Ok(()) => self.bump(),
                Err(e) if e.is_network() => failed.push((k, rid)),
                Err(e) => tracing::warn!("queued delete of {rid} failed: {e}"),
            }
        }
        if !failed.is_empty() {
            let mut q = self.queue.lock();
            q.deletes.extend(failed);
            // Back off a little: wait on the condvar with a timeout.
            self.queue_cv.wait_for(&mut q, Duration::from_secs(10));
        }
    }

    /// Upload one node. Err((error, retry?)).
    fn upload_one(&self, id: NodeId) -> Result<(), (ApiError, bool)> {
        // Pending directory: just create it.
        {
            let mut ns = self.ns.lock();
            let Some(n) = ns.get(id) else { return Ok(()) };
            if n.is_dir {
                if n.remote_id.is_none() {
                    return self
                        .ensure_remote_dir(&mut ns, id)
                        .map(|_| ())
                        .map_err(|e| (ApiError::Network(e.to_string()), true));
                }
                return Ok(());
            }
        }
        let mut conflict_attempt = false;
        loop {
            let (rid, name, parent_rid, key, gen) = {
                let mut ns = self.ns.lock();
                let Some(n) = ns.get(id) else { return Ok(()) };
                let needs = n.dirty || n.remote_id.is_none();
                if !needs {
                    return Ok(());
                }
                if n.remote_id.is_none() && is_temp_name(&n.name) {
                    return Ok(()); // scratch file: stays local
                }
                let (rid, name, key, gen, parent) =
                    (n.remote_id.clone(), n.name.clone(), n.cache_key.clone(), n.gen, n.parent);
                let parent_rid = if rid.is_none() {
                    match self.ensure_remote_dir(&mut ns, parent) {
                        Ok(p) => p,
                        Err(e) => {
                            return Err((
                                ApiError::Network(format!("creating parent folder: {e}")),
                                true,
                            ))
                        }
                    }
                } else {
                    None
                };
                (rid, name, parent_rid, key, gen)
            };
            let upload_name = if conflict_attempt {
                conflict_name(&name, &self.opts.host_name)
            } else {
                name.clone()
            };
            let path = {
                let e = self.cache.entry(&key);
                let mut g = e.lock();
                if !g.is_complete() {
                    tracing::error!("{name}: local copy incomplete, not uploading it");
                    drop(g);
                    if let Some(n) = self.ns.lock().get_mut(id) {
                        n.dirty = false;
                    }
                    return Ok(());
                }
                g.flush();
                g.data_path().to_path_buf()
            };
            tracing::info!(
                "uploading {name} ({})",
                if rid.is_some() { "replace" } else { "new" }
            );
            let r = match &rid {
                Some(r) => self.api.upload_replace(r, &path),
                None => self
                    .api
                    .upload_new(&self.storage_id, parent_rid.as_deref(), &upload_name, &path),
            };
            let r = self.net_result(r);
            match r {
                Ok(pf) => {
                    let mut new_id = pf.file_id.clone().or(rid.clone());
                    if new_id.is_none() {
                        // Server didn't say; find it in a fresh tree.
                        if let Ok(Some(t)) = self.api.tree(&self.storage_id, None) {
                            new_id = t
                                .files
                                .iter()
                                .find(|f| f.folder_id == parent_rid && f.name == upload_name)
                                .map(|f| f.file_id.clone());
                        }
                    }
                    let tag = remote_tag(pf.size_bytes.unwrap_or(0), pf.updated_at.as_deref());
                    let mut ns = self.ns.lock();
                    match ns.get(id) {
                        None => {
                            // Deleted while uploading: undo the new file.
                            if rid.is_none() {
                                if let Some(fid) = &new_id {
                                    let _ = self.api.delete_item(ItemType::File, fid);
                                }
                            }
                        }
                        Some(_) => {
                            if rid.is_none() {
                                ns.set_remote_id(id, new_id.clone());
                            }
                            let n = ns.get_mut(id).unwrap();
                            if rid.is_none() {
                                n.remote_name = pf.name.clone().unwrap_or(upload_name.clone());
                                n.remote_parent = parent_rid.clone();
                            }
                            n.remote_tag = tag.clone();
                            if n.gen == gen {
                                n.dirty = false;
                            } else {
                                // Written again meanwhile; go again.
                                drop(ns);
                                self.enqueue(id);
                                ns = self.ns.lock();
                            }
                        }
                    }
                    self.bump();
                    drop(ns);
                    self.cache.entry(&key).lock().set_tag(&tag);
                    tracing::info!("uploaded {upload_name}");
                    return Ok(());
                }
                Err(e) if rid.is_none() && e.is_name_taken() && !conflict_attempt => {
                    tracing::warn!("{name} already exists on the server; uploading as a conflicted copy");
                    conflict_attempt = true;
                    continue;
                }
                Err(e) if rid.is_some() && e.is_not_found() => {
                    tracing::warn!("{name} vanished from the server; re-creating it");
                    self.ns.lock().set_remote_id(id, None);
                    continue;
                }
                Err(e) if rid.is_none() && e.is_not_found() => {
                    // Parent folder vanished: forget its id so it's re-created.
                    let mut ns = self.ns.lock();
                    if let Some(p) = ns.get(id).map(|n| n.parent) {
                        if p != ROOT {
                            ns.set_remote_id(p, None);
                        }
                    }
                    return Err((e, true));
                }
                Err(e) => {
                    let retry = e.status() != Some(413) && e.status() != Some(400);
                    return Err((e, retry));
                }
            }
        }
    }

    /// Stop background work after flushing uploads (bounded).
    pub fn shutdown(&self, wait: Duration) {
        // Stopping makes the uploader drain its queue immediately (even when
        // paused) and then exit.
        self.stop();
        if !self.wait_idle(wait) {
            tracing::warn!("{}: quitting with uploads still pending", self.label());
        }
        self.cache.close_all();
    }
}

/// A consistent copy of what I/O needs from a node.
pub(crate) struct Snap {
    pub is_dir: bool,
    pub name: String,
    pub remote_id: Option<String>,
    pub key: String,
    pub tag: String,
    pub size: u64,
    pub dirty: bool,
    pub local_only: bool,
}

