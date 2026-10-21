//! Folder backup against the mock panel (PROTOCOL.md "Folder backup"):
//! device → profile → uploads → replace keeping a version → deletion mark
//! → reappear; the photo backup's migration; an older panel; quota; files
//! another program holds open.

mod common;

use common::mock::MockServer;
use othcloud_drive::api::Api;
use othcloud_drive::backup::{BackupService, BackupState, Ctx, Folder, FolderJob, FolderStatus, ServiceConfig, ServiceOptions};
use othcloud_drive::config::{BackupFolder, FolderKind, PHOTOS_KEY};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn folder(key: &str, label: &str, path: &Path, kind: FolderKind) -> BackupFolder {
    BackupFolder {
        folder_key: key.into(),
        label: label.into(),
        local_path: path.to_path_buf(),
        kind,
        enabled: true,
        excludes: vec![],
        remote_folder: None,
    }
}

fn fast() -> ServiceOptions {
    ServiceOptions {
        settle: Duration::from_millis(300),
        rescan_every: Duration::from_secs(3600),
        status_every: Duration::from_millis(100),
        uploaders: 3,
        api_retry: Duration::from_secs(3600),
    }
}

fn service_config(sid: &str, data: &Path, folders: Vec<BackupFolder>) -> ServiceConfig {
    ServiceConfig {
        storage_id: sid.into(),
        device_key: "devkey-123".into(),
        device_name: "TESTPC".into(),
        app_version: "9.9.9".into(),
        os_version: "Windows 11 (10.0.26200)".into(),
        folders,
        paused: false,
        data_dir: data.to_path_buf(),
        skip_inside: vec![],
    }
}

/// One pass of a folder, outside the service.
fn pass(api: &Api, sid: &str, f: &mut Folder, dev: Option<&str>) -> othcloud_drive::backup::Report {
    let cancel = || false;
    let mut progress = |_: &FolderStatus| {};
    let mut ctx = Ctx {
        api,
        storage_id: sid,
        device_id: dev,
        settle: Duration::ZERO,
        uploaders: 3,
        cancel: &cancel,
        on_progress: &mut progress,
    };
    f.run(&mut ctx, None).unwrap()
}

fn mtime_tick() {
    // The state keeps whole-second mtimes.
    std::thread::sleep(Duration::from_millis(1100));
}

#[test]
fn folder_backup_device_profile_uploads_versions_and_deletion_marks() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Backup drive", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let docs = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let p = docs.path();
    std::fs::create_dir_all(p.join("sub")).unwrap();
    std::fs::create_dir_all(p.join("node_modules/pkg")).unwrap();
    std::fs::create_dir_all(p.join(".git")).unwrap();
    std::fs::write(p.join("a.txt"), b"alpha").unwrap();
    std::fs::write(p.join("sub/b.txt"), b"bravo").unwrap();
    std::fs::write(p.join("sub/skip.iso"), b"excluded").unwrap();
    std::fs::write(p.join("node_modules/pkg/i.js"), b"x").unwrap();
    std::fs::write(p.join(".git/HEAD"), b"x").unwrap();
    std::fs::write(p.join("desktop.ini"), b"x").unwrap();
    std::fs::write(p.join("~$draft.docx"), b"x").unwrap();
    // Old enough to be settled at the first scan.
    mtime_tick();

    let mut f = folder("k1", "Docs", p, FolderKind::Folder);
    f.excludes = vec!["*.iso".into()];
    let mut svc = BackupService::start(
        api.clone(),
        service_config(&sid, data.path(), vec![f.clone()]),
        Arc::new(AtomicBool::new(false)),
        fast(),
    );
    let a = "Backups/TESTPC/Docs/a.txt";
    let b = "Backups/TESTPC/Docs/sub/b.txt";
    wait_for("first uploads", 20, || server.get(&sid, a).is_some() && server.get(&sid, b).is_some());

    {
        let st = server.state.lock().unwrap();
        // Device: the key, once; the profile with the folder.
        assert_eq!(st.backup.devices.len(), 1);
        let (key, _, body) = &st.backup.devices[0];
        assert_eq!(key, "devkey-123");
        assert_eq!(body["platform"], "windows");
        assert_eq!(body["name"], "TESTPC");
        assert_eq!(body["appVersion"], "9.9.9");
        let put = &st.backup.profile_puts[0];
        assert_eq!(put["storageId"], sid.as_str());
        assert_eq!(put["paused"], false);
        assert_eq!(put["folders"][0]["folderKey"], "k1");
        assert_eq!(put["folders"][0]["kind"], "folder");
        assert_eq!(put["folders"][0]["excludes"][0], "*.iso");
        assert_eq!(put["folders"][0]["localPath"], p.display().to_string());
    }
    // Nothing skipped got out.
    let all = server.paths(&sid);
    assert!(
        all.iter().all(|x| !x.contains("node_modules") && !x.contains(".git") && !x.contains("desktop.ini") && !x.contains("~$") && !x.contains(".iso")),
        "{all:?}"
    );
    let a_id = server.file_id(&sid, a).unwrap();
    let b_id = server.file_id(&sid, b).unwrap();

    // Changed: replaced in place, the old contents kept as a version.
    mtime_tick();
    std::fs::write(p.join("a.txt"), b"alpha two").unwrap();
    wait_for("replace", 20, || server.get(&sid, a).as_deref() == Some(b"alpha two".as_slice()));
    assert_eq!(server.file_id(&sid, a).unwrap(), a_id, "same file, same id");
    assert_eq!(server.state.lock().unwrap().backup.versions[&a_id], vec![b"alpha".to_vec()]);

    // Deleted on the PC: marked, never deleted in the drive.
    std::fs::remove_file(p.join("sub/b.txt")).unwrap();
    wait_for("deletion mark", 20, || server.state.lock().unwrap().backup.deleted.get(&b_id) == Some(&true));
    assert!(server.get(&sid, b).is_some());
    assert_eq!(server.requests("DELETE", "/item"), 0);

    // Back again: unmarked (and, being newer, uploaded as a new version).
    mtime_tick();
    std::fs::write(p.join("sub/b.txt"), b"bravo again").unwrap();
    wait_for("unmark", 20, || server.state.lock().unwrap().backup.deleted.get(&b_id) == Some(&false));
    wait_for("reupload", 20, || server.get(&sid, b).as_deref() == Some(b"bravo again".as_slice()));
    assert_eq!(server.file_id(&sid, b).unwrap(), b_id);

    // Status reported for the folder.
    wait_for("status", 20, || {
        let st = server.state.lock().unwrap();
        st.backup.statuses.last().is_some_and(|s| {
            let f = &s["folders"][0];
            f["folderKey"] == "k1" && f["files"] == 2 && f["pending"] == 0 && f["lastBackupAt"].is_string()
        })
    });
    let local = svc.status.lock().folders["k1"].clone();
    assert_eq!(local.files, 2);
    assert!(local.skipped >= 4, "{local:?}");

    // Paused: a new file waits; the profile says paused.
    let paused_cfg = ServiceConfig { paused: true, ..service_config(&sid, data.path(), vec![f.clone()]) };
    svc.reconfigure(paused_cfg);
    wait_for("paused profile", 20, || {
        server.state.lock().unwrap().backup.profile_puts.last().is_some_and(|p| p["paused"] == true)
    });
    std::fs::write(p.join("c.txt"), b"charlie").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(server.get(&sid, "Backups/TESTPC/Docs/c.txt").is_none());
    svc.reconfigure(service_config(&sid, data.path(), vec![f.clone()]));
    wait_for("upload after resume", 20, || server.get(&sid, "Backups/TESTPC/Docs/c.txt").is_some());

    // Turned off: watched no more.
    let off = BackupFolder { enabled: false, ..f.clone() };
    svc.reconfigure(service_config(&sid, data.path(), vec![off]));
    wait_for("off profile", 20, || {
        server.state.lock().unwrap().backup.profile_puts.last().is_some_and(|p| p["folders"][0]["enabled"] == false)
    });
    std::fs::write(p.join("d.txt"), b"delta").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(server.get(&sid, "Backups/TESTPC/Docs/d.txt").is_none());
    svc.stop(Duration::from_secs(10));

    // The state survives: a new service on the same data uploads nothing.
    let puts = server.requests("PUT", "/file");
    let mut svc2 = BackupService::start(
        api,
        service_config(&sid, data.path(), vec![BackupFolder { folder_key: "k1".into(), ..folder("k1", "Docs", p, FolderKind::Folder) }]),
        Arc::new(AtomicBool::new(false)),
        fast(),
    );
    wait_for("d.txt after restart", 20, || server.get(&sid, "Backups/TESTPC/Docs/d.txt").is_some());
    std::thread::sleep(Duration::from_millis(800));
    // Only d.txt was new (and the *.iso is no longer excluded in this config).
    assert_eq!(server.requests("PUT", "/file") - puts, 2, "{:?}", server.paths(&sid));
    assert_eq!(server.state.lock().unwrap().backup.devices.len(), 1, "same key, same device");
    svc2.stop(Duration::from_secs(10));
}

#[test]
fn photos_folder_keeps_the_photo_backup_and_works_on_an_older_panel() {
    let server = MockServer::start(0);
    server.state.lock().unwrap().backup.disabled = true;
    let sid = server.add_drive("Photos", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let pics = tempfile::tempdir().unwrap();
    let docs = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(pics.path().join("Holiday")).unwrap();
    std::fs::write(pics.path().join("Holiday/a.JPG"), b"jpeg").unwrap();
    std::fs::write(pics.path().join("notes.txt"), b"not a picture").unwrap();
    std::fs::write(docs.path().join("doc.txt"), b"doc").unwrap();
    mtime_tick();
    let mut svc = BackupService::start(
        api,
        service_config(
            &sid,
            data.path(),
            vec![
                folder(PHOTOS_KEY, "Photos", pics.path(), FolderKind::Photos),
                folder("k2", "Docs", docs.path(), FolderKind::Folder),
            ],
        ),
        Arc::new(AtomicBool::new(false)),
        fast(),
    );
    wait_for("photo upload", 20, || server.get(&sid, "Photo backup/TESTPC/Holiday/a.JPG").is_some());
    wait_for("docs waiting for the panel", 20, || {
        svc.status.lock().folders.get("k2").is_some_and(|f| f.phase == "panel" && f.pending == 1)
    });
    assert_eq!(svc.status.lock().panel, "unavailable");
    assert!(server.paths(&sid).iter().all(|p| !p.starts_with("Backups") && !p.contains("notes.txt")));
    // The photos folder's state is the photo backup's own file.
    let st = BackupState::load(&data.path().join(format!("backup-{sid}.json")));
    assert!(st.files.contains_key("Holiday/a.JPG"));
    svc.stop(Duration::from_secs(10));
}

#[test]
fn migrated_photo_backup_uploads_nothing_twice() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Photos", None);
    let pics = tempfile::tempdir().unwrap();
    let cfg_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(pics.path().join("Screenshots")).unwrap();
    std::fs::write(pics.path().join("Screenshots/s1.png"), b"png-1").unwrap();
    std::fs::write(pics.path().join("old.jpg"), b"jpg-old").unwrap();
    // What the photo backup left: the files in the drive and its state.
    let id1 = server.put(&sid, "Photo backup/TESTPC/Screenshots/s1.png", b"png-1");
    let id2 = server.put(&sid, "Photo backup/TESTPC/old.jpg", b"jpg-old");
    let mt = |rel: &str| {
        std::fs::metadata(pics.path().join(rel))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let state = format!(
        r#"{{"files":{{"Screenshots/s1.png":{{"size":5,"mtime":{},"file_id":"{id1}"}},"old.jpg":{{"size":7,"mtime":{},"file_id":"{id2}"}}}}}}"#,
        mt("Screenshots/s1.png"),
        mt("old.jpg")
    );
    std::fs::write(data.path().join(format!("backup-{sid}.json")), state).unwrap();
    // The photo backup era's settings (the drive kept off this PC: no sync root in a test).
    let config = serde_json::json!({
        "origin": server.origin(),
        "org_id": "org_1",
        "drives": { sid.clone(): false },
        "backup": { "enabled": true, "folder": pics.path(), "storage_id": sid, "remote_folder": "Photo backup/TESTPC" },
    });
    std::fs::write(cfg_dir.path().join("config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
    mtime_tick();

    let mut app = othcloud_drive::app::App::load_in(cfg_dir.path().to_path_buf(), data.path().to_path_buf());
    app.backup_options = fast();
    app.sign_in_with_token(othcloud_drive::config::Credentials {
        origin: server.origin(),
        token: "test-token".into(),
        user_name: None,
        user_email: None,
    })
    .unwrap();
    assert!(app.mounts.is_empty());
    let f = app.cfg.backup.folder(PHOTOS_KEY).cloned().expect("migrated");
    assert_eq!(f.kind, FolderKind::Photos);
    assert!(f.enabled);
    assert_eq!(f.local_path, pics.path());
    // And it was saved.
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(cfg_dir.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["backup"]["migrated"], true);
    assert!(saved["backup"]["device_key"].as_str().is_some_and(|k| k.len() == 32));

    let status = app.backup.as_ref().unwrap().status.clone();
    wait_for("first pass", 20, || status.lock().folders.get(PHOTOS_KEY).is_some_and(|f| f.last_run_at.is_some() && f.phase == "idle"));
    assert_eq!(server.requests("PUT", "/file"), 0, "nothing uploaded again");
    assert_eq!(status.lock().folders[PHOTOS_KEY].files, 2);

    // New pictures still go where they always went.
    std::fs::write(pics.path().join("Screenshots/s2.png"), b"png-2").unwrap();
    wait_for("new upload", 20, || server.get(&sid, "Photo backup/TESTPC/Screenshots/s2.png").is_some());
    assert_eq!(server.requests("PUT", "/file"), 1);
    // The photos folder told the panel which folder it uses, and a changed
    // picture keeps a version there.
    let photos_id = server.mkdirs(&sid, "Photo backup/TESTPC").unwrap();
    wait_for("profile", 20, || {
        server.state.lock().unwrap().backup.profile_puts.last().is_some_and(|p| p["folders"][0]["remoteFolderId"] == photos_id.as_str())
    });
    mtime_tick();
    std::fs::write(pics.path().join("old.jpg"), b"jpg-new").unwrap();
    wait_for("replace", 20, || server.get(&sid, "Photo backup/TESTPC/old.jpg").as_deref() == Some(b"jpg-new".as_slice()));
    assert_eq!(server.state.lock().unwrap().backup.versions[&id2], vec![b"jpg-old".to_vec()]);

    // Paths that can't be backed up.
    // (A temp folder is in AppData, refused for that alone.)
    assert!(app.backup_check_path(&pics.path().join("Screenshots"), None).is_err());
    assert!(app.backup_check_path(Path::new(r"C:\Windows\Temp"), None).is_err());
    assert!(app.backup_check_path(&othcloud_drive::app::roots_base().join("Any drive"), None).is_err());
    app.shutdown();
}

#[test]
fn quota_stops_the_folder_and_locked_files_are_retried_then_reported() {
    use std::os::windows::fs::OpenOptionsExt;
    let server = MockServer::start(0);
    let sid = server.add_drive("Small", Some(10));
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let d = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("big.bin"), vec![0u8; 64]).unwrap();
    std::fs::write(d.path().join("big2.bin"), vec![0u8; 64]).unwrap();
    let remote = server.mkdirs(&sid, "Backups/TESTPC/Docs");
    let mut job = FolderJob::new(&folder("k", "Docs", d.path(), FolderKind::Folder), &sid, "TESTPC", data.path(), vec![]);
    job.remote_id = remote.clone();
    let mut f = Folder::new(job);
    let r = pass(&api, &sid, &mut f, None);
    assert!(r.quota, "{r:?}");
    assert_eq!(r.uploaded, 0);
    assert_eq!(f.status.phase, "full");
    assert!(f.status.last_error.as_deref().unwrap().contains("full"));
    // Blocked for a while: the next pass doesn't even try.
    let puts = server.requests("PUT", "/file");
    let r = pass(&api, &sid, &mut f, None);
    assert!(r.quota);
    assert_eq!(server.requests("PUT", "/file"), puts);

    // A file another program holds open.
    let sid2 = server.add_drive("Big", None);
    let remote2 = server.mkdirs(&sid2, "Backups/TESTPC/Docs");
    let d2 = tempfile::tempdir().unwrap();
    let path = d2.path().join("open.docx");
    std::fs::write(&path, b"busy").unwrap();
    std::fs::write(d2.path().join("free.txt"), b"free").unwrap();
    let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
    let mut job = FolderJob::new(&folder("k", "Docs", d2.path(), FolderKind::Folder), &sid2, "TESTPC", data.path(), vec![]);
    job.remote_id = remote2;
    let mut f = Folder::new(job);
    let r1 = pass(&api, &sid2, &mut f, None);
    assert_eq!((r1.uploaded, r1.pending, r1.failed), (1, 1, 0), "{r1:?}");
    let r2 = pass(&api, &sid2, &mut f, None);
    assert_eq!((r2.pending, r2.failed), (1, 0));
    let r3 = pass(&api, &sid2, &mut f, None);
    assert_eq!((r3.pending, r3.failed), (0, 1), "reported after a few tries");
    assert!(f.status.problems.iter().any(|p| p.contains("open.docx") && p.contains("in use")), "{:?}", f.status.problems);
    drop(lock);
    let r4 = pass(&api, &sid2, &mut f, None);
    assert_eq!(r4.uploaded, 1);
    assert!(server.get(&sid2, "Backups/TESTPC/Docs/open.docx").is_some());
}

#[test]
fn name_collisions_in_photos_and_in_folders() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Photos", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let data = tempfile::tempdir().unwrap();
    // Photos (the photo backup's rules): someone else's file keeps its
    // name, an identical copy is taken as ours.
    server.put(&sid, "Photo backup/TESTPC/x.jpg", b"different content");
    server.put(&sid, "Photo backup/TESTPC/same.jpg", b"12345");
    let pics = tempfile::tempdir().unwrap();
    std::fs::write(pics.path().join("x.jpg"), b"mine").unwrap();
    std::fs::write(pics.path().join("same.jpg"), b"12345").unwrap();
    let job = FolderJob::new(&folder(PHOTOS_KEY, "Photos", pics.path(), FolderKind::Photos), &sid, "TESTPC", data.path(), vec![]);
    let mut f = Folder::new(job);
    let r = pass(&api, &sid, &mut f, None);
    assert_eq!((r.uploaded, r.already), (1, 1));
    assert_eq!(server.get(&sid, "Photo backup/TESTPC/x.jpg").unwrap(), b"different content");
    assert_eq!(server.get(&sid, "Photo backup/TESTPC/x (1).jpg").unwrap(), b"mine");

    // A folder backup's own remote folder is made to match the PC, keeping
    // what was there as a version (a lost state file, a reinstall).
    let docs = tempfile::tempdir().unwrap();
    std::fs::write(docs.path().join("r.txt"), b"new text").unwrap();
    // The panel's profile makes the folder and allows versions under it.
    let dev = api
        .backup_device(&othcloud_drive::api::DeviceRequest {
            device_key: "k".into(),
            name: "TESTPC".into(),
            platform: "windows".into(),
            os_version: "w".into(),
            app_version: "1".into(),
        })
        .unwrap();
    let prof = api
        .backup_put_profile(&othcloud_drive::api::ProfileRequest {
            device_id: dev.clone(),
            storage_id: sid.clone(),
            paused: false,
            folders: vec![othcloud_drive::api::ProfileFolderRequest {
                folder_key: "d".into(),
                label: "Docs".into(),
                local_path: docs.path().display().to_string(),
                kind: "folder".into(),
                enabled: true,
                excludes: vec![],
                remote_folder_id: None,
            }],
        })
        .unwrap();
    let rid = prof.folders[0].remote_folder_id.clone().unwrap();
    assert_eq!(api.backup_profile(&dev).unwrap().unwrap().folders[0].remote_folder_id.as_deref(), Some(rid.as_str()));
    let old_id = server.put(&sid, "Backups/TESTPC/Docs/r.txt", b"old");
    let mut job = FolderJob::new(&folder("d", "Docs", docs.path(), FolderKind::Folder), &sid, "TESTPC", data.path(), vec![]);
    job.remote_id = Some(rid);
    let mut f = Folder::new(job);
    let r = pass(&api, &sid, &mut f, Some(&dev));
    assert_eq!(r.uploaded, 1);
    assert_eq!(server.file_id(&sid, "Backups/TESTPC/Docs/r.txt").unwrap(), old_id);
    assert_eq!(server.get(&sid, "Backups/TESTPC/Docs/r.txt").unwrap(), b"new text");
    assert_eq!(server.state.lock().unwrap().backup.versions[&old_id], vec![b"old".to_vec()]);
}

#[test]
fn a_backup_folder_deleted_in_the_drive_is_made_again_by_the_panel() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Backup drive", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let docs = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(docs.path().join("a.txt"), b"a").unwrap();
    mtime_tick();
    let mut svc = BackupService::start(
        api,
        service_config(&sid, data.path(), vec![folder("k1", "Docs", docs.path(), FolderKind::Folder)]),
        Arc::new(AtomicBool::new(false)),
        fast(),
    );
    wait_for("upload", 20, || server.get(&sid, "Backups/TESTPC/Docs/a.txt").is_some());
    let puts = server.state.lock().unwrap().backup.profile_puts.len();
    assert!(server.rmdir(&sid, "Backups/TESTPC/Docs"));
    std::fs::write(docs.path().join("b.txt"), b"b").unwrap();
    wait_for("new folder + upload", 20, || server.get(&sid, "Backups/TESTPC/Docs/b.txt").is_some());
    assert_eq!(server.state.lock().unwrap().backup.profile_puts.len(), puts + 1, "profile sent again");
    svc.stop(Duration::from_secs(10));
}

#[test]
fn a_refused_version_is_reported_never_overwritten() {
    let server = MockServer::start(0);
    let sid = server.add_drive("Backup drive", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let docs = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(docs.path().join("r.txt"), b"one").unwrap();
    // A folder no profile names: the panel refuses to keep versions there.
    let remote = server.mkdirs(&sid, "Not a backup");
    let mut job = FolderJob::new(&folder("d", "Docs", docs.path(), FolderKind::Folder), &sid, "TESTPC", data.path(), vec![]);
    job.remote_id = remote;
    let mut f = Folder::new(job);
    assert_eq!(pass(&api, &sid, &mut f, None).uploaded, 1);
    mtime_tick();
    std::fs::write(docs.path().join("r.txt"), b"two").unwrap();
    let r = pass(&api, &sid, &mut f, None);
    assert_eq!((r.uploaded, r.failed), (0, 1), "{r:?}");
    assert_eq!(server.get(&sid, "Not a backup/r.txt").unwrap(), b"one", "not overwritten");
    assert!(f.status.problems.iter().any(|p| p.contains("not_backup_file")), "{:?}", f.status.problems);
}

/// What File Explorer shows (shellstatus.rs) follows the passes: a new file
/// is backed up after its upload, a file another program holds open waits,
/// then is a problem, then is backed up once it is free; the folder shows
/// the worst of it.
#[test]
fn explorer_marks_follow_the_backup() {
    use othcloud_drive::shellstatus::{self, Mark};
    use std::os::windows::fs::OpenOptionsExt;
    std::env::set_var("OTHCLOUD_BACKUP_BOARD", format!(r"Local\othcloud-backup-status-itest-{}", std::process::id()));
    std::env::set_var("OTHCLOUD_DRIVE_THUMBS_PIPE", format!(r"\.\pipe\othcloud-drive-thumbs-itest-{}", std::process::id()));
    shellstatus::set_enabled(true);
    let server = MockServer::start(0);
    let sid = server.add_drive("Big", None);
    let api = Api::new(&server.origin(), "test-token", Some("org_1".into()));
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("Marked");
    std::fs::create_dir_all(root.join("Sub")).unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(root.join("free.txt"), b"free").unwrap();
    std::fs::write(root.join("Sub").join("open.docx"), b"busy").unwrap();
    std::fs::write(root.join("~$scratch.docx"), b"x").unwrap();
    let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(root.join("Sub").join("open.docx")).unwrap();
    let remote = server.mkdirs(&sid, "Backups/TESTPC/Marked");
    let mut job = FolderJob::new(&folder("marks-itest", "Marked", &root, FolderKind::Folder), &sid, "TESTPC", data.path(), vec![]);
    job.remote_id = remote;
    let mut f = Folder::new(job);
    let marks = |dir: &Path| -> std::collections::HashMap<String, Mark> {
        shellstatus::dir_marks(dir)
            .map(|(_, v)| v.into_iter().map(|(n, m)| (n.to_string_lossy().to_string(), m)).collect())
            .unwrap_or_default()
    };

    pass(&api, &sid, &mut f, None);
    let m = marks(&root);
    assert_eq!(m.get("free.txt"), Some(&Mark::Ok), "{m:?}");
    assert_eq!(m.get("Sub"), Some(&Mark::Pending), "a file inside is retried: {m:?}");
    assert!(!m.contains_key("~$scratch.docx"), "never backed up: no mark");
    assert_eq!(marks(&root.join("Sub")).get("open.docx"), Some(&Mark::Pending));
    assert_eq!(marks(parent.path()).get("Marked"), Some(&Mark::Pending));

    pass(&api, &sid, &mut f, None);
    pass(&api, &sid, &mut f, None);
    assert_eq!(marks(&root.join("Sub")).get("open.docx"), Some(&Mark::Problem), "reported after a few tries");
    assert_eq!(marks(&root).get("Sub"), Some(&Mark::Problem));
    assert_eq!(marks(parent.path()).get("Marked"), Some(&Mark::Problem));

    drop(lock);
    pass(&api, &sid, &mut f, None);
    assert_eq!(marks(&root.join("Sub")).get("open.docx"), Some(&Mark::Ok));
    assert_eq!(marks(parent.path()).get("Marked"), Some(&Mark::Ok));

    // Changed on the PC: waiting again, before any pass has seen it.
    mtime_tick();
    std::fs::write(root.join("free.txt"), b"changed").unwrap();
    assert_eq!(marks(&root).get("free.txt"), Some(&Mark::Pending));

    shellstatus::remove("marks-itest");
    assert!(shellstatus::dir_marks(&root).is_none());
}
