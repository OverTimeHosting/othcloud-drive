//! Backup status in File Explorer (PROTOCOL.md "Backup status in File
//! Explorer"): which files and folders inside a backed-up folder are backed
//! up, still to be backed up, or can't be — for the overlay handlers in the
//! Companion's shell DLL.
//!
//! - The backup publishes a [`FolderMarks`] per folder (`backup.rs`, at the
//!   points it saves its state anyway): what was uploaded (size + modified
//!   time), what is waiting, what failed.
//! - [`dir_marks`] answers "the entries of this directory" from that plus
//!   ONE listing of the directory (sizes and times come with the listing):
//!   a file is backed up when its size and time are what was uploaded; a
//!   folder shows the worst of what the backup knows is inside it, from
//!   sets built when the marks are published — never a walk of the tree.
//! - The status board, a shared-memory section the DLL reads without
//!   asking anything: a generation (bumped whenever an answer may have
//!   changed) and the backed-up folders' paths (so every path outside them
//!   is "no" in the DLL at once).
//! - Changes are batched (once a second) into `SHChangeNotify` for the
//!   items whose mark changed and their folders, so Explorer asks again.
//!
//! Nothing here reaches the network.

use crate::backup::{FolderJob, Skip};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Mark {
    /// Uploaded and unchanged since.
    Ok = 1,
    /// New or changed: waiting to settle, queued, uploading, or paused.
    Pending = 2,
    /// Can't be backed up: failed, in use, too large, the drive is full.
    Problem = 3,
}

/// One backed-up folder as the backup sees it, for Explorer. Relative paths
/// are lower-case, '/'-separated.
#[derive(Debug, Clone)]
pub struct FolderMarks {
    pub key: String,
    pub job: FolderJob,
    /// rel → (size, mtime secs) uploaded.
    pub uploaded: Arc<HashMap<String, (u64, u64)>>,
    pub pending: HashSet<String>,
    pub problems: HashSet<String>,
    /// Every folder (rel, "" = the backed-up folder) with something pending
    /// / a problem somewhere inside.
    pub pending_dirs: HashSet<String>,
    pub problem_dirs: HashSet<String>,
    /// The drive is full: nothing that isn't backed up yet can be.
    pub stuck: bool,
    /// A whole pass has looked at every file since the app started (until
    /// then a folder's contents are "still checking": pending).
    pub checked: bool,
}

pub fn rel_key(rel: &str) -> String {
    rel.replace('\\', "/").trim_matches('/').to_lowercase()
}

fn ancestors(rel: &str, into: &mut HashSet<String>) {
    let mut r = rel;
    while let Some(i) = r.rfind('/') {
        r = &r[..i];
        into.insert(r.to_string());
    }
    into.insert(String::new());
}

impl FolderMarks {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: &str,
        job: FolderJob,
        uploaded: Arc<HashMap<String, (u64, u64)>>,
        pending: impl IntoIterator<Item = String>,
        problems: impl IntoIterator<Item = String>,
        stuck: bool,
        checked: bool,
    ) -> FolderMarks {
        let pending: HashSet<String> = pending.into_iter().map(|r| rel_key(&r)).filter(|r| !r.is_empty()).collect();
        let problems: HashSet<String> = problems.into_iter().map(|r| rel_key(&r)).filter(|r| !r.is_empty()).collect();
        let mut pending_dirs = HashSet::new();
        for r in &pending {
            ancestors(r, &mut pending_dirs);
        }
        let mut problem_dirs = HashSet::new();
        for r in &problems {
            ancestors(r, &mut problem_dirs);
        }
        FolderMarks { key: key.to_string(), job, uploaded, pending, problems, pending_dirs, problem_dirs, stuck, checked }
    }

    /// The uploaded map, keyed for lookups here, from the backup's state.
    pub fn uploaded_index(files: &HashMap<String, crate::backup::Uploaded>) -> Arc<HashMap<String, (u64, u64)>> {
        Arc::new(files.iter().filter(|(_, u)| !u.gone).map(|(r, u)| (rel_key(r), (u.size, u.mtime))).collect())
    }

    fn file_mark(&self, rel: &str, size: u64, mtime: u64) -> Mark {
        if self.problems.contains(rel) {
            Mark::Problem
        } else if self.uploaded.get(rel) == Some(&(size, mtime)) {
            Mark::Ok
        } else if self.stuck {
            Mark::Problem
        } else {
            Mark::Pending
        }
    }

    fn dir_mark(&self, rel: &str) -> Mark {
        if self.problem_dirs.contains(rel) {
            Mark::Problem
        } else if self.pending_dirs.contains(rel) || !self.checked {
            Mark::Pending
        } else {
            Mark::Ok
        }
    }

    /// What changed between two publications of a folder, as relative paths
    /// (with their folders, whose roll-up may have changed).
    fn changes(old: Option<&FolderMarks>, new: &FolderMarks) -> HashSet<String> {
        let mut out = HashSet::new();
        let Some(old) = old else {
            out.insert(String::new());
            return out;
        };
        let mut items: Vec<&String> = Vec::new();
        items.extend(old.pending.symmetric_difference(&new.pending));
        items.extend(old.problems.symmetric_difference(&new.problems));
        if !Arc::ptr_eq(&old.uploaded, &new.uploaded) {
            for (k, v) in new.uploaded.iter() {
                if old.uploaded.get(k) != Some(v) {
                    items.push(k);
                }
            }
            for k in old.uploaded.keys() {
                if !new.uploaded.contains_key(k) {
                    items.push(k);
                }
            }
        }
        for r in items {
            out.insert(r.clone());
            ancestors(r, &mut out);
        }
        if old.stuck != new.stuck || old.checked != new.checked {
            out.insert(String::new());
        }
        out
    }
}

// --------------------------------------------------------------- registry

/// Paths to tell the shell about at once, at most (past it: their folders).
const MAX_NOTIFY: usize = 300;
/// A directory listing's entries looked at, at most.
pub const MAX_ENTRIES: usize = 20_000;
/// The board's size and layout (the DLL reads the same).
pub const BOARD_SIZE: usize = 64 * 1024;
pub const BOARD_MAGIC: &[u8; 4] = b"OTHB";

#[derive(Default)]
struct Registry {
    enabled: bool,
    folders: HashMap<String, Arc<FolderMarks>>,
    /// Absolute paths whose overlay may have changed, not yet told.
    changed: HashSet<PathBuf>,
    /// Folders to refresh whole (UPDATEDIR).
    changed_dirs: HashSet<PathBuf>,
    board: Option<Board>,
    generation: u64,
    notifier: bool,
}

static REG: std::sync::LazyLock<Mutex<Registry>> = std::sync::LazyLock::new(|| Mutex::new(Registry::default()));

/// Show backup status in Explorer (or stop). On: the board exists and the
/// pipe answers; off: the board lists no folders, so the DLL shows nothing.
pub fn set_enabled(on: bool) {
    let mut r = REG.lock();
    if r.enabled == on {
        return;
    }
    r.enabled = on;
    if on && r.board.is_none() {
        match Board::create() {
            Ok(b) => r.board = Some(b),
            Err(e) => tracing::warn!("backup status board: {e}; Explorer shows no backup status"),
        }
    }
    let roots: Vec<PathBuf> = r.folders.values().map(|f| f.job.root.clone()).collect();
    for root in roots {
        r.changed.insert(root);
    }
    write_board(&mut r);
    ensure_notifier(&mut r);
    drop(r);
    // The overlay handlers ask over the thumbnail pipe (tests serve their own).
    #[cfg(not(test))]
    if on {
        crate::thumbs::ensure_serving();
    }
}

pub fn enabled() -> bool {
    REG.lock().enabled
}

/// A folder's marks changed (or appeared).
pub fn publish(marks: FolderMarks) {
    let mut r = REG.lock();
    let changes = FolderMarks::changes(r.folders.get(&marks.key).map(|a| a.as_ref()), &marks);
    let root = marks.job.root.clone();
    let roots_changed = r.folders.get(&marks.key).is_none_or(|o| o.job.root != root);
    if changes.len() > MAX_NOTIFY {
        r.changed_dirs.insert(root.clone());
        r.changed.insert(root.clone());
    } else {
        for c in changes {
            let p = if c.is_empty() { root.clone() } else { root.join(c.replace('/', "\\")) };
            r.changed.insert(p);
        }
    }
    r.folders.insert(marks.key.clone(), Arc::new(marks));
    if roots_changed {
        write_board(&mut r);
    }
    ensure_notifier(&mut r);
}

/// A folder is no longer backed up (removed, turned off, the backup stopped).
pub fn remove(key: &str) {
    let mut r = REG.lock();
    if let Some(f) = r.folders.remove(key) {
        r.changed.insert(f.job.root.clone());
        r.changed_dirs.insert(f.job.root.clone());
        write_board(&mut r);
        ensure_notifier(&mut r);
    }
}

/// The folders currently published (keys).
pub fn published() -> Vec<String> {
    REG.lock().folders.keys().cloned().collect()
}

/// The app is quitting: the board lists nothing, so Explorer shows no stale
/// status (the DLL also stops when the pipe is gone).
pub fn shutdown() {
    let mut r = REG.lock();
    r.folders.clear();
    write_board(&mut r);
}

fn write_board(r: &mut Registry) {
    r.generation += 1;
    let roots: Vec<String> = if r.enabled {
        r.folders.values().map(|f| format!("{}\\", crate::backup::norm(&f.job.root))).collect()
    } else {
        Vec::new()
    };
    let generation = r.generation;
    if let Some(b) = &r.board {
        b.write(generation, &roots);
    }
}

fn ensure_notifier(r: &mut Registry) {
    if r.notifier || (r.changed.is_empty() && r.changed_dirs.is_empty()) {
        return;
    }
    r.notifier = true;
    let spawned = std::thread::Builder::new().name("shell-status".into()).spawn(|| loop {
        // Batch: whatever changed in the last second goes out together.
        std::thread::sleep(Duration::from_secs(1));
        let (items, dirs, enabled) = {
            let mut r = REG.lock();
            if r.changed.is_empty() && r.changed_dirs.is_empty() {
                r.notifier = false;
                return;
            }
            let items: Vec<PathBuf> = r.changed.drain().collect();
            let dirs: Vec<PathBuf> = r.changed_dirs.drain().collect();
            // New answers from here on: the DLL's cached ones are stale.
            write_board(&mut r);
            (items, dirs, r.enabled || r.board.is_some())
        };
        if enabled {
            notify(&items, &dirs);
        }
    });
    if spawned.is_err() {
        r.notifier = false;
    }
}

/// `SHChangeNotify` for each item (FLUSHNOWAIT: never waits on Explorer);
/// past [`MAX_NOTIFY`], one UPDATEDIR per parent folder instead.
fn notify(items: &[PathBuf], dirs: &[PathBuf]) {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_UPDATEDIR, SHCNE_UPDATEITEM, SHCNF_FLUSHNOWAIT, SHCNF_PATHW};
    let call = |ev, p: &Path| {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        unsafe { SHChangeNotify(ev, SHCNF_PATHW | SHCNF_FLUSHNOWAIT, Some(w.as_ptr() as *const _), None) };
    };
    let mut dirs: HashSet<PathBuf> = dirs.iter().cloned().collect();
    if items.len() > MAX_NOTIFY {
        dirs.extend(items.iter().filter_map(|p| p.parent().map(Path::to_path_buf)));
    } else {
        for p in items {
            call(SHCNE_UPDATEITEM, p);
        }
    }
    for d in dirs.iter().take(MAX_NOTIFY) {
        call(SHCNE_UPDATEDIR, d);
    }
    tracing::debug!("backup status: told Explorer about {} item(s), {} folder(s)", items.len().min(MAX_NOTIFY), dirs.len());
}

// ---------------------------------------------------------------- answers

/// The marks of `dir`'s entries (name as on disk), or None when `dir` is
/// neither inside a backed-up folder nor the folder one is in (or the
/// status display is off). Entries without a mark are left out.
pub fn dir_marks(dir: &Path) -> Option<(u64, Vec<(OsString, Mark)>)> {
    let (generation, folders) = {
        let r = REG.lock();
        if !r.enabled {
            return None;
        }
        (r.generation, r.folders.values().cloned().collect::<Vec<_>>())
    };
    let d = crate::backup::norm(dir);
    let mut out = Vec::new();
    let mut ours = false;
    for f in &folders {
        let root = crate::backup::norm(&f.job.root);
        if d == root || d.starts_with(&format!("{root}\\")) {
            ours = true;
            let rel = d[root.len()..].trim_start_matches('\\').replace('\\', "/");
            list(f, &rel, dir, &mut out);
        } else if f.job.root.parent().map(crate::backup::norm).as_deref() == Some(d.as_str()) {
            // The backed-up folder itself, seen from the folder it is in.
            ours = true;
            if let Some(name) = f.job.root.file_name() {
                out.push((name.to_os_string(), f.dir_mark("")));
            }
        }
    }
    ours.then_some((generation, out))
}

fn list(f: &FolderMarks, rel_dir: &str, dir: &Path, out: &mut Vec<(OsString, Mark)>) {
    // Inside a skipped folder (node_modules, excluded, hidden…): nothing.
    let parts: Vec<&str> = rel_dir.split('/').filter(|s| !s.is_empty()).collect();
    for i in 0..parts.len() {
        let sub = parts[..=i].join("/");
        let ap = f.job.root.join(sub.replace('/', "\\"));
        match std::fs::symlink_metadata(&ap) {
            Ok(m) if m.is_dir() && f.job.dir_skip(&sub, parts[i], &m, &ap).is_none() => {}
            _ => return,
        }
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten().take(MAX_ENTRIES) {
        let Ok(md) = e.metadata() else { continue };
        let name = e.file_name();
        let n = name.to_string_lossy().to_string();
        let rel = if rel_dir.is_empty() { n.clone() } else { format!("{rel_dir}/{n}") };
        let key = rel.to_lowercase();
        let mark = if md.is_dir() {
            match f.job.dir_skip(&rel, &n, &md, &e.path()) {
                None => Some(f.dir_mark(&key)),
                Some(_) => None,
            }
        } else {
            match f.job.file_skip(&rel, &n, &md) {
                None => Some(f.file_mark(&key, md.len(), crate::backup::mtime_secs(&md))),
                Some(Skip::TooLarge) => Some(Mark::Problem),
                Some(_) => None,
            }
        };
        if let Some(m) = mark {
            out.push((name, m));
        }
    }
}

// ------------------------------------------------------------------ board

/// The status board: `Local\othcloud-backup-status-<user SID>`, 64 KiB.
///
/// ```text
/// 0   "OTHB"
/// 4   u32 seq         odd while being written (seqlock)
/// 8   u64 generation  bumped whenever an answer may have changed
/// 16  u32 roots bytes
/// 20  roots           UTF-16LE, each lower-case and ending in '\', NUL-separated
/// ```
pub fn board_name() -> String {
    std::env::var("OTHCLOUD_BACKUP_BOARD")
        .unwrap_or_else(|_| format!(r"Local\othcloud-backup-status-{}", crate::cf::user_sid()))
}

struct Board {
    view: windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    _h: windows::Win32::Foundation::HANDLE,
}
unsafe impl Send for Board {}

impl Board {
    fn create() -> std::io::Result<Board> {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{LocalFree, HLOCAL, INVALID_HANDLE_VALUE};
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_WRITE, PAGE_READWRITE};
        // This user and SYSTEM only.
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", crate::cf::user_sid()));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None) }
            .map_err(std::io::Error::other)?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let h = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&sa), PAGE_READWRITE, 0, BOARD_SIZE as u32, &HSTRING::from(board_name()))
        };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        // An existing one (a DLL still holds it from our last run) is opened
        // and reused: that is the point.
        let h = h.map_err(std::io::Error::other)?;
        let view = unsafe { MapViewOfFile(h, FILE_MAP_WRITE, 0, 0, BOARD_SIZE) };
        if view.Value.is_null() {
            let e = std::io::Error::last_os_error();
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(h);
            }
            return Err(e);
        }
        Ok(Board { view, _h: h })
    }

    fn write(&self, generation: u64, roots: &[String]) {
        use std::sync::atomic::{fence, AtomicU32, Ordering};
        let base = self.view.Value as *mut u8;
        let mut w: Vec<u8> = Vec::new();
        for r in roots {
            let bytes: Vec<u8> = r.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
            if w.len() + bytes.len() > BOARD_SIZE - 20 {
                break;
            }
            w.extend(bytes);
        }
        unsafe {
            let seq = &*(base.add(4) as *const AtomicU32);
            let s = seq.load(Ordering::Relaxed);
            // A board left mid-write by a crash is odd already.
            let start = if s % 2 == 1 { s } else { s.wrapping_add(1) };
            seq.store(start, Ordering::Relaxed);
            fence(Ordering::Release);
            std::ptr::copy_nonoverlapping(BOARD_MAGIC.as_ptr(), base, 4);
            // Never go backwards: a restarted app continues the old count.
            let old = u64::from_le_bytes(std::ptr::read_unaligned(base.add(8) as *const [u8; 8]));
            let g = generation.max(old.wrapping_add(1));
            std::ptr::write_unaligned(base.add(8) as *mut [u8; 8], g.to_le_bytes());
            std::ptr::write_unaligned(base.add(16) as *mut [u8; 4], (w.len() as u32).to_le_bytes());
            std::ptr::copy_nonoverlapping(w.as_ptr(), base.add(20), w.len());
            fence(Ordering::Release);
            seq.store(start.wrapping_add(1), Ordering::Release);
        }
    }

    /// The generation as written (it may be ahead of ours, see `write`).
    fn generation(&self) -> u64 {
        unsafe { u64::from_le_bytes(std::ptr::read_unaligned((self.view.Value as *const u8).add(8) as *const [u8; 8])) }
    }
}

/// The board's generation right now (what an answer is tagged with).
pub fn current_generation() -> u64 {
    let r = REG.lock();
    r.board.as_ref().map(|b| b.generation()).unwrap_or(r.generation)
}

// ------------------------------------------------------------------- wire

/// The `OTHS` answer body: u64 generation | u32 count | count × (u8 mark |
/// u8 0 | u16 name units | name UTF-16LE), at most `max` bytes.
pub fn encode_marks(generation: u64, entries: &[(OsString, Mark)], max: usize) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut out = Vec::with_capacity(12 + entries.len() * 24);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let mut n = 0u32;
    for (name, mark) in entries {
        let w: Vec<u16> = name.encode_wide().collect();
        if w.is_empty() || w.len() > u16::MAX as usize || out.len() + 4 + w.len() * 2 > max {
            continue;
        }
        out.push(*mark as u8);
        out.push(0);
        out.extend_from_slice(&(w.len() as u16).to_le_bytes());
        for c in w {
            out.extend_from_slice(&c.to_le_bytes());
        }
        n += 1;
    }
    out[8..12].copy_from_slice(&n.to_le_bytes());
    out
}

/// The pipe's answer to a status request: Ok(body) or "not ours".
pub fn answer(dir: &Path, max: usize) -> Option<Vec<u8>> {
    // Tagged with the board's generation from BEFORE the answer was worked
    // out: anything published meanwhile bumps it, so the DLL won't keep an
    // answer that may have missed it.
    let generation = current_generation();
    let (_, entries) = dir_marks(dir)?;
    Some(encode_marks(generation, &entries, max))
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackupFolder, FolderKind};

    fn job(root: &Path) -> FolderJob {
        let f = BackupFolder {
            folder_key: "k".into(),
            label: "Test".into(),
            local_path: root.to_path_buf(),
            kind: FolderKind::Folder,
            enabled: true,
            excludes: vec!["*.iso".into()],
            remote_folder: None,
        };
        FolderJob::new(&f, "s", "PC", root, Vec::new())
    }

    fn stat(p: &Path) -> (u64, u64) {
        let m = std::fs::metadata(p).unwrap();
        (m.len(), crate::backup::mtime_secs(&m))
    }

    fn marks_of(dir: &Path) -> HashMap<String, Mark> {
        dir_marks(dir).unwrap().1.into_iter().map(|(n, m)| (n.to_string_lossy().to_string(), m)).collect()
    }

    #[test]
    fn files_and_folders_get_the_worst_mark() {
        let _g = TEST_LOCK.lock();
        std::env::set_var("OTHCLOUD_BACKUP_BOARD", format!(r"Local\othcloud-backup-status-test-{}", std::process::id()));
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("Docs");
        std::fs::create_dir_all(root.join("Sub/Deep")).unwrap();
        std::fs::create_dir_all(root.join("Clean")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        for f in ["ok.txt", "new.txt", "bad.txt", "skip.tmp", "big.iso", "Sub/Deep/wait.txt", "Clean/a.txt"] {
            std::fs::write(root.join(f), f).unwrap();
        }
        let mut up = HashMap::new();
        up.insert("ok.txt".to_string(), stat(&root.join("ok.txt")));
        up.insert("bad.txt".to_string(), stat(&root.join("bad.txt")));
        up.insert("clean/a.txt".to_string(), stat(&root.join("Clean/a.txt")));
        let m = FolderMarks::new(
            "k",
            job(&root),
            Arc::new(up),
            ["Sub/Deep/wait.txt".to_string()],
            ["bad.txt".to_string()],
            false,
            true,
        );
        publish(m);
        set_enabled(true);
        let got = marks_of(&root);
        assert_eq!(got.get("ok.txt"), Some(&Mark::Ok));
        assert_eq!(got.get("new.txt"), Some(&Mark::Pending), "not uploaded yet");
        assert_eq!(got.get("bad.txt"), Some(&Mark::Problem));
        assert_eq!(got.get("Sub"), Some(&Mark::Pending), "something inside waits");
        assert_eq!(got.get("Clean"), Some(&Mark::Ok));
        assert!(!got.contains_key("skip.tmp"), "skipped by rule: no mark");
        assert!(!got.contains_key("big.iso"), "excluded: no mark");
        assert!(!got.contains_key("node_modules"));
        assert_eq!(marks_of(&root.join("Sub")).get("Deep"), Some(&Mark::Pending));
        assert_eq!(marks_of(&root.join("sub\\deep")).get("wait.txt"), Some(&Mark::Pending), "any case");
        assert!(marks_of(&root.join("node_modules")).is_empty());
        // The folder itself, from its parent.
        assert_eq!(marks_of(t.path()).get("Docs"), Some(&Mark::Problem));
        // Anything else: not ours.
        assert!(dir_marks(Path::new(r"C:\Windows")).is_none());

        // Changed since the upload: pending again.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(root.join("ok.txt"), "changed!").unwrap();
        assert_eq!(marks_of(&root).get("ok.txt"), Some(&Mark::Pending));

        // Off: nothing answers; removed: nothing either.
        set_enabled(false);
        assert!(dir_marks(&root).is_none());
        set_enabled(true);
        remove("k");
        assert!(dir_marks(&root).is_none());
        set_enabled(false);
    }

    #[test]
    fn changes_name_the_items_and_their_folders() {
        let root = PathBuf::from(r"C:\x");
        let a = FolderMarks::new("k", job(&root), Arc::new(HashMap::new()), ["A/b/c.txt".to_string()], [], false, true);
        let mut up = HashMap::new();
        up.insert("a/b/c.txt".to_string(), (1, 2));
        let b = FolderMarks::new("k", job(&root), Arc::new(up), [], [], false, true);
        let ch = FolderMarks::changes(Some(&a), &b);
        let mut v: Vec<_> = ch.into_iter().collect();
        v.sort();
        assert_eq!(v, vec!["", "a", "a/b", "a/b/c.txt"]);
        assert!(FolderMarks::changes(Some(&b), &b).is_empty());
        assert_eq!(FolderMarks::changes(None, &b).into_iter().collect::<Vec<_>>(), vec![String::new()]);
    }

    #[test]
    fn wire_format() {
        let body = encode_marks(7, &[(OsString::from("a.txt"), Mark::Ok), (OsString::from("Ünï"), Mark::Problem)], 1 << 20);
        assert_eq!(&body[..8], &7u64.to_le_bytes());
        assert_eq!(&body[8..12], &2u32.to_le_bytes());
        assert_eq!(body[12], 1);
        assert_eq!(u16::from_le_bytes([body[14], body[15]]), 5);
        // Capped: entries that don't fit are left out, the count says so.
        let small = encode_marks(7, &[(OsString::from("a.txt"), Mark::Ok), (OsString::from("b.txt"), Mark::Ok)], 12 + 14);
        assert_eq!(&small[8..12], &1u32.to_le_bytes());
    }
}
