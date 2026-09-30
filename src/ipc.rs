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

#[cfg(test)]
mod tests {
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
