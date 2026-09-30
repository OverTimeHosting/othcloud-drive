//! The same flows as tests/drive.rs, against a REAL panel. Opt-in:
//!
//!   OTHCLOUD_REAL_PANEL=http://127.0.0.1:3900 OTHCLOUD_REAL_TOKEN=… \
//!   OTHCLOUD_REAL_ORG=… OTHCLOUD_REAL_STORAGE=… cargo test --test real_panel
//!
//! Without OTHCLOUD_REAL_PANEL every test returns immediately. Everything is
//! done inside a fresh top-level folder that is deleted at the end.

use othcloud_drive::api::{Api, ItemType};
use othcloud_drive::drive::{status, DriveCore, DriveOptions};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

struct Env {
    api: Api,
    sid: String,
}

fn env() -> Option<Env> {
    let origin = std::env::var("OTHCLOUD_REAL_PANEL").ok()?;
    let token = std::env::var("OTHCLOUD_REAL_TOKEN").unwrap_or_else(|_| "drive-test-alice".into());
    let org = std::env::var("OTHCLOUD_REAL_ORG").unwrap_or_else(|_| "org_test_acme".into());
    let sid = std::env::var("OTHCLOUD_REAL_STORAGE").unwrap_or_else(|_| "stor_test_alice".into());
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("othcloud_drive=info")
        .try_init();
    Some(Env {
        api: Api::new(&origin, &token, Some(org)),
        sid,
    })
}

fn core(e: &Env, dir: &std::path::Path) -> Arc<DriveCore> {
    let c = DriveCore::new(
        e.api.clone(),
        &e.sid,
        "Real",
        dir.join(&e.sid),
        Arc::new(AtomicBool::new(false)),
        DriveOptions {
            poll_interval: Duration::from_secs(3600),
            upload_delay: Duration::from_millis(200),
            host_name: "TESTPC".into(),
        },
    )
    .unwrap();
    c.refresh().unwrap();
    c.start_workers();
    c
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
        let end = (off + (1 << 20)).min(out.len());
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
    for (i, chunk) in data.chunks(1 << 20).enumerate() {
        core.write(info.id, chunk, (i << 20) as u64, false, false).unwrap();
    }
    core.flush(info.id);
    core.close(info.id);
}

/// Server file by path under the test root, via a fresh tree.
fn server_file(e: &Env, path: &[&str]) -> Option<(String, u64)> {
    let t = e.api.tree(&e.sid, None).unwrap().unwrap();
    let mut parent: Option<String> = None;
    for dir in &path[..path.len() - 1] {
        parent = Some(
            t.folders
                .iter()
                .find(|f| f.parent_id == parent && f.name == *dir)?
                .folder_id
                .clone(),
        );
    }
    let name = path[path.len() - 1];
    t.files
        .iter()
        .find(|f| f.folder_id == parent && f.name == name)
        .map(|f| (f.file_id.clone(), f.size_bytes))
}

fn server_bytes(e: &Env, id: &str) -> Vec<u8> {
    let mut v = Vec::new();
    e.api.download(id, &mut v).unwrap();
    v
}

#[test]
fn real_panel_end_to_end() {
    let Some(e) = env() else {
        eprintln!("OTHCLOUD_REAL_PANEL not set; skipping");
        return;
    };
    let root = format!("drive-client-test-{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let c = core(&e, dir.path());
    let r = format!("\\{root}");

    // mkdir
    let d = c.create(&r, true).unwrap();
    c.close(d.id);
    let sub = c.create(&format!("{r}\\Sub"), true).unwrap();
    c.close(sub.id);

    // multi-MB upload, then read back through a FRESH core (ranged GETs)
    let big = pattern(3 * 1024 * 1024 + 777, 9);
    write_file(&c, &format!("{r}\\Sub\\big.bin"), &big);
    assert!(c.wait_idle(Duration::from_secs(60)), "upload didn't finish");
    let (bid, bsize) = server_file(&e, &[&root, "Sub", "big.bin"]).expect("big.bin on server");
    assert_eq!(bsize, big.len() as u64);
    assert_eq!(server_bytes(&e, &bid), big);
    let dir2 = tempfile::tempdir().unwrap();
    let c2 = core(&e, dir2.path());
    let info = c2.open(&format!("{r}\\SUB\\BIG.BIN")).unwrap();
    let mut buf = vec![0u8; 5000];
    c2.read(info.id, &mut buf, 2_000_000).unwrap();
    assert_eq!(&buf[..], &big[2_000_000..2_005_000]);
    c2.close(info.id);
    assert_eq!(read_all(&c2, &format!("{r}\\Sub\\big.bin")), big);
    c2.stop();

    // rename + move
    let b = c.open(&format!("{r}\\Sub\\big.bin")).unwrap();
    c.rename(b.id, &format!("{r}\\big2.bin"), false).unwrap();
    c.close(b.id);
    assert_eq!(server_file(&e, &[&root, "big2.bin"]).unwrap().0, bid, "move keeps id");
    // case-only rename
    let b = c.open(&format!("{r}\\big2.bin")).unwrap();
    c.rename(b.id, &format!("{r}\\BIG2.bin"), false).unwrap();
    c.close(b.id);
    assert!(server_file(&e, &[&root, "BIG2.bin"]).is_some());

    // editor pattern: write sibling, rename over with replace -> same id
    write_file(&c, &format!("{r}\\notes.txt"), b"v1");
    assert!(c.wait_idle(Duration::from_secs(30)));
    let (nid, _) = server_file(&e, &[&root, "notes.txt"]).unwrap();
    write_file(&c, &format!("{r}\\notes.txt.new"), b"version two");
    let n = c.open(&format!("{r}\\notes.txt.new")).unwrap();
    c.rename(n.id, &format!("{r}\\notes.txt"), true).unwrap();
    c.close(n.id);
    assert!(c.wait_idle(Duration::from_secs(30)));
    let (nid2, _) = server_file(&e, &[&root, "notes.txt"]).unwrap();
    assert_eq!(nid2, nid);
    assert_eq!(server_bytes(&e, &nid), b"version two");
    assert!(server_file(&e, &[&root, "notes.txt.new"]).is_none());

    // Word save dance
    write_file(&c, &format!("{r}\\Report.docx"), b"doc v1");
    assert!(c.wait_idle(Duration::from_secs(30)));
    let (did, _) = server_file(&e, &[&root, "Report.docx"]).unwrap();
    write_file(&c, &format!("{r}\\~$Report.docx"), b"lock");
    write_file(&c, &format!("{r}\\~WRD0000.tmp"), b"doc v2 longer");
    let o = c.open(&format!("{r}\\Report.docx")).unwrap();
    c.rename(o.id, &format!("{r}\\~WRL0001.tmp"), false).unwrap();
    c.close(o.id);
    let t = c.open(&format!("{r}\\~WRD0000.tmp")).unwrap();
    c.rename(t.id, &format!("{r}\\Report.docx"), false).unwrap();
    c.close(t.id);
    for p in ["~WRL0001.tmp", "~$Report.docx"] {
        let x = c.open(&format!("{r}\\{p}")).unwrap();
        c.delete(x.id).unwrap();
        c.close(x.id);
    }
    assert!(c.wait_idle(Duration::from_secs(30)));
    assert_eq!(server_file(&e, &[&root, "Report.docx"]).unwrap().0, did);
    assert_eq!(server_bytes(&e, &did), b"doc v2 longer");

    // name collision with a folder (server: unique across kinds)
    let f = c.create(&format!("{r}\\Clash"), true).unwrap();
    c.close(f.id);
    assert_eq!(
        c.create(&format!("{r}\\clash"), false).unwrap_err().0,
        status::OBJECT_NAME_COLLISION
    );

    // server-side change shows up after a poll (ETag honoured)
    // Our own changes must not come back as remote changes (other agents
    // may be using the same drive, so only look inside our folder).
    c.refresh().unwrap();
    let ours: Vec<_> = c
        .take_changes()
        .into_iter()
        .filter(|ch| format!("{ch:?}").contains(&root))
        .collect();
    assert!(ours.is_empty(), "own changes echoed back: {ours:?}");
    assert!(!c.refresh().unwrap(), "second poll should be a 304");
    let (rid, _) = server_file(&e, &[&root, "Report.docx"]).unwrap();
    let tmp = dir.path().join("remote.bin");
    std::fs::write(&tmp, b"changed on the server").unwrap();
    e.api.upload_replace(&rid, &tmp).unwrap();
    assert!(c.refresh().unwrap());
    assert_eq!(read_all(&c, &format!("{r}\\Report.docx")), b"changed on the server");

    // delete a file, then the whole test folder (recursive on the server)
    let x = c.open(&format!("{r}\\BIG2.bin")).unwrap();
    c.delete(x.id).unwrap();
    c.close(x.id);
    assert!(server_file(&e, &[&root, "BIG2.bin"]).is_none());
    let t = e.api.tree(&e.sid, None).unwrap().unwrap();
    let rootf = t.folders.iter().find(|f| f.name == root && f.parent_id.is_none()).unwrap();
    e.api.delete_item(ItemType::Folder, &rootf.folder_id).unwrap();
    c.refresh().unwrap();
    assert!(c.lookup(&r).is_err());
    c.stop();
}

#[test]
fn real_panel_quota_and_errors() {
    let Some(e) = env() else { return };
    // 1000-byte quota drive from the seed (900 used).
    let small = Api::new(
        e.api.origin(),
        &std::env::var("OTHCLOUD_REAL_TOKEN").unwrap_or_else(|_| "drive-test-alice".into()),
        e.api.org().map(String::from),
    );
    let err = small
        .upload_new_bytes("stor_test_small", None, "too-big.bin", vec![0u8; 5000])
        .unwrap_err();
    assert!(err.is_quota(), "{err}");
    let bad = Api::new(e.api.origin(), "not-a-token", e.api.org().map(String::from));
    assert!(bad.drives().unwrap_err().is_auth());
    let t = e.api.tree(&e.sid, None).unwrap().unwrap();
    assert!(e.api.tree(&e.sid, Some(&t.etag)).unwrap().is_none(), "304 expected");
}
