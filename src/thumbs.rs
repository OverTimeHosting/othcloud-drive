//! Thumbnails for Explorer. The shell thumbnail handler (a DLL OTHCloud
//! Companion registers as each sync root's `ThumbnailProvider`) asks this
//! process over `ipc::serve_thumbnails` for a path and a size; this module
//! decides whether the path is ours, finds the remote file from the
//! placeholder's identity (never hydrating it) and fetches
//! `GET thumbnail` from the panel — with the token only this process holds.
//!
//! Answers are cached in memory (bounded), keyed on what the placeholder says
//! about its contents, so a re-thumbnail after Explorer drops its own cache
//! costs nothing and a changed file never gets the old picture.

use crate::api::Api;
use crate::ipc::ThumbStatus;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Sizes asked of the panel (it keeps the same ones): Explorer's 16…256 px
/// views land on a few cached copies instead of one per pixel.
const SIZES: [u32; 4] = [96, 256, 512, 1024];
/// Explorer gives up on the handler after its own timeout; the DLL after 5 s.
const FETCH_TIMEOUT: Duration = Duration::from_millis(4500);
/// Panel requests at once (the panel itself resizes two at a time).
const MAX_FETCHES: usize = 4;
const CACHE_MAX_ENTRIES: usize = 1024;
const CACHE_MAX_BYTES: usize = 48 * 1024 * 1024;

/// What the panel can make a thumbnail of (see the panel's
/// `lib/desktop-thumbnail.ts`). Anything else is answered without a request.
const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jfif", "jpe", "png", "gif", "webp", "heic", "heif", "avif", "tif", "tiff",
];

struct Root {
    /// Lower-cased, with a trailing `\`, for prefix tests.
    key: String,
    api: Api,
}

struct CacheEntry {
    /// None: the panel has no thumbnail for it (remembered too).
    bytes: Option<Arc<Vec<u8>>>,
    tick: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<String, CacheEntry>,
    bytes: usize,
    tick: u64,
}

impl Cache {
    fn get(&mut self, k: &str) -> Option<Option<Arc<Vec<u8>>>> {
        self.tick += 1;
        let t = self.tick;
        self.map.get_mut(k).map(|e| {
            e.tick = t;
            e.bytes.clone()
        })
    }

    fn put(&mut self, k: String, v: Option<Arc<Vec<u8>>>) {
        self.tick += 1;
        let size = v.as_ref().map(|b| b.len()).unwrap_or(0);
        if size > CACHE_MAX_BYTES / 4 {
            return;
        }
        if let Some(old) = self.map.insert(k, CacheEntry { bytes: v, tick: self.tick }) {
            self.bytes -= old.bytes.map(|b| b.len()).unwrap_or(0);
        }
        self.bytes += size;
        while self.map.len() > CACHE_MAX_ENTRIES || self.bytes > CACHE_MAX_BYTES {
            let Some(oldest) = self.map.iter().min_by_key(|(_, e)| e.tick).map(|(k, _)| k.clone()) else {
                break;
            };
            if let Some(e) = self.map.remove(&oldest) {
                self.bytes -= e.bytes.map(|b| b.len()).unwrap_or(0);
            }
        }
    }
}

struct State {
    roots: Vec<Root>,
    cache: Cache,
    serving: bool,
}

static STATE: std::sync::LazyLock<Mutex<State>> =
    std::sync::LazyLock::new(|| Mutex::new(State { roots: Vec::new(), cache: Cache::default(), serving: false }));
static FETCHES: (Mutex<usize>, parking_lot::Condvar) = (Mutex::new(0), parking_lot::Condvar::new());

fn root_key(p: &Path) -> String {
    let mut s = strip_verbatim(&p.display().to_string()).to_lowercase();
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

fn strip_verbatim(s: &str) -> String {
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{r}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(s).to_string()
    }
}

/// A synced drive's folder: its placeholders get thumbnails from `api`.
pub fn add_root(root: &Path, api: Api) {
    let key = root_key(root);
    let mut s = STATE.lock();
    s.roots.retain(|r| r.key != key);
    s.roots.push(Root { key, api });
}

pub fn remove_root(root: &Path) {
    let key = root_key(root);
    STATE.lock().roots.retain(|r| r.key != key);
}

/// Forget every root (sign-out, a revoked token): no answer is given with a
/// token that is no longer ours to use.
pub fn clear_roots() {
    let mut s = STATE.lock();
    s.roots.clear();
    s.cache = Cache::default();
}

/// Start the pipe server, once per process. Only for the real app's roots,
/// and only once a handler is registered (otherwise nothing would ask).
pub fn ensure_serving() {
    {
        let mut s = STATE.lock();
        if s.serving {
            return;
        }
        s.serving = true;
    }
    match crate::ipc::serve_thumbnails(Arc::new(|p: &Path, size| thumbnail(p, size))) {
        Ok(()) => tracing::info!("thumbnail pipe: serving {}", crate::ipc::thumbs_pipe_name()),
        Err(e) => tracing::warn!("thumbnail pipe: not serving ({e}); Explorer will show icons for cloud files"),
    }
}

/// The kept size at or above `size` (the largest for anything bigger).
pub fn snap(size: u32) -> u32 {
    SIZES.iter().copied().find(|s| *s >= size).unwrap_or(SIZES[SIZES.len() - 1])
}

pub fn is_image_name(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Where `p` really is: through a drive letter alias (`subst`), `.`/`..`,
/// 8.3 names. Opens for attributes only — a placeholder is not hydrated.
fn final_path(p: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFinalPathNameByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    if !p.is_absolute() {
        return None;
    }
    let w: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let h = CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .ok()?;
        let mut buf = vec![0u16; 1024];
        let mut n = GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) as usize;
        if n > buf.len() {
            buf = vec![0u16; n + 1];
            n = GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) as usize;
        }
        let _ = CloseHandle(h);
        if n == 0 || n > buf.len() {
            return None;
        }
        let s = std::ffi::OsString::from_wide(&buf[..n]).to_string_lossy().to_string();
        Some(PathBuf::from(strip_verbatim(&s)))
    }
}

/// Which root's API serves `real` (already a final path), if any.
fn api_for(real: &Path) -> Option<Api> {
    let k = root_key(real);
    let s = STATE.lock();
    s.roots
        .iter()
        .filter(|r| k.starts_with(&r.key) && k.len() > r.key.len())
        .max_by_key(|r| r.key.len())
        .map(|r| r.api.clone())
}

/// The pipe's handler: image bytes for a placeholder in one of our roots.
pub fn thumbnail(path: &Path, size: u32) -> Result<Vec<u8>, ThumbStatus> {
    let started = std::time::Instant::now();
    let real = final_path(path).ok_or(ThumbStatus::NotOurs)?;
    let api = api_for(&real).ok_or(ThumbStatus::NotOurs)?;
    let meta = std::fs::symlink_metadata(&real).map_err(|_| ThumbStatus::NotOurs)?;
    if !meta.is_file() {
        return Err(ThumbStatus::NoThumbnail);
    }
    if !is_image_name(&real) {
        return Err(ThumbStatus::NoThumbnail);
    }
    let info = crate::cf::placeholder_info(&real)
        .ok()
        .flatten()
        .ok_or(ThumbStatus::NotOurs)?;
    let Some((false, file_id)) = crate::cf::parse_identity(&info.identity) else {
        return Err(ThumbStatus::NotOurs);
    };
    // Changed here and not uploaded yet: the panel's picture would be the
    // old one. Explorer shows the icon until it is in sync again.
    if !info.in_sync {
        return Err(ThumbStatus::NoThumbnail);
    }
    let w = snap(size.max(1));
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let key = format!("{file_id}|{w}|{}|{mtime}", meta.len());
    if let Some(hit) = STATE.lock().cache.get(&key) {
        tracing::debug!("thumbnail {} @{w}: cached", real.display());
        return hit.map(|b| b.as_ref().clone()).ok_or(ThumbStatus::NoThumbnail);
    }
    let r = {
        let _slot = fetch_slot(started + FETCH_TIMEOUT).ok_or(ThumbStatus::Busy)?;
        let left = (started + FETCH_TIMEOUT).saturating_duration_since(std::time::Instant::now());
        api.thumbnail(&file_id, w, left.max(Duration::from_millis(500)))
    };
    match r {
        Ok(v) => {
            let v = v.map(Arc::new);
            STATE.lock().cache.put(key, v.clone());
            tracing::debug!(
                "thumbnail {} @{w}: {} in {} ms",
                real.display(),
                v.as_ref().map(|b| format!("{} B", b.len())).unwrap_or_else(|| "none".into()),
                started.elapsed().as_millis()
            );
            v.map(|b| b.as_ref().clone()).ok_or(ThumbStatus::NoThumbnail)
        }
        Err(e) if e.is_not_found() => {
            STATE.lock().cache.put(key, None);
            Err(ThumbStatus::NoThumbnail)
        }
        Err(e) => {
            tracing::debug!("thumbnail {}: {e}", real.display());
            Err(ThumbStatus::Failed)
        }
    }
}

struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        *FETCHES.0.lock() -= 1;
        FETCHES.1.notify_one();
    }
}

fn fetch_slot(deadline: std::time::Instant) -> Option<Slot> {
    let mut n = FETCHES.0.lock();
    while *n >= MAX_FETCHES {
        if FETCHES.1.wait_until(&mut n, deadline).timed_out() {
            return None;
        }
    }
    *n += 1;
    Some(Slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_snap_up() {
        assert_eq!(snap(16), 96);
        assert_eq!(snap(96), 96);
        assert_eq!(snap(97), 256);
        assert_eq!(snap(256), 256);
        assert_eq!(snap(768), 1024);
        assert_eq!(snap(4096), 1024);
    }

    #[test]
    fn image_names() {
        assert!(is_image_name(Path::new(r"C:\a\IMG_1.JPG")));
        assert!(is_image_name(Path::new(r"C:\a\b.heic")));
        assert!(!is_image_name(Path::new(r"C:\a\b.mp4")));
        assert!(!is_image_name(Path::new(r"C:\a\jpg")));
    }

    #[test]
    fn root_matching_is_by_component() {
        let api = Api::new("http://127.0.0.1:1", "t", None);
        let base = tempfile::tempdir().unwrap();
        let a = base.path().join("Drive");
        let b = base.path().join("Drive 2");
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("sub").join("x.jpg"), b"x").unwrap();
        std::fs::write(b.join("y.jpg"), b"y").unwrap();
        let ra = final_path(&a).unwrap();
        add_root(&ra, api);
        assert!(api_for(&final_path(&a.join("sub").join("x.jpg")).unwrap()).is_some());
        // A sibling folder whose name starts the same is not inside it.
        assert!(api_for(&final_path(&b.join("y.jpg")).unwrap()).is_none());
        // The root itself is not a file in it.
        assert!(api_for(&ra).is_none());
        // Not a placeholder: refused before any request.
        assert_eq!(thumbnail(&a.join("sub").join("x.jpg"), 256), Err(ThumbStatus::NotOurs));
        assert_eq!(thumbnail(Path::new("relative.jpg"), 256), Err(ThumbStatus::NotOurs));
        remove_root(&ra);
        assert!(api_for(&final_path(&a.join("sub").join("x.jpg")).unwrap()).is_none());
    }

    #[test]
    fn cache_is_bounded() {
        let mut c = Cache::default();
        for i in 0..(CACHE_MAX_ENTRIES + 10) {
            c.put(format!("k{i}"), Some(Arc::new(vec![0u8; 10])));
        }
        assert_eq!(c.map.len(), CACHE_MAX_ENTRIES);
        assert!(c.get("k0").is_none(), "oldest evicted");
        assert!(c.get(&format!("k{}", CACHE_MAX_ENTRIES + 9)).is_some());
        let big = CACHE_MAX_BYTES / 8;
        for i in 0..20 {
            c.put(format!("b{i}"), Some(Arc::new(vec![0u8; big])));
        }
        assert!(c.bytes <= CACHE_MAX_BYTES);
        c.put("none".into(), None);
        assert_eq!(c.get("none"), Some(None));
    }
}
