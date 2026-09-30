//! Local content cache for one drive: `<cache>/<storageId>/<key>.bin` holds
//! the bytes (a sparse file, filled chunk by chunk as ranges are read) and
//! `<key>.json` records which 1 MiB chunks are present and which server
//! version (`tag` = size|updatedAt) they belong to.
//!
//! A file being written is always made COMPLETE first (every chunk fetched),
//! so the cache file is then the authoritative local copy that gets uploaded.

use crate::api::{Api, ApiError};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const CHUNK: u64 = 1 << 20;
/// Largest single ranged GET.
const MAX_RUN_CHUNKS: u64 = 16;
/// Read-ahead: a miss fetches at least this many chunks.
const MIN_RUN_CHUNKS: u64 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Meta {
    tag: String,
    size: u64,
    complete: bool,
    chunks: Vec<u64>,
}

impl Meta {
    fn has(&self, c: u64) -> bool {
        self.chunks
            .get((c / 64) as usize)
            .map(|w| w & (1 << (c % 64)) != 0)
            .unwrap_or(false)
    }
    fn set(&mut self, c: u64) {
        let i = (c / 64) as usize;
        if self.chunks.len() <= i {
            self.chunks.resize(i + 1, 0);
        }
        self.chunks[i] |= 1 << (c % 64);
    }
    fn nchunks(&self) -> u64 {
        self.size.div_ceil(CHUNK)
    }
    fn all_present(&self) -> bool {
        (0..self.nchunks()).all(|c| self.has(c))
    }
}

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    Api(ApiError),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Io(e) => write!(f, "cache I/O: {e}"),
            CacheError::Api(e) => write!(f, "{e}"),
        }
    }
}
impl From<io::Error> for CacheError {
    fn from(e: io::Error) -> Self {
        CacheError::Io(e)
    }
}
impl From<ApiError> for CacheError {
    fn from(e: ApiError) -> Self {
        CacheError::Api(e)
    }
}

pub struct Entry {
    data_path: PathBuf,
    meta_path: PathBuf,
    meta: Meta,
    file: Option<File>,
    meta_dirty: bool,
}

fn set_sparse(f: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows::Win32::System::IO::DeviceIoControl;
    let mut ret = 0u32;
    // Best effort: without it a far read just costs zero-filled disk space.
    unsafe {
        let _ = DeviceIoControl(
            HANDLE(f.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut ret),
            None,
        );
    }
}

impl Entry {
    fn open(dir: &Path, key: &str) -> Entry {
        let data_path = dir.join(format!("{key}.bin"));
        let meta_path = dir.join(format!("{key}.json"));
        let mut meta: Meta = std::fs::read(&meta_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if !data_path.exists() {
            meta = Meta::default();
        }
        Entry {
            data_path,
            meta_path,
            meta,
            file: None,
            meta_dirty: false,
        }
    }

    fn file(&mut self) -> io::Result<&File> {
        if self.file.is_none() {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&self.data_path)?;
            set_sparse(&f);
            self.file = Some(f);
        }
        Ok(self.file.as_ref().unwrap())
    }

    pub fn data_path(&self) -> &Path {
        &self.data_path
    }
    pub fn size(&self) -> u64 {
        self.meta.size
    }
    pub fn tag(&self) -> &str {
        &self.meta.tag
    }
    pub fn is_complete(&self) -> bool {
        self.meta.complete
    }

    /// Make sure the entry describes server version `tag` of `size` bytes;
    /// if it held another version, throw its bytes away.
    pub fn prepare(&mut self, tag: &str, size: u64) -> io::Result<()> {
        if self.meta.tag == tag && self.meta.size == size {
            return Ok(());
        }
        self.reset(tag, size)
    }

    fn reset(&mut self, tag: &str, size: u64) -> io::Result<()> {
        let f = self.file()?;
        f.set_len(0)?;
        f.set_len(size)?;
        self.meta = Meta {
            tag: tag.to_string(),
            size,
            complete: size == 0,
            chunks: Vec::new(),
        };
        self.meta_dirty = true;
        self.save_meta();
        Ok(())
    }

    /// A brand-new local file: complete and empty.
    pub fn init_new(&mut self) -> io::Result<()> {
        self.reset("", 0)
    }

    /// Fetch whatever part of [off, off+len) isn't cached yet.
    pub fn ensure(
        &mut self,
        api: &Api,
        file_id: &str,
        off: u64,
        len: u64,
    ) -> Result<(), CacheError> {
        if self.meta.complete || len == 0 || off >= self.meta.size {
            return Ok(());
        }
        let end = (off + len).min(self.meta.size);
        let first = off / CHUNK;
        let last = (end - 1) / CHUNK;
        let total = self.meta.nchunks();
        let mut c = first;
        while c <= last {
            if self.meta.has(c) {
                c += 1;
                continue;
            }
            // A run of missing chunks starting at c.
            let mut run_end = c;
            while run_end + 1 < total
                && !self.meta.has(run_end + 1)
                && (run_end < last || run_end + 1 - c < MIN_RUN_CHUNKS)
                && run_end + 1 - c < MAX_RUN_CHUNKS
            {
                run_end += 1;
            }
            let start = c * CHUNK;
            let stop = ((run_end + 1) * CHUNK).min(self.meta.size);
            let bytes = api.read_range(file_id, start, stop - start)?;
            if (bytes.len() as u64) < stop - start {
                return Err(CacheError::Api(ApiError::Decode(format!(
                    "short read: wanted {} bytes at {start}, got {}",
                    stop - start,
                    bytes.len()
                ))));
            }
            self.file()?.seek_write(&bytes, start)?;
            for x in c..=run_end {
                self.meta.set(x);
            }
            self.meta_dirty = true;
            c = run_end + 1;
        }
        if self.meta.all_present() {
            self.meta.complete = true;
        }
        Ok(())
    }

    pub fn ensure_complete(&mut self, api: &Api, file_id: &str) -> Result<(), CacheError> {
        let size = self.meta.size;
        self.ensure(api, file_id, 0, size)?;
        self.save_meta();
        Ok(())
    }

    pub fn read_at(&mut self, buf: &mut [u8], off: u64) -> io::Result<usize> {
        if off >= self.meta.size {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.meta.size - off) as usize;
        let f = self.file()?;
        let mut done = 0;
        while done < want {
            let n = f.seek_read(&mut buf[done..want], off + done as u64)?;
            if n == 0 {
                // Sparse hole past the data file's end: zeros.
                buf[done..want].fill(0);
                done = want;
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// Only valid on a complete entry (the caller makes it so first).
    pub fn write_at(&mut self, buf: &[u8], off: u64) -> io::Result<usize> {
        debug_assert!(self.meta.complete);
        let f = self.file()?;
        let mut done = 0;
        while done < buf.len() {
            done += f.seek_write(&buf[done..], off + done as u64)?;
        }
        let end = off + buf.len() as u64;
        if end > self.meta.size {
            self.meta.size = end;
        }
        self.meta_dirty = true;
        Ok(done)
    }

    pub fn set_len(&mut self, n: u64) -> io::Result<()> {
        debug_assert!(self.meta.complete);
        self.file()?.set_len(n)?;
        self.meta.size = n;
        self.meta_dirty = true;
        Ok(())
    }

    /// After an upload of this content: the server version is now `tag`.
    pub fn set_tag(&mut self, tag: &str) {
        self.meta.tag = tag.to_string();
        self.meta_dirty = true;
        self.save_meta();
    }

    pub fn flush(&mut self) {
        if let Some(f) = &self.file {
            let _ = f.sync_data();
        }
        self.save_meta();
    }

    pub fn save_meta(&mut self) {
        if !self.meta_dirty {
            return;
        }
        if let Ok(b) = serde_json::to_vec(&self.meta) {
            let tmp = self.meta_path.with_extension("json.tmp");
            if std::fs::write(&tmp, b).is_ok() && std::fs::rename(&tmp, &self.meta_path).is_ok() {
                self.meta_dirty = false;
            }
        }
    }

    /// Release the OS handle (keeps the bytes).
    pub fn close_file(&mut self) {
        self.save_meta();
        self.file = None;
    }
}

pub type EntryRef = Arc<Mutex<Entry>>;

pub struct Cache {
    dir: PathBuf,
    entries: Mutex<HashMap<String, EntryRef>>,
}

impl Cache {
    pub fn new(dir: PathBuf) -> io::Result<Cache> {
        std::fs::create_dir_all(&dir)?;
        Ok(Cache {
            dir,
            entries: Mutex::new(HashMap::new()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entry(&self, key: &str) -> EntryRef {
        let mut m = self.entries.lock();
        m.entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Entry::open(&self.dir, key))))
            .clone()
    }

    /// Forget and delete a key's bytes.
    pub fn remove(&self, key: &str) {
        let e = self.entries.lock().remove(key);
        if let Some(e) = e {
            e.lock().file = None;
        }
        let _ = std::fs::remove_file(self.dir.join(format!("{key}.bin")));
        let _ = std::fs::remove_file(self.dir.join(format!("{key}.json")));
    }

    /// Server content changed: drop cached bytes (the next read re-fetches).
    pub fn invalidate(&self, key: &str) {
        let e = self.entries.lock().get(key).cloned();
        match e {
            Some(e) => {
                let mut g = e.lock();
                g.meta.tag = String::from("stale");
                g.meta.complete = false;
                g.meta.chunks.clear();
                g.meta_dirty = true;
                g.save_meta();
            }
            None => {
                let _ = std::fs::remove_file(self.dir.join(format!("{key}.json")));
            }
        }
    }

    /// Close every open cache handle (on unmount).
    pub fn close_all(&self) {
        for e in self.entries.lock().values() {
            e.lock().close_file();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap() {
        let mut m = Meta {
            size: CHUNK * 3 + 1,
            ..Default::default()
        };
        assert_eq!(m.nchunks(), 4);
        m.set(0);
        m.set(3);
        assert!(m.has(0) && m.has(3) && !m.has(1));
        assert!(!m.all_present());
        m.set(1);
        m.set(2);
        assert!(m.all_present());
        m.set(130);
        assert!(m.has(130));
    }

    #[test]
    fn new_entry_write_read() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path().to_path_buf()).unwrap();
        let e = c.entry("k");
        let mut g = e.lock();
        g.init_new().unwrap();
        g.write_at(b"hello world", 0).unwrap();
        g.write_at(b"W", 6).unwrap();
        let mut buf = [0u8; 32];
        let n = g.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"hello World");
        g.set_len(5).unwrap();
        let n = g.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"hello");
        g.close_file();
        drop(g);
        // Persisted across a reopen.
        let c2 = Cache::new(dir.path().to_path_buf()).unwrap();
        let e2 = c2.entry("k");
        let mut g2 = e2.lock();
        assert!(g2.is_complete());
        assert_eq!(g2.size(), 5);
        let n = g2.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }
}
