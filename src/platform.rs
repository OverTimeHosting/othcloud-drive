//! Thin wrappers over the Windows APIs the app needs: DPAPI, per-user
//! registry (URL scheme, drive icons), known folders, drive letters, shell.

use std::path::{Path, PathBuf};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

// ------------------------------------------------------------------ DPAPI

/// Encrypt for the current Windows user (CryptProtectData).
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            windows::core::w!("OTHCloud Drive credentials"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| format!("CryptProtectData: {e}"))?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        Ok(v)
    }
}

pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| format!("CryptUnprotectData: {e}"))?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        Ok(v)
    }
}

// -------------------------------------------------------------- folders

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn pictures_dir() -> Option<PathBuf> {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_Pictures)
}

/// The user's Desktop, Documents, Pictures and Downloads (Known Folders —
/// wherever they were moved, e.g. into OneDrive): (id, label, path).
pub fn backup_known_folders() -> Vec<(&'static str, &'static str, PathBuf)> {
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Pictures};
    [
        ("desktop", "Desktop", FOLDERID_Desktop),
        ("documents", "Documents", FOLDERID_Documents),
        ("pictures", "Pictures", FOLDERID_Pictures),
        ("downloads", "Downloads", FOLDERID_Downloads),
    ]
    .into_iter()
    .filter_map(|(id, label, guid)| known_folder(&guid).map(|p| (id, label, p)))
    .collect()
}

/// Folders a backup must never be inside of: Windows, the program folders,
/// ProgramData and the user's AppData.
pub fn system_dirs() -> Vec<PathBuf> {
    use windows::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86,
        FOLDERID_RoamingAppData, FOLDERID_Windows,
    };
    let mut out: Vec<PathBuf> = [
        FOLDERID_Windows,
        FOLDERID_ProgramFiles,
        FOLDERID_ProgramFilesX86,
        FOLDERID_ProgramData,
        FOLDERID_RoamingAppData,
        FOLDERID_LocalAppData,
    ]
    .iter()
    .filter_map(known_folder)
    .collect();
    // %USERPROFILE%\AppData itself (LocalLow and the rest).
    if let Some(local) = known_folder(&FOLDERID_LocalAppData) {
        if let Some(p) = local.parent() {
            out.push(p.to_path_buf());
        }
    }
    for v in ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
        if let Ok(p) = std::env::var(v) {
            out.push(PathBuf::from(p));
        }
    }
    out
}

/// "Windows 11 (10.0.26200)" — Windows 11 still calls itself 10.0, the
/// build tells them apart.
pub fn os_version() -> String {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let build: u32 = key
        .as_ref()
        .ok()
        .and_then(|k| k.get_value::<String, _>("CurrentBuild").ok())
        .and_then(|b| b.parse().ok())
        .unwrap_or(0);
    let name = if build >= 22000 { "Windows 11" } else { "Windows 10" };
    format!("{name} (10.0.{build})")
}

/// `n` random bytes as hex, from the system RNG.
pub fn random_hex(n: usize) -> String {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut buf = vec![0u8; n];
    let ok = unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.is_ok();
    if !ok {
        // Never expected; still unique enough for an install/folder id.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (t >> ((i % 16) * 8)) as u8 ^ (std::process::id() as u8).wrapping_mul(i as u8 + 1);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// %APPDATA%\OTHCloud Drive (settings, DPAPI-protected token).
pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OTHCLOUD_DRIVE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    known_folder(&windows::Win32::UI::Shell::FOLDERID_RoamingAppData)
        .or_else(|| std::env::var("APPDATA").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("OTHCloud Drive")
}

/// %LOCALAPPDATA%\OTHCloud Drive (cache, logs, icons, backup state).
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OTHCLOUD_DRIVE_DATA_DIR") {
        return PathBuf::from(d);
    }
    known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)
        .or_else(|| std::env::var("LOCALAPPDATA").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("OTHCloud Drive")
}

// --------------------------------------------------------- drive letters

/// Letters currently in use (bitmask A=bit0), per GetLogicalDrives.
fn used_letters() -> u32 {
    unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() }
}

/// Free letters from O: upward (O..Z), then down from N: to D:.
pub fn free_letters(exclude: &[char]) -> Vec<char> {
    let used = used_letters();
    let order = ('O'..='Z').chain(('D'..='N').rev());
    order
        .filter(|c| used & (1 << (*c as u32 - 'A' as u32)) == 0)
        .filter(|c| !exclude.contains(c))
        .collect()
}

pub fn letter_in_use(c: char) -> bool {
    used_letters() & (1 << (c.to_ascii_uppercase() as u32 - 'A' as u32)) != 0
}

// ------------------------------------------------------------ URL scheme

pub const URL_SCHEME: &str = "othcloud-drive";

/// Register `othcloud-drive://` for the current user, pointing at `exe`.
pub fn register_url_scheme(exe: &Path) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(format!(r"Software\Classes\{URL_SCHEME}"))?;
    key.set_value("", &"URL:OTHCloud Drive")?;
    key.set_value("URL Protocol", &"")?;
    let (icon, _) = key.create_subkey("DefaultIcon")?;
    icon.set_value("", &format!("\"{}\",0", exe.display()))?;
    let (cmd, _) = key.create_subkey(r"shell\open\command")?;
    cmd.set_value("", &format!("\"{}\" \"%1\"", exe.display()))?;
    Ok(())
}

pub fn url_scheme_command() -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"Software\Classes\{URL_SCHEME}\shell\open\command"))
        .ok()?
        .get_value("")
        .ok()
}

pub fn unregister_url_scheme() {
    let _ = RegKey::predef(HKEY_CURRENT_USER)
        .delete_subkey_all(format!(r"Software\Classes\{URL_SCHEME}"));
}

// ------------------------------------------------------------ drive icons

/// Where per-letter label/icon keys live under HKCU. Tests point it
/// elsewhere ([`set_drive_key_base`]) so they never dress up a real letter.
static DRIVE_KEY_BASE: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

const DRIVE_KEY_BASE_DEFAULT: &str = r"Software\Classes\Applications\Explorer.exe\Drives";

/// Use `base` (under HKCU) instead of Explorer's per-user `Drives` key for
/// the label/icon keys. For tests; None restores the real one.
pub fn set_drive_key_base(base: Option<&str>) {
    *DRIVE_KEY_BASE.write() = base.map(String::from);
}

fn drive_key_path(letter: char) -> String {
    let base = DRIVE_KEY_BASE.read().clone();
    format!(
        r"{}\{}",
        base.as_deref().unwrap_or(DRIVE_KEY_BASE_DEFAULT),
        letter.to_ascii_uppercase()
    )
}

/// Point Explorer's icon and label for `letter` at our .ico / name.
/// Explorer is told (batched, see [`notify_shell`]).
pub fn set_drive_icon(letter: char, ico: Option<&Path>, label: &str) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (k, _) = hkcu.create_subkey(drive_key_path(letter))?;
    // Marker so we only ever delete keys we wrote.
    k.set_value("OTHCloudDrive", &1u32)?;
    if let Some(ico) = ico {
        let (i, _) = k.create_subkey("DefaultIcon")?;
        i.set_value("", &ico.display().to_string())?;
    }
    let (l, _) = k.create_subkey("DefaultLabel")?;
    l.set_value("", &label.to_string())?;
    notify_shell(&format!("{}:\\", letter.to_ascii_uppercase()));
    Ok(())
}

/// Tell Explorer a folder's (sync root's) look changed (batched).
pub fn shell_refresh_path(path: &Path) {
    notify_shell(&path.display().to_string());
}

/// Remove what `set_drive_icon` wrote (only if it carries our marker).
/// Tells Explorer at once: this often runs just before the process exits.
pub fn remove_drive_icon(letter: char) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let ours = hkcu
        .open_subkey(drive_key_path(letter))
        .and_then(|k| k.get_value::<u32, _>("OTHCloudDrive"))
        .is_ok();
    if ours {
        let _ = hkcu.delete_subkey_all(drive_key_path(letter));
        notify_shell(&format!("{}:\\", letter.to_ascii_uppercase()));
        flush_shell_notifications();
    }
}

/// (DefaultIcon, DefaultLabel) currently registered for a letter.
pub fn drive_icon_registration(letter: char) -> Option<(Option<String>, Option<String>)> {
    let k = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(drive_key_path(letter))
        .ok()?;
    let icon = k
        .open_subkey("DefaultIcon")
        .ok()
        .and_then(|i| i.get_value::<String, _>("").ok());
    let label = k
        .open_subkey("DefaultLabel")
        .ok()
        .and_then(|i| i.get_value::<String, _>("").ok());
    Some((icon, label))
}

// ------------------------------------------------- shell notifications
//
// Icon and label changes are batched: several drives changing in the same
// poll (or one drive's letter + sync root) become one burst of
// SHCNE_UPDATEITEM (one per path) and a single SHCNE_ASSOCCHANGED — the
// event that makes Explorer drop its cached icons for drives and the
// navigation pane. ASSOCCHANGED is expensive for the shell (every view
// refreshes), so it must not go out once per drive.

const SHELL_BATCH: std::time::Duration = std::time::Duration::from_millis(400);

struct ShellQueue {
    paths: Vec<String>,
    worker: bool,
}

static SHELL_Q: parking_lot::Mutex<ShellQueue> = parking_lot::Mutex::new(ShellQueue { paths: Vec::new(), worker: false });

/// Queue "the item at `path` changed its look". Sent within ~0.4 s, together
/// with anything else queued meanwhile.
pub fn notify_shell(path: &str) {
    let mut q = SHELL_Q.lock();
    if !q.paths.iter().any(|p| p.eq_ignore_ascii_case(path)) {
        q.paths.push(path.to_string());
    }
    if !q.worker {
        q.worker = true;
        let spawned = std::thread::Builder::new()
            .name("shell-notify".into())
            .spawn(|| {
                std::thread::sleep(SHELL_BATCH);
                SHELL_Q.lock().worker = false;
                flush_shell_notifications();
            })
            .is_ok();
        if !spawned {
            q.worker = false;
            drop(q);
            flush_shell_notifications();
        }
    }
}

/// Send whatever is queued now (call before a short-lived process exits).
pub fn flush_shell_notifications() {
    use windows::Win32::UI::Shell::{
        SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNE_UPDATEDIR, SHCNE_UPDATEITEM, SHCNF_FLUSHNOWAIT, SHCNF_IDLIST,
        SHCNF_PATHW,
    };
    let paths = std::mem::take(&mut SHELL_Q.lock().paths);
    if paths.is_empty() {
        return;
    }
    tracing::debug!("shell: {} item(s) changed: {paths:?}", paths.len());
    for p in &paths {
        let w: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            SHChangeNotify(SHCNE_UPDATEITEM, SHCNF_PATHW | SHCNF_FLUSHNOWAIT, Some(w.as_ptr() as *const _), None);
            if !(p.len() == 3 && p.ends_with(":\\")) {
                SHChangeNotify(SHCNE_UPDATEDIR, SHCNF_PATHW | SHCNF_FLUSHNOWAIT, Some(w.as_ptr() as *const _), None);
            }
        }
    }
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSHNOWAIT, None, None) };
}

// ------------------------------------------------------------------ shell

pub fn open_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

pub fn open_url(url: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &HSTRING::from(url),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

pub fn message_box(title: &str, text: &str, error: bool) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
    };
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_OK | icon | MB_SETFOREGROUND,
        );
    }
}

pub fn confirm(title: &str, text: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO,
    };
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip() {
        let secret = b"token-123 \x00\xff";
        let enc = dpapi_protect(secret).unwrap();
        assert_ne!(&enc[..], &secret[..]);
        assert_eq!(dpapi_unprotect(&enc).unwrap(), secret);
        assert!(dpapi_unprotect(b"garbage").is_err());
    }

    #[test]
    fn letters() {
        let free = free_letters(&[]);
        assert!(!free.contains(&'C'));
        for c in &free {
            assert!(!letter_in_use(*c));
        }
        if let Some(first) = free.first() {
            assert!(!free_letters(&[*first]).contains(first));
        }
    }

    #[test]
    fn pictures_exists() {
        assert!(pictures_dir().is_some());
    }
}
