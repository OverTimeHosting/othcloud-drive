# OTHCloud Drive (Windows)

Puts your OTHCloud drives in Windows Explorer with two-way sync, the way
OneDrive does it: each OTHCloud drive is a **cloud-files folder**
(`%USERPROFILE%\OTHCloud\<drive name>`) whose files are downloaded only when
you open them, and each is **also its own drive letter** (from `O:` upward),
labelled with the drive's name and shown with the drive's cover (or colour)
as its icon.

It is a Windows *cloud files provider* built on the Cloud Files API
(`cldapi.dll`, part of Windows 10 1809+ / 11). **Nothing to install besides
the app**: no WinFsp, no kernel driver, no Visual C++ runtime (the CRT is
linked statically).

The contract with the panel is [PROTOCOL.md](PROTOCOL.md).

## Build

```powershell
cargo build --release     # target\release\othcloud-drive.exe
```

Plain stable Rust + the MSVC build tools. (No LLVM/libclang any more — that
was only needed by the old WinFsp bindings.)

Tests (they register real, clearly-named `OTHCloudDriveTest!…` sync roots in
temp folders and always unregister them, even when a test fails):

```powershell
cargo test                      # unit + integration tests (mock server, real cfapi)
cargo clippy --all-targets

# The engine against a real panel (opt-in):
$env:OTHCLOUD_REAL_PANEL = "http://127.0.0.1:3900"   # + OTHCLOUD_REAL_TOKEN / _ORG / _STORAGE
cargo test --test real_panel -- --test-threads=1

# End-to-end, driving the drive letter and the folder from PowerShell:
powershell -File scripts\mount-test.ps1 -Mock
powershell -File scripts\mount-test.ps1 -Origin http://127.0.0.1:3900 -Token … -Org … -Storage … [-Psql path\to\psql.exe]
```

Dev helpers:

```powershell
cargo run --example mock_server -- 8787          # in-memory PROTOCOL.md server
cargo run --example sync_roots                   # list this app's sync roots
cargo run --example render_icons -- <out dir>    # sample drive icons + contact sheet
othcloud-drive cleanup-test-roots                # remove any OTHCloudDriveTest roots/letters
```

## Run

```powershell
othcloud-drive.exe              # tray app (single instance)
```

| Command | What it does |
|---|---|
| `othcloud-drive "othcloud-drive://auth?code=…&origin=…"` | sign-in link (Windows runs this for you; handed to the running instance if there is one) |
| `othcloud-drive quit` | ask the running instance to flush, disconnect and quit |
| `othcloud-drive register` | (re)register the `othcloud-drive://` URL scheme for this user |
| `othcloud-drive mount --origin URL --token T --org ORG --storage ID [--storage ID…] [--root DIR…] [--letter L…] [--no-letter] [--poll-secs N] [--cover-secs N] [--stop-file PATH] [--remove-on-exit] [--provider-id ID]` | headless sync without the tray; prints `MOUNTED <storageId> <L>: <path>`; stops on `quit`/EOF or when the stop file appears |
| `othcloud-drive cleanup-test-roots` | dev: unregister test sync roots (provider `OTHCloudDriveTest`) and their letters |

Environment overrides: `OTHCLOUD_ORIGIN` (panel to sign in to, default
`https://othcloud.xyz`), `OTHCLOUD_DRIVE_LOG` (tracing filter, default
`info`), `OTHCLOUD_DRIVE_CONFIG_DIR` / `OTHCLOUD_DRIVE_DATA_DIR` /
`OTHCLOUD_DRIVE_ROOT_BASE` (relocate settings / data / the `OTHCloud` folder),
`OTHCLOUD_DRIVE_PIPE` / `OTHCLOUD_DRIVE_MUTEX` (single-instance names).

### Tray menu

Status line · one submenu per drive (**Sync this drive** toggle, **Open in
Explorer**) · **Pause sync** · **Photo backup** (on/off, folder picker,
target drive, last result) · **Organization** (if more than one) · Open
OTHCloud in browser · Sign out · Open log folder · Quit.

## How it works

- **Sync roots.** Each drive is registered with the shell
  (`StorageProviderSyncRootManager`) as sync root
  `OTHCloudDrive!<SID>!<storageId>` with hydration policy **Progressive** and
  population policy **AlwaysFull**. The protocol only offers the whole
  `tree`, so every file and folder becomes a placeholder up front; listing,
  search and thumbnails-of-cached-files work offline, and nothing is
  downloaded to show a folder.
- **Drive letters.** Each sync root folder is aliased as a drive letter
  with `subst` semantics (`DefineDosDeviceW` in the user's session): the
  letter is just another path to the same NTFS folder, so hydration,
  pinning and uploads behave identically through it (the test suite reads,
  writes, copies in and out through both). Letters don't survive logoff; the
  app re-creates them at every start and removes them on quit. The letter's
  Explorer label/icon: `HKCU\Software\Classes\Applications\Explorer.exe\Drives\<L>`.
- **Reads (hydration).** On first access Windows asks the app for the file's
  data (FETCH_DATA); it streams exactly the requested ranges from `GET file`
  (≤ 4 MiB per request). The placeholder file *is* the cache.
  **Free up space** / **Always keep on this device** in Explorer
  (`attrib +U` / `attrib +P`) dehydrate / hydrate files.
- **Remote changes** (polled every ~5 s with `If-None-Match`): new items
  become placeholders; changed files are updated and dehydrated (re-fetched
  on next open); renames/moves and deletes are applied locally. A file with
  local changes that aren't uploaded yet is never overwritten or deleted.
- **Local changes.** Renames/moves and deletes of synced items arrive as
  platform callbacks and go to the server immediately (`POST move`;
  `DELETE item` after a few seconds' grace). New and modified files and
  folders are picked up by a directory watcher (plus a periodic rescan),
  uploaded (`PUT file`, create or replace, always with `Content-Length`) and
  then turned into in-sync placeholders.
- **Office / editor saves keep the file's server id.** Scratch files (`~$*`,
  `*.tmp`, `.~lock.*`, `*.crdownload`, `*.part(ial)`, `Thumbs.db`,
  `desktop.ini`) are never uploaded. Word/Excel's "rename the original to
  `~WRL0001.tmp`, rename the new file into place, delete the original" and
  editors' `MoveFileEx(REPLACE_EXISTING)` are recognised: the new bytes are
  written into the original server file (`PUT file?fileId=`), so shares and
  links survive and no temp file reaches the server.
- **Conflicts.** If a new local file's name is already taken on the server
  by a different file, it's uploaded as
  `name (conflicted copy <PC> <time>).ext`. A file edited locally that also
  changed on the server: local wins (logged).
- **Offline.** Hydrated files stay readable; opening one that was never
  downloaded fails with "the network is unavailable". New and edited files
  are queued and uploaded with backoff when the server is reachable again.
- **Drive icons.** The drive's cover in a rounded tile (a logo with
  transparency is fitted, not cropped; a badge from 48 px up); without a
  readable cover, a tile in the drive's colour with the OTH logo. The same
  `.ico` is used for the drive letter and the sync root's sidebar entry.
  The drive's look and name are re-checked about every 15 s from `GET
  drives`; a change writes a new file name (Explorer caches icons by path),
  repoints both, updates the labels, deletes the old file and notifies the
  shell. `cargo run --example render_icons -- <dir> [cover files…]` renders
  a contact sheet for review.
- **Pause sync** stops polling, uploads and remote deletes; opening files
  and renames/deletes still reach the server.

## How sign-in works

1. The tray's **Sign in…** opens `<panel>/dashboard/apps/storage`; **Connect
   Windows app** there makes a one-shot pairing code and opens
   `othcloud-drive://auth?code=…&origin=…`.
2. Windows starts `othcloud-drive.exe` with that link (the scheme is
   registered per user under `HKCU\Software\Classes\othcloud-drive` at every
   start); a running instance receives it over a per-user named pipe.
3. The code is exchanged at `<origin>/api/desktop/token` for a long-lived
   token (device name `OTHCloud Drive (<PC name>)`). A link for a panel other
   than the configured one asks for confirmation first.
4. The token is stored with DPAPI (current Windows user). The first
   organization is used; switch in the tray.

**Sign out** stops syncing, unregisters every sync root and deletes the
placeholders. Anything not uploaded yet is first copied to
`%USERPROFILE%\OTHCloud\Not uploaded - <drive> - <time>` (and the log says
so) — nothing unsynced is ever deleted. Revoke the token in the panel to
invalidate it everywhere.

## Where data lives

| What | Where |
|---|---|
| Synced drives | `%USERPROFILE%\OTHCloud\<drive name>` (one sync root each) + a drive letter per drive |
| Settings | `%APPDATA%\OTHCloud Drive\config.json` |
| Token (DPAPI) | `%APPDATA%\OTHCloud Drive\credentials.bin` |
| Drive icons | `%LOCALAPPDATA%\OTHCloud Drive\icons\<storageId>-<hash>.ico` |
| Photo backup state | `%LOCALAPPDATA%\OTHCloud Drive\backup-<storageId>.json` |
| Logs (daily, 14 kept) | `%LOCALAPPDATA%\OTHCloud Drive\logs\` |
| Shell registration | `HKLM\…\Explorer\SyncRootManager\OTHCloudDrive!<SID>!<storageId>` (written by Windows on the app's behalf; no admin rights needed) |
| Letter label/icon | `HKCU\Software\Classes\Applications\Explorer.exe\Drives\<L>` |

## Photo backup

Off by default. Watches a folder (default: Pictures) recursively and uploads
new or changed images (jpg, png, heic, webp, gif, tiff, avif, camera raw) to
`<chosen drive>/Photo backup/<PC name>/<same sub-folders>/`. Remembers what
it uploaded (path, size, mtime, file id), replaces an edited photo in place,
keeps a same-named different file (uploads `name (1).ext`), backs off on
network errors, and **never deletes anything** on the server.

## Limitations (v1)

- Windows asks for whole files on first access (even with a
  Partial/Progressive policy): opening a large video starts its full
  download, streamed range by range.
- The upload queue isn't persisted across restarts; files still unsynced at
  quit are found again by the start-up scan (they aren't in-sync
  placeholders) and uploaded then.
- Pause also doesn't block renames/deletes (the platform needs an answer
  for those immediately).
- A file edited locally while it changed on the server: local wins.
- Timestamps set locally aren't stored on the server (no protocol field).
- Names the server allows but Windows doesn't (`<>:"|?*` from browser
  uploads) are shown with full-width look-alikes; two names differing only
  by case (or a file and folder with the same name) show only one.
- Moving a file OUT of a drive folder deletes it from the server (like
  OneDrive); moving files between two OTHCloud drives is a delete + upload.
- One account and one organization at a time.
- The drive letter needs a free letter; the folder under
  `%USERPROFILE%\OTHCloud` always works.

## Installer note

Only the exe is needed (plus a Start-menu / per-user Run entry so the tray
starts at sign-in). No drivers or prerequisites: the Cloud Files API ships
with Windows 10 1809+ and Windows 11, and the C runtime is statically linked.
The app registers its URL scheme itself at every start. An uninstaller
should run the app's sign-out path first (unregisters the sync roots,
removes letters and placeholders, keeps unsynced files) before deleting the
exe.

## Licences

All dependencies are permissively licensed (MIT / Apache-2.0 / BSD /
ISC / Zlib / Unicode-3.0 / CC0 / CDLA-Permissive-2.0); nothing GPL, LGPL,
AGPL or MPL. The OTH logo in `assets/` is the project's own brand asset.
