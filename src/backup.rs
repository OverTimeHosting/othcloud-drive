//! Photo backup: watch a local folder and upload new or changed images to
//! `<drive>/<remote folder>/<same sub-folders>/<name>`. Nothing is ever
//! deleted on the server. What has been uploaded is remembered in
//! `%LOCALAPPDATA%\OTHCloud Drive\backup-<storageId>.json` (path, size,
//! mtime, fileId), so a restart doesn't re-upload.

use crate::api::{Api, ApiError, Tree};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Uploaded {
    pub size: u64,
    pub mtime: u64,
    pub file_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupState {
    /// Relative path with '/' separators -> what we uploaded.
    #[serde(default)]
    pub files: HashMap<String, Uploaded>,
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
}

#[derive(Debug, Clone)]
pub struct BackupJob {
    pub folder: PathBuf,
    pub storage_id: String,
    /// Remote folder path components, e.g. ["Photo backup", "MYPC"].
    pub remote: Vec<String>,
    pub state_path: PathBuf,
    /// Files modified more recently than this are still being written.
    pub settle: Duration,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub uploaded: usize,
    pub already: usize,
    pub failed: usize,
    pub pending: usize,
}

fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every image under `root`: (relative path with '/', size, mtime secs, age).
pub fn scan(root: &Path) -> Vec<(String, u64, u64, Duration)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let now = SystemTime::now();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && is_image(&p) {
                let Ok(m) = e.metadata() else { continue };
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                let age = m
                    .modified()
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .unwrap_or_default();
                out.push((rel, m.len(), mtime_secs(&m), age));
            }
        }
    }
    out.sort();
    out
}

/// Remote folder ids by path ("a/b"), from a tree. "" = root (None).
fn folder_index(t: &Tree) -> HashMap<String, String> {
    let by_id: HashMap<&str, &crate::api::RemoteFolder> =
        t.folders.iter().map(|f| (f.folder_id.as_str(), f)).collect();
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
                let t = api.tree(storage_id, None)?.unwrap_or_else(|| Tree {
                    storage_id: storage_id.into(),
                    etag: String::new(),
                    folders: vec![],
                    files: vec![],
                });
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

/// One pass: upload whatever is new or changed. Saves state as it goes.
pub fn sync_once(api: &Api, job: &BackupJob, state: &mut BackupState) -> Result<Report, ApiError> {
    let mut rep = Report::default();
    let todo: Vec<(String, u64, u64)> = scan(&job.folder)
        .into_iter()
        .filter(|(rel, size, mtime, age)| {
            let changed = match state.files.get(rel) {
                Some(u) => u.size != *size || u.mtime != *mtime,
                None => true,
            };
            if changed && *age < job.settle {
                rep.pending += 1;
                return false;
            }
            changed
        })
        .map(|(r, s, m, _)| (r, s, m))
        .collect();
    if todo.is_empty() {
        return Ok(rep);
    }
    let tree = api.tree(&job.storage_id, None)?.unwrap_or_else(|| Tree {
        storage_id: job.storage_id.clone(),
        etag: String::new(),
        folders: vec![],
        files: vec![],
    });
    let mut index = folder_index(&tree);
    let mut names_in: HashMap<Option<String>, Vec<(String, String, u64)>> = HashMap::new();
    for f in &tree.files {
        names_in
            .entry(f.folder_id.clone())
            .or_default()
            .push((f.name.clone(), f.file_id.clone(), f.size_bytes));
    }
    for (rel, size, mtime) in todo {
        let local = job.folder.join(rel.replace('/', "\\"));
        let mut parts = job.remote.clone();
        let mut segs: Vec<&str> = rel.split('/').collect();
        let name = segs.pop().unwrap_or_default().to_string();
        parts.extend(segs.iter().map(|s| s.to_string()));
        let result: Result<(Uploaded, bool), ApiError> = (|| {
            // Changed file we uploaded before: replace in place.
            if let Some(prev) = state.files.get(&rel) {
                match api.upload_replace(&prev.file_id, &local) {
                    Ok(pf) => {
                        let file_id = pf.file_id.unwrap_or(prev.file_id.clone());
                        return Ok((Uploaded { size, mtime, file_id }, true));
                    }
                    Err(e) if e.is_not_found() => {} // removed remotely: upload anew
                    Err(e) => return Err(e),
                }
            }
            let folder = ensure_folders(api, &job.storage_id, &parts, &mut index)?;
            let siblings = names_in.entry(folder.clone()).or_default();
            if let Some((_, id, sz)) = siblings
                .iter()
                .find(|(n, _, _)| n.eq_ignore_ascii_case(&name))
            {
                if *sz == size {
                    // Same name and size already there (earlier install).
                    let file_id = id.clone();
                    return Ok((Uploaded { size, mtime, file_id }, false));
                }
            }
            let taken: Vec<String> = siblings.iter().map(|s| s.0.clone()).collect();
            let upload_name = free_name(&taken, &name);
            let pf = api.upload_new(&job.storage_id, folder.as_deref(), &upload_name, &local)?;
            let id = pf.file_id.ok_or_else(|| ApiError::Decode("upload returned no fileId".into()))?;
            siblings.push((upload_name, id.clone(), size));
            Ok((Uploaded { size, mtime, file_id: id }, true))
        })();
        match result {
            Ok((u, transferred)) => {
                tracing::info!("photo backup: {rel} -> {}", u.file_id);
                state.files.insert(rel, u);
                state.save(&job.state_path);
                if transferred {
                    rep.uploaded += 1;
                } else {
                    rep.already += 1;
                }
            }
            Err(e) if e.is_network() || e.is_auth() => return Err(e),
            Err(e) => {
                tracing::warn!("photo backup: {rel}: {e}");
                rep.failed += 1;
            }
        }
    }
    Ok(rep)
}

#[derive(Debug, Clone, Default)]
pub struct BackupStatus {
    pub last: Option<Report>,
    pub last_error: Option<String>,
    pub total_backed_up: usize,
}

/// A running backup watcher.
pub struct BackupHandle {
    stop: Arc<AtomicBool>,
    wake: Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
    _watcher: Option<notify::RecommendedWatcher>,
    pub status: Arc<parking_lot::Mutex<BackupStatus>>,
    pub job: BackupJob,
}

impl BackupHandle {
    pub fn start(api: Api, job: BackupJob, paused: Arc<AtomicBool>) -> Result<BackupHandle, String> {
        use notify::Watcher;
        if !job.folder.is_dir() {
            return Err(format!("{} is not a folder", job.folder.display()));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((parking_lot::Mutex::new(true), parking_lot::Condvar::new()));
        let status = Arc::new(parking_lot::Mutex::new(BackupStatus::default()));

        let w2 = wake.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if ev.paths.iter().any(|p| is_image(p) || p.is_dir()) {
                    *w2.0.lock() = true;
                    w2.1.notify_all();
                }
            }
        })
        .map_err(|e| format!("watch: {e}"))?;
        watcher
            .watch(&job.folder, notify::RecursiveMode::Recursive)
            .map_err(|e| format!("watch {}: {e}", job.folder.display()))?;

        let (s2, w3, st2, j2) = (stop.clone(), wake.clone(), status.clone(), job.clone());
        let thread = std::thread::Builder::new()
            .name("photo-backup".into())
            .spawn(move || {
                let mut state = BackupState::load(&j2.state_path);
                let mut backoff = Duration::from_secs(5);
                while !s2.load(Ordering::SeqCst) {
                    // Wait for a change, a periodic rescan, or stop.
                    {
                        let mut woken = w3.0.lock();
                        if !*woken {
                            w3.1.wait_for(&mut woken, Duration::from_secs(300));
                        }
                        *woken = false;
                    }
                    if s2.load(Ordering::SeqCst) {
                        break;
                    }
                    // Let a burst of writes (camera import) settle.
                    std::thread::sleep(Duration::from_millis(500));
                    if paused.load(Ordering::Relaxed) {
                        *w3.0.lock() = true;
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    match sync_once(&api, &j2, &mut state) {
                        Ok(rep) => {
                            backoff = Duration::from_secs(5);
                            let again = rep.pending > 0 || rep.failed > 0;
                            let mut st = st2.lock();
                            st.total_backed_up = state.files.len();
                            st.last = Some(rep);
                            st.last_error = None;
                            drop(st);
                            if again {
                                // Unsettled / failed files: look again soon.
                                std::thread::sleep(j2.settle.max(Duration::from_secs(2)));
                                *w3.0.lock() = true;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("photo backup paused for {backoff:?}: {e}");
                            st2.lock().last_error = Some(e.to_string());
                            let mut woken = w3.0.lock();
                            w3.1.wait_for(&mut woken, backoff);
                            *woken = true;
                            backoff = (backoff * 2).min(Duration::from_secs(600));
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(BackupHandle {
            stop,
            wake,
            thread: Some(thread),
            _watcher: Some(watcher),
            status,
            job,
        })
    }

    pub fn kick(&self) {
        *self.wake.0.lock() = true;
        self.wake.1.notify_all();
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.kick();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for BackupHandle {
    fn drop(&mut self) {
        self.stop();
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
        assert_eq!(
            free_name(&["a.jpg".into(), "a (1).jpg".into()], "a.jpg"),
            "a (2).jpg"
        );
    }

    #[test]
    fn scan_finds_images_recursively() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub/deeper")).unwrap();
        std::fs::write(d.path().join("a.jpg"), b"a").unwrap();
        std::fs::write(d.path().join("sub/deeper/b.PNG"), b"bb").unwrap();
        std::fs::write(d.path().join("sub/notes.txt"), b"n").unwrap();
        let s = scan(d.path());
        let rels: Vec<&str> = s.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(rels, vec!["a.jpg", "sub/deeper/b.PNG"]);
        assert_eq!(s[1].1, 2);
    }
}
