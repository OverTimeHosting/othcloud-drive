//! Sync engine integration tests: REAL Cloud Files sync roots (in temp
//! folders on this NTFS volume) against the in-memory PROTOCOL.md server.
//! Each test registers its own sync root and unregisters it at the end.

mod common;

use common::mock::MockServer;
use othcloud_drive::api::Api;
use othcloud_drive::cf;
use othcloud_drive::engine::{remove_root, Engine, EngineOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fx {
    server: MockServer,
    _sid: String,
    engine: Option<Arc<Engine>>,
    root: PathBuf,
    base: tempfile::TempDir,
    paused: Arc<AtomicBool>,
    /// Always unregistered on drop, even if a test panicked mid-way.
    root_id: String,
}

fn opts() -> EngineOptions {
    EngineOptions {
        poll_interval: Duration::from_secs(3600),
        settle: Duration::from_millis(300),
        delete_grace: Duration::from_millis(800),
        host_name: "TESTPC".into(),
        rescan_interval: Duration::from_secs(3600),
        provider: cf::TEST_PROVIDER.into(),
        cover_interval: Duration::from_secs(3600),
    }
}

fn start(server: MockServer, sid: String) -> Fx {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(std::env::var("TEST_LOG").unwrap_or_else(|_| "othcloud_drive=info".into()))
        .try_init();
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("Drive");
    let paused = Arc::new(AtomicBool::new(false));
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let engine = Engine::start(api, &sid, "Test", &root, "%SystemRoot%\\System32\\imageres.dll,-1043", paused.clone(), opts())
        .expect("engine start");
    let root = engine.root.clone();
    eprintln!("root policies: {:?}", cf::root_policies(&root));
    let root_id = engine.sync_root_id.clone();
    Fx { server, _sid: sid, engine: Some(engine), root, base, paused, root_id }
}

impl Fx {
    fn e(&self) -> &Engine {
        self.engine.as_ref().unwrap()
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
    fn idle(&self) {
        assert!(self.e().wait_idle(Duration::from_secs(20)), "engine didn't go idle");
    }
    /// Let watcher events arrive, then wait for the queue to drain.
    fn settle(&self) {
        std::thread::sleep(Duration::from_millis(700));
        self.idle();
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        if let Some(e) = self.engine.take() {
            e.shutdown(Duration::from_secs(5));
            let _ = remove_root(&e.sync_root_id, &e.root, "Test", self.base.path());
        }
        let _ = cf::unregister_sync_root(&self.root_id);
    }
}

fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    f()
}

fn attrs(p: &Path) -> u32 {
    cf::attributes(p).unwrap_or(0)
}

fn dehydrated(p: &Path) -> bool {
    attrs(p) & cf::ATTR_RECALL_ON_DATA_ACCESS != 0
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2654435761) >> 11) as u8)
        .collect()
}

/// File operations from ANOTHER process (cmd.exe), like a user's app. The
/// Cloud Files platform doesn't send rename/delete notifications for I/O
/// issued by the sync provider's own process, so in-process std::fs calls
/// would never reach the engine's callbacks.
fn cmd(args: &[&str]) -> std::io::Result<()> {
    let out = std::process::Command::new("cmd").args(["/d", "/c"]).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "cmd {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

fn s(p: impl AsRef<Path>) -> String {
    p.as_ref().display().to_string()
}

/// rename / move (fails if the target exists)
fn sh_move(a: impl AsRef<Path>, b: impl AsRef<Path>) -> std::io::Result<()> {
    cmd(&["move", "/-y", &s(a), &s(b)])
}

/// MoveFileEx(MOVEFILE_REPLACE_EXISTING), as editors save
fn sh_move_replace(a: impl AsRef<Path>, b: impl AsRef<Path>) -> std::io::Result<()> {
    cmd(&["move", "/y", &s(a), &s(b)])
}

fn sh_del(a: impl AsRef<Path>) -> std::io::Result<()> {
    cmd(&["del", "/f", "/q", &s(a)])
}

fn sh_rmdir(a: impl AsRef<Path>) -> std::io::Result<()> {
    cmd(&["rmdir", "/s", "/q", &s(a)])
}
#[test]
fn placeholders_hydrate_on_demand_by_range() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let big = pattern(5 * 1024 * 1024 + 321, 7);
    server.put(&sid, "Docs/big.bin", &big);
    server.put(&sid, "hello.txt", b"hello");
    let fx = start(server, sid);

    let bp = fx.p("Docs\\big.bin");
    assert!(bp.exists());
    assert_eq!(std::fs::metadata(&bp).unwrap().len(), big.len() as u64);
    let pi = cf::placeholder_info(&bp).unwrap().expect("is a placeholder");
    assert!(pi.in_sync);
    assert!(dehydrated(&bp), "remote file should start dehydrated: {:#x}", attrs(&bp));
    assert!(attrs(&bp) & cf::ATTR_OFFLINE != 0);
    assert_eq!(fx.server.requests("GET", "/file"), 0, "listing must not download");

    // A small read in the middle fetches only a range.
    let mut f = std::fs::File::open(&bp).unwrap();
    f.seek(SeekFrom::Start(3_000_000)).unwrap();
    let mut buf = vec![0u8; 4096];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(&buf[..], &big[3_000_000..3_004_096]);
    drop(f);
    let ranges: Vec<String> = fx.server.state.lock().unwrap().log.iter()
        .filter(|l| l.1.ends_with("/file")).filter_map(|l| l.2.clone()).collect();
    // Windows asks the provider for the whole file on first access (even
    // with a Partial/Progressive policy); it arrives as ranged GETs of at
    // most 4 MiB, streamed to the platform chunk by chunk.
    let gets = fx.server.requests("GET", "/file");
    assert!(gets >= 2, "{ranges:?}");
    assert_eq!(ranges.len(), gets, "every GET is ranged: {ranges:?}");
    for r in &ranges {
        let (a, b) = r.trim_start_matches("bytes=").split_once('-').unwrap();
        let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
        assert!(b - a < 4 << 20, "{r}");
    }

    // Whole file.
    assert_eq!(std::fs::read(&bp).unwrap(), big);
    assert!(!dehydrated(&bp), "fully hydrated after a full read: {:#x}", attrs(&bp));
    let gets = fx.server.requests("GET", "/file");
    assert_eq!(std::fs::read(&bp).unwrap(), big);
    assert_eq!(fx.server.requests("GET", "/file"), gets, "second read is local");
}

#[test]
fn local_create_modify_rename_move_delete() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let orig = server.put(&sid, "a.txt", b"server a");
    let fx = start(server, sid.clone());

    // New file -> uploaded, then an in-sync placeholder.
    let data = pattern(2 * 1024 * 1024 + 5, 1);
    std::fs::write(fx.p("new.bin"), &data).unwrap();
    fx.settle();
    assert_eq!(fx.server.get(&sid, "new.bin").unwrap(), data);
    let pi = cf::placeholder_info(&fx.p("new.bin")).unwrap().expect("converted");
    assert!(pi.in_sync);

    // New folder tree.
    std::fs::create_dir_all(fx.p("Dir\\Sub")).unwrap();
    std::fs::write(fx.p("Dir\\Sub\\x.txt"), b"x").unwrap();
    fx.settle();
    assert!(fx.server.paths(&sid).contains(&"Dir/Sub/x.txt".to_string()), "{:?}", fx.server.paths(&sid));

    // Modify a remote file: replace keeps the id.
    std::fs::write(fx.p("a.txt"), b"edited locally, longer").unwrap();
    fx.settle();
    assert_eq!(fx.server.get(&sid, "a.txt").unwrap(), b"edited locally, longer");
    assert_eq!(fx.server.file_id(&sid, "a.txt").unwrap(), orig);

    // Rename + move: no re-upload.
    let puts = fx.server.requests("PUT", "/file");
    sh_move(fx.p("a.txt"), fx.p("b.txt")).unwrap();
    sh_move(fx.p("b.txt"), fx.p("Dir\\b.txt")).unwrap();
    sh_move(fx.p("Dir\\Sub"), fx.p("Dir\\Sub2")).unwrap();
    sh_move(fx.p("Dir"), fx.p("Top")).unwrap();
    fx.settle();
    let paths = fx.server.paths(&sid);
    assert!(paths.contains(&"Top/b.txt".to_string()), "{paths:?}");
    assert!(paths.contains(&"Top/Sub2/x.txt".to_string()), "{paths:?}");
    assert_eq!(fx.server.file_id(&sid, "Top/b.txt").unwrap(), orig);
    assert_eq!(fx.server.requests("PUT", "/file"), puts, "a rename must not re-upload");

    // Case-only rename.
    sh_move(fx.p("Top\\b.txt"), fx.p("Top\\B.TXT")).unwrap();
    fx.settle();
    assert!(fx.server.get(&sid, "Top/B.TXT").is_some(), "{:?}", fx.server.paths(&sid));

    // Deletes (after the grace period).
    sh_del(fx.p("new.bin")).unwrap();
    sh_rmdir(fx.p("Top")).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    fx.idle();
    assert!(fx.server.paths(&sid).is_empty(), "{:?}", fx.server.paths(&sid));
}

#[test]
fn office_and_editor_saves_keep_the_file_id() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let doc = server.put(&sid, "Report.docx", b"doc v1");
    let book = server.put(&sid, "Book.xlsx", b"book v1");
    let notes = server.put(&sid, "notes.txt", b"notes v1");
    let fx = start(server, sid.clone());

    // Word: temp write, original -> ~WRL, temp -> original, delete ~WRL.
    std::fs::write(fx.p("~$Report.docx"), b"owner").unwrap();
    std::fs::write(fx.p("~WRD0000.tmp"), b"doc v2, longer").unwrap();
    sh_move(fx.p("Report.docx"), fx.p("~WRL0001.tmp")).unwrap();
    sh_move(fx.p("~WRD0000.tmp"), fx.p("Report.docx")).unwrap();
    sh_del(fx.p("~WRL0001.tmp")).unwrap();
    sh_del(fx.p("~$Report.docx")).unwrap();

    // Excel: extension-less temp.
    std::fs::write(fx.p("5A3C1D00"), b"book v2").unwrap();
    sh_move(fx.p("Book.xlsx"), fx.p("1B2C3D4E.tmp")).unwrap();
    sh_move(fx.p("5A3C1D00"), fx.p("Book.xlsx")).unwrap();
    sh_del(fx.p("1B2C3D4E.tmp")).unwrap();

    // Editor: write sibling, MoveFileEx(REPLACE_EXISTING) over the original.
    std::fs::write(fx.p("notes.txt.new"), b"notes v2").unwrap();
    sh_move_replace(fx.p("notes.txt.new"), fx.p("notes.txt")).unwrap();

    std::thread::sleep(Duration::from_millis(1500));
    fx.idle();
    assert_eq!(fx.server.paths(&sid), vec!["Book.xlsx", "Report.docx", "notes.txt"]);
    assert_eq!(fx.server.get(&sid, "Report.docx").unwrap(), b"doc v2, longer");
    assert_eq!(fx.server.file_id(&sid, "Report.docx").unwrap(), doc);
    assert_eq!(fx.server.get(&sid, "Book.xlsx").unwrap(), b"book v2");
    assert_eq!(fx.server.file_id(&sid, "Book.xlsx").unwrap(), book);
    assert_eq!(fx.server.get(&sid, "notes.txt").unwrap(), b"notes v2");
    assert_eq!(fx.server.file_id(&sid, "notes.txt").unwrap(), notes);
    let log = fx.server.state.lock().unwrap().log.clone();
    assert!(
        !log.iter().any(|(_, p, _)| p.contains(".tmp") || p.contains("~$") || p.contains("5A3C1D00")),
        "temp names reached the server: {log:?}"
    );
    for f in ["Report.docx", "Book.xlsx", "notes.txt"] {
        assert!(cf::placeholder_info(&fx.p(f)).unwrap().unwrap().in_sync, "{f} in sync");
    }
}

#[test]
fn remote_changes_are_applied() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    server.put(&sid, "a.txt", b"one");
    server.put(&sid, "gone.txt", b"bye");
    server.put(&sid, "Folder/inner.txt", b"in");
    let fx = start(server, sid.clone());
    assert_eq!(std::fs::read(fx.p("a.txt")).unwrap(), b"one");
    assert!(wait_until(5, || !dehydrated(&fx.p("a.txt"))), "hydrated after a full read");

    fx.server.put(&sid, "a.txt", b"two, longer");
    fx.server.put(&sid, "New/b.txt", b"bee");
    {
        let mut s = fx.server.state.lock().unwrap();
        let d = s.drive_mut(&sid).unwrap();
        let g = d.file_by_path("gone.txt").unwrap().id.clone();
        d.files.remove(&g);
        let fid = d.folder_by_path("Folder").unwrap().unwrap();
        d.folders.get_mut(&fid).unwrap().name = "Renamed".into();
        d.version += 1;
    }
    assert!(fx.e().refresh().unwrap());
    assert!(dehydrated(&fx.p("a.txt")), "changed file is dehydrated again");
    assert_eq!(std::fs::read(fx.p("a.txt")).unwrap(), b"two, longer");
    assert_eq!(std::fs::read(fx.p("New\\b.txt")).unwrap(), b"bee");
    assert!(!fx.p("gone.txt").exists());
    assert!(!fx.p("Folder").exists());
    assert_eq!(std::fs::read(fx.p("Renamed\\inner.txt")).unwrap(), b"in");
    // Our own apply produced no server writes.
    fx.settle();
    assert_eq!(fx.server.requests("PUT", "/file"), 0);
    assert_eq!(fx.server.requests("POST", "/move"), 0);
    assert_eq!(fx.server.requests("DELETE", "/item"), 0);
    assert!(!fx.e().refresh().unwrap(), "304");
}

#[test]
fn pin_hydrates_and_unpin_frees_space() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let data = pattern(1024 * 1024 + 3, 9);
    server.put(&sid, "photo.jpg", &data);
    let fx = start(server, sid);
    let p = fx.p("photo.jpg");
    assert!(dehydrated(&p));
    cf::set_pin(&p, true).unwrap();
    assert!(wait_until(15, || !dehydrated(&p)), "pinned file not hydrated: {:#x}", attrs(&p));
    assert!(attrs(&p) & cf::ATTR_PINNED != 0);
    cf::set_pin(&p, false).unwrap();
    assert!(wait_until(15, || dehydrated(&p)), "unpinned file not freed: {:#x}", attrs(&p));
    assert_eq!(std::fs::read(&p).unwrap(), data, "still readable (re-fetched)");
}

#[test]
fn conflicting_new_file_becomes_a_conflicted_copy() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let fx = start(server, sid.clone());
    fx.paused.store(true, Ordering::SeqCst);
    std::fs::write(fx.p("x.txt"), b"mine, different size").unwrap();
    fx.server.put(&sid, "x.txt", b"theirs");
    std::thread::sleep(Duration::from_millis(500));
    let _ = fx.e().refresh(); // placeholder can't be created: a local file is there
    fx.paused.store(false, Ordering::SeqCst);
    fx.settle();
    let paths = fx.server.paths(&sid);
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert_eq!(fx.server.get(&sid, "x.txt").unwrap(), b"theirs");
    let copy = paths.iter().find(|p| p.contains("conflicted copy TESTPC")).unwrap();
    assert_eq!(fx.server.get(&sid, copy).unwrap(), b"mine, different size");
}

#[test]
fn offline_cached_reads_work_and_uploads_queue() {
    let server = MockServer::start(0);
    let port = server.addr.port();
    let sid = server.add_drive("Test", None);
    server.put(&sid, "cached.txt", b"cached!");
    server.put(&sid, "uncached.txt", b"never read");
    let state = server.state.clone();
    let mut fx = start(server, sid.clone());
    assert_eq!(std::fs::read(fx.p("cached.txt")).unwrap(), b"cached!");
    fx.server.stop();

    assert_eq!(std::fs::read(fx.p("cached.txt")).unwrap(), b"cached!");
    assert!(std::fs::read(fx.p("uncached.txt")).is_err(), "uncached read must fail offline");
    std::fs::write(fx.p("offline.txt"), b"written offline").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!fx.e().status().online);
    assert!(!fx.e().is_idle());

    let server2 = MockServer::start_with(port, state);
    fx.e().expedite();
    assert!(fx.e().wait_idle(Duration::from_secs(40)), "queue didn't drain");
    assert_eq!(server2.get(&sid, "offline.txt").unwrap(), b"written offline");
    fx.server = server2;
}

#[test]
fn remove_local_moves_unsynced_files_aside_and_unregisters() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    server.put(&sid, "synced.txt", b"s");
    let mut fx = start(server, sid);
    fx.paused.store(true, Ordering::SeqCst);
    std::fs::write(fx.p("unsynced.txt"), b"not uploaded yet").unwrap();
    let id = fx.e().sync_root_id.clone();
    let listed = cf::sync_roots(cf::TEST_PROVIDER);
    assert!(
        listed.iter().any(|(i, p)| *i == id && p == &fx.root),
        "registered: {listed:?}"
    );
    assert_eq!(cf::root_policies(&fx.root).unwrap().0, 1, "progressive hydration");
    let e = fx.engine.take().unwrap();
    e.shutdown(Duration::from_millis(100));
    let aside = e.remove_local(fx.base.path()).unwrap().expect("something moved aside");
    assert_eq!(std::fs::read(aside.join("unsynced.txt")).unwrap(), b"not uploaded yet");
    assert!(!fx.root.exists(), "sync root folder removed");
    assert!(!cf::sync_roots(cf::TEST_PROVIDER).iter().any(|(i, _)| *i == id), "unregistered");
}

fn sync_root_icon(id: &str) -> Option<String> {
    winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\{id}"
        ))
        .ok()?
        .get_value("IconResource")
        .ok()
}

#[test]
fn drive_icon_follows_cover_and_colour() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let fx = start(server, sid.clone());
    let icons = fx.base.path().join("data");
    let set_cover = |bytes: &[u8]| {
        fx.server.state.lock().unwrap().drive_mut(&sid).unwrap().cover =
            (!bytes.is_empty()).then(|| bytes.to_vec());
    };
    let set_color = |c: Option<&str>| {
        fx.server.state.lock().unwrap().drive_mut(&sid).unwrap().color = c.map(String::from);
    };
    let png = |rgb: [u8; 3]| {
        let img = image::RgbaImage::from_pixel(64, 48, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    let decode = |p: &Path| {
        let d = ico::IconDir::read(std::fs::File::open(p).unwrap()).unwrap();
        d.entries().iter().find(|e| e.width() == 64).unwrap().decode().unwrap()
    };
    let px = |img: &ico::IconImage, x: u32, y: u32| {
        let i = ((y * img.width() + x) * 4) as usize;
        [img.rgba_data()[i], img.rgba_data()[i + 1], img.rgba_data()[i + 2]]
    };

    // No cover: branded default icon, registered for the sync root.
    fx.e().attach_icons(None, &icons);
    let a = fx.e().icon_path().expect("default icon");
    assert!(a.exists());
    assert_eq!(sync_root_icon(&fx.root_id).unwrap(), format!("{},0", a.display()));
    assert!(!fx.e().check_cover().unwrap(), "nothing changed");

    // Colour change -> new file, old deleted, tile takes the colour.
    set_color(Some("#f43f5e"));
    assert!(fx.e().check_cover().unwrap());
    let b = fx.e().icon_path().unwrap();
    assert_ne!(a, b);
    assert!(!a.exists(), "old icon deleted");
    let top = px(&decode(&b), 32, 6);
    assert!(top[0] > 200 && top[1] < 140, "rose tile: {top:?}");
    assert_eq!(sync_root_icon(&fx.root_id).unwrap(), format!("{},0", b.display()));

    // Cover set -> cover icon (green shows through), sync root updated.
    set_cover(&png([20, 200, 40]));
    assert!(fx.e().check_cover().unwrap());
    let c = fx.e().icon_path().unwrap();
    assert!(!b.exists());
    let p = px(&decode(&c), 20, 20);
    assert!(p[1] > 150 && p[0] < 80, "cover pixels: {p:?}");
    assert_eq!(sync_root_icon(&fx.root_id).unwrap(), format!("{},0", c.display()));
    // Unchanged -> 304, nothing rewritten.
    assert!(!fx.e().check_cover().unwrap());
    assert_eq!(fx.e().icon_path().unwrap(), c);

    // New cover -> new file name (Explorer caches by path).
    set_cover(&png([30, 60, 220]));
    assert!(fx.e().check_cover().unwrap());
    let d = fx.e().icon_path().unwrap();
    assert_ne!(c, d);
    assert!(!c.exists());

    // Cover cleared -> back to the colour tile.
    set_cover(&[]);
    assert!(fx.e().check_cover().unwrap());
    let e = fx.e().icon_path().unwrap();
    assert!(!d.exists());
    let top = px(&decode(&e), 32, 6);
    assert!(top[0] > 200 && top[1] < 140, "rose tile again: {top:?}");
}
