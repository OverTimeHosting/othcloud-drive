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

fn drive_key_path(letter: char) -> String {
    format!(
        r"Software\Classes\Applications\Explorer.exe\Drives\{}",
        letter.to_ascii_uppercase()
    )
}

/// Point Explorer's icon and label for `letter` at our .ico / name.
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
    shell_refresh(letter);
    Ok(())
}

/// Remove what `set_drive_icon` wrote (only if it carries our marker).
pub fn remove_drive_icon(letter: char) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let ours = hkcu
        .open_subkey(drive_key_path(letter))
        .and_then(|k| k.get_value::<u32, _>("OTHCloudDrive"))
        .is_ok();
    if ours {
        let _ = hkcu.delete_subkey_all(drive_key_path(letter));
        shell_refresh(letter);
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

fn shell_refresh(letter: char) {
    use windows::Win32::UI::Shell::{
        SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNE_UPDATEITEM, SHCNF_IDLIST, SHCNF_PATHW,
    };
    let path: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEITEM,
            SHCNF_PATHW,
            Some(path.as_ptr() as *const _),
            None,
        );
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
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
