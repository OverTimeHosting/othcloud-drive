//! Blocking HTTP client for the panel's `/api/desktop/drive/*` endpoints
//! (see PROTOCOL.md). Blocking on purpose: every WinFsp callback is
//! synchronous and runs on a dispatcher thread, so an async client would only
//! add a runtime hop.

use serde::{Deserialize, Deserializer};
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

/// Everything that can go wrong talking to the panel.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// Could not reach the server at all (DNS, connect, TLS, reset, timeout).
    Network(String),
    /// The server answered with a non-success status.
    Status {
        status: u16,
        code: String,
        message: String,
    },
    /// The server answered 2xx but the body was not what the protocol says.
    Decode(String),
    /// Local I/O while streaming an upload.
    Io(String),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
    pub fn code(&self) -> &str {
        match self {
            ApiError::Status { code, .. } => code,
            _ => "",
        }
    }
    pub fn is_network(&self) -> bool {
        matches!(self, ApiError::Network(_))
    }
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
    pub fn is_name_taken(&self) -> bool {
        self.status() == Some(409) && self.code() != "quota_exceeded"
    }
    pub fn is_quota(&self) -> bool {
        self.status() == Some(409) && self.code() == "quota_exceeded"
    }
    pub fn is_auth(&self) -> bool {
        self.status() == Some(401)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Network(m) => write!(f, "network error: {m}"),
            ApiError::Status {
                status,
                code,
                message,
            } => write!(f, "HTTP {status} {code}: {message}"),
            ApiError::Decode(m) => write!(f, "bad response: {m}"),
            ApiError::Io(m) => write!(f, "local I/O: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

fn from_reqwest(e: reqwest::Error) -> ApiError {
    if e.is_decode() {
        ApiError::Decode(e.to_string())
    } else {
        ApiError::Network(e.to_string())
    }
}

/// Accept a JSON number, a numeric string (Postgres bigint through JSON) or null.
fn de_u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(de_opt_u64_lenient(d)?.unwrap_or(0))
}

fn de_opt_u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Org {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveInfo {
    pub storage_id: String,
    pub name: String,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub size_bytes: u64,
    #[serde(default, deserialize_with = "de_opt_u64_lenient")]
    pub quota_bytes: Option<u64>,
    #[serde(default)]
    pub has_cover: bool,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFolder {
    pub folder_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    pub file_id: String,
    #[serde(default)]
    pub folder_id: Option<String>,
    pub name: String,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub size_bytes: u64,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tree {
    #[serde(default)]
    pub storage_id: String,
    #[serde(default)]
    pub etag: String,
    #[serde(default)]
    pub folders: Vec<RemoteFolder>,
    #[serde(default)]
    pub files: Vec<RemoteFile>,
}

/// What `/api/desktop/token` returns.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub token: String,
    #[serde(default)]
    pub user: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemType {
    File,
    Folder,
}

impl ItemType {
    fn as_str(self) -> &'static str {
        match self {
            ItemType::File => "file",
            ItemType::Folder => "folder",
        }
    }
}

/// The client. Cheap to clone (reqwest's client is an Arc inside).
#[derive(Clone)]
pub struct Api {
    http: reqwest::blocking::Client,
    /// For uploads/downloads: no overall timeout, only connect + read idle.
    http_long: reqwest::blocking::Client,
    origin: String,
    token: String,
    org: Option<String>,
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api")
            .field("origin", &self.origin)
            .field("org", &self.org)
            .finish()
    }
}

fn user_agent() -> String {
    format!("OTHCloud-Drive/{} (Windows)", env!("CARGO_PKG_VERSION"))
}

fn build_client(timeout: Option<Duration>) -> reqwest::blocking::Client {
    let mut b = reqwest::blocking::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(60));
    b = match timeout {
        Some(t) => b.timeout(t),
        None => b.timeout(None::<Duration>),
    };
    b.build().expect("reqwest client")
}

/// Normalise an origin: scheme + host (+ port), no trailing slash.
pub fn normalize_origin(origin: &str) -> Result<String, String> {
    let u = url::Url::parse(origin.trim()).map_err(|e| format!("bad origin {origin:?}: {e}"))?;
    if u.scheme() != "https" && u.scheme() != "http" {
        return Err(format!("origin must be http(s): {origin}"));
    }
    let host = u.host_str().ok_or_else(|| format!("origin has no host: {origin}"))?;
    let mut s = format!("{}://{}", u.scheme(), host);
    if let Some(p) = u.port() {
        s.push_str(&format!(":{p}"));
    }
    Ok(s)
}

impl Api {
    pub fn new(origin: &str, token: &str, org: Option<String>) -> Self {
        Api {
            http: build_client(Some(Duration::from_secs(60))),
            http_long: build_client(None),
            origin: origin.trim_end_matches('/').to_string(),
            token: token.to_string(),
            org,
        }
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn org(&self) -> Option<&str> {
        self.org.as_deref()
    }
    pub fn with_org(&self, org: &str) -> Api {
        let mut a = self.clone();
        a.org = Some(org.to_string());
        a
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/desktop/drive/{}", self.origin, path)
    }

    fn req(
        &self,
        client: &reqwest::blocking::Client,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::blocking::RequestBuilder {
        let mut r = client
            .request(method, self.url(path))
            .bearer_auth(&self.token);
        if let Some(org) = &self.org {
            r = r.header("X-OTH-Org", org);
        }
        r
    }

    fn send(rb: reqwest::blocking::RequestBuilder) -> ApiResult<reqwest::blocking::Response> {
        let resp = rb.send().map_err(from_reqwest)?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 304 {
            return Ok(resp);
        }
        let code = status.as_u16();
        let body = resp.text().unwrap_or_default();
        let (ecode, msg) = match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => (
                v.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("message")
                    .and_then(|e| e.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            Err(_) => (String::new(), body.chars().take(200).collect()),
        };
        Err(ApiError::Status {
            status: code,
            code: ecode,
            message: msg,
        })
    }

    fn json<T: serde::de::DeserializeOwned>(resp: reqwest::blocking::Response) -> ApiResult<T> {
        let text = resp.text().map_err(from_reqwest)?;
        serde_json::from_str(&text).map_err(|e| {
            ApiError::Decode(format!("{e} in {}", text.chars().take(200).collect::<String>()))
        })
    }

    /// POST `/api/desktop/token` — exchange a one-shot pairing code.
    pub fn exchange_code(origin: &str, code: &str, device_name: &str) -> ApiResult<TokenResponse> {
        let client = build_client(Some(Duration::from_secs(30)));
        let url = format!("{}/api/desktop/token", origin.trim_end_matches('/'));
        let rb = client
            .post(url)
            .json(&serde_json::json!({ "code": code, "name": device_name }));
        let resp = Self::send(rb)?;
        Self::json(resp)
    }

    pub fn orgs(&self) -> ApiResult<Vec<Org>> {
        let mut rb = self.http.get(self.url("orgs")).bearer_auth(&self.token);
        // /orgs is the one endpoint without X-OTH-Org; sending it is harmless
        // but pointless.
        rb = rb.header("Accept", "application/json");
        Self::json(Self::send(rb)?)
    }

    pub fn drives(&self) -> ApiResult<Vec<DriveInfo>> {
        Self::json(Self::send(self.req(&self.http, reqwest::Method::GET, "drives"))?)
    }

    /// The drive's cover image, or None when it has none (404).
    pub fn cover(&self, storage_id: &str) -> ApiResult<Option<Vec<u8>>> {
        let rb = self
            .req(&self.http, reqwest::Method::GET, "cover")
            .query(&[("storageId", storage_id)]);
        match Self::send(rb) {
            Ok(resp) => Ok(Some(resp.bytes().map_err(from_reqwest)?.to_vec())),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whole-drive tree. `Ok(None)` means 304 Not Modified for `etag`.
    pub fn tree(&self, storage_id: &str, etag: Option<&str>) -> ApiResult<Option<Tree>> {
        let mut rb = self
            .req(&self.http, reqwest::Method::GET, "tree")
            .query(&[("storageId", storage_id)]);
        if let Some(e) = etag {
            if !e.is_empty() {
                rb = rb.header("If-None-Match", e);
            }
        }
        let resp = Self::send(rb)?;
        if resp.status().as_u16() == 304 {
            return Ok(None);
        }
        let header_etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let mut t: Tree = Self::json(resp)?;
        if t.etag.is_empty() {
            t.etag = header_etag.unwrap_or_default();
        }
        Ok(Some(t))
    }

    /// Read `len` bytes at `offset`. Returns fewer bytes at EOF. Tolerates a
    /// server that ignores Range (200 + whole body).
    pub fn read_range(&self, file_id: &str, offset: u64, len: u64) -> ApiResult<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = offset + len - 1;
        let rb = self
            .req(&self.http_long, reqwest::Method::GET, "file")
            .query(&[("fileId", file_id)])
            .header("Range", format!("bytes={offset}-{end}"));
        let resp = match Self::send(rb) {
            Ok(r) => r,
            Err(e) if e.status() == Some(416) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let status = resp.status().as_u16();
        let content_range = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let mut body = Vec::with_capacity(len as usize);
        resp.take(if status == 206 { len } else { offset + len })
            .read_to_end(&mut body)
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if status == 206 {
            if let Some(start) = content_range.as_deref().and_then(parse_content_range_start) {
                if start != offset {
                    return Err(ApiError::Decode(format!(
                        "asked for offset {offset}, server sent {start}"
                    )));
                }
            }
            Ok(body)
        } else {
            // Whole file came back: slice our window out of it.
            let start = (offset as usize).min(body.len());
            Ok(body.split_off(start))
        }
    }

    /// Download the entire file into `out`.
    pub fn download(&self, file_id: &str, out: &mut dyn std::io::Write) -> ApiResult<u64> {
        let rb = self
            .req(&self.http_long, reqwest::Method::GET, "file")
            .query(&[("fileId", file_id)]);
        let mut resp = Self::send(rb)?;
        std::io::copy(&mut resp, out).map_err(|e| ApiError::Network(e.to_string()))
    }

    fn upload_body(path: &Path) -> ApiResult<(reqwest::blocking::Body, u64)> {
        let f = std::fs::File::open(path).map_err(|e| ApiError::Io(e.to_string()))?;
        let len = f.metadata().map_err(|e| ApiError::Io(e.to_string()))?.len();
        Ok((reqwest::blocking::Body::sized(f, len), len))
    }

    fn parse_file_row(resp: reqwest::blocking::Response) -> ApiResult<PartialFile> {
        let text = resp.text().map_err(from_reqwest)?;
        if text.trim().is_empty() {
            return Ok(PartialFile::default());
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| ApiError::Decode(e.to_string()))?;
        // Accept the row at top level or wrapped as { file: {...} }.
        let row = v.get("file").cloned().unwrap_or(v);
        Ok(serde_json::from_value(row).unwrap_or_default())
    }

    /// PUT file?storageId=&folderId=&name= — create a NEW file from `path`.
    pub fn upload_new(
        &self,
        storage_id: &str,
        folder_id: Option<&str>,
        name: &str,
        path: &Path,
    ) -> ApiResult<PartialFile> {
        let (body, len) = Self::upload_body(path)?;
        let rb = self
            .req(&self.http_long, reqwest::Method::PUT, "file")
            .query(&[
                ("storageId", storage_id),
                ("folderId", folder_id.unwrap_or("")),
                ("name", name),
            ])
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", len)
            .body(body);
        Self::parse_file_row(Self::send(rb)?)
    }

    /// PUT file?fileId= — replace an existing file's bytes from `path`.
    pub fn upload_replace(&self, file_id: &str, path: &Path) -> ApiResult<PartialFile> {
        let (body, len) = Self::upload_body(path)?;
        let rb = self
            .req(&self.http_long, reqwest::Method::PUT, "file")
            .query(&[("fileId", file_id)])
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", len)
            .body(body);
        Self::parse_file_row(Self::send(rb)?)
    }

    /// Same as `upload_new` but from memory (used by the photo backup tests
    /// and small writes).
    pub fn upload_new_bytes(
        &self,
        storage_id: &str,
        folder_id: Option<&str>,
        name: &str,
        bytes: Vec<u8>,
    ) -> ApiResult<PartialFile> {
        let rb = self
            .req(&self.http_long, reqwest::Method::PUT, "file")
            .query(&[
                ("storageId", storage_id),
                ("folderId", folder_id.unwrap_or("")),
                ("name", name),
            ])
            .header("Content-Type", "application/octet-stream")
            .body(bytes);
        Self::parse_file_row(Self::send(rb)?)
    }

    pub fn create_folder(
        &self,
        storage_id: &str,
        parent_id: Option<&str>,
        name: &str,
    ) -> ApiResult<RemoteFolder> {
        let rb = self
            .req(&self.http, reqwest::Method::POST, "folder")
            .json(&serde_json::json!({ "storageId": storage_id, "parentId": parent_id, "name": name }));
        let resp = Self::send(rb)?;
        let v: serde_json::Value = Self::json(resp)?;
        let row = v.get("folder").cloned().unwrap_or(v);
        serde_json::from_value(row).map_err(|e| ApiError::Decode(e.to_string()))
    }

    /// Rename and/or move. `parent`: None = unchanged, Some(None) = root.
    pub fn move_item(
        &self,
        kind: ItemType,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> ApiResult<PartialFile> {
        let mut body = serde_json::json!({ "type": kind.as_str(), "id": id });
        if let Some(n) = name {
            body["name"] = serde_json::Value::String(n.to_string());
        }
        if let Some(p) = parent {
            body["parentId"] = match p {
                Some(p) => serde_json::Value::String(p.to_string()),
                None => serde_json::Value::Null,
            };
        }
        let rb = self.req(&self.http, reqwest::Method::POST, "move").json(&body);
        // The moved row: a move bumps a file's updatedAt on the panel, and the
        // client needs the new value to keep its cache valid.
        Self::parse_file_row(Self::send(rb)?)
    }

    pub fn delete_item(&self, kind: ItemType, id: &str) -> ApiResult<()> {
        let rb = self
            .req(&self.http, reqwest::Method::DELETE, "item")
            .query(&[("type", kind.as_str()), ("id", id)]);
        match Self::send(rb) {
            Ok(_) => Ok(()),
            // Already gone is what we wanted.
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// A file row from a write endpoint; every field optional so a terse server
/// response doesn't turn a successful upload into an error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialFile {
    #[serde(default, alias = "id")]
    pub file_id: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64_lenient")]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn parse_content_range_start(v: &str) -> Option<u64> {
    // "bytes 100-199/1000"
    let rest = v.trim().strip_prefix("bytes")?.trim();
    let dash = rest.find('-')?;
    rest[..dash].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range_start("bytes 100-199/1000"), Some(100));
        assert_eq!(parse_content_range_start("bytes 0-0/*"), Some(0));
        assert_eq!(parse_content_range_start("garbage"), None);
    }

    #[test]
    fn lenient_numbers() {
        let d: DriveInfo = serde_json::from_str(
            r#"{"storageId":"s","name":"n","sizeBytes":"123","quotaBytes":null,"hasCover":true}"#,
        )
        .unwrap();
        assert_eq!(d.size_bytes, 123);
        assert_eq!(d.quota_bytes, None);
        let f: RemoteFile =
            serde_json::from_str(r#"{"fileId":"f","folderId":null,"name":"a","sizeBytes":5}"#)
                .unwrap();
        assert_eq!(f.size_bytes, 5);
    }

    #[test]
    fn origin_normalisation() {
        assert_eq!(
            normalize_origin("https://othcloud.xyz/").unwrap(),
            "https://othcloud.xyz"
        );
        assert_eq!(
            normalize_origin("http://127.0.0.1:3000/x?y").unwrap(),
            "http://127.0.0.1:3000"
        );
        assert!(normalize_origin("file:///c").is_err());
    }
}
