//! WinFsp is delay-loaded (see build.rs). Load the installed DLL by full
//! path before the first WinFsp call, so the app can start — and explain
//! itself — on a machine without WinFsp.

use std::sync::OnceLock;

static LOADED: OnceLock<Result<(), String>> = OnceLock::new();

pub fn install_dir() -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY};
    use winreg::RegKey;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for (path, flags) in [
        (r"SOFTWARE\WinFsp", KEY_READ | KEY_WOW64_32KEY),
        (r"SOFTWARE\WOW6432Node\WinFsp", KEY_READ),
        (r"SOFTWARE\WinFsp", KEY_READ),
    ] {
        if let Ok(k) = hklm.open_subkey_with_flags(path, flags) {
            if let Ok(dir) = k.get_value::<String, _>("InstallDir") {
                return Some(std::path::PathBuf::from(dir));
            }
        }
    }
    let fallback = std::path::PathBuf::from(r"C:\Program Files (x86)\WinFsp");
    fallback.exists().then_some(fallback)
}

fn dll_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "winfsp-a64.dll"
    } else if cfg!(target_arch = "x86") {
        "winfsp-x86.dll"
    } else {
        "winfsp-x64.dll"
    }
}

/// Load WinFsp once. Err explains what's missing.
pub fn load() -> Result<(), String> {
    LOADED
        .get_or_init(|| {
            use windows::core::HSTRING;
            use windows::Win32::System::LibraryLoader::LoadLibraryW;
            let dir = install_dir().ok_or_else(|| {
                "WinFsp is not installed. Install it from https://winfsp.dev/rel/ and start OTHCloud Drive again."
                    .to_string()
            })?;
            let dll = dir.join("bin").join(dll_name());
            unsafe { LoadLibraryW(&HSTRING::from(dll.as_os_str())) }
                .map_err(|e| format!("could not load {}: {e}", dll.display()))?;
            winfsp::winfsp_init().map_err(|e| format!("WinFsp init failed: {e}"))?;
            Ok(())
        })
        .clone()
}
