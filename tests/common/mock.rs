//! In-memory implementation of PROTOCOL.md, for tests and manual mounting.
//!
//! Faithful where it matters to the client: bearer + X-OTH-Org auth, JSON
//! errors with real statuses, `tree` ETag / If-None-Match -> 304, Range ->
//! 206 + Content-Range, 409 name_taken (case-insensitive, files+folders), 409
//! quota_exceeded from Content-Length, 400 for moving a folder into itself.
//!
//! Extra `/__mock/*` endpoints let scripts inspect and change server state.
#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct MFolder {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct MFile {
    pub id: String,
    pub folder: Option<String>,
    pub name: String,
    pub data: Vec<u8>,
    pub content_type: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct MDrive {
    pub storage_id: String,
    pub org: String,
    pub name: String,
    pub quota: Option<u64>,
    pub cover: Option<Vec<u8>>,
    /// What `cover?w=` answers with (the panel's scaled JPEG/PNG of a
    /// cover it can read but a client may not, e.g. HEIC). None: `w` is
    /// ignored and the original is sent, as by an older panel.
    pub cover_scaled: Option<Vec<u8>>,
    pub color: Option<String>,
    pub folders: HashMap<String, MFolder>,
    pub files: HashMap<String, MFile>,
    pub version: u64,
}

impl MDrive {
    fn used(&self) -> u64 {
        self.files.values().map(|f| f.data.len() as u64).sum()
    }
    fn etag(&self) -> String {
        format!("\"v{}\"", self.version)
    }
    fn touch(&mut self) {
        self.version += 1;
    }
    /// PROTOCOL.md: names are unique per parent across files AND folders,
    /// case-insensitively.
    fn name_taken(&self, parent: &Option<String>, name: &str, except: Option<&str>) -> bool {
        let l = name.to_lowercase();
        self.files.values().any(|f| {
            &f.folder == parent && f.name.to_lowercase() == l && Some(f.id.as_str()) != except
        }) || self.folders.values().any(|f| {
            &f.parent == parent && f.name.to_lowercase() == l && Some(f.id.as_str()) != except
        })
    }
    fn is_descendant(&self, candidate: &str, ancestor: &str) -> bool {
        let mut cur = Some(candidate.to_string());
        let mut guard = 0;
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.folders.get(&c).and_then(|f| f.parent.clone());
            guard += 1;
            if guard > 1000 {
                return true;
            }
        }
        false
    }
    fn folder_path(&self, id: &Option<String>) -> String {
        let mut parts = Vec::new();
        let mut cur = id.clone();
        while let Some(c) = cur {
            match self.folders.get(&c) {
                Some(f) => {
                    parts.push(f.name.clone());
                    cur = f.parent.clone();
                }
                None => break,
            }
        }
        parts.reverse();
        parts.join("/")
    }
    pub fn file_path(&self, f: &MFile) -> String {
        let p = self.folder_path(&f.folder);
        if p.is_empty() {
            f.name.clone()
        } else {
            format!("{p}/{}", f.name)
        }
    }
    /// Resolve "a/b/c" (exact names) to a folder id.
    pub fn folder_by_path(&self, path: &str) -> Option<Option<String>> {
        let mut cur: Option<String> = None;
        for part in path.split('/').filter(|s| !s.is_empty()) {
            let f = self
                .folders
                .values()
                .find(|f| f.parent == cur && f.name == part)?;
            cur = Some(f.id.clone());
        }
        Some(cur)
    }
    pub fn file_by_path(&self, path: &str) -> Option<&MFile> {
        let (dir, name) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        let folder = self.folder_by_path(dir)?;
        self.files.values().find(|f| f.folder == folder && f.name == name)
    }
    fn remove_folder_recursive(&mut self, id: &str) {
        let kids: Vec<String> = self
            .folders
            .values()
            .filter(|f| f.parent.as_deref() == Some(id))
            .map(|f| f.id.clone())
            .collect();
        for k in kids {
            self.remove_folder_recursive(&k);
        }
        self.files.retain(|_, f| f.folder.as_deref() != Some(id));
        self.folders.remove(id);
    }
}

#[derive(Debug, Default)]
pub struct MockState {
    pub token: String,
    pub orgs: Vec<(String, String)>,
    pub drives: Vec<MDrive>,
    pub pair_codes: HashMap<String, String>,
    pub seq: u64,
    /// (method, path, range header) of every drive API request.
    pub log: Vec<(String, String, Option<String>)>,
    /// If set, every drive request fails with a connection-level error
    /// (simulated by a 503 the client treats as a server error) — tests use
    /// shutdown instead for real network failure.
    pub fail_all: bool,
    /// Omit ids from write responses (to exercise the client's fallback).
    pub terse: bool,
    /// Answer `GET drives` as an older panel: no coverFileId / coverVersion.
    pub legacy_drives: bool,
    /// Per-server id prefix, so parallel servers never share ids.
    pub tag: String,
    /// The folder-backup API (`/api/desktop/backup/*`).
    pub backup: MockBackup,
}

/// PROTOCOL.md "Folder backup", in memory.
#[derive(Debug, Default)]
pub struct MockBackup {
    /// Answer 404 for every backup route (a panel without the feature).
    pub disabled: bool,
    /// (deviceKey, deviceId, body of the last PUT device)
    pub devices: Vec<(String, String, Value)>,
    /// deviceId -> the profile as returned
    pub profiles: HashMap<String, Value>,
    /// Every PUT profile body.
    pub profile_puts: Vec<Value>,
    /// Every POST status body.
    pub statuses: Vec<Value>,
    /// fileId -> deleted on the PC?
    pub deleted: HashMap<String, bool>,
    /// Every POST deleted body.
    pub deleted_posts: Vec<Value>,
    /// fileId -> previous contents kept by `X-OTH-Keep-Version: 1` (newest last, 5 kept).
    pub versions: HashMap<String, Vec<Vec<u8>>>,
}

impl MockState {
    fn next_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}_{}{:06}", self.tag, self.seq)
    }
    pub fn drive(&self, sid: &str) -> Option<&MDrive> {
        self.drives.iter().find(|d| d.storage_id == sid)
    }
    pub fn drive_mut(&mut self, sid: &str) -> Option<&mut MDrive> {
        self.drives.iter_mut().find(|d| d.storage_id == sid)
    }
}

fn now_iso() -> String {
    // Millisecond resolution plus a counter would be ideal; the tests only
    // need updatedAt to change between writes, so use nanos.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let secs = t.as_secs() as i64;
    let nanos = t.subsec_nanos();
    // Minimal RFC3339 formatter (UTC).
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:09}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        nanos
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn file_json(f: &MFile) -> Value {
    json!({
        "fileId": f.id, "folderId": f.folder, "name": f.name,
        "sizeBytes": f.data.len(), "contentType": f.content_type, "updatedAt": f.updated_at
    })
}
fn folder_json(f: &MFolder) -> Value {
    json!({ "folderId": f.id, "parentId": f.parent, "name": f.name, "createdAt": f.created_at })
}

fn error(status: StatusCode, code: &str, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": code, "message": msg }))).into_response()
}

type Shared = Arc<Mutex<MockState>>;

pub struct MockServer {
    pub addr: SocketAddr,
    pub state: Shared,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    /// Start on 127.0.0.1:`port` (0 = any) in a background thread.
    pub fn start(port: u16) -> MockServer {
        let state: Shared = Arc::new(Mutex::new(MockState {
            token: "test-token".into(),
            orgs: vec![("org_1".into(), "Test Org".into())],
            tag: {
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
                format!("{:x}{n}", (t ^ (std::process::id() as u64) << 20) & 0xffffff)
            },
            ..Default::default()
        }));
        Self::start_with(port, state)
    }

    pub fn start_with(port: u16, state: Shared) -> MockServer {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (atx, arx) = std::sync::mpsc::channel();
        let st = state.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = Router::new()
                    .fallback(any(handler))
                    .layer(axum::extract::DefaultBodyLimit::disable())
                    .with_state(st);
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                    .await
                    .expect("bind");
                atx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        let addr = arx.recv().unwrap();
        MockServer {
            addr,
            state,
            shutdown: Some(tx),
            thread: Some(thread),
        }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn add_drive(&self, name: &str, quota: Option<u64>) -> String {
        let mut s = self.state.lock().unwrap();
        let id = s.next_id("stor");
        s.drives.push(MDrive {
            storage_id: id.clone(),
            org: "org_1".into(),
            name: name.into(),
            quota,
            cover: None,
            cover_scaled: None,
            color: None,
            folders: HashMap::new(),
            files: HashMap::new(),
            version: 1,
        });
        id
    }

    /// Create folders along `path` ("a/b") on the server side.
    pub fn mkdirs(&self, sid: &str, path: &str) -> Option<String> {
        let mut s = self.state.lock().unwrap();
        mkdirs_locked(&mut s, sid, path)
    }

    /// Server-side write of a file at `path` (creating folders).
    pub fn put(&self, sid: &str, path: &str, data: &[u8]) -> String {
        let mut s = self.state.lock().unwrap();
        put_locked(&mut s, sid, path, data)
    }

    pub fn get(&self, sid: &str, path: &str) -> Option<Vec<u8>> {
        let s = self.state.lock().unwrap();
        s.drive(sid)?.file_by_path(path).map(|f| f.data.clone())
    }

    pub fn file_id(&self, sid: &str, path: &str) -> Option<String> {
        let s = self.state.lock().unwrap();
        s.drive(sid)?.file_by_path(path).map(|f| f.id.clone())
    }

    /// Sorted "path" list; folders end with '/'.
    pub fn paths(&self, sid: &str) -> Vec<String> {
        let s = self.state.lock().unwrap();
        let d = s.drive(sid).unwrap();
        let mut out: Vec<String> = d.files.values().map(|f| d.file_path(f)).collect();
        for f in d.folders.values() {
            out.push(format!("{}/", d.folder_path(&Some(f.id.clone()))));
        }
        out.sort();
        out
    }

    /// Server-side delete of the folder at `path` and everything in it.
    pub fn rmdir(&self, sid: &str, path: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        let Some(d) = s.drive_mut(sid) else { return false };
        let Some(Some(id)) = d.folder_by_path(path) else { return false };
        d.remove_folder_recursive(&id);
        d.touch();
        true
    }

    pub fn requests(&self, method: &str, path_contains: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(m, p, _)| m == method && p.contains(path_contains))
            .count()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn mkdirs_locked(s: &mut MockState, sid: &str, path: &str) -> Option<String> {
    let mut ids = Vec::new();
    for _ in path.split('/').filter(|p| !p.is_empty()) {
        ids.push(s.next_id("fold"));
    }
    let d = s.drive_mut(sid)?;
    let mut cur: Option<String> = None;
    for (i, part) in path.split('/').filter(|p| !p.is_empty()).enumerate() {
        let existing = d
            .folders
            .values()
            .find(|f| f.parent == cur && f.name == part)
            .map(|f| f.id.clone());
        cur = Some(match existing {
            Some(e) => e,
            None => {
                let id = ids[i].clone();
                d.folders.insert(
                    id.clone(),
                    MFolder {
                        id: id.clone(),
                        parent: cur.clone(),
                        name: part.into(),
                        created_at: now_iso(),
                    },
                );
                d.touch();
                id
            }
        });
    }
    cur
}

fn put_locked(s: &mut MockState, sid: &str, path: &str, data: &[u8]) -> String {
    let (dir, name) = match path.rfind('/') {
        Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
        None => (String::new(), path.to_string()),
    };
    let folder = mkdirs_locked(s, sid, &dir);
    let new_id = s.next_id("file");
    let d = s.drive_mut(sid).unwrap();
    let existing = d
        .files
        .values()
        .find(|f| f.folder == folder && f.name == name)
        .map(|f| f.id.clone());
    let id = existing.unwrap_or(new_id);
    d.files.insert(
        id.clone(),
        MFile {
            id: id.clone(),
            folder,
            name,
            data: data.to_vec(),
            content_type: "application/octet-stream".into(),
            updated_at: now_iso(),
        },
    );
    d.touch();
    id
}

async fn handler(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let mut s = state.lock().unwrap();

    // ---- test-control endpoints
    if let Some(rest) = path.strip_prefix("/__mock/") {
        return mock_admin(&mut s, rest, &method, &q, body);
    }

    if path == "/api/desktop/token" && method == Method::POST {
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
        return match s.pair_codes.remove(code) {
            Some(tok) => axum::Json(json!({
                "token": tok,
                "user": { "id": "user_1", "email": "test@example.com", "name": "Test User" }
            }))
            .into_response(),
            None => error(StatusCode::BAD_REQUEST, "invalid_code", "unknown code"),
        };
    }

    if let Some(bep) = path.strip_prefix("/api/desktop/backup/") {
        s.log.push((method.to_string(), path.clone(), None));
        return backup_api(&mut s, bep, &method, &q, &headers, body);
    }

    let Some(ep) = path.strip_prefix("/api/desktop/drive/") else {
        return error(StatusCode::NOT_FOUND, "not_found", "no such route");
    };
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    s.log.push((method.to_string(), path.clone(), range.clone()));

    // ---- auth
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != format!("Bearer {}", s.token) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized", "bad token");
    }
    if ep == "orgs" {
        let orgs: Vec<Value> = s
            .orgs
            .iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect();
        return axum::Json(Value::Array(orgs)).into_response();
    }
    let org = match headers.get("x-oth-org").and_then(|v| v.to_str().ok()) {
        Some(o) => o.to_string(),
        None => return error(StatusCode::BAD_REQUEST, "missing_org", "X-OTH-Org required"),
    };
    if !s.orgs.iter().any(|(id, _)| *id == org) {
        return error(StatusCode::FORBIDDEN, "forbidden", "not a member");
    }
    if s.fail_all {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "fail_all");
    }
    let terse = s.terse;

    // Resolve a drive of this org by storageId.
    macro_rules! drive {
        ($sid:expr) => {{
            let sid: String = $sid;
            match s
                .drives
                .iter()
                .position(|d| d.storage_id == sid && d.org == org)
            {
                Some(i) => i,
                None => return error(StatusCode::NOT_FOUND, "not_found", "no such drive"),
            }
        }};
    }
    // Find the drive holding a file / folder id.
    let find_file = |s: &MockState, id: &str| {
        s.drives
            .iter()
            .position(|d| d.org == org && d.files.contains_key(id))
    };
    let find_folder = |s: &MockState, id: &str| {
        s.drives
            .iter()
            .position(|d| d.org == org && d.folders.contains_key(id))
    };

    match (method.as_str(), ep) {
        ("GET", "drives") => {
            let v: Vec<Value> = s
                .drives
                .iter()
                .filter(|d| d.org == org)
                .map(|d| {
                    let mut v = json!({
                        "storageId": d.storage_id, "name": d.name, "sizeBytes": d.used(),
                        "quotaBytes": d.quota, "hasCover": d.cover.is_some(), "color": d.color, "updatedAt": now_iso()
                    });
                    if !s.legacy_drives {
                        // A new picture is a new file id; coverVersion follows the bytes.
                        let h = d.cover.as_ref().map(|c| hex(&Sha256::digest(c))[..16].to_string());
                        v["coverFileId"] = json!(h.as_ref().map(|h| format!("file_cover_{h}")));
                        v["coverVersion"] = json!(h);
                    }
                    v
                })
                .collect();
            axum::Json(Value::Array(v)).into_response()
        }
        ("GET", "cover") => {
            let i = drive!(q.get("storageId").cloned().unwrap_or_default());
            let d = &s.drives[i];
            let scaled = if q.contains_key("w") { d.cover_scaled.as_ref().filter(|_| d.cover.is_some()) } else { None };
            match scaled.or(d.cover.as_ref()) {
                Some(c) => {
                    let etag = format!("\"c{}\"", &hex(&Sha256::digest(c))[..16]);
                    let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok());
                    if inm == Some(etag.as_str()) {
                        return (StatusCode::NOT_MODIFIED, [("etag", etag)]).into_response();
                    }
                    ([("content-type", "image/png".to_string()), ("etag", etag)], c.clone()).into_response()
                }
                None => error(StatusCode::NOT_FOUND, "not_found", "no cover"),
            }
        }
        ("GET", "tree") => {
            let i = drive!(q.get("storageId").cloned().unwrap_or_default());
            let d = &s.drives[i];
            let etag = d.etag();
            if headers
                .get("if-none-match")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == etag)
                .unwrap_or(false)
            {
                return (StatusCode::NOT_MODIFIED, [("etag", etag)]).into_response();
            }
            let body = json!({
                "storageId": d.storage_id,
                "etag": etag,
                "folders": d.folders.values().map(folder_json).collect::<Vec<_>>(),
                "files": d.files.values().map(file_json).collect::<Vec<_>>(),
            });
            ([("etag", etag)], axum::Json(body)).into_response()
        }
        // A "thumbnail" is "thumb:<w>:<first bytes of the file>" for files
        // named *.jpg / *.png; anything else is 415, like the panel.
        ("GET", "thumbnail") => {
            let id = q.get("fileId").cloned().unwrap_or_default();
            let Some(i) = find_file(&s, &id) else {
                return error(StatusCode::NOT_FOUND, "not_found", "no such file");
            };
            let f = &s.drives[i].files[&id];
            let w = q.get("w").cloned().unwrap_or_default();
            let lower = f.name.to_lowercase();
            if !(lower.ends_with(".jpg") || lower.ends_with(".png")) {
                return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "not_an_image", "no thumbnail");
            }
            let mut body = format!("thumb:{w}:").into_bytes();
            body.extend_from_slice(&f.data[..f.data.len().min(8)]);
            ([("content-type", "image/jpeg")], body).into_response()
        }
        ("GET", "file") => {
            let id = q.get("fileId").cloned().unwrap_or_default();
            let Some(i) = find_file(&s, &id) else {
                return error(StatusCode::NOT_FOUND, "not_found", "no such file");
            };
            let data = &s.drives[i].files[&id].data;
            let len = data.len() as u64;
            if let Some(r) = range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
                let (a, b) = r.split_once('-').unwrap_or((r, ""));
                let start: u64 = a.parse().unwrap_or(0);
                let end: u64 = if b.is_empty() {
                    len.saturating_sub(1)
                } else {
                    b.parse::<u64>().unwrap_or(0).min(len.saturating_sub(1))
                };
                if start >= len || start > end {
                    return (
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [("content-range", format!("bytes */{len}"))],
                    )
                        .into_response();
                }
                let slice = data[start as usize..=end as usize].to_vec();
                return (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        ("content-range", format!("bytes {start}-{end}/{len}")),
                        ("accept-ranges", "bytes".to_string()),
                    ],
                    slice,
                )
                    .into_response();
            }
            ([("accept-ranges", "bytes")], data.clone()).into_response()
        }
        ("PUT", "file") => {
            let Some(content_length) = headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
            else {
                return error(StatusCode::LENGTH_REQUIRED, "length_required", "Content-Length");
            };
            if content_length > 10 * 1024 * 1024 * 1024 {
                return error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "10 GB cap");
            }
            if let Some(fid) = q.get("fileId").cloned() {
                let Some(i) = find_file(&s, &fid) else {
                    return error(StatusCode::NOT_FOUND, "not_found", "no such file");
                };
                let keep = headers.get("x-oth-keep-version").and_then(|v| v.to_str().ok()) == Some("1");
                if keep && !under_backup_folder(&s, i, &fid) {
                    return error(StatusCode::FORBIDDEN, "not_backup_file", "not under a backup folder");
                }
                let st: &mut MockState = &mut s;
                let d = &mut st.drives[i];
                let old = d.files[&fid].data.len() as u64;
                if let Some(qb) = d.quota {
                    if d.used() - old + content_length > qb {
                        return error(StatusCode::CONFLICT, "quota_exceeded", "quota");
                    }
                }
                let f = d.files.get_mut(&fid).unwrap();
                if keep {
                    let old = f.data.clone();
                    let v = st.backup.versions.entry(fid.clone()).or_default();
                    v.push(old);
                    if v.len() > 5 {
                        v.remove(0);
                    }
                }
                f.data = body.to_vec();
                f.updated_at = now_iso();
                let out = file_json(f);
                d.touch();
                return if terse {
                    axum::Json(json!({"ok": true})).into_response()
                } else {
                    axum::Json(out).into_response()
                };
            }
            let i = drive!(q.get("storageId").cloned().unwrap_or_default());
            let folder = q.get("folderId").filter(|f| !f.is_empty()).cloned();
            let name = q.get("name").cloned().unwrap_or_default();
            if name.is_empty() || name.contains('/') || name.contains('\\') {
                return error(StatusCode::BAD_REQUEST, "bad_name", "bad name");
            }
            let new_id = s.next_id("file");
            let d = &mut s.drives[i];
            if let Some(f) = &folder {
                if !d.folders.contains_key(f) {
                    return error(StatusCode::NOT_FOUND, "not_found", "no such folder");
                }
            }
            if d.name_taken(&folder, &name, None) {
                return error(StatusCode::CONFLICT, "name_taken", "a file with that name exists");
            }
            if let Some(qb) = d.quota {
                if d.used() + content_length > qb {
                    return error(StatusCode::CONFLICT, "quota_exceeded", "quota");
                }
            }
            let f = MFile {
                id: new_id.clone(),
                folder,
                name,
                data: body.to_vec(),
                content_type: "application/octet-stream".into(),
                updated_at: now_iso(),
            };
            let out = file_json(&f);
            d.files.insert(new_id, f);
            d.touch();
            if terse {
                (StatusCode::CREATED, axum::Json(json!({"ok": true}))).into_response()
            } else {
                (StatusCode::CREATED, axum::Json(out)).into_response()
            }
        }
        ("POST", "folder") => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let sid = v["storageId"].as_str().unwrap_or("").to_string();
            let parent = v["parentId"].as_str().map(|s| s.to_string());
            let name = v["name"].as_str().unwrap_or("").to_string();
            let i = drive!(sid);
            let id = s.next_id("fold");
            let d = &mut s.drives[i];
            if name.is_empty() {
                return error(StatusCode::BAD_REQUEST, "bad_name", "bad name");
            }
            if let Some(p) = &parent {
                if !d.folders.contains_key(p) {
                    return error(StatusCode::NOT_FOUND, "not_found", "no such parent");
                }
            }
            if d.name_taken(&parent, &name, None) {
                return error(StatusCode::CONFLICT, "name_taken", "folder exists");
            }
            let f = MFolder {
                id: id.clone(),
                parent,
                name,
                created_at: now_iso(),
            };
            let out = folder_json(&f);
            d.folders.insert(id, f);
            d.touch();
            axum::Json(out).into_response()
        }
        ("POST", "move") => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let kind = v["type"].as_str().unwrap_or("");
            let id = v["id"].as_str().unwrap_or("").to_string();
            let name = v.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());
            let parent: Option<Option<String>> = match v.get("parentId") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(p)) => Some(Some(p.clone())),
                Some(_) => return error(StatusCode::BAD_REQUEST, "bad_parent", "bad parentId"),
            };
            if kind == "file" {
                let Some(i) = find_file(&s, &id) else {
                    return error(StatusCode::NOT_FOUND, "not_found", "no such file");
                };
                let d = &mut s.drives[i];
                let cur = d.files[&id].clone();
                let new_parent = parent.clone().unwrap_or(cur.folder.clone());
                let new_name = name.clone().unwrap_or(cur.name.clone());
                if let Some(p) = &new_parent {
                    if !d.folders.contains_key(p) {
                        return error(StatusCode::NOT_FOUND, "not_found", "no such parent");
                    }
                }
                if d.name_taken(&new_parent, &new_name, Some(&id)) {
                    return error(StatusCode::CONFLICT, "name_taken", "name taken");
                }
                let f = d.files.get_mut(&id).unwrap();
                f.folder = new_parent;
                f.name = new_name;
                // Like the panel: a move bumps updatedAt.
                f.updated_at = now_iso();
                let out = file_json(f);
                d.touch();
                axum::Json(out).into_response()
            } else if kind == "folder" {
                let Some(i) = find_folder(&s, &id) else {
                    return error(StatusCode::NOT_FOUND, "not_found", "no such folder");
                };
                let d = &mut s.drives[i];
                let cur = d.folders[&id].clone();
                let new_parent = parent.clone().unwrap_or(cur.parent.clone());
                let new_name = name.clone().unwrap_or(cur.name.clone());
                if let Some(p) = &new_parent {
                    if !d.folders.contains_key(p) {
                        return error(StatusCode::NOT_FOUND, "not_found", "no such parent");
                    }
                    if d.is_descendant(p, &id) {
                        return error(StatusCode::BAD_REQUEST, "cycle", "into itself");
                    }
                }
                if d.name_taken(&new_parent, &new_name, Some(&id)) {
                    return error(StatusCode::CONFLICT, "name_taken", "name taken");
                }
                let f = d.folders.get_mut(&id).unwrap();
                f.parent = new_parent;
                f.name = new_name;
                let out = folder_json(f);
                d.touch();
                axum::Json(out).into_response()
            } else {
                error(StatusCode::BAD_REQUEST, "bad_type", "type")
            }
        }
        ("DELETE", "item") => {
            let kind = q.get("type").cloned().unwrap_or_default();
            let id = q.get("id").cloned().unwrap_or_default();
            if kind == "file" {
                let Some(i) = find_file(&s, &id) else {
                    return error(StatusCode::NOT_FOUND, "not_found", "no such file");
                };
                s.drives[i].files.remove(&id);
                s.drives[i].touch();
            } else if kind == "folder" {
                let Some(i) = find_folder(&s, &id) else {
                    return error(StatusCode::NOT_FOUND, "not_found", "no such folder");
                };
                s.drives[i].remove_folder_recursive(&id);
                s.drives[i].touch();
            } else {
                return error(StatusCode::BAD_REQUEST, "bad_type", "type");
            }
            axum::Json(json!({ "ok": true })).into_response()
        }
        _ => error(StatusCode::NOT_FOUND, "not_found", "no such endpoint"),
    }
}

fn mock_admin(
    s: &mut MockState,
    rest: &str,
    method: &Method,
    q: &HashMap<String, String>,
    body: Bytes,
) -> Response {
    let sid = q.get("storageId").cloned().unwrap_or_default();
    match (method.as_str(), rest) {
        // Register a one-shot pairing code that exchanges for the token.
        ("POST", "pair") => {
            let code = q.get("code").cloned().unwrap_or_else(|| "code1".into());
            let tok = s.token.clone();
            s.pair_codes.insert(code.clone(), tok);
            axum::Json(json!({ "code": code })).into_response()
        }
        ("GET", "drives") => {
            let v: Vec<Value> = s
                .drives
                .iter()
                .map(|d| json!({ "storageId": d.storage_id, "name": d.name }))
                .collect();
            axum::Json(Value::Array(v)).into_response()
        }
        // Every path with size + sha256, folders with a trailing '/'.
        ("GET", "state") => {
            let Some(d) = s.drive(&sid) else {
                return error(StatusCode::NOT_FOUND, "not_found", "drive");
            };
            let mut items: Vec<Value> = d
                .files
                .values()
                .map(|f| {
                    json!({
                        "path": d.file_path(f), "id": f.id, "size": f.data.len(),
                        "sha256": hex(&Sha256::digest(&f.data)),
                    })
                })
                .collect();
            for f in d.folders.values() {
                items.push(json!({ "path": format!("{}/", d.folder_path(&Some(f.id.clone()))), "id": f.id }));
            }
            items.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            axum::Json(json!({ "etag": d.etag(), "items": items })).into_response()
        }
        ("GET", "content") => {
            let path = q.get("path").cloned().unwrap_or_default();
            match s.drive(&sid).and_then(|d| d.file_by_path(&path)) {
                Some(f) => f.data.clone().into_response(),
                None => error(StatusCode::NOT_FOUND, "not_found", "file"),
            }
        }
        // Server-side change: write a file at path (creating folders).
        ("POST", "put") => {
            let path = q.get("path").cloned().unwrap_or_default();
            let id = put_locked(s, &sid, &path, &body);
            axum::Json(json!({ "id": id })).into_response()
        }
        ("POST", "delete") => {
            let path = q.get("path").cloned().unwrap_or_default();
            let Some(d) = s.drive_mut(&sid) else {
                return error(StatusCode::NOT_FOUND, "not_found", "drive");
            };
            let id = d.file_by_path(&path).map(|f| f.id.clone());
            match id {
                Some(id) => {
                    d.files.remove(&id);
                    d.touch();
                    axum::Json(json!({ "ok": true })).into_response()
                }
                None => error(StatusCode::NOT_FOUND, "not_found", "file"),
            }
        }
        // Set the cover (body = image bytes); an empty body clears it.
        ("POST", "cover") => {
            if let Some(d) = s.drive_mut(&sid) {
                d.cover = (!body.is_empty()).then(|| body.to_vec());
            }
            axum::Json(json!({ "ok": true })).into_response()
        }
        // ?color=%2338bdf8 sets the accent colour; no param clears it.
        ("POST", "color") => {
            let c = q.get("color").cloned();
            if let Some(d) = s.drive_mut(&sid) {
                d.color = c;
            }
            axum::Json(json!({ "ok": true })).into_response()
        }
        ("GET", "log") => {
            let v: Vec<Value> = s
                .log
                .iter()
                .map(|(m, p, r)| json!({ "method": m, "path": p, "range": r }))
                .collect();
            axum::Json(Value::Array(v)).into_response()
        }
        _ => error(StatusCode::NOT_FOUND, "not_found", "mock admin"),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Is file `fid` (in drive `i`) under a backup folder of any profile?
fn under_backup_folder(s: &MockState, i: usize, fid: &str) -> bool {
    let d = &s.drives[i];
    let roots: Vec<String> = s
        .backup
        .profiles
        .values()
        .filter(|p| p["storageId"].as_str() == Some(d.storage_id.as_str()))
        .flat_map(|p| p["folders"].as_array().cloned().unwrap_or_default())
        .filter_map(|f| f["remoteFolderId"].as_str().map(String::from))
        .collect();
    let mut cur = d.files.get(fid).and_then(|f| f.folder.clone());
    let mut guard = 0;
    while let Some(c) = cur {
        if roots.contains(&c) {
            return true;
        }
        cur = d.folders.get(&c).and_then(|f| f.parent.clone());
        guard += 1;
        if guard > 256 {
            break;
        }
    }
    false
}

/// `/api/desktop/backup/*` (PROTOCOL.md "Folder backup").
fn backup_api(
    s: &mut MockState,
    ep: &str,
    method: &Method,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if s.backup.disabled {
        return error(StatusCode::NOT_FOUND, "not_found", "no such route");
    }
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    if auth != format!("Bearer {}", s.token) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized", "bad token");
    }
    let org = match headers.get("x-oth-org").and_then(|v| v.to_str().ok()) {
        Some(o) => o.to_string(),
        None => return error(StatusCode::BAD_REQUEST, "missing_org", "X-OTH-Org required"),
    };
    if !s.orgs.iter().any(|(id, _)| *id == org) {
        return error(StatusCode::FORBIDDEN, "not_member", "not a member");
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match (method.as_str(), ep) {
        ("PUT", "device") => {
            let key = v["deviceKey"].as_str().unwrap_or("").to_string();
            if key.is_empty() || v["name"].as_str().unwrap_or("").is_empty() || v["platform"] != "windows" {
                return error(StatusCode::BAD_REQUEST, "bad_request", "device");
            }
            let existing = s.backup.devices.iter().position(|(k, _, _)| *k == key);
            let id = match existing {
                Some(i) => {
                    s.backup.devices[i].2 = v.clone();
                    s.backup.devices[i].1.clone()
                }
                None => {
                    let id = s.next_id("dev");
                    s.backup.devices.push((key, id.clone(), v.clone()));
                    id
                }
            };
            axum::Json(json!({ "deviceId": id })).into_response()
        }
        ("GET", "profile") => {
            let dev = q.get("deviceId").cloned().unwrap_or_default();
            let p = s.backup.profiles.get(&dev).cloned().unwrap_or(Value::Null);
            axum::Json(json!({ "profile": p })).into_response()
        }
        ("PUT", "profile") => {
            let dev = v["deviceId"].as_str().unwrap_or("").to_string();
            let Some(name) = s.backup.devices.iter().find(|d| d.1 == dev).and_then(|d| d.2["name"].as_str().map(String::from)) else {
                return error(StatusCode::NOT_FOUND, "not_found", "device");
            };
            let sid = v["storageId"].as_str().unwrap_or("").to_string();
            if !s.drives.iter().any(|d| d.storage_id == sid && d.org == org) {
                return error(StatusCode::NOT_FOUND, "not_found", "drive");
            }
            s.backup.profile_puts.push(v.clone());
            let mut folders = Vec::new();
            for f in v["folders"].as_array().cloned().unwrap_or_default() {
                let path = if f["kind"] == "photos" {
                    format!("Photo backup/{name}")
                } else {
                    format!("Backups/{name}/{}", f["label"].as_str().unwrap_or(""))
                };
                let sent = f["remoteFolderId"].as_str().map(String::from).filter(|id| {
                    f["kind"] == "photos" && s.drive(&sid).is_some_and(|d| d.folders.contains_key(id))
                });
                let id = match sent {
                    Some(id) => Some(id),
                    None => mkdirs_locked(s, &sid, &path),
                };
                let mut f = f.clone();
                f["remoteFolderId"] = json!(id);
                f["status"] = Value::Null;
                folders.push(f);
            }
            let p = json!({
                "profileId": format!("prof_{dev}"), "deviceId": dev, "storageId": sid,
                "paused": v["paused"], "folders": folders, "updatedAt": now_iso(),
            });
            s.backup.profiles.insert(dev, p.clone());
            axum::Json(json!({ "profile": p })).into_response()
        }
        ("POST", "status") => {
            s.backup.statuses.push(v);
            StatusCode::NO_CONTENT.into_response()
        }
        ("POST", "deleted") => {
            let deleted = v["deleted"].as_bool().unwrap_or(false);
            for id in v["fileIds"].as_array().cloned().unwrap_or_default() {
                if let Some(id) = id.as_str() {
                    s.backup.deleted.insert(id.to_string(), deleted);
                }
            }
            s.backup.deleted_posts.push(v);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => error(StatusCode::NOT_FOUND, "not_found", "no such route"),
    }
}
