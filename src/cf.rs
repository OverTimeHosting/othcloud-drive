//! Thin, safe-ish wrappers over the Windows Cloud Files API (cfapi): sync
//! root registration, the callback connection, and placeholder operations.
//!
//! Everything here is policy-free; `engine.rs` decides what to do.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS, STATUS_SUCCESS};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

pub type CfResult<T> = windows::core::Result<T>;

// File attribute bits cfapi uses (not all are in the bindings' enum).
pub const ATTR_OFFLINE: u32 = 0x0000_1000;
pub const ATTR_REPARSE_POINT: u32 = 0x0000_0400;
pub const ATTR_SPARSE: u32 = 0x0000_0200;
pub const ATTR_PINNED: u32 = 0x0008_0000;
pub const ATTR_UNPINNED: u32 = 0x0010_0000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn from_pcwstr(p: PCWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        p.to_string().unwrap_or_default()
    }
}

// ------------------------------------------------------------ identity

/// What we store in a placeholder's FileIdentity: `f:<fileId>` / `d:<folderId>`.
pub fn identity(is_dir: bool, remote_id: &str) -> Vec<u8> {
    format!("{}:{remote_id}", if is_dir { 'd' } else { 'f' }).into_bytes()
}

/// Parse an identity blob back into (is_dir, remote id).
pub fn parse_identity(blob: &[u8]) -> Option<(bool, String)> {
    let s = std::str::from_utf8(blob).ok()?;
    let (k, id) = s.split_once(':')?;
    if id.is_empty() {
        return None;
    }
    match k {
        "f" => Some((false, id.to_string())),
        "d" => Some((true, id.to_string())),
        _ => None,
    }
}

// ------------------------------------------------------------ sync root

/// The current user's SID as a string (part of the sync root id).
pub fn user_sid() -> String {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return "S-1-0-0".into();
        }
        let mut buf = vec![0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(
            tok,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            buf.len() as u32,
            &mut len,
        );
        let _ = CloseHandle(tok);
        if ok.is_err() {
            return "S-1-0-0".into();
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = windows::core::PWSTR::null();
        if ConvertSidToStringSidW(tu.User.Sid, &mut s).is_err() {
            return "S-1-0-0".into();
        }
        let out = s.to_string().unwrap_or_default();
        let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
        out
    }
}

/// Sync root id in the documented `Provider!SID!Account` form.
/// Provider id of the real app's sync roots.
pub const PROVIDER: &str = "OTHCloudDrive";
/// Provider id for tests and the mount-test script, so test roots can never
/// be confused with (or clean up) a user's real ones.
pub const TEST_PROVIDER: &str = "OTHCloudDriveTest";

pub fn sync_root_id(provider: &str, account: &str) -> String {
    let acct: String = account
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    format!("{provider}!{}!{acct}", user_sid())
}

pub struct RootRegistration<'a> {
    pub id: &'a str,
    pub path: &'a Path,
    pub display_name: &'a str,
    /// "C:\path\icon.ico,0" or a system resource.
    pub icon: &'a str,
}

/// WinRT calls must run on an MTA thread; the tray's UI thread is STA, so
/// always hop to a fresh thread.
fn on_mta<T: Send + 'static>(f: impl FnOnce() -> CfResult<T> + Send + 'static) -> CfResult<T> {
    // Keep the process-wide MTA alive for the life of the process: the
    // worker threads below come and go, and windows-rs caches WinRT
    // activation factories, which must not outlive the apartment they were
    // created in (without this, the second call crashed).
    static MTA: std::sync::Once = std::sync::Once::new();
    MTA.call_once(|| unsafe {
        let _ = windows::Win32::System::Com::CoIncrementMTAUsage();
    });
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        f()
    })
    .join()
    .unwrap_or_else(|_| Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000FFFFu32 as i32))))
}

/// Register (or re-register) a sync root with the shell's
/// StorageProviderSyncRootManager.
pub fn register_sync_root(r: &RootRegistration) -> CfResult<()> {
    use windows::Security::Cryptography::{BinaryStringEncoding, CryptographicBuffer};
    use windows::Storage::Provider::*;
    use windows::Storage::StorageFolder;
    let (id, path, name, icon) = (
        HSTRING::from(r.id),
        HSTRING::from(r.path.as_os_str()),
        HSTRING::from(r.display_name),
        HSTRING::from(r.icon),
    );
    on_mta(move || {
        let folder = StorageFolder::GetFolderFromPathAsync(&path)?.get()?;
        let info = StorageProviderSyncRootInfo::new()?;
        info.SetId(&id)?;
        info.SetPath(&folder)?;
        info.SetDisplayNameResource(&name)?;
        info.SetIconResource(&icon)?;
        info.SetHydrationPolicy(StorageProviderHydrationPolicy::Progressive)?;
        info.SetHydrationPolicyModifier(
            StorageProviderHydrationPolicyModifier::AutoDehydrationAllowed,
        )?;
        info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull)?;
        info.SetInSyncPolicy(StorageProviderInSyncPolicy::Default)?;
        info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
        info.SetShowSiblingsAsGroup(false)?;
        info.SetVersion(&HSTRING::from(env!("CARGO_PKG_VERSION")))?;
        info.SetAllowPinning(true)?;
        info.SetProtectionMode(StorageProviderProtectionMode::Unknown)?;
        info.SetContext(&CryptographicBuffer::ConvertStringToBinary(
            &id,
            BinaryStringEncoding::Utf8,
        )?)?;
        StorageProviderSyncRootManager::Register(&info)
    })?;
    // (Re-)registering rewrites the shell's key, so the handler goes back on
    // every time.
    apply_thumbnail_provider(r.id);
    Ok(())
}

pub fn unregister_sync_root(id: &str) -> CfResult<()> {
    set_root_value(id, THUMBNAIL_VALUE, None);
    set_root_value(id, STATUS_VALUE, None);
    let id = HSTRING::from(id);
    on_mta(move || windows::Storage::Provider::StorageProviderSyncRootManager::Unregister(&id))
}

// ---------------------------------------------------- thumbnail handler

/// The registry value under a sync root's `SyncRootManager\<id>` key that
/// names the COM class Explorer asks for thumbnails of placeholders (what a
/// packaged provider declares as `desktop3:ThumbnailProviderHandler`). Without
/// one, the shell answers WTS_E_NOSTORAGEPROVIDERTHUMBNAILHANDLER for every
/// file that isn't downloaded, and Explorer shows only icons.
const THUMBNAIL_VALUE: &str = "ThumbnailProvider";

static THUMBNAIL_CLSID: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

/// The thumbnail handler (a COM CLSID, `{…}`) to put on every sync root this
/// process registers, or None for none. The embedding app sets it once it has
/// registered the class (OTHCloud Companion ships the handler DLL; the
/// standalone app has none). Applied to roots registered from now on; see
/// [`apply_thumbnail_provider_to_all`] for the ones already there.
pub fn set_thumbnail_provider(clsid: Option<String>) {
    *THUMBNAIL_CLSID.write() = clsid;
}

pub fn thumbnail_provider() -> Option<String> {
    THUMBNAIL_CLSID.read().clone()
}

/// The value naming the class Explorer asks for the provider's status when
/// the user opens the sync root (Windows 11's cloud-provider status; what a
/// packaged provider declares as `StorageProviderStatusUISourceFactory`).
/// The Companion's class also starts the app if it isn't running.
const STATUS_VALUE: &str = "StorageProviderStatusUISourceFactory";

static STATUS_CLSID: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

/// Like [`set_thumbnail_provider`], for the status class.
pub fn set_status_provider(clsid: Option<String>) {
    *STATUS_CLSID.write() = clsid;
}

/// Write (or remove) the shell handlers on one registered root.
pub fn apply_thumbnail_provider(id: &str) {
    let clsid = thumbnail_provider();
    set_root_value(id, THUMBNAIL_VALUE, clsid.as_deref());
    let status = STATUS_CLSID.read().clone();
    set_root_value(id, STATUS_VALUE, status.as_deref());
}

/// Bring every sync root of `provider` in line with the current handler.
pub fn apply_thumbnail_provider_to_all(provider: &str) {
    for (id, _) in sync_roots(provider) {
        apply_thumbnail_provider(&id);
    }
}

/// The shell's key for a root is under HKLM, but `BUILTIN\Users` may set
/// values in it (that is how the platform's own registration works for an
/// unelevated provider).
fn set_root_value(id: &str, name: &str, value: Option<&str>) {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE};
    let path = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\{id}");
    let Ok(k) = winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(&path, KEY_SET_VALUE | KEY_QUERY_VALUE)
    else {
        return;
    };
    let r = match value {
        Some(v) => {
            if k.get_value::<String, _>(name).ok().as_deref() == Some(v) {
                return;
            }
            k.set_value(name, &v.to_string())
        }
        None => match k.delete_value(name) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
    };
    if let Err(e) = r {
        tracing::warn!("{id}: setting {name}: {e}");
    }
}

/// The handler named on a registered root (tests, diagnostics).
pub fn root_thumbnail_provider(id: &str) -> Option<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\{id}"))
        .and_then(|k| k.get_value::<String, _>(THUMBNAIL_VALUE))
        .ok()
}

/// Path of a registered sync root, if registered.
pub fn sync_root_path(id: &str) -> Option<PathBuf> {
    sync_roots(id.split('!').next().unwrap_or(PROVIDER))
        .into_iter()
        .find(|(i, _)| i == id)
        .map(|(_, p)| p)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Ids of every sync root registered by this app for this user.
/// This user's sync roots of the real app.
pub fn our_sync_roots() -> Vec<(String, PathBuf)> {
    sync_roots(PROVIDER)
}

/// This user's sync roots registered under `provider`.
pub fn sync_roots(provider: &str) -> Vec<(String, PathBuf)> {
    // StorageProviderSyncRootManager::GetCurrentSyncRoots only returns the
    // calling PACKAGE's roots (nothing for an unpackaged app), so read the
    // shell's registrations directly.
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let sid = user_sid();
    let prefix = format!("{provider}!{sid}!");
    let mut out = Vec::new();
    let Ok(k) = winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager")
    else {
        return out;
    };
    for name in k.enum_keys().flatten() {
        if !name.starts_with(&prefix) {
            continue;
        }
        let path = k
            .open_subkey(format!(r"{name}\UserSyncRoots"))
            .and_then(|u| u.get_value::<String, _>(&sid))
            .unwrap_or_default();
        out.push((name, PathBuf::from(path)));
    }
    out
}

// ----------------------------------------------------------- callbacks

/// A FETCH_DATA request, answered from another thread.
pub struct FetchRequest {
    pub connection_key: CF_CONNECTION_KEY,
    pub transfer_key: i64,
    pub request_key: i64,
    pub path: PathBuf,
    pub identity: Vec<u8>,
    pub file_size: u64,
    pub offset: u64,
    pub length: u64,
    pub optional_offset: u64,
    pub optional_length: u64,
}
unsafe impl Send for FetchRequest {}

impl FetchRequest {
    fn op_info(&self, ty: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
        CF_OPERATION_INFO {
            StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
            Type: ty,
            ConnectionKey: self.connection_key,
            TransferKey: self.transfer_key,
            CorrelationVector: std::ptr::null(),
            SyncStatus: std::ptr::null(),
            RequestKey: self.request_key,
        }
    }

    /// Hand `data` (at `offset`, 4 KiB aligned; length a multiple of 4 KiB
    /// unless it ends at EOF) to the platform.
    pub fn transfer(&self, offset: u64, data: &[u8]) -> CfResult<()> {
        self.execute_transfer(STATUS_SUCCESS, offset, data)
    }

    pub fn fail(&self, status: NTSTATUS) {
        let _ = self.execute_transfer(status, self.offset, &[]);
    }

    fn execute_transfer(&self, status: NTSTATUS, offset: u64, data: &[u8]) -> CfResult<()> {
        let info = self.op_info(CF_OPERATION_TYPE_TRANSFER_DATA);
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                TransferData: CF_OPERATION_PARAMETERS_0_0 {
                    Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                    CompletionStatus: status,
                    Buffer: data.as_ptr() as *const c_void,
                    Offset: offset as i64,
                    Length: if status == STATUS_SUCCESS {
                        data.len() as i64
                    } else {
                        self.length.max(1) as i64
                    },
                },
            },
        };
        unsafe { CfExecute(&info, &mut params) }
    }
}

pub struct DeleteNotice {
    pub path: PathBuf,
    pub is_dir: bool,
    pub identity: Vec<u8>,
    pub pid: u32,
}

pub struct RenameNotice {
    pub from: PathBuf,
    pub to: PathBuf,
    pub is_dir: bool,
    pub identity: Vec<u8>,
    pub target_in_scope: bool,
    pub source_in_scope: bool,
    pub pid: u32,
}

/// What the engine implements.
pub trait Callbacks: Send + Sync + 'static {
    /// Must eventually call `transfer`/`fail` (may do so on another thread).
    fn fetch_data(&self, req: FetchRequest);
    /// Before a placeholder is deleted. Return STATUS_SUCCESS to allow it.
    fn notify_delete(&self, n: DeleteNotice) -> NTSTATUS;
    /// Before a placeholder is renamed/moved. Return STATUS_SUCCESS to allow it.
    fn notify_rename(&self, n: RenameNotice) -> NTSTATUS;
}

struct Ctx {
    cb: Arc<dyn Callbacks>,
}

unsafe fn full_path(info: &CF_CALLBACK_INFO, p: PCWSTR) -> PathBuf {
    // NormalizedPath / TargetPath are volume-relative ("\Users\…").
    let vol = from_pcwstr(info.VolumeDosName);
    let rel = from_pcwstr(p);
    if rel.len() >= 2 && rel.as_bytes()[1] == b':' {
        PathBuf::from(rel)
    } else {
        PathBuf::from(format!("{vol}{rel}"))
    }
}

unsafe fn blob(ptr: *const c_void, len: u32) -> Vec<u8> {
    if ptr.is_null() || len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec()
    }
}

unsafe fn ctx(info: &CF_CALLBACK_INFO) -> &Ctx {
    &*(info.CallbackContext as *const Ctx)
}

fn pid_of(info: &CF_CALLBACK_INFO) -> u32 {
    unsafe { info.ProcessInfo.as_ref().map(|p| p.ProcessId).unwrap_or(0) }
}

unsafe extern "system" fn on_fetch_data(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let p = (*params).Anonymous.FetchData;
    let req = FetchRequest {
        connection_key: info.ConnectionKey,
        transfer_key: info.TransferKey,
        request_key: info.RequestKey,
        path: full_path(info, info.NormalizedPath),
        identity: blob(info.FileIdentity, info.FileIdentityLength),
        file_size: info.FileSize.max(0) as u64,
        offset: p.RequiredFileOffset.max(0) as u64,
        length: p.RequiredLength.max(0) as u64,
        optional_offset: p.OptionalFileOffset.max(0) as u64,
        optional_length: p.OptionalLength.max(0) as u64,
    };
    ctx(info).cb.fetch_data(req);
}

unsafe extern "system" fn on_cancel_fetch(_info: *const CF_CALLBACK_INFO, _p: *const CF_CALLBACK_PARAMETERS) {
    // Nothing to cancel eagerly: an in-flight transfer just fails when the
    // platform no longer wants it.
}

unsafe fn ack(info: &CF_CALLBACK_INFO, ty: CF_OPERATION_TYPE, status: NTSTATUS) {
    let op = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: ty,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        ..Default::default()
    };
    if ty == CF_OPERATION_TYPE_ACK_DELETE {
        params.Anonymous.AckDelete = CF_OPERATION_PARAMETERS_0_7 {
            Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
            CompletionStatus: status,
        };
    } else {
        params.Anonymous.AckRename = CF_OPERATION_PARAMETERS_0_6 {
            Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
            CompletionStatus: status,
        };
    }
    if let Err(e) = CfExecute(&op, &mut params) {
        tracing::warn!("cfapi ack failed: {e}");
    }
}

unsafe extern "system" fn on_notify_delete(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let flags = (*params).Anonymous.Delete.Flags;
    let n = DeleteNotice {
        path: full_path(info, info.NormalizedPath),
        is_dir: flags.contains(CF_CALLBACK_DELETE_FLAG_IS_DIRECTORY),
        identity: blob(info.FileIdentity, info.FileIdentityLength),
        pid: pid_of(info),
    };
    let st = if flags.contains(CF_CALLBACK_DELETE_FLAG_IS_UNDELETE) {
        STATUS_SUCCESS
    } else {
        ctx(info).cb.notify_delete(n)
    };
    ack(info, CF_OPERATION_TYPE_ACK_DELETE, st);
}

unsafe extern "system" fn on_notify_rename(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let r = (*params).Anonymous.Rename;
    let n = RenameNotice {
        from: full_path(info, info.NormalizedPath),
        to: full_path(info, r.TargetPath),
        is_dir: r.Flags.contains(CF_CALLBACK_RENAME_FLAG_IS_DIRECTORY),
        identity: blob(info.FileIdentity, info.FileIdentityLength),
        target_in_scope: r.Flags.contains(CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE),
        source_in_scope: r.Flags.contains(CF_CALLBACK_RENAME_FLAG_SOURCE_IN_SCOPE),
        pid: pid_of(info),
    };
    let st = ctx(info).cb.notify_rename(n);
    ack(info, CF_OPERATION_TYPE_ACK_RENAME, st);
}

/// A live connection; dropping it disconnects.
pub struct Connection {
    key: CF_CONNECTION_KEY,
    ctx: *mut Ctx,
}
unsafe impl Send for Connection {}
unsafe impl Sync for Connection {}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            if let Err(e) = CfDisconnectSyncRoot(self.key) {
                tracing::warn!("CfDisconnectSyncRoot: {e}");
            }
            // Callbacks can't run after disconnect returns.
            drop(Box::from_raw(self.ctx));
        }
    }
}

pub fn connect(root: &Path, cb: Arc<dyn Callbacks>) -> CfResult<Connection> {
    let table = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            Callback: Some(on_cancel_fetch),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(on_notify_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(on_notify_rename),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    let ctx = Box::into_raw(Box::new(Ctx { cb }));
    let w = wide(root);
    let r = unsafe {
        CfConnectSyncRoot(
            PCWSTR(w.as_ptr()),
            table.as_ptr(),
            Some(ctx as *const c_void),
            CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
        )
    };
    match r {
        Ok(key) => Ok(Connection { key, ctx }),
        Err(e) => {
            unsafe { drop(Box::from_raw(ctx)) };
            Err(e)
        }
    }
}

// --------------------------------------------------------- placeholders

#[derive(Debug, Clone, Copy)]
pub struct Meta {
    pub is_dir: bool,
    pub size: u64,
    /// FILETIMEs
    pub created: i64,
    pub modified: i64,
}

fn fs_metadata(m: &Meta) -> CF_FS_METADATA {
    CF_FS_METADATA {
        FileSize: if m.is_dir { 0 } else { m.size as i64 },
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: m.created,
            LastAccessTime: m.modified,
            LastWriteTime: m.modified,
            ChangeTime: m.modified,
            FileAttributes: if m.is_dir {
                FILE_ATTRIBUTE_DIRECTORY.0
            } else {
                FILE_ATTRIBUTE_NORMAL.0
            },
        },
    }
}

/// Create one placeholder `name` in directory `dir` (in sync, dehydrated).
pub fn create_placeholder(dir: &Path, name: &str, meta: &Meta, ident: &[u8]) -> CfResult<()> {
    let wname = wide_str(name);
    let mut flags = CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;
    if meta.is_dir {
        flags |= CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION;
    }
    let mut info = [CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(wname.as_ptr()),
        FsMetadata: fs_metadata(meta),
        FileIdentity: ident.as_ptr() as *const c_void,
        FileIdentityLength: ident.len() as u32,
        Flags: flags,
        Result: windows::core::HRESULT(0),
        CreateUsn: 0,
    }];
    let wdir = wide(dir);
    let mut done = 0u32;
    unsafe {
        CfCreatePlaceholders(
            PCWSTR(wdir.as_ptr()),
            &mut info,
            CF_CREATE_FLAG_NONE,
            Some(&mut done),
        )?;
    }
    info[0].Result.ok()
}

/// Open for metadata only (never triggers hydration).
fn open_attr(path: &Path) -> CfResult<HANDLE> {
    let w = wide(path);
    unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
}

struct Handle(HANDLE, bool);
impl Handle {
    /// The Win32 handle (a protected cfapi handle is unwrapped; most Cf*
    /// functions reject protected handles).
    fn raw(&self) -> HANDLE {
        if self.1 {
            unsafe { CfGetWin32HandleFromProtectedHandle(self.0) }
        } else {
            self.0
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            if self.1 {
                CfCloseHandle(self.0);
            } else {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn open_cf(path: &Path, flags: CF_OPEN_FILE_FLAGS) -> CfResult<Handle> {
    let w = wide(path);
    let h = unsafe { CfOpenFileWithOplock(PCWSTR(w.as_ptr()), flags)? };
    Ok(Handle(h, true))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderInfo {
    pub in_sync: bool,
    pub pinned: bool,
    pub unpinned: bool,
    pub identity: Vec<u8>,
}

/// None if `path` is not a placeholder.
pub fn placeholder_info(path: &Path) -> CfResult<Option<PlaceholderInfo>> {
    let h = Handle(open_attr(path)?, false);
    let mut buf = vec![0u8; 4096];
    let mut ret = 0u32;
    let r = unsafe {
        CfGetPlaceholderInfo(
            h.0,
            CF_PLACEHOLDER_INFO_BASIC,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            Some(&mut ret),
        )
    };
    if let Err(e) = r {
        // ERROR_NOT_A_CLOUD_FILE
        if e.code().0 as u32 == 0x8007_0178 {
            return Ok(None);
        }
        return Err(e);
    }
    let bi = unsafe { &*(buf.as_ptr() as *const CF_PLACEHOLDER_BASIC_INFO) };
    let off = std::mem::offset_of!(CF_PLACEHOLDER_BASIC_INFO, FileIdentity);
    let len = (bi.FileIdentityLength as usize).min(buf.len() - off);
    Ok(Some(PlaceholderInfo {
        in_sync: bi.InSyncState == CF_IN_SYNC_STATE_IN_SYNC,
        pinned: bi.PinState == CF_PIN_STATE_PINNED,
        unpinned: bi.PinState == CF_PIN_STATE_UNPINNED,
        identity: buf[off..off + len].to_vec(),
    }))
}

/// Raw attributes (for status: RECALL_ON_DATA_ACCESS = not fully local).
pub fn attributes(path: &Path) -> Option<u32> {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| m.file_attributes())
}

/// Make an ordinary file/folder a placeholder with `ident`, in sync.
pub fn convert_to_placeholder(path: &Path, ident: &[u8]) -> CfResult<()> {
    let h = open_cf(path, CF_OPEN_FILE_FLAG_WRITE_ACCESS)?;
    let is_dir = path.is_dir();
    let mut flags = CF_CONVERT_FLAG_MARK_IN_SYNC;
    if is_dir {
        // Full population: nothing to fetch on enumeration.
        flags |= CF_CONVERT_FLAG_NONE;
    }
    unsafe {
        CfConvertToPlaceholder(
            h.raw(),
            Some(ident.as_ptr() as *const c_void),
            ident.len() as u32,
            flags,
            None,
            None,
        )
    }
}

pub fn set_in_sync(path: &Path) -> CfResult<()> {
    let h = open_cf(path, CF_OPEN_FILE_FLAG_WRITE_ACCESS)?;
    unsafe { CfSetInSyncState(h.raw(), CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAG_NONE, None) }
}

/// Server content changed: new metadata/identity, drop local bytes, in sync.
/// With `verify`, fails if the file has unsent local changes.
pub fn update_placeholder(path: &Path, meta: &Meta, ident: &[u8], dehydrate: bool) -> CfResult<()> {
    update(path, meta, ident, dehydrate, true)
}

/// New identity + metadata for a file we just uploaded (it isn't in sync
/// yet, so no verification), marking it in sync.
pub fn update_placeholder_unverified(path: &Path, meta: &Meta, ident: &[u8]) -> CfResult<()> {
    update(path, meta, ident, false, false)
}

fn update(path: &Path, meta: &Meta, ident: &[u8], dehydrate: bool, verify: bool) -> CfResult<()> {
    let flags_open = if dehydrate {
        CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS
    } else {
        CF_OPEN_FILE_FLAG_WRITE_ACCESS
    };
    let h = open_cf(path, flags_open)?;
    let md = fs_metadata(meta);
    let mut flags = CF_UPDATE_FLAG_MARK_IN_SYNC;
    if verify {
        flags |= CF_UPDATE_FLAG_VERIFY_IN_SYNC;
    }
    if dehydrate && !meta.is_dir {
        flags |= CF_UPDATE_FLAG_DEHYDRATE;
    }
    unsafe {
        CfUpdatePlaceholder(
            h.raw(),
            Some(&md),
            Some(ident.as_ptr() as *const c_void),
            ident.len() as u32,
            None,
            flags,
            None,
            None,
        )
    }
}

pub fn hydrate(path: &Path) -> CfResult<()> {
    let h = open_cf(path, CF_OPEN_FILE_FLAG_NONE)?;
    unsafe { CfHydratePlaceholder(h.raw(), 0, -1, CF_HYDRATE_FLAG_NONE, None) }
}

pub fn dehydrate(path: &Path) -> CfResult<()> {
    let h = open_cf(path, CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS)?;
    unsafe { CfDehydratePlaceholder(h.raw(), 0, -1, CF_DEHYDRATE_FLAG_NONE, None) }
}

/// ERROR_CLOUD_FILE_PINNED: Windows won't dehydrate a pinned file ("Always keep
/// on this device") — including one pinned only through its folder.
pub fn is_pinned_error(e: &windows::core::Error) -> bool {
    e.code().0 as u32 == 0x8007_0188
}

/// Clear a file's own pin state so it can be dehydrated (see
/// [`restore_pin`]). Changes nothing on the folder it inherits from.
pub fn clear_pin(path: &Path) -> CfResult<()> {
    let h = open_cf(path, CF_OPEN_FILE_FLAG_NONE)?;
    unsafe { CfSetPinState(h.raw(), CF_PIN_STATE_UNSPECIFIED, CF_SET_PIN_FLAG_NONE, None) }
}

/// Put a pin state back after [`clear_pin`]: pinned itself, or following its
/// folder again.
pub fn restore_pin(path: &Path, explicitly_pinned: bool) -> CfResult<()> {
    let h = open_cf(path, CF_OPEN_FILE_FLAG_NONE)?;
    let st = if explicitly_pinned { CF_PIN_STATE_PINNED } else { CF_PIN_STATE_INHERIT };
    unsafe { CfSetPinState(h.raw(), st, CF_SET_PIN_FLAG_NONE, None) }
}

pub fn set_pin(path: &Path, pinned: bool) -> CfResult<()> {
    let h = open_cf(path, CF_OPEN_FILE_FLAG_NONE)?;
    let st = if pinned { CF_PIN_STATE_PINNED } else { CF_PIN_STATE_UNPINNED };
    unsafe { CfSetPinState(h.raw(), st, CF_SET_PIN_FLAG_NONE, None) }
}

/// (hydration primary, hydration modifier, population primary) of the sync
/// root containing `path` — what the platform actually enforces.
pub fn root_policies(path: &Path) -> CfResult<(u16, u16, u16)> {
    let w = wide(path);
    let mut buf = vec![0u8; std::mem::size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + 4096];
    let mut ret = 0u32;
    unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(w.as_ptr()),
            CF_SYNC_ROOT_INFO_STANDARD,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            Some(&mut ret),
        )?;
        let i = &*(buf.as_ptr() as *const CF_SYNC_ROOT_STANDARD_INFO);
        Ok((i.HydrationPolicy.Primary.0, i.HydrationPolicy.Modifier.0, i.PopulationPolicy.Primary.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities() {
        assert_eq!(parse_identity(&identity(false, "file_1")), Some((false, "file_1".into())));
        assert_eq!(parse_identity(&identity(true, "fold_1")), Some((true, "fold_1".into())));
        assert_eq!(parse_identity(b"x:1"), None);
        assert_eq!(parse_identity(b"f:"), None);
        assert_eq!(parse_identity(&[0xff, 0xfe]), None);
    }

    /// Unregisters on drop, so a failing assertion can't leak a sync root
    /// into the user's Explorer.
    struct RootGuard(Vec<String>);
    impl Drop for RootGuard {
        fn drop(&mut self) {
            for id in &self.0 {
                let _ = unregister_sync_root(id);
            }
        }
    }

    #[test]
    fn register_and_unregister_several_roots_in_one_process() {
        // Regression: the 2nd WinRT call from a fresh thread used to crash.
        let base = tempfile::tempdir().unwrap();
        let mut guard = RootGuard(Vec::new());
        let mut ids = Vec::new();
        for i in 0..3 {
            let p = base.path().join(format!("root{i}"));
            std::fs::create_dir_all(&p).unwrap();
            let id = sync_root_id(TEST_PROVIDER, &format!("unit_{}_{i}", std::process::id()));
            guard.0.push(id.clone());
            register_sync_root(&RootRegistration {
                id: &id,
                path: &p,
                display_name: "unit test",
                icon: r"%SystemRoot%\System32\imageres.dll,-1043",
            })
            .unwrap();
            ids.push((id, p));
        }
        let listed = sync_roots(TEST_PROVIDER);
        for (id, p) in &ids {
            assert!(listed.iter().any(|(i, lp)| i == id && lp == p), "{id} listed");
            assert_eq!(sync_root_path(id).as_deref(), Some(p.as_path()));
            // No handler configured in this process: none written.
            assert_eq!(root_thumbnail_provider(id), None);
        }
        // The handler value goes on (and comes off) without elevation.
        let clsid = "{00000000-1111-2222-3333-444444444444}";
        set_root_value(&ids[0].0, THUMBNAIL_VALUE, Some(clsid));
        assert_eq!(root_thumbnail_provider(&ids[0].0).as_deref(), Some(clsid));
        set_root_value(&ids[0].0, THUMBNAIL_VALUE, None);
        assert_eq!(root_thumbnail_provider(&ids[0].0), None);
        assert!(!our_sync_roots().iter().any(|(i, _)| ids.iter().any(|(t, _)| t == i)), "test roots never show as the app's");
        for (id, _) in &ids {
            unregister_sync_root(id).unwrap();
        }
        let listed = sync_roots(TEST_PROVIDER);
        assert!(ids.iter().all(|(id, _)| !listed.iter().any(|(i, _)| i == id)));
    }

    #[test]
    fn sid_and_root_id() {
        let sid = user_sid();
        assert!(sid.starts_with("S-1-"), "{sid}");
        let id = sync_root_id(PROVIDER, "stor_a/b");
        assert!(id.starts_with("OTHCloudDrive!S-1-"));
        assert!(id.ends_with("!stor_a_b"));
        assert!(sync_root_id(TEST_PROVIDER, "x").starts_with("OTHCloudDriveTest!S-1-"));
    }
}
