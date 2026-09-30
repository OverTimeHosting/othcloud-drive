//! Photo backup against the mock server: uploads new/changed images into
//! "Photo backup/<PC>/…", never deletes, remembers what it uploaded,
//! handles name collisions, and the watcher picks up new files.

mod common;

use common::mock::MockServer;
use othcloud_drive::api::Api;
use othcloud_drive::backup::{sync_once, BackupHandle, BackupJob, BackupState};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn job(dir: &std::path::Path, state: &std::path::Path, sid: &str) -> BackupJob {
    BackupJob {
        folder: dir.to_path_buf(),
        storage_id: sid.to_string(),
        remote: vec!["Photo backup".into(), "TESTPC".into()],
        state_path: state.to_path_buf(),
        settle: Duration::ZERO,
    }
}

#[test]
fn backup_uploads_new_and_changed_images_only() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Photos", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let pics = tempfile::tempdir().unwrap();
    let st = tempfile::tempdir().unwrap();
    let state_path = st.path().join("backup.json");
    std::fs::create_dir_all(pics.path().join("Holiday/Day 1")).unwrap();
    std::fs::write(pics.path().join("a.jpg"), b"jpeg-a").unwrap();
    std::fs::write(pics.path().join("Holiday/Day 1/b.PNG"), b"png-b").unwrap();
    std::fs::write(pics.path().join("notes.txt"), b"not an image").unwrap();

    let j = job(pics.path(), &state_path, &sid);
    let mut state = BackupState::load(&state_path);
    let r = sync_once(&api, &j, &mut state).unwrap();
    assert_eq!(r.uploaded, 2);
    assert_eq!(
        server.paths(&sid),
        vec![
            "Photo backup/",
            "Photo backup/TESTPC/",
            "Photo backup/TESTPC/Holiday/",
            "Photo backup/TESTPC/Holiday/Day 1/",
            "Photo backup/TESTPC/Holiday/Day 1/b.PNG",
            "Photo backup/TESTPC/a.jpg",
        ]
    );
    assert_eq!(server.get(&sid, "Photo backup/TESTPC/a.jpg").unwrap(), b"jpeg-a");

    // Nothing new: no uploads, no tree fetch.
    let puts = server.requests("PUT", "/file");
    let r = sync_once(&api, &j, &mut state).unwrap();
    assert_eq!(r.uploaded, 0);
    assert_eq!(server.requests("PUT", "/file"), puts);

    // State persisted: a fresh load (app restart) also uploads nothing.
    let mut state2 = BackupState::load(&state_path);
    assert_eq!(state2.files.len(), 2);
    assert_eq!(sync_once(&api, &j, &mut state2).unwrap().uploaded, 0);

    // Changed image: replaced in place (same id).
    let id = server.file_id(&sid, "Photo backup/TESTPC/a.jpg").unwrap();
    std::thread::sleep(Duration::from_millis(1100)); // mtime granularity (secs)
    std::fs::write(pics.path().join("a.jpg"), b"jpeg-a edited").unwrap();
    let r = sync_once(&api, &j, &mut state2).unwrap();
    assert_eq!(r.uploaded, 1);
    assert_eq!(server.file_id(&sid, "Photo backup/TESTPC/a.jpg").unwrap(), id);
    assert_eq!(server.get(&sid, "Photo backup/TESTPC/a.jpg").unwrap(), b"jpeg-a edited");

    // Local delete never deletes remotely.
    std::fs::remove_file(pics.path().join("Holiday/Day 1/b.PNG")).unwrap();
    sync_once(&api, &j, &mut state2).unwrap();
    assert!(server.get(&sid, "Photo backup/TESTPC/Holiday/Day 1/b.PNG").is_some());
    assert_eq!(server.requests("DELETE", "/item"), 0);
}

#[test]
fn backup_name_collisions_and_existing_copies() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Photos", None);
    // Someone else's different file with the same name, and an identical
    // copy from an earlier install.
    server.put(&sid, "Photo backup/TESTPC/x.jpg", b"different content");
    server.put(&sid, "Photo backup/TESTPC/same.jpg", b"12345");
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let pics = tempfile::tempdir().unwrap();
    let st = tempfile::tempdir().unwrap();
    std::fs::write(pics.path().join("x.jpg"), b"mine").unwrap();
    std::fs::write(pics.path().join("same.jpg"), b"12345").unwrap();
    let j = job(pics.path(), &st.path().join("s.json"), &sid);
    let mut state = BackupState::default();
    let r = sync_once(&api, &j, &mut state).unwrap();
    assert_eq!(r.uploaded, 1);
    assert_eq!(r.already, 1);
    assert_eq!(server.get(&sid, "Photo backup/TESTPC/x.jpg").unwrap(), b"different content");
    assert_eq!(server.get(&sid, "Photo backup/TESTPC/x (1).jpg").unwrap(), b"mine");
}

#[test]
fn backup_network_failure_is_retryable() {
    let mut server = MockServer::start(0);
    let sid = server.add_drive("Photos", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let port = server.addr.port();
    let state_arc = server.state.clone();
    server.stop();
    let pics = tempfile::tempdir().unwrap();
    let st = tempfile::tempdir().unwrap();
    std::fs::write(pics.path().join("a.jpg"), b"a").unwrap();
    let j = job(pics.path(), &st.path().join("s.json"), &sid);
    let mut state = BackupState::default();
    let e = sync_once(&api, &j, &mut state).unwrap_err();
    assert!(e.is_network(), "{e}");
    assert!(state.files.is_empty());
    let server = MockServer::start_with(port, state_arc);
    assert_eq!(sync_once(&api, &j, &mut state).unwrap().uploaded, 1);
    assert!(server.get(&sid, "Photo backup/TESTPC/a.jpg").is_some());
}

#[test]
fn backup_watcher_uploads_new_files() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Photos", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let pics = tempfile::tempdir().unwrap();
    let st = tempfile::tempdir().unwrap();
    let mut j = job(pics.path(), &st.path().join("s.json"), &sid);
    j.settle = Duration::from_millis(200);
    let mut h = BackupHandle::start(api, j, Arc::new(AtomicBool::new(false))).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::create_dir_all(pics.path().join("Camera")).unwrap();
    std::fs::write(pics.path().join("Camera/IMG_0001.HEIC"), b"heic bytes").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline
        && server.get(&sid, "Photo backup/TESTPC/Camera/IMG_0001.HEIC").is_none()
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        server.get(&sid, "Photo backup/TESTPC/Camera/IMG_0001.HEIC").unwrap(),
        b"heic bytes"
    );
    h.stop();
    assert_eq!(h.status.lock().total_backed_up, 1);
}
