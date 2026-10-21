//! Folder backup (PROTOCOL.md "Folder backup"): local folders → ONE drive,
//! one way. Each folder goes to `<drive>/Backups/<device>/<label>/…`
//! (the panel creates that folder and hands back its id in `PUT profile`);
//! the photo backup this grew out of is a `photos` folder that keeps
//! `<drive>/Photo backup/<device>` and its upload state.
//!
//! - Nothing is ever deleted remotely. A file gone from the PC is marked
//!   *deleted on this PC* (`POST deleted`), and unmarked if it comes back.
//! - A changed file replaces its remote copy with `X-OTH-Keep-Version: 1`,
//!   so the panel keeps the old contents as a version.
//! - A file is uploaded once it has been unchanged for [`SETTLE`]; a full
//!   rescan runs at start and every [`RESCAN_EVERY`], watcher events in
//!   between; at most [`UPLOADERS`] uploads at a time.
//! - What was uploaded is remembered per folder in
//!   `<data dir>/backup-<storageId>[-<folderKey>].json` (relative path →
//!   size, mtime, fileId), so a restart uploads nothing twice.
//!
//! Everything runs on threads of its own ([`BackupService`]) with its own
//! HTTP calls; it never waits on, or holds anything of, the sync engine.

use crate::api::{
    Api, ApiError, DeviceRequest, FolderStatusReport, ProfileFolderRequest, ProfileRequest, StatusRequest, Tree,
};
use crate::config::{BackupFolder, FolderKind, PHOTOS_KEY};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A file must be unchanged this long before it is uploaded.
pub const SETTLE: Duration = Duration::from_secs(10);
/// Full rescan (catches whatever the watcher missed).
pub const RESCAN_EVERY: Duration = Duration::from_secs(6 * 3600);
/// `POST status` at most this often while busy.
pub const STATUS_EVERY: Duration = Duration::from_secs(60);
/// Uploads at once.
pub const UPLOADERS: usize = 3;
/// The panel's per-file cap (`PUT file` answers 413 above it).
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// A file another program holds open is tried this often, then reported.
const LOCKED_TRIES: u32 = 3;
/// After a `quota_exceeded` the folder waits this long before trying again.
const QUOTA_WAIT: Duration = Duration::from_secs(3600);
/// Longest wait between tries while OTHCloud can't be reached.
pub const NETWORK_RETRY_CAP: Duration = Duration::from_secs(30);

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "tif", "tiff", "avif", "jxl",
    // camera raw
    "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "srw", "pef",
];

pub fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

// ------------------------------------------------------------ skip rules

/// Why something isn't backed up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// desktop.ini, Thumbs.db, ~$*, *.tmp, .git/, node_modules/, …
    Name,
    /// Hidden or system.
    Hidden,
    /// A symbolic link or junction (never followed).
    Link,
    /// A cloud file that isn't on this PC (OneDrive & co.): reading it
    /// would download it first.
    CloudOnly,
    /// One of the folder's exclude patterns.
    Excluded,
    /// Not a picture, in a `photos` folder.
    NotAPicture,
    /// Over [`MAX_FILE_BYTES`].
    TooLarge,
    /// Inside one of our own drives' folders.
    OurDrive,
}

/// Names never backed up (PROTOCOL.md "Skipped"): scratch and shell files,
/// and whole `.git` / `node_modules` folders.
pub fn skip_name(name: &str, is_dir: bool) -> bool {
    if is_dir {
        matches!(
            name.to_ascii_lowercase().as_str(),
            ".git" | "node_modules" | "$recycle.bin" | "system volume information"
        )
    } else {
        crate::util::is_temp_name(name)
    }
}

const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// Skip by file attributes. Folders are only skipped for hidden/system: a
/// cloud folder that isn't populated yet is still listed.
pub fn skip_attributes(attrs: u32, is_dir: bool) -> Option<Skip> {
    if attrs & (ATTR_HIDDEN | ATTR_SYSTEM) != 0 {
        return Some(Skip::Hidden);
    }
    if !is_dir && attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0 {
        return Some(Skip::CloudOnly);
    }
    None
}

fn attributes(md: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

// -------------------------------------------------------------- excludes

/// A folder's exclude patterns, matched case-insensitively against paths
/// relative to the folder ('/'-separated):
///
/// - `*.iso`, `cache` — no '/': matches a file or folder NAME anywhere;
/// - `Projects/*/bin`, `/build` — with a '/': matches the whole relative
///   path (a leading '/' only anchors);
/// - a trailing '/' (`temp/`) matches folders only;
/// - `*` and `?` stay within one name, `**` crosses folders, `**/` may
///   match no folder at all; `#` starts a comment line.
///
/// An excluded folder is not walked at all.
#[derive(Debug, Clone, Default)]
pub struct Excludes {
    pats: Vec<Pattern>,
}

#[derive(Debug, Clone)]
struct Pattern {
    toks: Vec<Tok>,
    path: bool,
    dir_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Lit(char),
    One,
    Star,
    DStar,
    /// `**/`: nothing, or anything ending in '/'.
    DStarSlash,
}

fn tokens(p: &str) -> Vec<Tok> {
    let c: Vec<char> = p.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '*' if c.get(i + 1) == Some(&'*') => {
                if c.get(i + 2) == Some(&'/') {
                    out.push(Tok::DStarSlash);
                    i += 3;
                } else {
                    out.push(Tok::DStar);
                    i += 2;
                }
                continue;
            }
            '*' => out.push(Tok::Star),
            '?' => out.push(Tok::One),
            ch => out.push(Tok::Lit(ch)),
        }
        i += 1;
    }
    out
}

/// Dynamic-programming glob match (no backtracking blow-up).
fn glob(toks: &[Tok], s: &[char]) -> bool {
    let n = s.len();
    // next[j]: toks[i+1..] matches s[j..]
    let mut next = vec![false; n + 1];
    next[n] = true;
    for tok in toks.iter().rev() {
        let mut cur = vec![false; n + 1];
        match tok {
            Tok::DStarSlash => {
                // any k ≥ j with s[k] == '/' and next[k+1]
                let mut any = false;
                for j in (0..=n).rev() {
                    if j < n && s[j] == '/' && next[j + 1] {
                        any = true;
                    }
                    cur[j] = next[j] || any;
                }
            }
            _ => {
                for j in (0..=n).rev() {
                    cur[j] = match tok {
                        Tok::Lit(c) => j < n && s[j] == *c && next[j + 1],
                        Tok::One => j < n && s[j] != '/' && next[j + 1],
                        Tok::Star => next[j] || (j < n && s[j] != '/' && cur[j + 1]),
                        Tok::DStar => next[j] || (j < n && cur[j + 1]),
                        Tok::DStarSlash => unreachable!(),
                    };
                }
            }
        }
        next = cur;
    }
    next[0]
}

impl Excludes {
    pub fn new(patterns: &[String]) -> Excludes {
        let pats = patterns
            .iter()
            .filter_map(|p| {
                let mut s = p.trim().replace('\\', "/").to_lowercase();
                if s.is_empty() || s.starts_with('#') {
                    return None;
                }
                let dir_only = s.ends_with('/');
                while s.ends_with('/') {
                    s.pop();
                }
                let anchored = s.starts_with('/');
                let s = s.trim_start_matches('/');
                if s.is_empty() {
                    return None;
                }
                Some(Pattern {
                    toks: tokens(s),
                    path: anchored || s.contains('/'),
                    dir_only,
                })
            })
            .collect();
        Excludes { pats }
    }

    pub fn is_empty(&self) -> bool {
        self.pats.is_empty()
    }

    /// `rel`: relative to the backed-up folder, '/'-separated.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.pats.is_empty() {
            return false;
        }
        let lower = rel.to_lowercase();
        let full: Vec<char> = lower.chars().collect();
        let name: Vec<char> = lower.rsplit('/').next().unwrap_or("").chars().collect();
        self.pats
            .iter()
            .any(|p| (!p.dir_only || is_dir) && glob(&p.toks, if p.path { &full } else { &name }))
    }
}

/// Check exclude patterns typed by a person: at most 100, each 1–200
/// printable characters. Returns them trimmed, empties dropped.
pub fn clean_excludes(patterns: &[String]) -> Result<Vec<String>, String> {
    let out: Vec<String> = patterns.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    if out.len() > 100 {
        return Err("At most 100 exclude patterns".into());
    }
    for p in &out {
        if p.chars().count() > 200 || p.chars().any(|c| c.is_control()) {
            return Err(format!("Not a usable pattern: {p:?}"));
        }
    }
    Ok(out)
}

// ------------------------------------------------------- which folders

pub(crate) fn norm(p: &Path) -> String {
    p.display()
        .to_string()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// `child` is `parent` or inside it.
pub fn is_within(child: &Path, parent: &Path) -> bool {
    let (c, p) = (norm(child), norm(parent));
    !p.is_empty() && (c == p || c.starts_with(&format!("{p}\\")))
}

/// Why `path` can't be backed up, or Ok. Pure (no file system): the caller
/// also checks the path's real form (see [`check_backup_root`]).
pub fn check_root(path: &Path, our_roots: &[PathBuf], system: &[PathBuf]) -> Result<(), String> {
    let s = path.display().to_string();
    if s.starts_with(r"\\") && !s.starts_with(r"\\?\") {
        return Err("Choose a folder on this PC — network folders can't be backed up".into());
    }
    if !path.is_absolute() {
        return Err("Choose a folder on this PC".into());
    }
    if norm(path).len() <= 2 {
        return Err("A whole drive can't be backed up — choose a folder on it".into());
    }
    if our_roots.iter().any(|r| is_within(path, r)) {
        return Err("That folder is one of your OTHCloud drives — its files are already in the cloud".into());
    }
    if system.iter().any(|r| is_within(path, r)) {
        return Err("Windows, program and app-data folders can't be backed up".into());
    }
    Ok(())
}

/// [`check_root`] on the path as given, on what a drive-letter alias (subst,
/// like our own drives' letters) points to, and on its final form
/// (junctions resolved); and the path must be an existing folder that isn't
/// itself a link. Returns the path to keep.
pub fn check_backup_root(path: &Path, our_roots: &[PathBuf], system: &[PathBuf]) -> Result<PathBuf, String> {
    check_root(path, our_roots, system)?;
    let md = std::fs::symlink_metadata(path).map_err(|_| "That folder doesn't exist".to_string())?;
    if md.file_type().is_symlink() {
        return Err("That folder is a link to another place — choose the folder it points to".into());
    }
    if !md.is_dir() {
        return Err("That isn't a folder".into());
    }
    let s = path.display().to_string();
    let mut chars = s.chars();
    if let (Some(l), Some(':')) = (chars.next(), chars.next()) {
        if let Some(t) = crate::letters::target(l) {
            if let Some(real) = t.strip_prefix(r"\??\") {
                check_root(&PathBuf::from(real).join(s[2..].trim_start_matches('\\')), our_roots, system)?;
            }
        }
    }
    if let Ok(real) = std::fs::canonicalize(path) {
        let real = PathBuf::from(real.display().to_string().trim_start_matches(r"\\?\"));
        check_root(&real, our_roots, system)?;
    }
    let mut keep = s.trim_end_matches('\\').to_string();
    if keep.len() == 2 {
        keep.push('\\');
    }
    Ok(PathBuf::from(keep))
}

/// A label must be a usable folder name: 1–120 characters, none of
/// `\ / : * ? " < > |` or control characters, not `.`/`..`.
pub fn check_label(label: &str) -> Result<String, String> {
    let t = label.trim().trim_end_matches('.').trim().to_string();
    if t.is_empty() || t.chars().count() > 120 || !crate::util::is_valid_component(&t) {
        return Err("Give the folder a name of 1–120 characters without \\ / : * ? \" < > |".into());
    }
    Ok(t)
}

/// Where a folder goes in the drive when the panel hasn't named a folder.
pub fn remote_path(f: &BackupFolder, device: &str) -> Vec<String> {
    match (f.kind, &f.remote_folder) {
        (_, Some(r)) => r.split('/').filter(|s| !s.is_empty()).map(String::from).collect(),
        (FolderKind::Photos, None) => vec!["Photo backup".into(), device.to_string()],
        (FolderKind::Folder, None) => vec!["Backups".into(), device.to_string(), f.label.clone()],
    }
}

/// The folder's upload state file. The migrated photo backup keeps the
/// photo backup's own (`backup-<storageId>.json`).
pub fn state_path(data_dir: &Path, storage_id: &str, folder_key: &str) -> PathBuf {
    if folder_key == PHOTOS_KEY {
        data_dir.join(format!("backup-{storage_id}.json"))
    } else {
        data_dir.join(format!("backup-{storage_id}-{folder_key}.json"))
    }
}

// ----------------------------------------------------------------- state

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Uploaded {
    pub size: u64,
    pub mtime: u64,
    pub file_id: String,
    /// No longer on the PC (the copy in the drive stays).
    #[serde(default, skip_serializing_if = "is_false")]
    pub gone: bool,
    /// The panel has it marked *deleted on this PC*.
    #[serde(default, skip_serializing_if = "is_false")]
    pub marked: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupState {
    /// Relative path with '/' separators -> what we uploaded.
    #[serde(default)]
    pub files: HashMap<String, Uploaded>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup_at: Option<String>,
}

impl BackupState {
    pub fn load(path: &Path) -> BackupState {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(b) = serde_json::to_vec(self) {
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
    /// (files, bytes) backed up and still on the PC.
    pub fn totals(&self) -> (u64, u64) {
        self.files
            .values()
            .filter(|u| !u.gone)
            .fold((0, 0), |(n, b), u| (n + 1, b + u.size))
    }
}

// ------------------------------------------------------------- stability

/// The "unchanged for ~10 s" rule. A file is ready when the same size and
/// modified time have been seen for `settle`, or — first sight, e.g. the
/// scan at start — its modified time is `settle` old and the watcher hasn't
/// reported it within `settle`.
#[derive(Debug, Default)]
pub struct Stability {
    seen: HashMap<String, (u64, u64, Instant)>,
}

impl Stability {
    #[allow(clippy::too_many_arguments)]
    pub fn ready(
        &mut self,
        rel: &str,
        size: u64,
        mtime: u64,
        mtime_age: Duration,
        touched: Option<Instant>,
        settle: Duration,
        now: Instant,
    ) -> bool {
        let since = match self.seen.get(rel) {
            Some((s, m, t)) if *s == size && *m == mtime => *t,
            _ => {
                self.seen.insert(rel.to_string(), (size, mtime, now));
                now
            }
        };
        let quiet_seen = now.saturating_duration_since(since) >= settle;
        let quiet_mtime = mtime_age >= settle && touched.is_none_or(|t| now.saturating_duration_since(t) >= settle);
        quiet_seen || quiet_mtime
    }

    pub fn forget(&mut self, rel: &str) {
        self.seen.remove(rel);
    }
}

// ------------------------------------------------------------- scanning

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub rel: String,
    pub size: u64,
    pub mtime: u64,
    /// How long ago it was last modified.
    pub age: Duration,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub files: Vec<Found>,
    pub skipped: u64,
    pub too_large: Vec<String>,
    /// Folders that couldn't be listed (access denied).
    pub unreadable: u64,
}

pub(crate) fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One backed-up folder, as the uploader sees it.
#[derive(Debug, Clone)]
pub struct FolderJob {
    pub key: String,
    pub label: String,
    pub root: PathBuf,
    pub kind: FolderKind,
    pub excludes: Excludes,
    /// Remote folder path (used when `remote_id` is unknown).
    pub remote: Vec<String>,
    /// The folder the panel made for it (`PUT profile`).
    pub remote_id: Option<String>,
    pub state_path: PathBuf,
    /// Never walked into (our drives' folders).
    pub skip_inside: Vec<PathBuf>,
}

impl FolderJob {
    pub fn new(f: &BackupFolder, storage_id: &str, device: &str, data_dir: &Path, skip_inside: Vec<PathBuf>) -> FolderJob {
        FolderJob {
            key: f.folder_key.clone(),
            label: f.label.clone(),
            root: f.local_path.clone(),
            kind: f.kind,
            excludes: Excludes::new(&f.excludes),
            remote: remote_path(f, device),
            remote_id: None,
            state_path: state_path(data_dir, storage_id, &f.folder_key),
            skip_inside,
        }
    }

    fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel.replace('/', "\\"))
    }

    /// Why a folder isn't walked (None: it is).
    pub fn dir_skip(&self, rel: &str, name: &str, md: &std::fs::Metadata, path: &Path) -> Option<Skip> {
        if md.file_type().is_symlink() {
            return Some(Skip::Link);
        }
        if skip_name(name, true) {
            return Some(Skip::Name);
        }
        if let Some(s) = skip_attributes(attributes(md), true) {
            return Some(s);
        }
        if self.excludes.matches(rel, true) {
            return Some(Skip::Excluded);
        }
        if self.skip_inside.iter().any(|r| is_within(path, r)) {
            return Some(Skip::OurDrive);
        }
        None
    }

    /// Why a file isn't backed up (None: it is).
    pub fn file_skip(&self, rel: &str, name: &str, md: &std::fs::Metadata) -> Option<Skip> {
        if md.file_type().is_symlink() {
            return Some(Skip::Link);
        }
        if skip_name(name, false) {
            return Some(Skip::Name);
        }
        if let Some(s) = skip_attributes(attributes(md), false) {
            return Some(s);
        }
        if self.kind == FolderKind::Photos && !is_image(Path::new(name)) {
            return Some(Skip::NotAPicture);
        }
        if self.excludes.matches(rel, false) {
            return Some(Skip::Excluded);
        }
        if md.len() > MAX_FILE_BYTES {
            return Some(Skip::TooLarge);
        }
        None
    }

    fn walk(&self, start: PathBuf, out: &mut Scan) {
        let now = SystemTime::now();
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => {
                    out.unreadable += 1;
                    continue;
                }
            };
            for e in rd.flatten() {
                let p = e.path();
                // DirEntry metadata is the directory listing's, not a
                // second call, and does not follow links.
                let Ok(md) = e.metadata() else { continue };
                let name = e.file_name().to_string_lossy().to_string();
                let rel = p.strip_prefix(&self.root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                if md.is_dir() {
                    match self.dir_skip(&rel, &name, &md, &p) {
                        None => stack.push(p),
                        Some(_) => out.skipped += 1,
                    }
                } else if md.is_file() || md.file_type().is_symlink() {
                    match self.file_skip(&rel, &name, &md) {
                        None => out.files.push(Found {
                            rel,
                            size: md.len(),
                            mtime: mtime_secs(&md),
                            age: md.modified().ok().and_then(|t| now.duration_since(t).ok()).unwrap_or_default(),
                        }),
                        Some(Skip::TooLarge) => {
                            out.skipped += 1;
                            out.too_large.push(rel);
                        }
                        Some(_) => out.skipped += 1,
                    }
                }
            }
        }
    }

    /// Every file to back up.
    pub fn scan(&self) -> Scan {
        let mut out = Scan::default();
        self.walk(self.root.clone(), &mut out);
        out.files.sort_by(|a, b| a.rel.cmp(&b.rel));
        out
    }

    /// Only `rel` (a file, or a folder and everything in it), if neither it
    /// nor a folder above it is skipped. Returns false when `rel` doesn't
    /// exist (any more).
    pub fn scan_under(&self, rel: &str, out: &mut Scan) -> bool {
        let p = self.abs(rel);
        let Ok(md) = std::fs::symlink_metadata(&p) else { return false };
        // The folders above it, each against the folder rules.
        let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        for i in 0..parts.len().saturating_sub(1) {
            let sub = parts[..=i].join("/");
            let ap = self.abs(&sub);
            match std::fs::symlink_metadata(&ap) {
                Ok(m) if self.dir_skip(&sub, parts[i], &m, &ap).is_none() => {}
                _ => return true,
            }
        }
        let name = parts.last().copied().unwrap_or("");
        if md.is_dir() {
            if rel.is_empty() || self.dir_skip(rel, name, &md, &p).is_none() {
                self.walk(p, out);
            }
        } else {
            match self.file_skip(rel, name, &md) {
                None => out.files.push(Found {
                    rel: rel.to_string(),
                    size: md.len(),
                    mtime: mtime_secs(&md),
                    age: md
                        .modified()
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .unwrap_or_default(),
                }),
                Some(Skip::TooLarge) => out.too_large.push(rel.to_string()),
                Some(_) => out.skipped += 1,
            }
        }
        true
    }
}

// ---------------------------------------------------------- remote side

/// Remote folder ids by lowercased path ("a/b").
fn folder_index(t: &Tree) -> HashMap<String, String> {
    let by_id: HashMap<&str, &crate::api::RemoteFolder> = t.folders.iter().map(|f| (f.folder_id.as_str(), f)).collect();
    let mut out = HashMap::new();
    for f in &t.folders {
        let mut parts = vec![f.name.clone()];
        let mut cur = f.parent_id.as_deref();
        let mut guard = 0;
        while let Some(c) = cur {
            match by_id.get(c) {
                Some(p) => {
                    parts.push(p.name.clone());
                    cur = p.parent_id.as_deref();
                }
                None => break,
            }
            guard += 1;
            if guard > 256 {
                break;
            }
        }
        parts.reverse();
        out.insert(parts.join("/").to_lowercase(), f.folder_id.clone());
    }
    out
}

/// A folder id's path (real case) in the tree.
fn folder_path_of(t: &Tree, id: &str) -> Option<Vec<String>> {
    let by_id: HashMap<&str, &crate::api::RemoteFolder> = t.folders.iter().map(|f| (f.folder_id.as_str(), f)).collect();
    let mut parts = Vec::new();
    let mut cur = Some(id);
    while let Some(c) = cur {
        let f = by_id.get(c)?;
        parts.push(f.name.clone());
        cur = f.parent_id.as_deref();
        if parts.len() > 256 {
            return None;
        }
    }
    parts.reverse();
    Some(parts)
}

fn empty_tree(storage_id: &str) -> Tree {
    Tree {
        storage_id: storage_id.into(),
        etag: String::new(),
        folders: vec![],
        files: vec![],
    }
}

/// Make sure the folder chain exists; returns the deepest folder id.
fn ensure_folders(
    api: &Api,
    storage_id: &str,
    parts: &[String],
    index: &mut HashMap<String, String>,
) -> Result<Option<String>, ApiError> {
    let mut parent: Option<String> = None;
    let mut path = String::new();
    for p in parts {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(p);
        let key = path.to_lowercase();
        if let Some(id) = index.get(&key) {
            parent = Some(id.clone());
            continue;
        }
        match api.create_folder(storage_id, parent.as_deref(), p) {
            Ok(f) => {
                index.insert(key, f.folder_id.clone());
                parent = Some(f.folder_id);
            }
            Err(e) if e.is_name_taken() => {
                // Created meanwhile (or a same-named file): re-read.
                let t = api.tree(storage_id, None)?.unwrap_or_else(|| empty_tree(storage_id));
                *index = folder_index(&t);
                match index.get(&key) {
                    Some(id) => parent = Some(id.clone()),
                    None => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(parent)
}

fn free_name(taken: &[String], name: &str) -> String {
    let lower: Vec<String> = taken.iter().map(|t| t.to_lowercase()).collect();
    if !lower.contains(&name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1.. {
        let c = format!("{stem} ({n}){ext}");
        if !lower.contains(&c.to_lowercase()) {
            return c;
        }
    }
    unreachable!()
}

// ------------------------------------------------------------- one pass

/// What the UI and `POST status` show for one folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderStatus {
    /// idle · scanning · uploading · waiting (unstable files) · paused ·
    /// off · full (quota) · missing (folder not found) · error · panel
    /// (waiting for the panel to make the remote folder)
    pub phase: &'static str,
    pub files: u64,
    pub bytes: u64,
    pub pending: u64,
    /// Relative paths being uploaded right now.
    pub uploading: Vec<String>,
    /// Uploaded since the app started.
    pub uploaded: u64,
    pub skipped: u64,
    pub last_backup_at: Option<String>,
    pub last_run_at: Option<String>,
    pub last_error: Option<String>,
    /// Files that couldn't be backed up and why (a few).
    pub problems: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub uploaded: usize,
    pub already: usize,
    pub failed: usize,
    pub pending: usize,
    pub marked: usize,
    pub unmarked: usize,
    pub quota: bool,
}

/// What a pass may use.
pub struct Ctx<'a> {
    pub api: &'a Api,
    pub storage_id: &'a str,
    /// Registered device (`PUT device`); without it no deletion marks.
    pub device_id: Option<&'a str>,
    pub settle: Duration,
    pub uploaders: usize,
    /// Stop dispatching (pause, stop, reconfiguration).
    pub cancel: &'a (dyn Fn() -> bool + Sync),
    /// Called with the folder's status while uploads run.
    pub on_progress: &'a mut dyn FnMut(&FolderStatus),
}

enum Outcome {
    Done(String),
    Locked,
    Vanished,
    Failed(ApiError),
}

struct Item {
    rel: String,
    local: PathBuf,
    size: u64,
    mtime: u64,
    replace: Option<String>,
    folder: Option<String>,
    name: String,
    taken: Vec<String>,
}

enum Msg {
    Start(String),
    Done(Item, Outcome),
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn upload_one(api: &Api, storage_id: &str, item: &Item) -> Outcome {
    // Open first: tell "in use" and "gone" from a server problem.
    if let Err(e) = std::fs::File::open(&item.local) {
        return match e.raw_os_error() {
            Some(32) | Some(33) => Outcome::Locked,
            _ if e.kind() == std::io::ErrorKind::NotFound => Outcome::Vanished,
            _ => Outcome::Failed(ApiError::Io(e.to_string())),
        };
    }
    if let Some(id) = &item.replace {
        // No plain-replace fallback on 403 `not_backup_file`: a backup must
        // never overwrite without keeping the old version. It is reported.
        match api.upload_replace_keep_version(id, &item.local) {
            Ok(pf) => return Outcome::Done(pf.file_id.unwrap_or_else(|| id.clone())),
            Err(e) if e.is_not_found() => {} // removed in the drive: upload anew
            Err(e) => return Outcome::Failed(e),
        }
    }
    let mut taken = item.taken.clone();
    for _ in 0..20 {
        let name = free_name(&taken, &item.name);
        match api.upload_new(storage_id, item.folder.as_deref(), &name, &item.local) {
            Ok(pf) => {
                return match pf.file_id {
                    Some(id) => Outcome::Done(id),
                    None => Outcome::Failed(ApiError::Decode("upload returned no fileId".into())),
                }
            }
            Err(e) if e.is_name_taken() => taken.push(name),
            Err(e) => return Outcome::Failed(e),
        }
    }
    Outcome::Failed(ApiError::Decode("no free name".into()))
}

/// One backed-up folder with its state, between passes.
pub struct Folder {
    pub job: FolderJob,
    pub state: BackupState,
    pub status: FolderStatus,
    stability: Stability,
    locked: HashMap<String, u32>,
    /// Watcher events: relative path → when.
    pub touched: HashMap<String, Instant>,
    /// Relative paths still to look at (unsettled, locked, watcher events).
    pub dirty: HashSet<String>,
    /// A `quota_exceeded` stopped it until then.
    pub blocked_until: Option<Instant>,
    /// Its folder in the drive is gone: the panel must make a new one
    /// (`PUT profile` again) before anything more is uploaded.
    pub needs_profile: bool,
    // ---- what Explorer shows (shellstatus.rs)
    /// Files to upload that aren't yet (settling, queued, uploading, held
    /// back by a full drive).
    pending_rels: HashSet<String>,
    /// Files that couldn't be backed up (until they are, or the next full pass).
    problem_rels: HashSet<String>,
    /// A full pass has run since start.
    checked: bool,
    /// `state.files` as Explorer's index, rebuilt only when it changed.
    marks_uploaded: Option<Arc<HashMap<String, (u64, u64)>>>,
    marks_dirty: bool,
}

impl Folder {
    pub fn new(job: FolderJob) -> Folder {
        let state = BackupState::load(&job.state_path);
        let (files, bytes) = state.totals();
        let status = FolderStatus {
            phase: "idle",
            files,
            bytes,
            last_backup_at: state.last_backup_at.clone(),
            ..Default::default()
        };
        Folder {
            job,
            state,
            status,
            stability: Stability::default(),
            locked: HashMap::new(),
            touched: HashMap::new(),
            dirty: HashSet::new(),
            blocked_until: None,
            needs_profile: false,
            pending_rels: HashSet::new(),
            problem_rels: HashSet::new(),
            checked: false,
            marks_uploaded: None,
            marks_dirty: true,
        }
    }

    /// Tell Explorer's overlays what this folder looks like now
    /// (shellstatus.rs). Only while that is on; cheap unless the upload state
    /// changed since the last time (then one pass over it, as saving it is).
    pub fn publish_marks(&mut self) {
        if !crate::shellstatus::enabled() {
            return;
        }
        if self.marks_dirty || self.marks_uploaded.is_none() {
            self.marks_uploaded = Some(crate::shellstatus::FolderMarks::uploaded_index(&self.state.files));
            self.marks_dirty = false;
        }
        let stuck = self.blocked_until.is_some_and(|t| Instant::now() < t);
        let pending = self.pending_rels.iter().chain(self.dirty.iter()).chain(self.status.uploading.iter()).cloned();
        crate::shellstatus::publish(crate::shellstatus::FolderMarks::new(
            &self.job.key,
            self.job.clone(),
            self.marks_uploaded.clone().unwrap_or_default(),
            pending,
            self.problem_rels.iter().cloned(),
            stuck,
            self.checked,
        ));
    }

    /// One pass: `scope` None = the whole folder, else only these relative
    /// paths (watcher events, files still settling). Uploads what's new or
    /// changed and settled, marks/unmarks deletions, saves the state.
    /// Network and sign-in errors end the pass with `Err` (try again later).
    pub fn run(&mut self, ctx: &mut Ctx, scope: Option<Vec<String>>) -> Result<Report, ApiError> {
        let mut rep = Report::default();
        let now = Instant::now();
        self.status.last_run_at = Some(now_iso());
        if !self.job.root.is_dir() {
            // Unplugged disk, renamed folder: never treat that as "every
            // file deleted".
            self.status.phase = "missing";
            self.status.last_error = Some(format!("Folder not found: {}", self.job.root.display()));
            return Ok(rep);
        }
        self.status.phase = "scanning";
        (ctx.on_progress)(&self.status);
        let full = scope.is_none();
        let mut scan = Scan::default();
        let mut missing: Vec<String> = Vec::new();
        match &scope {
            None => scan = self.job.scan(),
            Some(paths) => {
                let mut ps: Vec<&String> = paths.iter().collect();
                ps.sort();
                ps.dedup();
                for rel in ps {
                    if !self.job.scan_under(rel, &mut scan) {
                        missing.push(rel.clone());
                    }
                }
                scan.files.sort_by(|a, b| a.rel.cmp(&b.rel));
                scan.files.dedup_by(|a, b| a.rel == b.rel);
            }
        }
        if full {
            self.status.skipped = scan.skipped;
        }

        // ---- deletions: gone from the PC (and not merely skipped now)
        let seen: HashSet<&str> = scan.files.iter().map(|f| f.rel.as_str()).collect();
        let root = self.job.root.clone();
        let is_candidate = |rel: &str| -> bool {
            if full {
                !seen.contains(rel)
            } else {
                missing.iter().any(|m| m.is_empty() || rel == m || rel.starts_with(&format!("{m}/")))
            }
        };
        for (rel, u) in self.state.files.iter_mut() {
            if !u.gone && is_candidate(rel) && !root.join(rel.replace('/', "\\")).exists() {
                u.gone = true;
            }
        }
        for f in &scan.files {
            if let Some(u) = self.state.files.get_mut(&f.rel) {
                u.gone = false;
            }
        }
        for m in &missing {
            self.stability.forget(m);
            self.locked.remove(m);
        }

        // ---- what to upload
        let mut todo: Vec<Found> = Vec::new();
        let mut waiting: Vec<String> = Vec::new();
        let mut unchanged: Vec<String> = Vec::new();
        for f in scan.files {
            let changed = match self.state.files.get(&f.rel) {
                Some(u) => u.size != f.size || u.mtime != f.mtime,
                None => true,
            };
            if !changed {
                self.stability.forget(&f.rel);
                if !full {
                    unchanged.push(f.rel);
                }
                continue;
            }
            let touched = self.touched.get(&f.rel).copied();
            if self.stability.ready(&f.rel, f.size, f.mtime, f.age, touched, ctx.settle, now) {
                todo.push(f);
            } else {
                waiting.push(f.rel);
            }
        }
        rep.pending = waiting.len();
        let mut problems: Vec<String> = scan
            .too_large
            .iter()
            .map(|r| format!("{r}: larger than the 10 GB limit"))
            .collect();
        // Explorer's view: what this pass looked at starts over.
        if full {
            // A full pass looks at everything (the coordinator clears this
            // before one too); what still waits is added back below.
            self.dirty.clear();
            self.pending_rels.clear();
            self.problem_rels.clear();
        } else {
            for m in &missing {
                let under = format!("{m}/");
                self.pending_rels.retain(|r| r != m && !r.starts_with(&under));
                self.problem_rels.retain(|r| r != m && !r.starts_with(&under));
            }
            for r in &unchanged {
                self.pending_rels.remove(r);
                self.problem_rels.remove(r);
            }
        }
        self.pending_rels.extend(todo.iter().map(|f| f.rel.clone()).chain(waiting.iter().cloned()));
        for r in &todo {
            self.problem_rels.remove(&r.rel);
        }
        self.problem_rels.extend(scan.too_large.iter().cloned());
        self.publish_marks();

        let mut waiting_panel = false;
        if !todo.is_empty() && self.blocked_until.is_some_and(|t| now < t) {
            rep.pending += todo.len();
            rep.quota = true;
            todo.clear();
        }
        if !todo.is_empty() && self.job.kind == FolderKind::Folder && self.job.remote_id.is_none() {
            // A folder backup goes where the panel says; until it has made
            // that folder (older panel, offline at start) nothing is sent.
            rep.pending += todo.len();
            todo.clear();
            waiting_panel = true;
        }

        if !todo.is_empty() {
            let r = self.upload(ctx, todo, &mut rep, &mut waiting, &mut problems);
            if r.is_err() {
                self.publish_marks();
            }
            if !r? {
                waiting_panel = true;
            }
        }

        // ---- deletion marks
        if let Some(dev) = ctx.device_id {
            for deleted in [true, false] {
                let ids: Vec<String> = self
                    .state
                    .files
                    .values()
                    .filter(|u| u.gone == deleted && u.marked != deleted)
                    .map(|u| u.file_id.clone())
                    .collect();
                for chunk in ids.chunks(200) {
                    match ctx.api.backup_deleted(dev, chunk, deleted) {
                        Ok(()) => {
                            let set: HashSet<&String> = chunk.iter().collect();
                            for u in self.state.files.values_mut() {
                                if set.contains(&u.file_id) {
                                    u.marked = deleted;
                                }
                            }
                            if deleted {
                                rep.marked += chunk.len();
                            } else {
                                rep.unmarked += chunk.len();
                            }
                        }
                        Err(e) if e.is_network() || e.is_auth() => {
                            self.state.save(&self.job.state_path);
                            return Err(e);
                        }
                        Err(e) => {
                            tracing::warn!("backup {}: deletion marks: {e}", self.job.label);
                            break;
                        }
                    }
                }
            }
        }

        // ---- wrap up
        for w in &waiting {
            self.dirty.insert(w.clone());
        }
        let (files, bytes) = self.state.totals();
        self.status.files = files;
        self.status.bytes = bytes;
        self.status.pending = rep.pending as u64;
        self.status.uploading.clear();
        problems.truncate(5);
        self.status.problems = problems;
        if rep.quota {
            self.status.phase = "full";
            self.status.last_error = Some("The drive is full — free up space or choose another drive".into());
        } else if waiting_panel {
            self.status.phase = "panel";
            self.status.last_error = Some("Waiting for OTHCloud to set up this folder's backup".into());
        } else if rep.failed > 0 {
            self.status.phase = "error";
        } else {
            self.status.phase = if rep.pending > 0 { "waiting" } else { "idle" };
            self.status.last_error = None;
        }
        if rep.pending == 0 && rep.failed == 0 && !rep.quota && (full || rep.uploaded + rep.already > 0) {
            self.state.last_backup_at = Some(now_iso());
            self.status.last_backup_at = self.state.last_backup_at.clone();
        }
        if full || rep.uploaded + rep.already + rep.marked + rep.unmarked > 0 || !missing.is_empty() {
            self.state.save(&self.job.state_path);
        }
        if full {
            self.checked = true;
        }
        self.publish_marks();
        Ok(rep)
    }

    fn upload(
        &mut self,
        ctx: &mut Ctx,
        todo: Vec<Found>,
        rep: &mut Report,
        waiting: &mut Vec<String>,
        problems: &mut Vec<String>,
    ) -> Result<bool, ApiError> {
        let api = ctx.api;
        let sid = ctx.storage_id;
        let tree = api.tree(sid, None)?.unwrap_or_else(|| empty_tree(sid));
        let mut index = folder_index(&tree);
        // A folder backup goes into the folder the panel made for it; the
        // photos folder keeps the photo backup's own path (a custom one
        // included) whatever the panel names, so nothing moves.
        let panel_folder = match self.job.kind {
            FolderKind::Folder => self.job.remote_id.as_deref().and_then(|id| folder_path_of(&tree, id)),
            FolderKind::Photos => None,
        };
        let base: Vec<String> = match (self.job.kind, panel_folder) {
            (_, Some(p)) => p,
            (FolderKind::Photos, None) => self.job.remote.clone(),
            (FolderKind::Folder, None) => {
                // Deleted in the drive (or never made): ask the panel for a
                // new one rather than guess its name.
                self.needs_profile = true;
                rep.pending += todo.len();
                waiting.extend(todo.into_iter().map(|f| f.rel));
                return Ok(false);
            }
        };
        let mut names_in: HashMap<Option<String>, Vec<(String, String, u64)>> = HashMap::new();
        for f in &tree.files {
            names_in
                .entry(f.folder_id.clone())
                .or_default()
                .push((f.name.clone(), f.file_id.clone(), f.size_bytes));
        }

        // ---- plan (folders are made here, one at a time)
        let mut queue: VecDeque<Item> = VecDeque::new();
        for f in todo {
            let local = self.job.abs(&f.rel);
            let mut segs: Vec<&str> = f.rel.split('/').collect();
            let name = segs.pop().unwrap_or_default().to_string();
            let mut parts = base.clone();
            parts.extend(segs.iter().map(|s| s.to_string()));
            let prev = self.state.files.get(&f.rel).map(|u| u.file_id.clone());
            let folder = match ensure_folders(api, sid, &parts, &mut index) {
                Ok(id) => id,
                Err(e) if e.is_network() || e.is_auth() => return Err(e),
                Err(e) if e.is_quota() => {
                    rep.quota = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!("backup {}: {}: {e}", self.job.label, f.rel);
                    rep.failed += 1;
                    problems.push(format!("{}: {e}", f.rel));
                    self.pending_rels.remove(&f.rel);
                    self.problem_rels.insert(f.rel.clone());
                    continue;
                }
            };
            let siblings = names_in.entry(folder.clone()).or_default();
            let mut replace = prev;
            if replace.is_none() {
                if let Some((_, id, sz)) = siblings.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(&name)) {
                    if *sz == f.size {
                        // Same name and size already there (an earlier
                        // install, a lost state file): take it as ours.
                        self.state.files.insert(
                            f.rel.clone(),
                            Uploaded { size: f.size, mtime: f.mtime, file_id: id.clone(), gone: false, marked: false },
                        );
                        self.marks_dirty = true;
                        self.pending_rels.remove(&f.rel);
                        rep.already += 1;
                        continue;
                    }
                    if self.job.kind == FolderKind::Folder {
                        // Our own backup folder: make it match the PC,
                        // keeping what was there as a version.
                        replace = Some(id.clone());
                    }
                }
            }
            let taken: Vec<String> = siblings.iter().map(|s| s.0.clone()).collect();
            if replace.is_none() {
                // Reserve the name so two new files can't pick the same one.
                let n = free_name(&taken, &name);
                siblings.push((n, String::new(), f.size));
            }
            queue.push_back(Item {
                rel: f.rel,
                local,
                size: f.size,
                mtime: f.mtime,
                replace,
                folder,
                name,
                taken,
            });
        }
        if rep.quota {
            rep.pending += queue.len();
            return Ok(true);
        }

        // ---- upload, a few at a time
        let total = queue.len();
        let queue = Mutex::new(queue);
        let abort = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<Msg>();
        let mut fatal: Option<ApiError> = None;
        let mut last_save = Instant::now();
        let workers = ctx.uploaders.clamp(1, 8).min(total.max(1));
        let label = self.job.label.clone();
        self.status.phase = "uploading";
        (ctx.on_progress)(&self.status);
        std::thread::scope(|sc| {
            for _ in 0..workers {
                let tx = tx.clone();
                let (queue, abort, cancel) = (&queue, &abort, ctx.cancel);
                sc.spawn(move || loop {
                    if abort.load(Ordering::SeqCst) || cancel() {
                        break;
                    }
                    let Some(item) = queue.lock().pop_front() else { break };
                    let _ = tx.send(Msg::Start(item.rel.clone()));
                    let out = upload_one(api, sid, &item);
                    let _ = tx.send(Msg::Done(item, out));
                });
            }
            drop(tx);
            for msg in rx {
                match msg {
                    Msg::Start(rel) => {
                        self.status.uploading.push(rel);
                    }
                    Msg::Done(item, out) => {
                        self.status.uploading.retain(|r| *r != item.rel);
                        match out {
                            Outcome::Done(file_id) => {
                                tracing::info!("backup {label}: {} -> {file_id}", item.rel);
                                let marked = self.state.files.get(&item.rel).is_some_and(|u| u.marked);
                                self.state.files.insert(
                                    item.rel.clone(),
                                    Uploaded { size: item.size, mtime: item.mtime, file_id, gone: false, marked },
                                );
                                self.locked.remove(&item.rel);
                                self.stability.forget(&item.rel);
                                self.marks_dirty = true;
                                self.pending_rels.remove(&item.rel);
                                self.problem_rels.remove(&item.rel);
                                self.dirty.remove(&item.rel);
                                rep.uploaded += 1;
                                self.status.uploaded += 1;
                                let (files, bytes) = self.state.totals();
                                self.status.files = files;
                                self.status.bytes = bytes;
                            }
                            Outcome::Vanished => {
                                self.pending_rels.remove(&item.rel);
                            }
                            Outcome::Locked => {
                                let n = self.locked.entry(item.rel.clone()).or_insert(0);
                                *n += 1;
                                if *n < LOCKED_TRIES {
                                    rep.pending += 1;
                                    waiting.push(item.rel.clone());
                                } else {
                                    rep.failed += 1;
                                    problems.push(format!("{}: in use by another program", item.rel));
                                    self.pending_rels.remove(&item.rel);
                                    self.problem_rels.insert(item.rel.clone());
                                }
                            }
                            Outcome::Failed(e) if e.is_network() || e.is_auth() => {
                                abort.store(true, Ordering::SeqCst);
                                rep.pending += 1;
                                fatal.get_or_insert(e);
                            }
                            Outcome::Failed(e) if e.is_quota() => {
                                abort.store(true, Ordering::SeqCst);
                                rep.quota = true;
                                rep.pending += 1;
                            }
                            Outcome::Failed(e) => {
                                tracing::warn!("backup {label}: {}: {e}", item.rel);
                                rep.failed += 1;
                                self.pending_rels.remove(&item.rel);
                                self.problem_rels.insert(item.rel.clone());
                                problems.push(if e.status() == Some(413) {
                                    format!("{}: larger than the drive allows", item.rel)
                                } else if e.status() == Some(403) {
                                    format!("{}: OTHCloud refused to keep a version of it ({})", item.rel, e.code())
                                } else {
                                    format!("{}: {e}", item.rel)
                                });
                            }
                        }
                        if last_save.elapsed() >= Duration::from_secs(2) {
                            self.state.save(&self.job.state_path);
                            self.publish_marks();
                            last_save = Instant::now();
                        }
                    }
                }
                self.status.pending = (rep.pending + queue.lock().len() + self.status.uploading.len()) as u64;
                (ctx.on_progress)(&self.status);
            }
        });
        // Whatever wasn't started (stopped, paused, quota) is still to do.
        let left = queue.into_inner();
        rep.pending += left.len();
        for i in left {
            waiting.push(i.rel);
        }
        if rep.quota {
            self.blocked_until = Some(Instant::now() + QUOTA_WAIT);
        } else {
            self.blocked_until = None;
        }
        self.state.save(&self.job.state_path);
        match fatal {
            Some(e) => Err(e),
            None => Ok(true),
        }
    }
}

// --------------------------------------------------------------- service

/// What the service runs. Changing it (`BackupService::reconfigure`) is
/// applied between files.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceConfig {
    pub storage_id: String,
    pub device_key: String,
    pub device_name: String,
    pub app_version: String,
    pub os_version: String,
    pub folders: Vec<BackupFolder>,
    /// Backup's own pause (the app-wide one is the `paused` flag).
    pub paused: bool,
    pub data_dir: PathBuf,
    /// Our drives' folders: never walked into.
    pub skip_inside: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ServiceOptions {
    pub settle: Duration,
    pub rescan_every: Duration,
    pub status_every: Duration,
    pub uploaders: usize,
    /// Asking an older panel (no backup API) again.
    pub api_retry: Duration,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        ServiceOptions {
            settle: SETTLE,
            rescan_every: RESCAN_EVERY,
            status_every: STATUS_EVERY,
            uploaders: UPLOADERS,
            api_retry: Duration::from_secs(3600),
        }
    }
}

/// The whole backup, for the UI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceStatus {
    pub device_id: Option<String>,
    /// "ok", "unavailable" (the panel has no backup API), or an error.
    pub panel: String,
    pub folders: HashMap<String, FolderStatus>,
    pub paused: bool,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Events {
    paths: HashSet<String>,
    full: bool,
    touched: HashMap<String, Instant>,
}

#[derive(Default)]
struct Signals {
    kicked: bool,
    config: Option<ServiceConfig>,
    events: HashMap<String, Events>,
}

struct Shared {
    stop: AtomicBool,
    signals: Mutex<Signals>,
    cv: Condvar,
    paused: Arc<AtomicBool>,
}

impl Shared {
    fn wake(&self) {
        self.signals.lock().kicked = true;
        self.cv.notify_all();
    }
}

/// The running backup: one coordinator thread (registration, profile,
/// scans, status) and, during a pass, up to [`UPLOADERS`] upload threads.
pub struct BackupService {
    shared: Arc<Shared>,
    done: mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub status: Arc<Mutex<ServiceStatus>>,
    pub storage_id: String,
}

impl BackupService {
    pub fn start(api: Api, cfg: ServiceConfig, paused: Arc<AtomicBool>, opts: ServiceOptions) -> BackupService {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            signals: Mutex::new(Signals { kicked: true, ..Default::default() }),
            cv: Condvar::new(),
            paused,
        });
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        let (done_tx, done) = mpsc::channel();
        let storage_id = cfg.storage_id.clone();
        let (sh, st) = (shared.clone(), status.clone());
        let thread = std::thread::Builder::new()
            .name("backup".into())
            .spawn(move || {
                Coordinator::new(api, cfg, sh, st, opts).run();
                let _ = done_tx.send(());
            })
            .expect("backup thread");
        BackupService { shared, done, thread: Some(thread), status, storage_id }
    }

    /// Apply new settings between files (no restart, no rescan of folders
    /// that didn't change).
    pub fn reconfigure(&self, cfg: ServiceConfig) {
        self.shared.signals.lock().config = Some(cfg);
        self.shared.wake();
    }

    /// Look now (resume, a person pressed something).
    pub fn kick(&self) {
        self.shared.wake();
    }

    /// Stop. Waits up to `wait` for the pass in progress to end between
    /// files; an upload still running after that finishes on its own
    /// thread and is dropped.
    pub fn stop(&mut self, wait: Duration) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake();
        if self.done.recv_timeout(wait).is_ok() {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        } else {
            tracing::info!("backup: an upload is still finishing; it ends on its own");
            self.thread.take();
        }
    }
}

impl Drop for BackupService {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop(Duration::from_secs(3));
        }
    }
}

struct Live {
    folder: Folder,
    enabled: bool,
    settings: BackupFolder,
    _watcher: Option<notify::RecommendedWatcher>,
    next_full: Instant,
    recheck: Option<Instant>,
}

struct Coordinator {
    api: Api,
    cfg: ServiceConfig,
    shared: Arc<Shared>,
    status: Arc<Mutex<ServiceStatus>>,
    opts: ServiceOptions,
    folders: Vec<Live>,
    device_id: Option<String>,
    register_at: Instant,
    profile_sent: Option<ProfileRequest>,
    /// The photos folders' own drive folders (sent as their `remoteFolderId`).
    photos_ids: HashMap<String, String>,
    /// A folder's drive folder was deleted: `PUT profile` again (not before).
    profile_retry_at: Option<Instant>,
    remote_ids: HashMap<String, String>,
    retry_at: Option<Instant>,
    backoff: Duration,
    last_post: Option<Instant>,
    posted: Option<StatusRequest>,
}

fn watch(key: &str, f: &BackupFolder, shared: &Arc<Shared>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let (key, root, kind, sh) = (key.to_string(), f.local_path.clone(), f.kind, shared.clone());
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let mut sig = sh.signals.lock();
        let ev = sig.events.entry(key.clone()).or_default();
        match res {
            Ok(e) => {
                if matches!(e.kind, notify::EventKind::Access(_)) {
                    return;
                }
                if e.need_rescan() {
                    ev.full = true;
                }
                let now = Instant::now();
                let mut any = false;
                for p in &e.paths {
                    let Ok(rel) = p.strip_prefix(&root) else { continue };
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    // Churn nobody backs up (.git, node_modules, scratch
                    // files) must not wake anything.
                    let parts: Vec<&str> = rel.split('/').collect();
                    let n = parts.len();
                    if parts.iter().enumerate().any(|(i, s)| skip_name(s, i + 1 < n)) {
                        continue;
                    }
                    if kind == FolderKind::Photos && !is_image(p) && p.is_file() {
                        continue;
                    }
                    ev.paths.insert(rel.clone());
                    ev.touched.insert(rel, now);
                    any = true;
                }
                if !any && !ev.full {
                    return;
                }
            }
            Err(_) => ev.full = true,
        }
        sig.kicked = true;
        drop(sig);
        sh.cv.notify_all();
    });
    match &mut w {
        Ok(w) => match w.watch(&f.local_path, notify::RecursiveMode::Recursive) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("backup {}: can't watch {}: {e}", f.label, f.local_path.display());
                return None;
            }
        },
        Err(e) => {
            tracing::warn!("backup {}: watcher: {e}", f.label);
            return None;
        }
    }
    w.ok()
}

impl Coordinator {
    fn new(api: Api, cfg: ServiceConfig, shared: Arc<Shared>, status: Arc<Mutex<ServiceStatus>>, opts: ServiceOptions) -> Self {
        let mut c = Coordinator {
            api,
            cfg: cfg.clone(),
            shared,
            status,
            opts,
            folders: Vec::new(),
            device_id: None,
            register_at: Instant::now(),
            profile_sent: None,
            photos_ids: HashMap::new(),
            profile_retry_at: None,
            remote_ids: HashMap::new(),
            retry_at: None,
            backoff: Duration::from_secs(5),
            last_post: None,
            posted: None,
        };
        c.apply(cfg);
        c
    }

    fn paused(&self) -> bool {
        self.cfg.paused || self.shared.paused.load(Ordering::Relaxed)
    }

    fn apply(&mut self, cfg: ServiceConfig) {
        let now = Instant::now();
        let mut old: HashMap<String, Live> = self.folders.drain(..).map(|l| (l.settings.folder_key.clone(), l)).collect();
        for f in &cfg.folders {
            let same = old.get(&f.folder_key).is_some_and(|l| {
                l.settings.local_path == f.local_path
                    && l.settings.kind == f.kind
                    && l.settings.label == f.label
                    && l.settings.remote_folder == f.remote_folder
            });
            let mut live = match old.remove(&f.folder_key).filter(|_| same) {
                Some(l) => l,
                None => {
                    let mut job =
                        FolderJob::new(f, &cfg.storage_id, &cfg.device_name, &cfg.data_dir, cfg.skip_inside.clone());
                    job.remote_id = self.remote_ids.get(&f.folder_key).cloned();
                    Live {
                        folder: Folder::new(job),
                        enabled: false,
                        settings: f.clone(),
                        _watcher: None,
                        next_full: now,
                        recheck: None,
                    }
                }
            };
            if live.settings.excludes != f.excludes {
                live.folder.job.excludes = Excludes::new(&f.excludes);
                live.next_full = now;
            }
            if f.enabled && live._watcher.is_none() {
                live._watcher = watch(&f.folder_key, f, &self.shared);
                live.next_full = now;
            } else if !f.enabled {
                live._watcher = None;
                live.folder.status.phase = "off";
                live.folder.status.uploading.clear();
                crate::shellstatus::remove(&f.folder_key);
            }
            live.enabled = f.enabled;
            live.settings = f.clone();
            self.folders.push(live);
        }
        // Removed folders: their watchers end here (the state file is the
        // app's to delete), and Explorer stops showing them.
        for k in old.keys() {
            crate::shellstatus::remove(k);
        }
        drop(old);
        self.cfg = cfg;
        self.publish();
        self.sync_marks();
    }

    /// Explorer's view (shellstatus.rs): every enabled folder published
    /// (once that is turned on, they appear without waiting for a pass).
    fn sync_marks(&mut self) {
        if !crate::shellstatus::enabled() {
            return;
        }
        let published: HashSet<String> = crate::shellstatus::published().into_iter().collect();
        for l in self.folders.iter_mut().filter(|l| l.enabled) {
            if !published.contains(&l.settings.folder_key) {
                l.folder.publish_marks();
            }
        }
    }

    fn publish(&self) {
        let mut s = self.status.lock();
        s.folders = self
            .folders
            .iter()
            .map(|l| {
                let mut st = l.folder.status.clone();
                if !l.enabled {
                    st.phase = "off";
                } else if self.paused() && st.phase != "missing" {
                    st.phase = "paused";
                }
                (l.settings.folder_key.clone(), st)
            })
            .collect();
        s.device_id = self.device_id.clone();
        s.paused = self.paused();
    }

    fn set_panel(&self, what: String) {
        self.status.lock().panel = what;
    }

    /// PUT device (when due) and PUT profile (when it changed).
    fn register(&mut self) -> Result<(), ApiError> {
        let now = Instant::now();
        if self.device_id.is_none() && now >= self.register_at {
            let req = DeviceRequest {
                device_key: self.cfg.device_key.clone(),
                name: self.cfg.device_name.clone(),
                platform: "windows".into(),
                os_version: self.cfg.os_version.clone(),
                app_version: self.cfg.app_version.clone(),
            };
            match self.api.backup_device(&req) {
                Ok(id) => {
                    tracing::info!("backup: device registered ({id})");
                    self.device_id = Some(id);
                    self.set_panel("ok".into());
                }
                Err(e) if matches!(e.status(), Some(404) | Some(405)) => {
                    tracing::info!("backup: this panel has no folder backup yet ({e}); photo backup continues on its own");
                    self.register_at = now + self.opts.api_retry;
                    self.set_panel("unavailable".into());
                    return Ok(());
                }
                Err(e) if e.is_network() || e.is_auth() => return Err(e),
                Err(e) => {
                    tracing::warn!("backup: registering this device: {e}");
                    self.register_at = now + Duration::from_secs(300);
                    self.set_panel(e.to_string());
                    return Ok(());
                }
            }
        }
        let Some(dev) = self.device_id.clone() else { return Ok(()) };
        // A folder whose drive folder was deleted: the panel makes a new one.
        if self.folders.iter().any(|l| l.folder.needs_profile) && self.profile_retry_at.is_none_or(|t| now >= t) {
            for l in &mut self.folders {
                l.folder.needs_profile = false;
            }
            self.profile_sent = None;
            self.profile_retry_at = Some(now + Duration::from_secs(300));
        }
        let mut folders = Vec::new();
        for f in self.cfg.folders.clone() {
            let remote_folder_id = match f.kind {
                FolderKind::Photos => self.photos_folder_id(&f)?,
                FolderKind::Folder => None,
            };
            folders.push(ProfileFolderRequest {
                folder_key: f.folder_key.clone(),
                label: f.label.clone(),
                local_path: f.local_path.display().to_string(),
                kind: f.kind.as_str().into(),
                enabled: f.enabled,
                excludes: f.excludes.clone(),
                remote_folder_id,
            });
        }
        let req = ProfileRequest {
            device_id: dev,
            storage_id: self.cfg.storage_id.clone(),
            paused: self.paused(),
            folders,
        };
        if self.profile_sent.as_ref() == Some(&req) {
            return Ok(());
        }
        match self.api.backup_put_profile(&req) {
            Ok(p) => {
                self.remote_ids = p
                    .folders
                    .into_iter()
                    .filter_map(|f| f.remote_folder_id.map(|id| (f.folder_key, id)))
                    .collect();
                for l in &mut self.folders {
                    l.folder.job.remote_id = self.remote_ids.get(&l.settings.folder_key).cloned();
                    if l.folder.status.phase == "panel" && l.folder.job.remote_id.is_some() {
                        l.next_full = Instant::now();
                    }
                }
                tracing::info!("backup: profile saved ({} folder(s))", req.folders.len());
                self.profile_sent = Some(req);
                Ok(())
            }
            Err(e) if e.is_network() || e.is_auth() => Err(e),
            Err(e) if e.is_not_found() => {
                // The device isn't known (any more) in this organization.
                tracing::warn!("backup: saving the profile: {e}; registering this device again");
                self.device_id = None;
                self.register_at = now + Duration::from_secs(300);
                Ok(())
            }
            Err(e) => {
                tracing::warn!("backup: saving the profile: {e}");
                self.set_panel(e.to_string());
                // Not again until something changes.
                self.profile_sent = Some(req);
                Ok(())
            }
        }
    }

    /// The drive folder a photos folder uploads to (made if missing), sent
    /// with the profile so the panel adopts it instead of making another.
    fn photos_folder_id(&mut self, f: &BackupFolder) -> Result<Option<String>, ApiError> {
        if let Some(id) = self.photos_ids.get(&f.folder_key) {
            return Ok(Some(id.clone()));
        }
        if !f.enabled {
            return Ok(None);
        }
        let sid = self.cfg.storage_id.clone();
        let tree = self.api.tree(&sid, None)?.unwrap_or_else(|| empty_tree(&sid));
        let mut index = folder_index(&tree);
        match ensure_folders(&self.api, &sid, &remote_path(f, &self.cfg.device_name), &mut index) {
            Ok(Some(id)) => {
                self.photos_ids.insert(f.folder_key.clone(), id.clone());
                Ok(Some(id))
            }
            Ok(None) => Ok(None),
            Err(e) if e.is_network() || e.is_auth() => Err(e),
            Err(e) => {
                tracing::warn!("backup {}: its drive folder: {e}", f.label);
                Ok(None)
            }
        }
    }

    fn status_request(&self) -> Option<StatusRequest> {
        let dev = self.device_id.clone()?;
        Some(StatusRequest {
            device_id: dev,
            folders: self
                .folders
                .iter()
                .map(|l| FolderStatusReport {
                    folder_key: l.settings.folder_key.clone(),
                    files: l.folder.status.files,
                    bytes: l.folder.status.bytes,
                    pending: l.folder.status.pending,
                    last_backup_at: l.folder.status.last_backup_at.clone(),
                    last_error: l.folder.status.last_error.clone(),
                })
                .collect(),
        })
    }

    /// POST status if it changed, at most once per `status_every` while
    /// busy (`force`: a pass just ended).
    fn post_status(&mut self, force: bool) {
        let Some(req) = self.status_request() else { return };
        if self.posted.as_ref() == Some(&req) {
            return;
        }
        let due = self.last_post.is_none_or(|t| t.elapsed() >= self.opts.status_every);
        if !due && !force {
            return;
        }
        match self.api.backup_status(&req) {
            Ok(()) => {
                self.posted = Some(req);
                self.last_post = Some(Instant::now());
            }
            Err(e) => {
                tracing::debug!("backup: status: {e}");
                self.last_post = Some(Instant::now());
            }
        }
    }

    fn take_signals(&mut self, timeout: Duration) -> bool {
        let (cfg, events) = {
            let mut sig = self.shared.signals.lock();
            if !sig.kicked && !self.shared.stop.load(Ordering::SeqCst) {
                self.shared.cv.wait_for(&mut sig, timeout);
            }
            sig.kicked = false;
            (sig.config.take(), std::mem::take(&mut sig.events))
        };
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        if let Some(c) = cfg {
            self.apply(c);
        }
        for (key, ev) in events {
            if let Some(l) = self.folders.iter_mut().find(|l| l.settings.folder_key == key) {
                if ev.full {
                    l.next_full = Instant::now();
                }
                l.folder.dirty.extend(ev.paths);
                l.folder.touched.extend(ev.touched);
                if l.recheck.is_none() && !l.folder.dirty.is_empty() {
                    l.recheck = Some(Instant::now() + self.opts.settle);
                }
            }
        }
        true
    }

    fn next_deadline(&self) -> Duration {
        let now = Instant::now();
        let mut t = now + Duration::from_secs(60);
        for l in self.folders.iter().filter(|l| l.enabled) {
            t = t.min(l.next_full);
            if let Some(r) = l.recheck {
                t = t.min(r);
            }
        }
        if let Some(r) = self.retry_at {
            t = t.max(r);
        }
        if self.device_id.is_none() {
            t = t.min(self.register_at);
        }
        t.saturating_duration_since(now).max(Duration::from_millis(50))
    }

    fn run(mut self) {
        tracing::info!("backup: {} folder(s) into drive {}", self.cfg.folders.len(), self.cfg.storage_id);
        loop {
            let wait = self.next_deadline();
            if !self.take_signals(wait) {
                break;
            }
            self.publish();
            self.sync_marks();
            if self.retry_at.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            match self.pass() {
                Ok(ran) => {
                    self.retry_at = None;
                    self.backoff = Duration::from_secs(5);
                    if ran {
                        // Right away once everything is done; while files are
                        // still waiting, at most once per `status_every`.
                        let busy = self.folders.iter().any(|l| l.enabled && l.folder.status.pending > 0);
                        self.post_status(!busy);
                    }
                    self.status.lock().last_error = None;
                }
                Err(e) => {
                    tracing::warn!("backup: waiting {:?} ({e})", self.backoff);
                    // A connection problem is retried at least every 30 s, so
                    // backup picks up again soon after the connection is back
                    // (it used to back off to 10 min and look stuck "offline").
                    // Panel-side errors keep the long back-off.
                    let cap = if e.is_network() { NETWORK_RETRY_CAP } else { Duration::from_secs(600) };
                    self.backoff = self.backoff.min(cap);
                    self.status.lock().last_error = Some(if e.is_network() {
                        format!(
                            "Can't reach OTHCloud right now — trying again in {} s",
                            self.backoff.as_secs().max(1)
                        )
                    } else {
                        e.to_string()
                    });
                    self.retry_at = Some(Instant::now() + self.backoff);
                    self.backoff = (self.backoff * 2).min(cap);
                }
            }
            self.publish();
        }
        for l in &self.folders {
            l.folder.state.save(&l.folder.job.state_path);
            crate::shellstatus::remove(&l.settings.folder_key);
        }
        tracing::info!("backup stopped");
    }

    /// One round over the folders that have something to do.
    fn pass(&mut self) -> Result<bool, ApiError> {
        self.register()?;
        if self.paused() {
            return Ok(false);
        }
        let now = Instant::now();
        let mut ran = false;
        for i in 0..self.folders.len() {
            if self.shared.stop.load(Ordering::SeqCst) || self.shared.signals.lock().config.is_some() {
                break;
            }
            let l = &mut self.folders[i];
            if !l.enabled {
                continue;
            }
            let full_due = now >= l.next_full;
            let recheck_due = l.recheck.is_some_and(|t| now >= t);
            if !full_due && !recheck_due {
                continue;
            }
            let scope = if full_due {
                l.folder.dirty.clear();
                None
            } else {
                Some(l.folder.dirty.drain().collect::<Vec<_>>())
            };
            // Forget watcher times nobody needs any more.
            let settle = self.opts.settle;
            l.folder.touched.retain(|_, t| now.saturating_duration_since(*t) < settle * 3);
            let shared = self.shared.clone();
            let cancel = move || {
                shared.stop.load(Ordering::SeqCst)
                    || shared.paused.load(Ordering::Relaxed)
                    || shared.signals.lock().config.is_some()
            };
            let api = self.api.clone();
            let dev = self.device_id.clone();
            let status = self.status.clone();
            let key = l.settings.folder_key.clone();
            let every = self.opts.status_every;
            let mut last_post = self.last_post;
            let mut progress = |st: &FolderStatus| {
                status.lock().folders.insert(key.clone(), st.clone());
                // While busy: at most once per `status_every`, this folder only.
                if let (Some(dev), Some(last)) = (&dev, last_post) {
                    if last.elapsed() >= every && st.phase == "uploading" {
                        let req = StatusRequest {
                            device_id: dev.clone(),
                            folders: vec![FolderStatusReport {
                                folder_key: key.clone(),
                                files: st.files,
                                bytes: st.bytes,
                                pending: st.pending,
                                last_backup_at: st.last_backup_at.clone(),
                                last_error: st.last_error.clone(),
                            }],
                        };
                        let _ = api.backup_status(&req);
                        last_post = Some(Instant::now());
                    }
                }
            };
            let mut ctx = Ctx {
                api: &self.api,
                storage_id: &self.cfg.storage_id,
                device_id: self.device_id.as_deref(),
                settle: self.opts.settle,
                uploaders: self.opts.uploaders,
                cancel: &cancel,
                on_progress: &mut progress,
            };
            let r = l.folder.run(&mut ctx, scope);
            self.last_post = last_post.or(self.last_post);
            if full_due {
                l.next_full = Instant::now() + self.opts.rescan_every;
            }
            l.recheck = if l.folder.dirty.is_empty() {
                None
            } else {
                Some(Instant::now() + self.opts.settle)
            };
            ran = true;
            match r {
                Ok(rep) => {
                    if rep.uploaded + rep.already + rep.marked + rep.unmarked > 0 {
                        tracing::info!(
                            "backup {}: {} uploaded, {} already there, {} marked deleted, {} back, {} waiting",
                            l.settings.label,
                            rep.uploaded,
                            rep.already,
                            rep.marked,
                            rep.unmarked,
                            rep.pending
                        );
                    } else {
                        tracing::debug!("backup {}: nothing to upload ({} waiting)", l.settings.label, rep.pending);
                    }
                }
                Err(e) => {
                    // The pass will be repeated (whole folder: nothing missed).
                    if full_due {
                        l.next_full = Instant::now();
                    }
                    return Err(e);
                }
            }
        }
        Ok(ran)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_detection_and_free_names() {
        assert!(is_image(Path::new("a/B.JPG")));
        assert!(is_image(Path::new("x.cr3")));
        assert!(!is_image(Path::new("notes.txt")));
        assert!(!is_image(Path::new("jpg")));
        assert_eq!(free_name(&[], "a.jpg"), "a.jpg");
        assert_eq!(free_name(&["A.JPG".into()], "a.jpg"), "a (1).jpg");
        assert_eq!(free_name(&["a.jpg".into(), "a (1).jpg".into()], "a.jpg"), "a (2).jpg");
    }

    fn job(root: &Path, kind: FolderKind, excludes: &[&str]) -> FolderJob {
        let f = BackupFolder {
            folder_key: "k".into(),
            label: "Docs".into(),
            local_path: root.to_path_buf(),
            kind,
            enabled: true,
            excludes: excludes.iter().map(|s| s.to_string()).collect(),
            remote_folder: None,
        };
        FolderJob::new(&f, "stor", "PC", root, vec![])
    }

    fn rels(s: &Scan) -> Vec<&str> {
        s.files.iter().map(|x| x.rel.as_str()).collect()
    }

    #[test]
    fn skip_rules() {
        assert!(skip_name("desktop.ini", false));
        assert!(skip_name("Thumbs.db", false));
        assert!(skip_name("~$Report.docx", false));
        assert!(skip_name("build.TMP", false));
        assert!(skip_name(".git", true));
        assert!(skip_name("node_modules", true));
        assert!(!skip_name("node_modules", false));
        assert!(!skip_name("report.docx", false));
        assert_eq!(skip_attributes(0x2, false), Some(Skip::Hidden));
        assert_eq!(skip_attributes(0x4, true), Some(Skip::Hidden));
        assert_eq!(skip_attributes(0x40_0000, false), Some(Skip::CloudOnly));
        assert_eq!(skip_attributes(0x40_0000, true), None);
        assert_eq!(skip_attributes(0x20, false), None);

        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        for dir in [".git/objects", "node_modules/x", "src", "Hidden"] {
            std::fs::create_dir_all(p.join(dir)).unwrap();
        }
        for f in [
            "a.txt", "desktop.ini", "Thumbs.db", "~$a.docx", "x.tmp", ".git/HEAD", "node_modules/x/i.js", "src/m.rs",
            "Hidden/in.txt", "h.txt",
        ] {
            std::fs::write(p.join(f), b"x").unwrap();
        }
        let hide = |path: &Path| {
            let st = std::process::Command::new("attrib").arg("+h").arg(path).status().unwrap();
            assert!(st.success());
        };
        hide(&p.join("h.txt"));
        hide(&p.join("Hidden"));
        let s = job(p, FolderKind::Folder, &[]).scan();
        assert_eq!(rels(&s), vec!["a.txt", "src/m.rs"]);
        // Photos: pictures only.
        std::fs::write(p.join("img.JPG"), b"j").unwrap();
        assert_eq!(rels(&job(p, FolderKind::Photos, &[]).scan()), vec!["img.JPG"]);
    }

    #[test]
    fn junctions_are_never_followed() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"s").unwrap();
        std::fs::write(d.path().join("mine.txt"), b"m").unwrap();
        let link = d.path().join("elsewhere");
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        assert!(link.join("secret.txt").exists());
        let s = job(d.path(), FolderKind::Folder, &[]).scan();
        assert_eq!(rels(&s), vec!["mine.txt"]);
        assert_eq!(s.skipped, 1);
    }

    #[test]
    fn exclude_globs() {
        let x = Excludes::new(&[
            "*.iso".into(),
            "cache/".into(),
            "Projects/*/bin".into(),
            "/top.txt".into(),
            "**/secret/**".into(),
            "# a comment".into(),
            "  ".into(),
            "a?c.log".into(),
        ]);
        assert!(x.matches("big.ISO", false));
        assert!(x.matches("deep/in/here/x.iso", false));
        assert!(x.matches("cache", true));
        assert!(x.matches("a/b/Cache", true));
        assert!(!x.matches("cache", false), "trailing / = folders only");
        assert!(x.matches("projects/app/bin", true));
        assert!(!x.matches("projects/app/sub/bin", true), "* stays inside one name");
        assert!(x.matches("top.txt", false));
        assert!(!x.matches("sub/top.txt", false), "leading / anchors");
        assert!(x.matches("secret/a.txt", false), "**/ may match no folder");
        assert!(x.matches("x/y/secret/z/a.txt", false));
        assert!(x.matches("abc.log", false));
        assert!(!x.matches("abbc.log", false));
        assert!(!x.matches("readme.md", false));
        assert!(Excludes::new(&[]).is_empty());
        let star = Excludes::new(&["*".into()]);
        assert!(star.matches("anything/at/all.txt", false));
        // No blow-up on pathological patterns.
        let slow = Excludes::new(&["*a*a*a*a*a*a*a*a*a*a*b".into()]);
        assert!(!slow.matches(&"a".repeat(200), false));
        assert!(clean_excludes(&["  *.iso ".into(), "".into()]).unwrap() == vec!["*.iso".to_string()]);
        assert!(clean_excludes(&["a\nb".into()]).is_err());

        // Walking: an excluded folder isn't entered.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("cache/deep")).unwrap();
        std::fs::write(d.path().join("cache/deep/f.txt"), b"x").unwrap();
        std::fs::write(d.path().join("keep.txt"), b"x").unwrap();
        std::fs::write(d.path().join("disk.iso"), b"x").unwrap();
        let s = job(d.path(), FolderKind::Folder, &["cache/", "*.iso"]).scan();
        assert_eq!(rels(&s), vec!["keep.txt"]);
    }

    #[test]
    fn stability_debounce() {
        let settle = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut s = Stability::default();
        // Old file, first seen at start, no watcher event: ready at once.
        assert!(s.ready("old", 5, 100, Duration::from_secs(3600), None, settle, t0));
        // Freshly written: not before it has been unchanged for 10 s.
        assert!(!s.ready("new", 5, 200, Duration::from_secs(1), None, settle, t0));
        assert!(!s.ready("new", 5, 200, Duration::from_secs(6), None, settle, t0 + Duration::from_secs(5)));
        // ...and growing again restarts the wait.
        assert!(!s.ready("new", 9, 211, Duration::from_secs(0), None, settle, t0 + Duration::from_secs(11)));
        assert!(!s.ready("new", 9, 211, Duration::from_secs(9), None, settle, t0 + Duration::from_secs(20)));
        assert!(s.ready("new", 9, 211, Duration::from_secs(10), None, settle, t0 + Duration::from_secs(21)));
        // A copy keeps the source's old mtime, but the watcher saw it
        // being written just now: wait anyway.
        let ev = t0 + Duration::from_secs(30);
        assert!(!s.ready("copy", 7, 50, Duration::from_secs(9999), Some(ev), settle, ev));
        assert!(!s.ready("copy", 7, 50, Duration::from_secs(9999), Some(ev), settle, ev + Duration::from_secs(9)));
        assert!(s.ready("copy", 7, 50, Duration::from_secs(9999), Some(ev), settle, ev + Duration::from_secs(10)));
    }

    #[test]
    fn roots_inside_our_drives_or_windows_are_refused() {
        let ours = vec![PathBuf::from(r"C:\Users\me\OTHCloud\Work"), PathBuf::from(r"C:\Users\me\OTHCloud")];
        let sys = vec![PathBuf::from(r"C:\Windows"), PathBuf::from(r"C:\Program Files")];
        let ok = |p: &str| check_root(Path::new(p), &ours, &sys);
        assert!(ok(r"C:\Users\me\Documents").is_ok());
        assert!(ok(r"C:\Users\me\OTHCloud-old").is_ok(), "a prefix of the name is not inside");
        assert!(ok(r"C:\Users\me\OTHCloud\Work").unwrap_err().contains("OTHCloud drives"));
        assert!(ok(r"c:\users\ME\othcloud\work\sub\").unwrap_err().contains("OTHCloud drives"));
        assert!(ok(r"\\?\C:\Users\me\OTHCloud\Work\x").is_err());
        assert!(ok(r"C:\Windows\System32").unwrap_err().contains("Windows"));
        assert!(ok(r"C:\Program Files").is_err());
        assert!(ok(r"C:\").unwrap_err().contains("whole drive"));
        assert!(ok(r"D:").is_err());
        assert!(ok(r"\\server\share\x").unwrap_err().contains("network"));
        assert!(ok(r"relative\path").is_err());
        assert!(is_within(Path::new(r"C:\a\b"), Path::new(r"C:\A\")));
        assert!(!is_within(Path::new(r"C:\ab"), Path::new(r"C:\a")));

        // The real thing: a folder that exists, a sync root, a link to it.
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Drive");
        let docs = d.path().join("Docs");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        let ours = vec![root.clone()];
        assert!(check_backup_root(&docs, &ours, &[]).is_ok());
        assert!(check_backup_root(&root.join("sub"), &ours, &[]).is_err());
        assert!(check_backup_root(&d.path().join("nope"), &ours, &[]).is_err());
        std::fs::write(d.path().join("f.txt"), b"x").unwrap();
        assert!(check_backup_root(&d.path().join("f.txt"), &ours, &[]).is_err());
        let j = d.path().join("j");
        let st = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&j).arg(&root).output().unwrap();
        assert!(st.status.success());
        assert!(check_backup_root(&j, &ours, &[]).unwrap_err().contains("link"));
        // A subst letter onto the drive is the drive.
        if let Some(l) = crate::platform::free_letters(&[]).last().copied() {
            crate::letters::map(l, &root).unwrap();
            let r = check_backup_root(Path::new(&format!("{l}:\\sub")), &ours, &[]);
            crate::letters::unmap(l, &root);
            assert!(r.unwrap_err().contains("OTHCloud drives"));
        }
        assert!(check_label("  Work stuff ").unwrap() == "Work stuff");
        assert!(check_label("a/b").is_err());
        assert!(check_label("..").is_err());
        assert!(check_label("").is_err());
    }

    #[test]
    fn remote_layout_and_state_files() {
        let f = BackupFolder {
            folder_key: "ab12".into(),
            label: "Documents".into(),
            kind: FolderKind::Folder,
            ..Default::default()
        };
        assert_eq!(remote_path(&f, "PC1"), vec!["Backups", "PC1", "Documents"]);
        let p = BackupFolder { folder_key: PHOTOS_KEY.into(), kind: FolderKind::Photos, ..Default::default() };
        assert_eq!(remote_path(&p, "PC1"), vec!["Photo backup", "PC1"]);
        let c = BackupFolder { remote_folder: Some("Phone/Cam/".into()), ..p.clone() };
        assert_eq!(remote_path(&c, "PC1"), vec!["Phone", "Cam"]);
        let dd = Path::new(r"C:\data");
        assert_eq!(state_path(dd, "s1", PHOTOS_KEY), dd.join("backup-s1.json"));
        assert_eq!(state_path(dd, "s1", "ab12"), dd.join("backup-s1-ab12.json"));
    }

    #[test]
    fn state_reads_the_photo_backup_format_and_marks_deletions() {
        // backup-<storageId>.json as the photo backup wrote it.
        let old = r#"{"files":{"Screenshots/a.png":{"size":3,"mtime":17,"file_id":"file_a"},"b.jpg":{"size":1,"mtime":2,"file_id":"file_b"}}}"#;
        let d = tempfile::tempdir().unwrap();
        let sp = d.path().join("backup-s.json");
        std::fs::write(&sp, old).unwrap();
        let st = BackupState::load(&sp);
        assert_eq!(st.files.len(), 2);
        assert_eq!(st.files["b.jpg"], Uploaded { size: 1, mtime: 2, file_id: "file_b".into(), gone: false, marked: false });
        assert_eq!(st.totals(), (2, 4));
        // Unchanged entries serialize exactly as before (no new keys).
        st.save(&sp);
        let back: serde_json::Value = serde_json::from_slice(&std::fs::read(&sp).unwrap()).unwrap();
        assert_eq!(back["files"]["b.jpg"], serde_json::json!({"size":1,"mtime":2,"file_id":"file_b"}));
    }
}
