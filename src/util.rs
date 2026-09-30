//! Small helpers shared by the filesystem, the cache and the backup watcher.

/// 100ns intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;

/// Windows FILETIME (100ns since 1601) for a unix time in milliseconds.
pub fn unix_ms_to_filetime(ms: i64) -> u64 {
    (ms.saturating_mul(10_000) + EPOCH_DIFF_100NS).max(0) as u64
}

pub fn now_filetime() -> u64 {
    unix_ms_to_filetime(chrono::Utc::now().timestamp_millis())
}

/// Parse an ISO-8601 timestamp from the panel into a FILETIME.
pub fn iso_to_filetime(s: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(s.trim()).ok()?;
    Some(unix_ms_to_filetime(dt.timestamp_millis()))
}

/// Windows-style case folding for name comparison. Windows uses an upcase
/// table; Unicode lowercase agrees with it for every name that matters here.
pub fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// Names that applications create as scratch files and never mean to keep.
/// A file CREATED under such a name stays local-only (it is never uploaded)
/// until it is renamed to a normal name.
pub fn is_temp_name(name: &str) -> bool {
    let l = name.to_lowercase();
    l.starts_with("~$")                 // Office owner/lock files
        || l.ends_with(".tmp")          // ~WRD0000.tmp, ~WRL0001.tmp, Excel's 1A2B3C4D.tmp, generic
        || l.starts_with(".~lock.")     // LibreOffice lock files
        || l.ends_with(".crdownload")   // Chrome partial downloads
        || l.ends_with(".partial")      // Edge/Firefox partial downloads
        || l.ends_with(".part")
        || l == "thumbs.db"
        || l == "desktop.ini"
        || l == ".ds_store"
}

/// Characters Windows forbids in a name component.
pub fn is_valid_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || (c as u32) < 32)
}

/// How a server name is shown on Windows. Names made in the browser can
/// contain characters Windows forbids (`<>:"|?*`, `\`, control chars); those
/// are shown as their full-width look-alikes so the file stays reachable.
/// The server keeps the real name (`Node::remote_name`).
pub fn display_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| match c {
            '<' => '\u{FF1C}',
            '>' => '\u{FF1E}',
            ':' => '\u{FF1A}',
            '"' => '\u{FF02}',
            '|' => '\u{FF5C}',
            '?' => '\u{FF1F}',
            '*' => '\u{FF0A}',
            '\\' => '\u{FF3C}',
            '/' => '\u{FF0F}',
            c if (c as u32) < 32 => '\u{FFFD}',
            c => c,
        })
        .collect();
    match mapped.as_str() {
        "" => "\u{FFFD}".to_string(),
        "." | ".." => mapped.replace('.', "\u{FF0E}"),
        _ => mapped,
    }
}

/// Split a WinFsp path (`\a\b\c`) into components. The root is `[]`.
pub fn split_path(path: &str) -> Vec<&str> {
    path.split(['\\', '/']).filter(|s| !s.is_empty()).collect()
}

/// "name (conflicted copy HOST).ext"
pub fn conflict_name(name: &str, host: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let when = chrono::Local::now().format("%Y-%m-%d %H%M%S");
    format!("{stem} (conflicted copy {host} {when}){ext}")
}

pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime() {
        // 1970-01-01T00:00:00Z
        assert_eq!(iso_to_filetime("1970-01-01T00:00:00Z"), Some(EPOCH_DIFF_100NS as u64));
        assert_eq!(
            iso_to_filetime("1970-01-01T00:00:01.000Z"),
            Some(EPOCH_DIFF_100NS as u64 + 10_000_000)
        );
        assert_eq!(iso_to_filetime("nope"), None);
    }

    #[test]
    fn temp_names() {
        assert!(is_temp_name("~$Report.docx"));
        assert!(is_temp_name("~WRD0000.tmp"));
        assert!(is_temp_name("A1B2C3D4.TMP"));
        assert!(is_temp_name(".~lock.foo.odt#"));
        assert!(is_temp_name("Thumbs.db"));
        assert!(!is_temp_name("Report.docx"));
        assert!(!is_temp_name("A1B2C3D4"));
    }

    #[test]
    fn paths() {
        assert_eq!(split_path("\\"), Vec::<&str>::new());
        assert_eq!(split_path("\\a\\b"), vec!["a", "b"]);
        assert!(is_valid_component("a.txt"));
        assert!(!is_valid_component("a:b"));
        assert!(!is_valid_component(".."));
    }

    #[test]
    fn conflicts() {
        let n = conflict_name("a.txt", "PC");
        assert!(n.starts_with("a (conflicted copy PC "));
        assert!(n.ends_with(").txt"));
        let n = conflict_name(".hidden", "PC");
        assert!(n.starts_with(".hidden (conflicted copy PC"));
    }
}
