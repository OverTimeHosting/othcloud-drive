//! Integration tests: the API client and the drive logic (tree mapping,
//! on-demand cache, upload on close, rename/move/delete mapping, Office save
//! patterns, offline behaviour) against the in-memory PROTOCOL.md server.

mod common;

use common::mock::MockServer;
use othcloud_drive::api::{Api, ItemType};
use othcloud_drive::drive::{status, DriveCore, DriveOptions};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

fn api(server: &MockServer) -> Api {
    Api::new(&server.origin(), "test-token", Some("org_1".into()))
}

struct Fixture {
    server: MockServer,
    sid: String,
    core: Arc<DriveCore>,
    _dir: tempfile::TempDir,
    _paused: Arc<AtomicBool>,
}

fn fixture_with(server: MockServer, sid: String) -> Fixture {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("othcloud_drive=debug")
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let paused = Arc::new(AtomicBool::new(false));
    let core = DriveCore::new(
        api(&server),
        &sid,
        "Test",
        dir.path().join(&sid),
        paused.clone(),
        DriveOptions {
            poll_interval: Duration::from_secs(3600),
            upload_delay: Duration::from_millis(150),
            host_name: "TESTPC".into(),
        },
    )
    .unwrap();
    core.refresh().unwrap();
    core.start_workers();
    Fixture {
        server,
        sid,
        core,
        _dir: dir,
        _paused: paused,
    }
}

fn fixture() -> Fixture {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    fixture_with(server, sid)
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.core.stop();
    }
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2654435761) >> 11) as u8)
        .collect()
}

fn read_all(core: &DriveCore, path: &str) -> Vec<u8> {
    let info = core.open(path).unwrap();
    let mut out = vec![0u8; info.size as usize];
    let mut off = 0;
    while off < out.len() {
        let end = (off + 65536).min(out.len());
        let n = core.read(info.id, &mut out[off..end], off as u64).unwrap();
        assert!(n > 0);
        off += n;
    }
    core.close(info.id);
    out
}

fn write_file(core: &DriveCore, path: &str, data: &[u8]) {
    let info = match core.create(path, false) {
        Ok(i) => i,
        Err(e) if e.0 == status::OBJECT_NAME_COLLISION => {
            let i = core.open(path).unwrap();
            core.overwrite(i.id).unwrap();
            i
        }
        Err(e) => panic!("create {path}: {e}"),
    };
    for (i, chunk) in data.chunks(64 * 1024).enumerate() {
        core.write(info.id, chunk, (i * 64 * 1024) as u64, false, false)
            .unwrap();
    }
    core.flush(info.id);
    core.close(info.id);
}

// ------------------------------------------------------------------ API

#[test]
fn api_client_round_trip() {
    let server = MockServer::start(0);
    let sid = server.add_drive("D", Some(1000));
    let a = api(&server);

    assert_eq!(a.orgs().unwrap()[0].id, "org_1");
    let drives = a.drives().unwrap();
    assert_eq!(drives.len(), 1);
    assert_eq!(drives[0].quota_bytes, Some(1000));

    // tree + ETag
    let t = a.tree(&sid, None).unwrap().unwrap();
    assert!(t.files.is_empty());
    assert!(a.tree(&sid, Some(&t.etag)).unwrap().is_none(), "expected 304");

    // folder + file + 409s
    let f = a.create_folder(&sid, None, "Docs").unwrap();
    assert_eq!(a.create_folder(&sid, None, "Docs").unwrap_err().status(), Some(409));
    let up = a
        .upload_new_bytes(&sid, Some(&f.folder_id), "a.txt", b"0123456789".to_vec())
        .unwrap();
    let fid = up.file_id.clone().unwrap();
    let e = a
        .upload_new_bytes(&sid, Some(&f.folder_id), "a.txt", b"x".to_vec())
        .unwrap_err();
    assert!(e.is_name_taken(), "{e}");
    // quota: 10 used of 1000
    let e = a
        .upload_new_bytes(&sid, None, "big", vec![0u8; 995])
        .unwrap_err();
    assert!(e.is_quota(), "{e}");

    // Range
    assert_eq!(a.read_range(&fid, 2, 3).unwrap(), b"234");
    assert_eq!(a.read_range(&fid, 8, 100).unwrap(), b"89");
    assert_eq!(a.read_range(&fid, 50, 10).unwrap(), b"");

    // move / rename / cycle
    let sub = a.create_folder(&sid, Some(&f.folder_id), "Sub").unwrap();
    a.move_item(ItemType::File, &fid, Some("b.txt"), Some(None)).unwrap();
    assert_eq!(server.get(&sid, "b.txt").unwrap(), b"0123456789");
    let e = a
        .move_item(ItemType::Folder, &f.folder_id, None, Some(Some(&sub.folder_id)))
        .unwrap_err();
    assert_eq!(e.status(), Some(400));

    // delete (idempotent)
    a.delete_item(ItemType::File, &fid).unwrap();
    a.delete_item(ItemType::File, &fid).unwrap();
    assert!(server.get(&sid, "b.txt").is_none());

    // auth failures
    let bad = Api::new(&server.origin(), "nope", Some("org_1".into()));
    assert!(bad.drives().unwrap_err().is_auth());
    let other_org = Api::new(&server.origin(), "test-token", Some("org_x".into()));
    assert_eq!(other_org.drives().unwrap_err().status(), Some(403));
}

#[test]
fn token_exchange() {
    let server = MockServer::start(0);
    server
        .state
        .lock()
        .unwrap()
        .pair_codes
        .insert("abc".into(), "tok123".into());
    let r = Api::exchange_code(&server.origin(), "abc", "OTHCloud Drive (TEST)").unwrap();
    assert_eq!(r.token, "tok123");
    // single use
    assert!(Api::exchange_code(&server.origin(), "abc", "x").is_err());
}

// ------------------------------------------------------------- reading

#[test]
fn reads_on_demand_with_ranges_and_caches() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let data = pattern(5 * 1024 * 1024 + 123, 7);
    server.put(&sid, "Docs/big.bin", &data);
    let fx = fixture_with(server, sid);

    let info = fx.core.open("\\DOCS\\BIG.BIN").unwrap(); // case-insensitive
    assert_eq!(info.size, data.len() as u64);
    let mut buf = vec![0u8; 4096];
    let off = 3 * 1024 * 1024 + 17;
    let n = fx.core.read(info.id, &mut buf, off as u64).unwrap();
    assert_eq!(n, 4096);
    assert_eq!(&buf[..], &data[off..off + 4096]);
    let ranged = fx.server.requests("GET", "/file");
    assert_eq!(ranged, 1, "one ranged GET for a small read");
    {
        let s = fx.server.state.lock().unwrap();
        let r = s.log.iter().rev().find(|l| l.1.ends_with("/file")).unwrap();
        assert!(r.2.as_deref().unwrap().starts_with("bytes=3145728-"), "{:?}", r.2);
    }
    // Same range again: served from cache.
    fx.core.read(info.id, &mut buf, off as u64).unwrap();
    assert_eq!(fx.server.requests("GET", "/file"), 1);
    fx.core.close(info.id);

    // Whole file.
    assert_eq!(read_all(&fx.core, "\\Docs\\big.bin"), data);
    let before = fx.server.requests("GET", "/file");
    assert_eq!(read_all(&fx.core, "\\Docs\\big.bin"), data);
    assert_eq!(fx.server.requests("GET", "/file"), before, "second full read is cached");
}

#[test]
fn tree_poll_uses_etag_and_picks_up_server_changes() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    server.put(&sid, "a.txt", b"one");
    let fx = fixture_with(server, sid.clone());
    assert_eq!(read_all(&fx.core, "\\a.txt"), b"one");

    assert!(!fx.core.refresh().unwrap(), "304 -> no change");
    fx.server.put(&sid, "a.txt", b"two, longer");
    fx.server.put(&sid, "New/b.txt", b"bee");
    assert!(fx.core.refresh().unwrap());
    let changes = fx.core.take_changes();
    assert!(changes.len() >= 3, "{changes:?}");
    assert_eq!(read_all(&fx.core, "\\a.txt"), b"two, longer");
    assert_eq!(read_all(&fx.core, "\\new\\B.TXT"), b"bee");

    // Server-side delete disappears locally.
    {
        let mut s = fx.server.state.lock().unwrap();
        let d = s.drive_mut(&sid).unwrap();
        let id = d.file_by_path("a.txt").unwrap().id.clone();
        d.files.remove(&id);
        d.version += 1;
    }
    fx.core.refresh().unwrap();
    assert_eq!(
        fx.core.lookup("\\a.txt").unwrap_err().0,
        status::OBJECT_NAME_NOT_FOUND
    );
}

// ------------------------------------------------------------- writing

#[test]
fn new_file_uploads_on_close() {
    let fx = fixture();
    let data = pattern(2 * 1024 * 1024 + 5, 1);
    write_file(&fx.core, "\\new.bin", &data);
    // Not uploaded synchronously...
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.get(&fx.sid, "new.bin").unwrap(), data);
    // ...and exactly one PUT.
    assert_eq!(fx.server.requests("PUT", "/file"), 1);
    // Reading it back needs no download (the cache is the local copy).
    let gets = fx.server.requests("GET", "/file");
    assert_eq!(read_all(&fx.core, "\\new.bin"), data);
    assert_eq!(fx.server.requests("GET", "/file"), gets);

    // Overwrite with shorter content: replace keeps the id.
    let id = fx.server.file_id(&fx.sid, "new.bin").unwrap();
    write_file(&fx.core, "\\new.bin", b"short");
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.get(&fx.sid, "new.bin").unwrap(), b"short");
    assert_eq!(fx.server.file_id(&fx.sid, "new.bin").unwrap(), id);

    // Empty file.
    let i = fx.core.create("\\empty.txt", false).unwrap();
    fx.core.flush(i.id);
    fx.core.close(i.id);
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.get(&fx.sid, "empty.txt").unwrap(), b"");
}

#[test]
fn partial_write_to_remote_file_fetches_rest_first() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let data = pattern(3 * 1024 * 1024, 3);
    server.put(&sid, "f.bin", &data);
    let fx = fixture_with(server, sid);
    let info = fx.core.open("\\f.bin").unwrap();
    fx.core.write(info.id, b"PATCH", 1_000_000, false, false).unwrap();
    // Append via write_to_eof.
    fx.core.write(info.id, b"TAIL", 0, true, false).unwrap();
    fx.core.flush(info.id);
    fx.core.close(info.id);
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    let mut expect = data.clone();
    expect[1_000_000..1_000_005].copy_from_slice(b"PATCH");
    expect.extend_from_slice(b"TAIL");
    assert_eq!(fx.server.get(&fx.sid, "f.bin").unwrap(), expect);

    // set_size shrink
    let info = fx.core.open("\\f.bin").unwrap();
    fx.core.set_size(info.id, 10).unwrap();
    fx.core.flush(info.id);
    fx.core.close(info.id);
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.get(&fx.sid, "f.bin").unwrap(), &expect[..10]);
}

// ---------------------------------------------- namespace operations

#[test]
fn mkdir_rename_move_delete_map_to_api() {
    let fx = fixture();
    let d = fx.core.create("\\Folder", true).unwrap();
    fx.core.close(d.id);
    assert_eq!(fx.server.paths(&fx.sid), vec!["Folder/"]);
    let s = fx.core.create("\\Folder\\Sub", true).unwrap();
    fx.core.close(s.id);
    write_file(&fx.core, "\\Folder\\Sub\\x.txt", b"x");
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(
        fx.server.paths(&fx.sid),
        vec!["Folder/", "Folder/Sub/", "Folder/Sub/x.txt"]
    );

    // Duplicate name
    assert_eq!(
        fx.core.create("\\folder", true).unwrap_err().0,
        status::OBJECT_NAME_COLLISION
    );

    // Rename file, move file to root, rename folder, move folder.
    let x = fx.core.open("\\Folder\\Sub\\x.txt").unwrap();
    fx.core.rename(x.id, "\\Folder\\Sub\\y.txt", false).unwrap();
    assert!(fx.server.get(&fx.sid, "Folder/Sub/y.txt").is_some());
    fx.core.rename(x.id, "\\y.txt", false).unwrap();
    fx.core.close(x.id);
    assert!(fx.server.get(&fx.sid, "y.txt").is_some());
    let sub = fx.core.open("\\Folder\\Sub").unwrap();
    fx.core.rename(sub.id, "\\Folder\\Renamed", false).unwrap();
    fx.core.rename(sub.id, "\\Renamed", false).unwrap();
    fx.core.close(sub.id);
    assert_eq!(fx.server.paths(&fx.sid), vec!["Folder/", "Renamed/", "y.txt"]);

    // Case-only rename
    let y = fx.core.open("\\y.txt").unwrap();
    fx.core.rename(y.id, "\\Y.TXT", false).unwrap();
    fx.core.close(y.id);
    assert_eq!(fx.server.paths(&fx.sid), vec!["Folder/", "Renamed/", "Y.TXT"]);

    // Moving a folder into itself is refused locally.
    let f = fx.core.open("\\Folder").unwrap();
    let r = fx.core.open("\\Renamed").unwrap();
    fx.core.rename(r.id, "\\Folder\\Renamed", false).unwrap();
    assert_eq!(
        fx.core.rename(f.id, "\\Folder\\Renamed\\F", false).unwrap_err().0,
        status::INVALID_PARAMETER
    );

    // Non-empty folder can't be deleted; empty one can; recursive via children.
    assert_eq!(fx.core.can_delete(f.id).unwrap_err().0, status::DIRECTORY_NOT_EMPTY);
    fx.core.can_delete(r.id).unwrap();
    fx.core.delete(r.id).unwrap();
    fx.core.close(r.id);
    fx.core.can_delete(f.id).unwrap();
    fx.core.delete(f.id).unwrap();
    fx.core.close(f.id);
    let y = fx.core.open("\\Y.TXT").unwrap();
    fx.core.delete(y.id).unwrap();
    fx.core.close(y.id);
    assert!(fx.server.paths(&fx.sid).is_empty());
}

#[test]
fn rename_over_existing_replaces_and_keeps_id() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let orig_id = server.put(&sid, "notes.txt", b"old contents");
    let fx = fixture_with(server, sid);

    // Editor pattern: write "notes.txt.new" (not a temp name), rename over.
    write_file(&fx.core, "\\notes.txt.new", b"new contents");
    let n = fx.core.open("\\notes.txt.new").unwrap();
    // Without replace: collision.
    assert_eq!(
        fx.core.rename(n.id, "\\notes.txt", false).unwrap_err().0,
        status::OBJECT_NAME_COLLISION
    );
    fx.core.rename(n.id, "\\notes.txt", true).unwrap();
    fx.core.close(n.id);
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.paths(&fx.sid), vec!["notes.txt"]);
    assert_eq!(fx.server.get(&fx.sid, "notes.txt").unwrap(), b"new contents");
    assert_eq!(fx.server.file_id(&fx.sid, "notes.txt").unwrap(), orig_id);
    assert_eq!(read_all(&fx.core, "\\notes.txt"), b"new contents");

    // Both already on the server: target deleted, source moved.
    fx.server.put(&fx.sid, "a.txt", b"A");
    fx.server.put(&fx.sid, "b.txt", b"B");
    fx.core.refresh().unwrap();
    let a = fx.core.open("\\a.txt").unwrap();
    fx.core.rename(a.id, "\\b.txt", true).unwrap();
    fx.core.close(a.id);
    assert_eq!(fx.server.paths(&fx.sid), vec!["b.txt", "notes.txt"]);
    assert_eq!(fx.server.get(&fx.sid, "b.txt").unwrap(), b"A");
}

#[test]
fn word_save_dance_keeps_id_and_never_uploads_temp_files() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let orig_id = server.put(&sid, "Report.docx", b"version 1");
    let fx = fixture_with(server, sid);

    // Owner lock file.
    write_file(&fx.core, "\\~$Report.docx", b"lock");
    // 1. Word writes the new version to a temp file.
    write_file(&fx.core, "\\~WRD0000.tmp", b"version 2 is longer");
    // 2. Renames the original away to a temp name.
    let o = fx.core.open("\\Report.docx").unwrap();
    fx.core.rename(o.id, "\\~WRL0001.tmp", false).unwrap();
    fx.core.close(o.id);
    // 3. Renames the temp file to the original name.
    let t = fx.core.open("\\~WRD0000.tmp").unwrap();
    fx.core.rename(t.id, "\\Report.docx", false).unwrap();
    fx.core.close(t.id);
    // 4. Deletes the old version and its lock file.
    let old = fx.core.open("\\~WRL0001.tmp").unwrap();
    fx.core.delete(old.id).unwrap();
    fx.core.close(old.id);
    let lock = fx.core.open("\\~$Report.docx").unwrap();
    fx.core.delete(lock.id).unwrap();
    fx.core.close(lock.id);

    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.paths(&fx.sid), vec!["Report.docx"]);
    assert_eq!(fx.server.get(&fx.sid, "Report.docx").unwrap(), b"version 2 is longer");
    assert_eq!(fx.server.file_id(&fx.sid, "Report.docx").unwrap(), orig_id);
    // No temp name ever reached the server.
    let s = fx.server.state.lock().unwrap();
    assert!(
        !s.log.iter().any(|(_, p, _)| p.contains(".tmp") || p.contains("~$")),
        "temp names leaked: {:?}",
        s.log
    );
    assert_eq!(
        s.log.iter().filter(|(m, p, _)| m == "POST" && p.ends_with("/move")).count(),
        0
    );
    drop(s);
    // Survives a poll without the shadow being resurrected.
    fx.core.refresh().unwrap();
    assert_eq!(read_all(&fx.core, "\\Report.docx"), b"version 2 is longer");
    assert!(fx.core.lookup("\\~WRL0001.tmp").is_err());
}

#[test]
fn excel_style_save_with_extensionless_temp() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let orig_id = server.put(&sid, "Book.xlsx", b"v1");
    let fx = fixture_with(server, sid);
    write_file(&fx.core, "\\5A3C1D00", b"v2"); // not a temp-looking name
    let o = fx.core.open("\\Book.xlsx").unwrap();
    fx.core.rename(o.id, "\\1B2C3D4E.tmp", false).unwrap();
    fx.core.close(o.id);
    let t = fx.core.open("\\5A3C1D00").unwrap();
    fx.core.rename(t.id, "\\Book.xlsx", false).unwrap();
    fx.core.close(t.id);
    let old = fx.core.open("\\1B2C3D4E.tmp").unwrap();
    fx.core.delete(old.id).unwrap();
    fx.core.close(old.id);
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.paths(&fx.sid), vec!["Book.xlsx"]);
    assert_eq!(fx.server.get(&fx.sid, "Book.xlsx").unwrap(), b"v2");
    assert_eq!(fx.server.file_id(&fx.sid, "Book.xlsx").unwrap(), orig_id);
}

#[test]
fn renaming_to_temp_and_back_restores_server_name() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    server.put(&sid, "keep.txt", b"k");
    let fx = fixture_with(server, sid);
    let k = fx.core.open("\\keep.txt").unwrap();
    fx.core.rename(k.id, "\\keep.tmp", false).unwrap();
    assert_eq!(fx.server.paths(&fx.sid), vec!["keep.txt"]); // untouched
    fx.core.rename(k.id, "\\final.txt", false).unwrap();
    fx.core.close(k.id);
    assert_eq!(fx.server.paths(&fx.sid), vec!["final.txt"]);
    // Deleting a shadow really deletes (user deleted it).
    let f = fx.core.open("\\final.txt").unwrap();
    fx.core.rename(f.id, "\\x.tmp", false).unwrap();
    fx.core.delete(f.id).unwrap();
    fx.core.close(f.id);
    assert!(fx.server.paths(&fx.sid).is_empty());
}

#[test]
fn name_conflict_on_upload_makes_conflicted_copy() {
    let fx = fixture();
    // Server gains "x.txt" after our last poll; we create x.txt locally.
    fx.server.put(&fx.sid, "x.txt", b"theirs");
    write_file(&fx.core, "\\x.txt", b"mine");
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    let paths = fx.server.paths(&fx.sid);
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert_eq!(fx.server.get(&fx.sid, "x.txt").unwrap(), b"theirs");
    let copy = paths.iter().find(|p| p.contains("conflicted copy TESTPC")).unwrap();
    assert_eq!(fx.server.get(&fx.sid, copy).unwrap(), b"mine");
}

#[test]
fn terse_server_responses_fall_back_to_tree() {
    let fx = fixture();
    fx.server.state.lock().unwrap().terse = true;
    write_file(&fx.core, "\\t.txt", b"terse");
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    let id = fx.server.file_id(&fx.sid, "t.txt").unwrap();
    // Second write must REPLACE (we learned the id), not create a copy.
    write_file(&fx.core, "\\t.txt", b"terse 2");
    assert!(fx.core.wait_idle(Duration::from_secs(10)));
    assert_eq!(fx.server.paths(&fx.sid), vec!["t.txt"]);
    assert_eq!(fx.server.file_id(&fx.sid, "t.txt").unwrap(), id);
    assert_eq!(fx.server.get(&fx.sid, "t.txt").unwrap(), b"terse 2");
}

#[test]
fn quota_exceeded_maps_to_disk_full_and_retries() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", Some(10));
    let fx = fixture_with(server, sid);
    write_file(&fx.core, "\\big.txt", b"more than ten bytes");
    std::thread::sleep(Duration::from_millis(800));
    // Still queued (backoff), and the status reports the error.
    assert!(!fx.core.idle());
    let st = fx.core.status();
    assert!(st.last_error.unwrap().contains("quota"));
    assert!(fx.server.paths(&fx.sid).is_empty());
}

// ------------------------------------------------------------- offline

#[test]
fn offline_reads_cached_fails_uncached_and_queues_uploads() {
    let server = MockServer::start(0);
    let port = server.addr.port();
    let sid = server.add_drive("Test", None);
    server.put(&sid, "cached.txt", b"cached!");
    server.put(&sid, "uncached.txt", b"never read");
    server.put(&sid, "gone.txt", b"delete me");
    let state = server.state.clone();
    let fx = fixture_with(server, sid.clone());
    assert_eq!(read_all(&fx.core, "\\cached.txt"), b"cached!");

    // Network down.
    let mut fx = fx;
    fx.server.stop();
    let core = fx.core.clone();

    assert_eq!(read_all(&core, "\\cached.txt"), b"cached!");
    let u = core.open("\\uncached.txt").unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(core.read(u.id, &mut buf, 0).unwrap_err().0, status::NETWORK_UNREACHABLE);
    core.close(u.id);

    // Writes and deletes queue.
    write_file(&core, "\\offline.txt", b"written offline");
    let g = core.open("\\gone.txt").unwrap();
    core.delete(g.id).unwrap();
    core.close(g.id);
    // Folder created offline is created later.
    let d = core.create("\\OfflineDir", true).unwrap();
    core.close(d.id);
    write_file(&core, "\\OfflineDir\\inner.txt", b"inner");
    std::thread::sleep(Duration::from_millis(600));
    assert!(!core.status().online);
    assert!(!core.idle());

    // Network back (same port, same state).
    let server2 = MockServer::start_with(port, state);
    core.expedite();
    // Backoff may be a few seconds.
    let ok = core.wait_idle(Duration::from_secs(40));
    assert!(ok, "queue did not drain");
    let paths = server2.paths(&sid);
    assert_eq!(
        paths,
        vec!["OfflineDir/", "OfflineDir/inner.txt", "cached.txt", "offline.txt", "uncached.txt"]
    );
    assert_eq!(server2.get(&sid, "offline.txt").unwrap(), b"written offline");
    core.stop();
}

#[test]
fn rename_keeps_cached_bytes_valid() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Test", None);
    let data = pattern(2 * 1024 * 1024, 5);
    server.put(&sid, "a.bin", &data);
    let fx = fixture_with(server, sid);
    assert_eq!(read_all(&fx.core, r"\a.bin"), data);
    let gets = fx.server.requests("GET", "/file");
    let a = fx.core.open(r"\a.bin").unwrap();
    fx.core.rename(a.id, r"\b.bin", false).unwrap();
    fx.core.close(a.id);
    // The server bumped updatedAt on the move; the next poll must not
    // treat that as new content.
    fx.core.refresh().unwrap();
    assert!(
        !fx.core.take_changes().iter().any(|c| matches!(c, othcloud_drive::ns::Change::Modified { .. })),
        "rename looked like a content change"
    );
    assert_eq!(read_all(&fx.core, r"\b.bin"), data);
    assert_eq!(fx.server.requests("GET", "/file"), gets, "re-downloaded after rename");
}
