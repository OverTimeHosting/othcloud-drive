# OTHCloud Drive (Windows)

Mounts your OTHCloud drives as drive letters in Windows Explorer, with
two-way sync. Each OTHCloud drive gets its **own** letter (from `O:` upward,
skipping taken letters), labelled with the drive's name and shown with the
drive's cover image as its icon.

The contract with the panel is [PROTOCOL.md](PROTOCOL.md).

## Requirements

- Windows 10/11 x64.
- **[WinFsp](https://winfsp.dev/rel/) 2.x** installed (the user-mode
  filesystem driver). The app starts without it (sign-in and photo backup
  still work) but can't mount drives, and says so.

## Build

```powershell
# Rust (stable) + the MSVC build tools, and:
winget install LLVM.LLVM        # libclang, needed by winfsp-sys's bindgen
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --release           # target\release\othcloud-drive.exe
```

WinFsp does **not** need to be installed to build: `winfsp-sys` ships the
headers and import library, and the DLL is delay-loaded at runtime from the
WinFsp install directory (`src/winfsp_loader.rs`, `build.rs`).

Tests:

```powershell
cargo test                      # unit + integration tests against the in-memory mock server
cargo clippy --all-targets

# The same flows against a real panel (opt-in):
$env:OTHCLOUD_REAL_PANEL = "http://127.0.0.1:3900"
$env:OTHCLOUD_REAL_TOKEN = "…"; $env:OTHCLOUD_REAL_ORG = "…"; $env:OTHCLOUD_REAL_STORAGE = "…"
cargo test --test real_panel -- --test-threads=1
```

The mock server (`tests/common/mock.rs`) implements PROTOCOL.md in memory
and can be run on its own for manual testing:

```powershell
cargo run --example mock_server -- 8787   # prints origin, token, org, drive ids
```

## Run

```powershell
othcloud-drive.exe              # tray app (single instance)
```

Command-line modes (mostly for scripting and testing):

| Command | What it does |
|---|---|
| `othcloud-drive "othcloud-drive://auth?code=…&origin=…"` | sign-in link (Windows runs this for you; handed to the running instance if there is one) |
| `othcloud-drive quit` | ask the running instance to unmount and quit |
| `othcloud-drive register` | (re)register the `othcloud-drive://` URL scheme for this user |
| `othcloud-drive mount --origin URL --token T --org ORG --storage ID [--storage ID…] [--letter L…] [--poll-secs N] [--stop-file PATH]` | headless mount without the tray; unmounts on `quit`/EOF on stdin or when the stop file appears |

Environment overrides: `OTHCLOUD_ORIGIN` (panel to sign in to, default
`https://othcloud.xyz`), `OTHCLOUD_DRIVE_LOG` (tracing filter, default
`info`), `OTHCLOUD_DRIVE_CONFIG_DIR` / `OTHCLOUD_DRIVE_DATA_DIR` (relocate
settings / cache, used by tests).

### The tray menu

Status line · one submenu per drive (**Mounted** toggle, **Open in
Explorer**) · **Pause sync** · **Photo backup** (on/off, folder picker,
which drive to upload to, last result) · **Organization** (if you're in
more than one) · Open OTHCloud in browser · Sign out · Open log folder ·
Quit (flushes pending uploads, unmounts, removes the drive icons).

## How sign-in works

1. The tray's **Sign in…** opens `<panel>/dashboard/apps/storage` in your
   browser. There, **Connect Windows app** creates a one-shot pairing code
   and navigates to `othcloud-drive://auth?code=…&origin=…`.
2. Windows starts `othcloud-drive.exe` with that link (the scheme is
   registered per-user under `HKCU\Software\Classes\othcloud-drive` on every
   start). If the app is already running, the new process hands the link to
   it over a per-user named pipe and exits.
3. The app exchanges the code at `<origin>/api/desktop/token` (device name
   `OTHCloud Drive (<PC name>)`) for a long-lived token. A link for a panel
   other than the one you last used (or `OTHCLOUD_ORIGIN`) asks for
   confirmation first, so a random web page can't silently point the app at
   another server.
4. The token is stored encrypted with DPAPI for your Windows account. The
   first organization is used; switch in the tray menu.

Sign out deletes the stored token (revoke it in the panel to invalidate it
everywhere).

## Where data lives

| What | Where |
|---|---|
| Settings | `%APPDATA%\OTHCloud Drive\config.json` |
| Token (DPAPI-encrypted) | `%APPDATA%\OTHCloud Drive\credentials.bin` |
| File cache | `%LOCALAPPDATA%\OTHCloud Drive\cache\<storageId>\<fileId>.bin` (+ `.json` chunk map) |
| Drive icons | `%LOCALAPPDATA%\OTHCloud Drive\icons\<storageId>.ico` |
| Photo backup state | `%LOCALAPPDATA%\OTHCloud Drive\backup-<storageId>.json` |
| Logs (daily, 14 kept) | `%LOCALAPPDATA%\OTHCloud Drive\logs\` |
| Explorer icon/label | `HKCU\Software\Classes\Applications\Explorer.exe\Drives\<L>\DefaultIcon`, `\DefaultLabel` (written on mount, removed on unmount) |

## How syncing works

- **Listing** comes from the drive's whole `tree`, fetched at mount and
  then every 15 s with `If-None-Match` (a 304 is cheap). Changes made
  elsewhere appear after the next poll, and Explorer windows are told
  (WinFsp change notifications). Lookups are case-insensitive; case is
  preserved.
- **Reading** fetches only the byte ranges that are read (1 MiB chunks, a
  small read-ahead) into a sparse cache file keyed by file id. Cached bytes
  stay valid until the file changes on the server (size/updatedAt), so
  they're readable offline.
- **Writing** goes to the cache; before the first write to a file that
  isn't fully cached, the rest is downloaded. The file is uploaded (streamed
  from the cache with a Content-Length) about 2 s after the last handle
  closes / is flushed — `PUT file?fileId=` for existing files (same id,
  shares kept), `PUT file?storageId=…` for new ones.
- **Create folder / rename / move / delete** call the API immediately.
  Renaming over an existing file (`MoveFileEx` with replace) is mapped so
  the file **keeps its server id**: the new bytes are uploaded into the old
  file rather than deleting it.
- **Office / editor save patterns.** Files created under scratch names
  (`~$*`, `*.tmp`, `.~lock.*`, `*.crdownload`, `*.part(ial)`, `Thumbs.db`,
  `desktop.ini`) are **kept local** and never uploaded unless renamed to a
  normal name. Renaming an existing file to a scratch name (Word/Excel move
  the original to `~WRL0001.tmp` before putting the new version in place)
  is kept local too, so when the new version is renamed to the original
  name it replaces the original's contents under the same id and nothing
  temporary ever reaches the server. Tested for Word's
  `~WRD/~WRL` dance, Excel's extension-less temp file, and the
  "write `x.new`, rename over `x`" editor pattern.
- **Conflicts.** If a new file's name was taken on the server meanwhile,
  it's uploaded as `name (conflicted copy <PC> <time>).ext`. If a file with
  unsent local edits changed on the server too, the local copy wins (logged).
- **Offline.** Cached files stay readable; reading something not cached
  fails with "network unreachable". New files, edits and deletes are queued
  and retried with backoff (up to 5 min); folders created offline are
  created on the server later. The tray shows the state.
- **Pause sync** stops polling and uploads; reads (and create/rename/delete,
  which are immediate API calls) still work.
- Names the server allows but Windows doesn't (`<>:"|?*` from browser
  uploads) are shown with full-width look-alike characters; the server name
  is untouched. If a folder contains two names that differ only by case (or
  a file and a folder with the same name), only one is shown.

## Photo backup

Off by default. When on, it watches a folder (default: your Pictures)
recursively and uploads new or changed images (jpg, png, heic, webp, gif,
tiff, avif, and common camera-raw formats) to
`<chosen drive>/Photo backup/<PC name>/<same sub-folders>/`, creating
folders as needed. It remembers what it uploaded (path, size, mtime, file
id) so restarts don't re-upload; a changed photo replaces its earlier upload
in place. It **never deletes anything** on the server. A same-named but
different file already there is kept and the new one uploaded as
`name (1).ext`. Network errors back off (5 s … 10 min) and retry.

## Limitations (v1)

- Needs WinFsp installed separately (see Installer below).
- The upload queue isn't persisted: if you quit while offline, edits made
  since stay in the cache but aren't re-uploaded on the next start (Quit
  waits up to 30 s per drive for pending uploads when online).
- No cache eviction yet: the cache grows with what you open. Delete the
  cache folder while the app isn't running to reclaim space.
- Timestamps you set locally (e.g. by `robocopy /COPY:T`) are shown locally
  but not stored on the server (the protocol has no field for them).
- ACLs, alternate data streams, hard links, reparse points and extended
  attributes aren't supported.
- A file edited locally while it also changed on the server: local wins.
- Only one account at a time; one organization at a time.

## Installer note

WinFsp is a prerequisite. For a proper installer (MSI/MSIX or Inno Setup),
bundle the WinFsp MSI (`winfsp-2.x.msi`, redistributable under WinFsp's
FLOSS exception — check the current licence terms) and run it silently
(`msiexec /i winfsp.msi /qn INSTALLLEVEL=1000`) before installing
`othcloud-drive.exe`, then add a per-user Run key or Startup shortcut so the
tray starts at sign-in. The URL scheme is registered by the app itself on
every start, so the installer doesn't need to.
