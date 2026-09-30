//! The engine flows against a REAL panel, through a real sync root. Opt-in:
//!
//!   OTHCLOUD_REAL_PANEL=http://127.0.0.1:3900 [OTHCLOUD_REAL_TOKEN=…]
//!   [OTHCLOUD_REAL_ORG=…] [OTHCLOUD_REAL_STORAGE=…] cargo test --test real_panel
//!
//! Without OTHCLOUD_REAL_PANEL every test returns immediately. Everything
//! is done inside a fresh top-level folder that is deleted at the end.

use othcloud_drive::api::{Api, ItemType, Tree};
use othcloud_drive::cf;
use othcloud_drive::engine::{remove_root, Engine, EngineOptions};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn env() -> Option<(Api, String)> {
    let origin = std::env::var("OTHCLOUD_REAL_PANEL").ok()?;
    let token = std::env::var("OTHCLOUD_REAL_TOKEN").unwrap_or_else(|_| "drive-test-alice".into());
    let org = std::env::var("OTHCLOUD_REAL_ORG").unwrap_or_else(|_| "org_test_acme".into());
    let sid = std::env::var("OTHCLOUD_REAL_STORAGE").unwrap_or_else(|_| "stor_test_alice".into());
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("othcloud_drive=info")
        .try_init();
    Some((Api::new(&origin, &token, Some(org)), sid))
}

fn cmd(args: &[&str]) {
    // Another process, so the platform sends rename/delete notifications.
    let out = std::process::Command::new("cmd").args(["/d", "/c"]).args(args).output().unwrap();
    assert!(out.status.success(), "cmd {args:?}: {}", String::from_utf8_lossy(&out.stdout));
}
fn s(p: &Path) -> String {
    p.display().to_string()
}

/// Unregisters on drop, so a failing assertion can't leak a sync root.
struct RootGuard(Vec<String>);
impl Drop for RootGuard {
    fn drop(&mut self) {
        for id in &self.0 {
            let _ = cf::unregister_sync_root(id);
        }
    }
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_add(seed).wrapping_mul(2654435761) >> 11) as u8).collect()
}

fn tree(api: &Api, sid: &str) -> Tree {
    api.tree(sid, None).unwrap().unwrap()
}

/// Server file (id, size) at path components under the drive root.
fn server_file(api: &Api, sid: &str, path: &[&str]) -> Option<(String, u64)> {
    let t = tree(api, sid);
    let mut parent: Option<String> = None;
    for dir in &path[..path.len() - 1] {
        parent = Some(t.folders.iter().find(|f| f.parent_id == parent && f.name == *dir)?.folder_id.clone());
    }
    t.files
        .iter()
        .find(|f| f.folder_id == parent && f.name == path[path.len() - 1])
        .map(|f| (f.file_id.clone(), f.size_bytes))
}

fn server_bytes(api: &Api, id: &str) -> Vec<u8> {
    let mut v = Vec::new();
    api.download(id, &mut v).unwrap();
    v
}

fn wait(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    f()
}

#[test]
fn real_panel_end_to_end() {
    let Some((api, sid)) = env() else {
        eprintln!("OTHCLOUD_REAL_PANEL not set; skipping");
        return;
    };
    let top = format!("drive-client-test-{}", std::process::id());
    let base = tempfile::tempdir().unwrap();
    let e = Engine::start(
        api.clone(),
        &sid,
        "Real",
        &base.path().join("Real"),
        "%SystemRoot%\\System32\\imageres.dll,-1043",
        Arc::new(AtomicBool::new(false)),
        EngineOptions {
            poll_interval: Duration::from_secs(3600),
            settle: Duration::from_millis(300),
            delete_grace: Duration::from_millis(800),
            host_name: "TESTPC".into(),
            rescan_interval: Duration::from_secs(3600),
            provider: cf::TEST_PROVIDER.into(),
            cover_interval: Duration::from_secs(3600),
        },
    )
    .unwrap();
    let _guard = RootGuard(vec![e.sync_root_id.clone()]);
    let r = e.root.join(&top);
    let idle = || {
        std::thread::sleep(Duration::from_millis(800));
        assert!(e.wait_idle(Duration::from_secs(60)));
    };

    std::fs::create_dir_all(r.join("Sub")).unwrap();
    let big = pattern(3 * 1024 * 1024 + 777, 9);
    std::fs::write(r.join("Sub\\big.bin"), &big).unwrap();
    idle();
    let (bid, bsize) = server_file(&api, &sid, &[&top, "Sub", "big.bin"]).expect("uploaded");
    assert_eq!(bsize, big.len() as u64);
    assert_eq!(server_bytes(&api, &bid), big);
    assert!(cf::placeholder_info(&r.join("Sub\\big.bin")).unwrap().unwrap().in_sync);

    // Rename + move keep the id and don't re-upload.
    cmd(&["move", &s(&r.join("Sub\\big.bin")), &s(&r.join("big2.bin"))]);
    idle();
    assert_eq!(server_file(&api, &sid, &[&top, "big2.bin"]).unwrap().0, bid);

    // Editor save (MoveFileEx replace) keeps the id.
    std::fs::write(r.join("notes.txt"), b"v1").unwrap();
    idle();
    let (nid, _) = server_file(&api, &sid, &[&top, "notes.txt"]).unwrap();
    std::fs::write(r.join("notes.txt.new"), b"version two").unwrap();
    cmd(&["move", "/y", &s(&r.join("notes.txt.new")), &s(&r.join("notes.txt"))]);
    std::thread::sleep(Duration::from_millis(1500));
    idle();
    assert_eq!(server_file(&api, &sid, &[&top, "notes.txt"]).unwrap().0, nid);
    assert_eq!(server_bytes(&api, &nid), b"version two");

    // Word dance keeps the id; no temp file on the server.
    std::fs::write(r.join("Report.docx"), b"doc v1").unwrap();
    idle();
    let (did, _) = server_file(&api, &sid, &[&top, "Report.docx"]).unwrap();
    std::fs::write(r.join("~WRD0000.tmp"), b"doc v2 longer").unwrap();
    cmd(&["move", &s(&r.join("Report.docx")), &s(&r.join("~WRL0001.tmp"))]);
    cmd(&["move", &s(&r.join("~WRD0000.tmp")), &s(&r.join("Report.docx"))]);
    cmd(&["del", "/q", &s(&r.join("~WRL0001.tmp"))]);
    std::thread::sleep(Duration::from_millis(1500));
    idle();
    assert_eq!(server_file(&api, &sid, &[&top, "Report.docx"]).unwrap().0, did);
    assert_eq!(server_bytes(&api, &did), b"doc v2 longer");
    let t = tree(&api, &sid);
    assert!(!t.files.iter().any(|f| f.name.ends_with(".tmp")), "temp leaked");

    // Server-side change: dehydrated, re-read gives the new bytes.
    let tmp = base.path().join("srv.txt");
    std::fs::write(&tmp, b"changed on the server").unwrap();
    api.upload_replace(&did, &tmp).unwrap();
    assert!(e.refresh().unwrap());
    let rp = r.join("Report.docx");
    assert!(cf::attributes(&rp).unwrap() & cf::ATTR_RECALL_ON_DATA_ACCESS != 0);
    assert_eq!(std::fs::read(&rp).unwrap(), b"changed on the server");
    assert!(!e.refresh().unwrap(), "no echo of our own state");

    // Fresh sync root elsewhere: the big file is a dehydrated placeholder
    // that hydrates via ranged GETs.
    let base2 = tempfile::tempdir().unwrap();
    let e2 = Engine::start(
        api.clone(),
        &sid,
        "Real2",
        &base2.path().join("Real"),
        "%SystemRoot%\\System32\\imageres.dll,-1043",
        Arc::new(AtomicBool::new(false)),
        EngineOptions { provider: cf::TEST_PROVIDER.into(), ..Default::default() },
    );
    // Same storage => same sync root id; registering a second path for it
    // re-points the id. Only do the check if it worked.
    if let Ok(e2) = e2 {
        let p2 = e2.root.join(&top).join("big2.bin");
        assert!(cf::attributes(&p2).unwrap() & cf::ATTR_RECALL_ON_DATA_ACCESS != 0);
        assert_eq!(std::fs::read(&p2).unwrap(), big);
        e2.shutdown(Duration::from_secs(5));
        let _ = remove_root(&e2.sync_root_id, &e2.root, "Real2", base2.path());
    }

    // Delete the test folder from the drive (recursive on the server).
    cmd(&["rmdir", "/s", "/q", &s(&r)]);
    assert!(wait(30, || !tree(&api, &sid).folders.iter().any(|f| f.name == top)), "folder not deleted");
    e.shutdown(Duration::from_secs(10));
    let _ = remove_root(&e.sync_root_id, &e.root, "Real", base.path());
    let t = tree(&api, &sid);
    if let Some(f) = t.folders.iter().find(|f| f.name == top) {
        let _ = api.delete_item(ItemType::Folder, &f.folder_id);
    }
}

#[test]
fn real_panel_quota_and_errors() {
    let Some((api, sid)) = env() else { return };
    let err = api
        .upload_new_bytes("stor_test_small", None, "too-big.bin", vec![0u8; 5000])
        .unwrap_err();
    assert!(err.is_quota(), "{err}");
    let bad = Api::new(api.origin(), "not-a-token", api.org().map(String::from));
    assert!(bad.drives().unwrap_err().is_auth());
    let t = api.tree(&sid, None).unwrap().unwrap();
    assert!(api.tree(&sid, Some(&t.etag)).unwrap().is_none(), "304 expected");
}
