//! Single instance: the first process holds a named mutex and serves a named
//! pipe; a second launch (e.g. Windows handing us an `othcloud-drive://`
//! link) writes its argument to the pipe and exits.
//!
//! Both are per-user (`Local\` namespace, the user's name in the pipe name;
//! a pipe's default DACL only lets its creator's account write to it).

use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;

fn user_tag() -> String {
    let u = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    u.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub fn pipe_name() -> String {
    std::env::var("OTHCLOUD_DRIVE_PIPE")
        .unwrap_or_else(|_| format!(r"\\.\pipe\othcloud-drive-{}", user_tag()))
}

/// Holds the instance mutex for the life of the process.
pub struct InstanceLock(windows::Win32::Foundation::HANDLE);
unsafe impl Send for InstanceLock {}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Some(lock) if we are the first instance.
pub fn acquire_instance() -> Option<InstanceLock> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = std::env::var("OTHCLOUD_DRIVE_MUTEX")
        .unwrap_or_else(|_| format!(r"Local\OTHCloudDrive-{}", user_tag()));
    unsafe {
        let h = CreateMutexW(None, true, &HSTRING::from(name)).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = windows::Win32::Foundation::CloseHandle(h);
            return None;
        }
        Some(InstanceLock(h))
    }
}

/// Send one message to the running instance. Retries briefly while the
/// server is between connections.
pub fn send(msg: &str) -> std::io::Result<()> {
    let name = pipe_name();
    let mut last = None;
    for _ in 0..50 {
        match std::fs::OpenOptions::new().write(true).open(&name) {
            Ok(mut f) => {
                f.write_all(msg.as_bytes())?;
                return f.flush();
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("pipe unavailable")))
}

/// Serve the pipe on a background thread; `on_msg` gets each message.
pub fn serve<F: Fn(String) + Send + 'static>(on_msg: F) {
    std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || loop {
            match accept_one() {
                Ok(Some(m)) => on_msg(m),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("ipc pipe: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        })
        .expect("spawn ipc");
}

fn accept_one() -> std::io::Result<Option<String>> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let h = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(pipe_name()),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            0,
            8192,
            0,
            None,
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_invalid() {
        return Err(std::io::Error::last_os_error());
    }
    // Owns the handle from here (closes on drop).
    let mut f = unsafe { std::fs::File::from_raw_handle(h.0) };
    let connected = unsafe { ConnectNamedPipe(h, None) };
    if connected.is_err() && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
        return Ok(None);
    }
    let mut buf = Vec::new();
    let _ = (&mut f).take(64 * 1024).read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf).trim().to_string();
    Ok((!s.is_empty()).then_some(s))
}

// ------------------------------------------------------------ thumbnails
//
// A second pipe, for the shell thumbnail handler (a DLL Explorer loads; see
// PROTOCOL.md "Thumbnails"). The DLL never sees the token: it sends a path
// and a size, this process checks the path is a placeholder in one of its
// sync roots and answers with image bytes it fetched from the panel.
//
//   request  "OTHQ" | u32 version (1) | u32 size px | u32 path bytes | path (UTF-16LE)
//   response "OTHR" | u32 status      | u32 body bytes | body (image bytes when status 0)
//
// All integers little-endian. Both sides enforce the limits below.

pub const THUMB_REQ_MAGIC: &[u8; 4] = b"OTHQ";
pub const THUMB_RESP_MAGIC: &[u8; 4] = b"OTHR";
pub const THUMB_VERSION: u32 = 1;
/// MAX_PATH-free Windows paths are at most 32767 UTF-16 units.
pub const THUMB_MAX_PATH_BYTES: u32 = 32767 * 2;
/// The biggest body the server sends (a thumbnail, not a photo).
pub const THUMB_MAX_BODY: u32 = crate::api::MAX_THUMBNAIL_BYTES as u32;
/// Connections handled at once; past that the answer is `Busy` at once.
const THUMB_MAX_ACTIVE: usize = 24;
/// One connection's whole life, request to response, on this side. The DLL
/// gives up at 5 s, so there is no point holding a thread longer.
const THUMB_DEADLINE: std::time::Duration = std::time::Duration::from_secs(6);

/// Why there is no thumbnail (the response's status; 0 is success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ThumbStatus {
    /// Not a placeholder in one of this app's sync roots.
    NotOurs = 1,
    /// Ours, but no picture to show (not an image, local changes, …).
    NoThumbnail = 2,
    /// The panel couldn't be asked or answered with an error.
    Failed = 3,
    Busy = 4,
    BadRequest = 5,
}

pub type ThumbHandler = std::sync::Arc<dyn Fn(&std::path::Path, u32) -> Result<Vec<u8>, ThumbStatus> + Send + Sync>;

pub fn thumbs_pipe_name() -> String {
    std::env::var("OTHCLOUD_DRIVE_THUMBS_PIPE")
        .unwrap_or_else(|_| format!(r"\\.\pipe\othcloud-drive-thumbs-{}", crate::cf::user_sid()))
}

/// The backup status request on the same pipe (PROTOCOL.md "Backup status
/// in File Explorer"): "OTHS" | u32 version (1) | u32 flags (0) | u32 path
/// bytes | directory path (UTF-16LE). Answered with the marks of the
/// directory's entries (`shellstatus::encode_marks`) or `NotOurs`.
pub const STATUS_REQ_MAGIC: &[u8; 4] = b"OTHS";

/// A status request's path length, from its header.
pub fn parse_status_header(h: &[u8; 16]) -> Result<u32, ThumbStatus> {
    let u = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
    if &h[..4] != STATUS_REQ_MAGIC || u(4) != THUMB_VERSION || u(8) != 0 {
        return Err(ThumbStatus::BadRequest);
    }
    let plen = u(12);
    if plen == 0 || plen > THUMB_MAX_PATH_BYTES || plen % 2 != 0 {
        return Err(ThumbStatus::BadRequest);
    }
    Ok(plen)
}

/// A request parsed from its header and path bytes.
pub fn parse_thumb_header(h: &[u8; 16]) -> Result<(u32, u32), ThumbStatus> {
    let u = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
    if &h[..4] != THUMB_REQ_MAGIC || u(4) != THUMB_VERSION {
        return Err(ThumbStatus::BadRequest);
    }
    let (size, plen) = (u(8), u(12));
    if size == 0 || size > 4096 || plen == 0 || plen > THUMB_MAX_PATH_BYTES || plen % 2 != 0 {
        return Err(ThumbStatus::BadRequest);
    }
    Ok((size, plen))
}

pub fn encode_thumb_request(path: &std::path::Path, size: u32) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut out = Vec::with_capacity(16 + w.len() * 2);
    out.extend_from_slice(THUMB_REQ_MAGIC);
    out.extend_from_slice(&THUMB_VERSION.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&((w.len() * 2) as u32).to_le_bytes());
    for c in w {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out
}

fn encode_thumb_response(r: &Result<Vec<u8>, ThumbStatus>) -> Vec<u8> {
    let (status, body): (u32, &[u8]) = match r {
        Ok(b) if b.len() <= THUMB_MAX_BODY as usize => (0, b),
        Ok(_) => (ThumbStatus::Failed as u32, &[]),
        Err(s) => (*s as u32, &[]),
    };
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(THUMB_RESP_MAGIC);
    out.extend_from_slice(&status.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// A pipe instance handle, closed on drop.
struct Pipe(windows::Win32::Foundation::HANDLE);
unsafe impl Send for Pipe {}
impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Pipes::DisconnectNamedPipe(self.0);
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// One overlapped operation with a deadline: started by `start`, waited for
/// here, cancelled (and drained) if the deadline passes.
fn overlapped(
    h: windows::Win32::Foundation::HANDLE,
    deadline: Option<std::time::Instant>,
    start: impl FnOnce(*mut windows::Win32::System::IO::OVERLAPPED) -> windows::core::Result<()>,
) -> std::io::Result<u32> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    let ev = unsafe { CreateEventW(None, true, false, None) }.map_err(std::io::Error::other)?;
    let mut ov = OVERLAPPED { hEvent: ev, ..Default::default() };
    let r = (|| {
        if let Err(e) = start(&mut ov) {
            if e.code() != ERROR_IO_PENDING.to_hresult() {
                return Err(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF));
            }
        }
        let ms = match deadline {
            None => INFINITE,
            Some(d) => d.saturating_duration_since(std::time::Instant::now()).as_millis().min(u32::MAX as u128 - 1) as u32,
        };
        let w = unsafe { WaitForSingleObject(ev, ms) };
        let mut n = 0u32;
        if w != WAIT_OBJECT_0 {
            unsafe {
                let _ = CancelIoEx(h, Some(&ov));
                let _ = GetOverlappedResult(h, &ov, &mut n, true);
            }
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
        }
        unsafe { GetOverlappedResult(h, &ov, &mut n, false) }
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
        Ok(n)
    })();
    unsafe {
        let _ = CloseHandle(ev);
    }
    r
}

fn read_exact_by(h: windows::Win32::Foundation::HANDLE, buf: &mut [u8], deadline: std::time::Instant) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::ReadFile;
    let mut off = 0;
    while off < buf.len() {
        let chunk = &mut buf[off..];
        let n = overlapped(h, Some(deadline), |ov| unsafe { ReadFile(h, Some(chunk), None, Some(ov)) })?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        off += n as usize;
    }
    Ok(())
}

fn write_all_by(h: windows::Win32::Foundation::HANDLE, mut buf: &[u8], deadline: std::time::Instant) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::WriteFile;
    while !buf.is_empty() {
        let chunk = buf;
        let n = overlapped(h, Some(deadline), |ov| unsafe { WriteFile(h, Some(chunk), None, Some(ov)) })?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// A pipe instance only this user (and SYSTEM) can open, local clients only.
fn create_thumb_instance(first: bool) -> std::io::Result<Pipe> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{LocalFree, HLOCAL, INVALID_HANDLE_VALUE};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    let sddl = HSTRING::from(format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", crate::cf::user_sid()));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None) }
        .map_err(std::io::Error::other)?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        // Fails if anything else already serves this name (a squatter).
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let h = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(thumbs_pipe_name()),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            4 * 1024,
            0,
            Some(&sa),
        )
    };
    let err = std::io::Error::last_os_error();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    if h == INVALID_HANDLE_VALUE || h.is_invalid() {
        return Err(err);
    }
    Ok(Pipe(h))
}

fn serve_thumb_conn(p: Pipe, handler: &ThumbHandler, busy: bool) {
    let deadline = std::time::Instant::now() + THUMB_DEADLINE;
    let h = p.0;
    let mut head = [0u8; 16];
    let result = (|| {
        read_exact_by(h, &mut head, deadline).map_err(|_| ThumbStatus::BadRequest)?;
        let status = &head[..4] == STATUS_REQ_MAGIC;
        let (size, plen) = if status { (0, parse_status_header(&head)?) } else { parse_thumb_header(&head)? };
        let mut raw = vec![0u8; plen as usize];
        read_exact_by(h, &mut raw, deadline).map_err(|_| ThumbStatus::BadRequest)?;
        if busy {
            return Err(ThumbStatus::Busy);
        }
        let wide: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        if wide.contains(&0) {
            return Err(ThumbStatus::BadRequest);
        }
        use std::os::windows::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&wide));
        if status {
            // Local only: the backup's state and one directory listing.
            return crate::shellstatus::answer(&path, THUMB_MAX_BODY as usize).ok_or(ThumbStatus::NotOurs);
        }
        handler(&path, size)
    })();
    if write_all_by(h, &encode_thumb_response(&result), deadline).is_ok() {
        // Let the client read it all before the instance is torn down: it
        // closes its end once it has, which ends this read.
        let mut b = [0u8; 1];
        let _ = read_exact_by(h, &mut b, deadline.min(std::time::Instant::now() + std::time::Duration::from_secs(1)));
    }
}

/// Serve the thumbnail pipe on background threads. Errors if the pipe can't
/// be created as its first instance (another process already serves it).
pub fn serve_thumbnails(handler: ThumbHandler) -> std::io::Result<()> {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    let first = create_thumb_instance(true)?;
    let active = Arc::new(AtomicUsize::new(0));
    // Several instances listen at once, so a burst (Explorer asks for a
    // folder's worth together) rarely sees ERROR_PIPE_BUSY; the client
    // waits for an instance when it does.
    let mut first = Some(first);
    for i in 0..THUMB_LISTENERS {
        let (handler, active, mine) = (handler.clone(), active.clone(), first.take());
        std::thread::Builder::new()
            .name(format!("thumb-pipe-{i}"))
            .spawn(move || thumb_accept_loop(mine, handler, active))?;
    }
    Ok(())
}

/// Instances listening at any moment.
const THUMB_LISTENERS: usize = 4;

fn thumb_accept_loop(mut next: Option<Pipe>, handler: ThumbHandler, active: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::ERROR_PIPE_CONNECTED;
    use windows::Win32::System::Pipes::ConnectNamedPipe;
    loop {
        let p = match next.take().map(Ok).unwrap_or_else(|| create_thumb_instance(false)) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("thumbnail pipe: {e}");
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };
        let h = p.0;
        match overlapped(h, None, |ov| unsafe { ConnectNamedPipe(h, Some(ov)) }) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_CONNECTED.0 as i32) => {}
            Err(e) => {
                tracing::debug!("thumbnail pipe connect: {e}");
                continue;
            }
        }
        let busy = active.fetch_add(1, Ordering::SeqCst) >= THUMB_MAX_ACTIVE;
        let (h2, a2) = (handler.clone(), active.clone());
        let spawned = std::thread::Builder::new().name("thumb-conn".into()).spawn(move || {
            serve_thumb_conn(p, &h2, busy);
            a2.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            // No thread: the instance was dropped with the closure (refused).
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("thumbnail pipe: no thread for a request");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn thumb_header_limits() {
        use super::*;
        let ok = encode_thumb_request(std::path::Path::new(r"C:\a\b.jpg"), 256);
        let h: [u8; 16] = ok[..16].try_into().unwrap();
        assert_eq!(parse_thumb_header(&h), Ok((256, 20)));
        let mut bad = h;
        bad[0] = b'X';
        assert_eq!(parse_thumb_header(&bad), Err(ThumbStatus::BadRequest));
        let mut big = h;
        big[8..12].copy_from_slice(&5000u32.to_le_bytes());
        assert_eq!(parse_thumb_header(&big), Err(ThumbStatus::BadRequest));
        let mut odd = h;
        odd[12..16].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(parse_thumb_header(&odd), Err(ThumbStatus::BadRequest));
        let mut huge = h;
        huge[12..16].copy_from_slice(&(THUMB_MAX_PATH_BYTES + 2).to_le_bytes());
        assert_eq!(parse_thumb_header(&huge), Err(ThumbStatus::BadRequest));
    }

    /// A minimal blocking client (the real one is the shell DLL).
    fn ask(path: &str, size: u32) -> (u32, Vec<u8>) {
        use std::io::{Read, Write};
        // As the DLL does: every listening instance taken → wait for one.
        let mut f = loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(super::thumbs_pipe_name()) {
                Ok(f) => break f,
                Err(e) if e.raw_os_error() == Some(231) => unsafe {
                    let _ = windows::Win32::System::Pipes::WaitNamedPipeW(
                        &windows::core::HSTRING::from(super::thumbs_pipe_name()),
                        2000,
                    );
                },
                Err(e) => panic!("{e}"),
            }
        };
        f.write_all(&super::encode_thumb_request(std::path::Path::new(path), size)).unwrap();
        let mut head = [0u8; 12];
        f.read_exact(&mut head).unwrap();
        assert_eq!(&head[..4], super::THUMB_RESP_MAGIC);
        let status = u32::from_le_bytes(head[4..8].try_into().unwrap());
        let n = u32::from_le_bytes(head[8..12].try_into().unwrap());
        let mut body = vec![0u8; n as usize];
        f.read_exact(&mut body).unwrap();
        (status, body)
    }

    #[test]
    fn thumb_pipe_round_trip() {
        std::env::set_var(
            "OTHCLOUD_DRIVE_THUMBS_PIPE",
            format!(r"\\.\pipe\othcloud-drive-thumbs-test-{}", std::process::id()),
        );
        super::serve_thumbnails(std::sync::Arc::new(|p: &std::path::Path, size| {
            if p.to_string_lossy().ends_with(".jpg") {
                Ok(format!("{}@{size}", p.display()).into_bytes())
            } else {
                Err(super::ThumbStatus::NotOurs)
            }
        }))
        .unwrap();
        // Only one server per name.
        assert!(super::serve_thumbnails(std::sync::Arc::new(|_: &std::path::Path, _| Ok(vec![]))).is_err());
        assert_eq!(ask(r"C:\Users\x\Ünï\a.jpg", 256), (0, r"C:\Users\x\Ünï\a.jpg@256".as_bytes().to_vec()));
        assert_eq!(ask(r"C:\x.txt", 96), (super::ThumbStatus::NotOurs as u32, vec![]));
        // Several at once.
        let hs: Vec<_> = (0..32)
            .map(|i| std::thread::spawn(move || ask(&format!(r"C:\{i}.jpg"), 96)))
            .collect();
        for (i, h) in hs.into_iter().enumerate() {
            assert_eq!(h.join().unwrap(), (0, format!(r"C:\{i}.jpg@96").into_bytes()));
        }
    }

    #[test]
    fn pipe_round_trip() {
        std::env::set_var(
            "OTHCLOUD_DRIVE_PIPE",
            format!(r"\\.\pipe\othcloud-drive-test-{}", std::process::id()),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        super::serve(move |m| {
            let _ = tx.send(m);
        });
        super::send("othcloud-drive://auth?code=1&origin=x").unwrap();
        super::send("second").unwrap();
        let t = std::time::Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(t).unwrap(), "othcloud-drive://auth?code=1&origin=x");
        assert_eq!(rx.recv_timeout(t).unwrap(), "second");
    }

    #[test]
    fn mutex_is_exclusive() {
        std::env::set_var(
            "OTHCLOUD_DRIVE_MUTEX",
            format!(r"Local\OTHCloudDrive-test-{}", std::process::id()),
        );
        let a = super::acquire_instance();
        assert!(a.is_some());
        assert!(super::acquire_instance().is_none());
        drop(a);
        assert!(super::acquire_instance().is_some());
    }
}
