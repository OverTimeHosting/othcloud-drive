//! Drive letters as `subst`-style aliases of a sync root folder
//! (DefineDosDeviceW in the user's logon session). A definition belongs to
//! the logon session, not to the process that made it: it stays after the
//! app quits (or is killed) and goes at logoff. So the app leaves letters in
//! place when it quits, removes one only when its drive leaves this PC (or
//! moves to another letter), and puts them back at the next sign-in with
//! [`crate::app::restore_letters`] — run by the app at start and, in
//! OTHCloud Companion, by a tiny `--letters` launch at logon.

use std::path::{Path, PathBuf};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    DefineDosDeviceW, QueryDosDeviceW, DDD_EXACT_MATCH_ON_REMOVE, DDD_REMOVE_DEFINITION,
    DEFINE_DOS_DEVICE_FLAGS,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// Serialises letter changes between processes of this user (the app, and
/// the logon `--letters` helper that may run beside it): DefineDosDevice
/// STACKS a second definition of a letter, so two processes mapping the same
/// letter at once would leave one behind after an unmap.
struct LetterLock(HANDLE);

impl Drop for LetterLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn lock() -> Option<LetterLock> {
    let name = HSTRING::from(format!(r"Local\OTHCloudDriveLetters-{}", crate::cf::user_sid()));
    unsafe {
        let h = CreateMutexW(None, false, &name).ok()?;
        let w = WaitForSingleObject(h, 5000);
        if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
            Some(LetterLock(h))
        } else {
            let _ = CloseHandle(h);
            None
        }
    }
}

fn dev(letter: char) -> HSTRING {
    HSTRING::from(format!("{}:", letter.to_ascii_uppercase()))
}

/// What a letter currently points at (None if unused). For a subst letter
/// this is `\??\C:\path`.
pub fn target(letter: char) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    let n = unsafe { QueryDosDeviceW(&dev(letter), Some(&mut buf)) };
    if n == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..n as usize]);
    s.split('\0').next().map(|x| x.to_string())
}

fn norm(p: &str) -> String {
    p.trim_start_matches(r"\??\")
        .trim_start_matches(r"\\?\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Is `letter` a subst alias of `path`?
pub fn points_to(letter: char, path: &Path) -> bool {
    target(letter)
        .map(|t| norm(&t) == norm(&path.display().to_string()))
        .unwrap_or(false)
}

/// Map `letter` to `path` (like `subst L: path`).
pub fn map(letter: char, path: &Path) -> Result<(), String> {
    let _g = lock();
    if points_to(letter, path) {
        return Ok(());
    }
    if target(letter).is_some() {
        return Err(format!("{letter}: is already in use"));
    }
    unsafe {
        DefineDosDeviceW(
            DEFINE_DOS_DEVICE_FLAGS(0),
            &dev(letter),
            &HSTRING::from(path.as_os_str()),
        )
    }
    .map_err(|e| format!("mapping {letter}: to {}: {e}", path.display()))?;
    notify_shell(letter, true);
    Ok(())
}

/// Remove the alias (only if it still points at `path`).
pub fn unmap(letter: char, path: &Path) {
    let _g = lock();
    let Some(t) = target(letter) else { return };
    if norm(&t) != norm(&path.display().to_string()) {
        return;
    }
    // Exact match needs the target exactly as stored.
    let raw = t.trim_start_matches(r"\??\").to_string();
    let r = unsafe {
        DefineDosDeviceW(
            DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE,
            &dev(letter),
            &HSTRING::from(raw),
        )
    };
    if r.is_err() {
        let _ = unsafe { DefineDosDeviceW(DDD_REMOVE_DEFINITION, &dev(letter), None) };
    }
    notify_shell(letter, false);
}

fn notify_shell(letter: char, added: bool) {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_DRIVEADD, SHCNE_DRIVEREMOVED, SHCNF_PATHW};
    let p: Vec<u16> = format!("{letter}:\\").encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        SHChangeNotify(
            if added { SHCNE_DRIVEADD } else { SHCNE_DRIVEREMOVED },
            SHCNF_PATHW,
            Some(p.as_ptr() as *const _),
            None,
        );
    }
}

/// The letter to use for `path`: one that already points there, else the
/// first free one from O: upward (then N: down to D:).
pub fn choose(path: &Path, exclude: &[char]) -> Option<char> {
    let order: Vec<char> = ('O'..='Z').chain(('D'..='N').rev()).collect();
    if let Some(c) = order.iter().find(|c| points_to(**c, path)) {
        return Some(*c);
    }
    crate::platform::free_letters(exclude).into_iter().find(|c| target(*c).is_none())
}

pub fn root_of(letter: char) -> Option<PathBuf> {
    target(letter).map(|t| PathBuf::from(t.trim_start_matches(r"\??\")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_and_unmap_a_letter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        let l = choose(dir.path(), &[]).expect("a free letter");
        map(l, dir.path()).unwrap();
        assert!(points_to(l, dir.path()));
        assert_eq!(std::fs::read(format!("{l}:\\hello.txt")).unwrap(), b"hi");
        // idempotent
        map(l, dir.path()).unwrap();
        unmap(l, dir.path());
        assert!(target(l).is_none());
        assert!(std::fs::read(format!("{l}:\\hello.txt")).is_err());
    }
}
