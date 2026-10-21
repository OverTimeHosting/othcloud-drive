# OTHCloud Drive — desktop protocol

The contract between the Windows app (`apps/othcloud-drive`, Rust) and the panel
(`apps/othcloud`). Both sides are built against THIS file; change it here first.

## Auth

Sign-in reuses the desktop pairing flow OTerminal uses (`pages/api/desktop/pair.ts`,
`pages/api/desktop/token.ts`, `lib/desktop-auth.ts`), with its own URL scheme:

1. The Drives web app shows **Connect Windows app**. It `POST /api/desktop/pair`
   (session cookie) → `{ code }`, then navigates to
   `othcloud-drive://auth?code=<code>&origin=<panel origin, urlencoded>`.
2. The Windows app (registered handler for `othcloud-drive://`) calls
   `POST <origin>/api/desktop/token` with `{ "code": "<code>", "name": "OTHCloud Drive (<hostname>)" }`
   → `{ token, user: {...} }` (existing response shape). The token is permanent
   until revoked; store it with Windows DPAPI (per-user). `name` is optional
   (omitted → "OTerminal", as before), at most 100 characters; a name the
   panel gives meaning to (ending " (Minecraft)", starting "OTerminal remote: ",
   or "Othcloud Editor (web)") is refused with 400 `invalid_name` WITHOUT
   burning the code.
3. Every call below sends `Authorization: Bearer <token>`.
4. Signing out: `DELETE <origin>/api/desktop/token` with that Bearer revokes
   it (→ `{ "ok": true }`).

OTHLauncher (iOS) is a second client of this protocol: its File Provider
extension shows the drives in the Files app. It gets its token without the
URL scheme, calling `pair` then `token` itself with the app's session, named
`OTHLauncher Files (<device>)`.

Drives belong to an ORGANIZATION. Every endpoint except `/orgs` requires
`X-OTH-Org: <organizationId>`; the panel checks the user is a member of it and
applies the same visibility the web app does (`storage.list`: org admins/owners
and platform admins see every drive in the org, members only their own).

Errors are JSON `{ "error": "<code>", "message": "<human text>" }` with a real
HTTP status: 400 bad input, 401 bad/missing token, 403 not allowed, 404 not found,
409 name conflict / quota, 411 no Content-Length on an upload, 413 too large,
416 unsatisfiable range. Codes the panel sends:

| Status | `error` |
|---|---|
| 400 | `bad_request`, `missing_org`, `invalid_name`, `invalid_move`, `length_mismatch` |
| 401 | `missing_bearer`, `invalid_token`, `token_expired` |
| 403 | `not_member` (X-OTH-Org), `wrong_client` (a game/remote token), `not_backup_file` (`X-OTH-Keep-Version` on a file outside your backup folders) |
| 404 | `not_found` (also for a drive/file you can't see — existence never leaks), `no_cover` |
| 405 | `method_not_allowed` |
| 409 | `name_taken`, `quota_exceeded` |
| 411 | `length_required` |
| 413 | `too_large` |
| 416 | `range_not_satisfiable` (with `Content-Range: bytes */<size>`) |

## Endpoints — all under `/api/desktop/drive/`

| Method | Path | Purpose |
|---|---|---|
| GET | `orgs` | `[{ id, name }]` orgs the user belongs to |
| GET | `drives` | `[{ storageId, name, sizeBytes, quotaBytes, hasCover, coverFileId, color, updatedAt }]` — `color` is the drive accent ("#38bdf8") or null; `coverFileId` (or null) changes when the cover does |
| POST | `drives` | JSON `{ name }` (1–80 chars) → 201 + a new, empty drive in the `drives` shape, owned by the caller. `GET drives` answers with the header `X-OTH-Drive-Manage: 1` on a panel that has this and the next row |
| PUT | `drives?storageId=` | JSON `{ name }` → the renamed drive. (Deleting a drive is web-only) |
| GET | `cover?storageId=` | the drive's cover image bytes (`Content-Type` image/*); 404 if none |
| GET | `tree?storageId=` | the WHOLE drive, flat (see below). Honors `If-None-Match` → 304 |
| GET/HEAD | `file?fileId=` | file bytes. Honors `Range` (single `bytes=a-b` / `a-` / `-n`) → 206 + `Content-Range`; past the end → 416. Sends `ETag` (changes with the contents) and honors `If-None-Match` → 304 |
| PUT | `file?storageId=&folderId=&name=` | create a NEW file from the raw body → 201. `folderId` omitted/empty = drive root. 409 `name_taken` if a file OR folder with that name exists in that folder |
| PUT | `file?fileId=` | REPLACE the contents of an existing file (same id, name, shares) |
| POST | `folder` | JSON `{ storageId, parentId: string\|null, name }` → the folder row. 409 `name_taken` |
| POST | `move` | JSON `{ type: "file"\|"folder", id, name?: string, parentId?: string\|null }` — rename and/or move (parentId null = root; omitted = unchanged). 409 `name_taken`; moving a folder into itself/descendant = 400 |
| DELETE | `item?type=file\|folder&id=` | delete (folder = recursive) |
| POST/DELETE | `push-token` | JSON `{ token }` (hex). OTHLauncher only, no `X-OTH-Org`: register / drop an iOS PushKit `fileProvider` token. The panel then polls on that user's behalf (`server/drive-change-watcher.ts`, every ~15 s) and sends a silent File Provider push, with the organization's id as the domain, when their drives change |
| GET | `thumbnail?fileId=&w=` | a small JPEG (PNG when the picture has transparency) of a picture, at most `w` px on its longer side; `w` is snapped up to 96 / 256 / 512 / 1024. Same access as `GET file`. `ETag` + `Cache-Control: private, max-age=600`, honors `If-None-Match` → 304. 415 `not_an_image` (not a picture the panel can read: JPEG, PNG, WebP, AVIF, TIFF, GIF, HEIC/HEIF), 413 `too_large` (> 50 MB), 400 without `w`. See "Thumbnails" below |

Uploads (`PUT file`) stream the body (no buffering), are quota-checked from
`Content-Length` before a byte moves (409 `quota_exceeded`), and cap at 10 GB
(413). `Content-Type` of the request is the file's type (octet-stream → derived
from the extension, as the web upload does; on REPLACE, octet-stream keeps the
file's existing type).

**`Content-Length` is REQUIRED** on both `PUT file` forms (411
`length_required` without it — a chunked body can't be quota-checked up
front). If the body that arrives isn't that long, a CREATE is undone (row and
object) and answered 400 `length_mismatch`. A REPLACE overwrites the object
only once the whole body has arrived (multipart upload), so an aborted upload
leaves the old contents intact.

### Response bodies of the write endpoints

(Added by the Windows client — it needs the new ids without re-fetching the tree.)

- `PUT file` (both forms) → `200`/`201` with the file row in the SAME shape as a
  `tree` file: `{ fileId, folderId, name, sizeBytes, contentType, updatedAt }`.
- `POST folder` → the folder row in the `tree` folder shape:
  `{ folderId, parentId, name, createdAt }`.
- `POST move` → the moved row (file or folder shape as above).
- `DELETE item` → `{ "ok": true, "files": <n>, "folders": <n> }` (rows removed).

The client tolerates extra fields, and falls back to re-reading `tree` if an id
is missing, but it relies on `fileId` / `folderId` being present.

### `tree` response

```json
{
  "storageId": "stor_…",
  "etag": "\"<opaque>\"",
  "folders": [ { "folderId": "fold_…", "parentId": null, "name": "Docs", "createdAt": "ISO" } ],
  "files":   [ { "fileId": "file_…", "folderId": null, "name": "a.txt", "sizeBytes": 12,
                 "contentType": "text/plain", "updatedAt": "ISO" } ]
}
```

`etag` changes whenever anything in the drive changes (files/folders added,
removed, renamed, moved, rewritten). The response's `ETag` header is the same
string. The client polls `tree` every ~5 s with `If-None-Match` and diffs
against its last copy — there is no change feed in v1. (Server side it is a hash
of the tree's rows computed in the database, so it changes on exactly the
changes the tree shows — including a folder rename, which touches no
timestamp — and a 304 costs no row transfer.)

**Names.** The panel refuses (400 `invalid_name`) empty names, `.`, `..`, and
names containing `/`, `\` or control characters; files ≤ 255 chars, folders ≤
120. It keeps names unique per PARENT across files AND folders,
case-insensitively (Windows semantics: no `a.txt` beside `A.TXT`, no file
beside a same-named folder) — on create, rename and move, from this API and
from the web app's tRPC. BUT web browser uploads (`/api/storage/upload`) and
older data don't go through that check, so a tree CAN contain duplicates; the
client must tolerate them (e.g. show the later ones with a suffix, or hide them)
rather than crash. Other Windows-invalid characters (`<>:"|?*`) can also appear
in names created from the web. The client maps `/<folder names…>/<file name>`
paths onto ids from this tree.

**Moves never copy bytes.** A file's storage key is fixed at upload; moving a
file or folder only rewrites rows, so moving a 10 GB folder is instant and a
file's `fileId` never changes.

## Windows app behaviour (v1, Cloud Files API)

The app is a Windows **cloud files provider** (the Cloud Files API /
`cldapi`, the same platform OneDrive uses). No kernel driver or third-party
filesystem is involved.

- **One sync root per drive**: a normal NTFS folder,
  `%USERPROFILE%\OTHCloud\<drive name>`, registered with the shell
  (`StorageProviderSyncRootManager`, id `OTHCloudDrive!<user SID>!<storageId>`,
  display name = drive name). Hydration policy *Progressive*, population
  policy *AlwaysFull*: the whole `tree` is turned into placeholders up front
  (the protocol only offers the whole tree anyway), so listing and search
  work offline and nothing is fetched to show a folder.
- **Also a drive letter per drive**: each sync root is aliased as its own
  letter from `O:` upward (skipping taken letters) with `subst` semantics
  (`DefineDosDeviceW`). A definition belongs to the LOGON SESSION, not the
  process: it outlives the app (quit or killed) and ends at logoff. So quit
  leaves letters (and their label/icon keys) in place; only removing a drive
  from this PC, moving it to another letter, or signing out removes one.
  The letter each drive has is kept in `config.json` (`mapped`), and
  `app::restore_letters` puts them back — only for drives still wanted
  whose folder is one of our registered sync roots, only onto a free letter,
  no network — at the app's start and, in OTHCloud Companion, from the
  per-user logon entry `HKCU\…\Run\OTHCloud Companion drive letters` =
  `"<exe>" --letters` (maps and exits in ~0.1 s; present while any drive
  has a letter). Letter changes are serialised across processes by the
  mutex `Local\OTHCloudDriveLetters-<SID>` (DefineDosDevice stacks a second
  definition of a letter). Explorer label/icon for the letter:
  `HKCU\Software\Classes\Applications\Explorer.exe\Drives\<L>\DefaultLabel`
  / `DefaultIcon`.
- **Quitting never removes anything Explorer shows**: sync roots stay
  registered (sidebar entries, placeholders with their cloud state), and
  downloaded files open with the app off. What the app's absence breaks is
  everything that needs the provider: opening a file that isn't downloaded
  fails at once with `0x8007016A` "The cloud file provider is not running"
  (`0x80070194` "…exited unexpectedly" after a crash). Windows has no way to
  start an unpackaged provider for that (nor a packaged one: there is no
  activation for `FETCH_DATA`); see "Starting on demand" below.
- **Drive icon** (letter + sync-root sidebar entry, the same .ico): the
  cover (`GET cover`, revalidated with `If-None-Match` at most once a minute)
  in a rounded tile with an OTHCloud badge; without a cover, a tile in the
  drive's `color` (from `GET drives`; fallback brand navy) with the OTH logo.
  A cover or colour change writes a NEW .ico file name (Explorer caches icons
  by path), repoints both icons and deletes the old file.
- **Remote → local**: `tree` is polled every ~5 s with `If-None-Match`; the
  diff becomes placeholder create / update+dehydrate (content changed) /
  rename / delete (only files that are in sync; local unsynced edits win and
  are uploaded).
- **Reads**: a placeholder is hydrated on first access by ranged `GET file`
  calls (≤ 4 MiB each, exactly the byte ranges Windows requests). The
  placeholder itself is the cache; *Free up space* / *Always keep on this
  device* (`attrib +U` / `+P`) dehydrate / hydrate it.
- **Local → remote**: renames/moves and deletes of placeholders arrive as
  platform callbacks and become `POST move` / `DELETE item` (deletes after a
  few seconds' grace, so a save-by-replace can reuse the id). New and
  modified files/folders are found by a directory watcher (+ a rescan) and
  uploaded (`PUT file`, create or replace, always with `Content-Length`),
  then marked in sync. Scratch files (`~$*`, `*.tmp`, …) never leave the PC;
  Office's "rename original away, rename new file into place" and editors'
  `MoveFileEx(REPLACE_EXISTING)` keep the server `fileId`.
- **Sign out** unregisters every sync root and deletes the placeholders;
  anything not uploaded is first copied to
  `%USERPROFILE%\OTHCloud\Not uploaded - <drive> - <time>`.
- Folder backup: optional — see "Folder backup" below (`src/backup.rs`). The
  photo backup of earlier versions is migrated once into its `photos` folder
  (`BackupSettings::migrate`), keeping `Photo backup/<PC name>` and its
  upload state `backup-<storageId>.json`, so nothing is uploaded twice.
- Tray icon: status, per-drive sync toggle + open, pause sync, photo backup,
  organization, sign in/out, quit.

## Thumbnails (Explorer, cloud files)

Without a handler, the shell answers `WTS_E_NOSTORAGEPROVIDERTHUMBNAILHANDLER`
for every placeholder that isn't downloaded, and Explorer shows icons for
cloud photos. An app that ships a handler (OTHCloud Companion,
`crates/thumbnails`) wires it up like this:

- **Registration.** The class is registered per user under
  `HKCU\Software\Classes\CLSID\{8164AFD8-19F7-4DF9-9B00-A83F2459AB2A}`
  (`InprocServer32` = the DLL, `ThreadingModel` = Both) with an AppID whose
  `DllSurrogate` is empty. The surrogate is required: the shell creates a sync
  root's thumbnail handler out of process (`CLSCTX_LOCAL_SERVER`); an
  in-proc-only class fails with `WTS_E_FAILEDEXTRACTION` without the DLL ever
  loading. The DLL is copied to a content-versioned file name before it is
  registered, because a surrogate can keep the old one loaded.
- **Sync roots.** The app calls `cf::set_thumbnail_provider(Some(clsid))`;
  every `register_sync_root` then writes the CLSID as the REG_SZ value
  `ThumbnailProvider` on the root's
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\<id>`
  key (`BUILTIN\Users` may set values there; no elevation), and
  `unregister_sync_root` removes it. The standalone app sets nothing.
- **The DLL holds no token and makes no network calls.** For each picture it
  asks the running app over the named pipe
  `\\.\pipe\othcloud-drive-thumbs-<user SID>` (DACL: this user and SYSTEM;
  remote clients refused; the DLL opens it with `SECURITY_IDENTIFICATION`
  and refuses a server process running as another user). One request per
  connection, little-endian:

  ```
  request   "OTHQ" | u32 version = 1 | u32 size px (1..4096) | u32 path bytes (even, ≤ 65534) | path, UTF-16LE
  response  "OTHR" | u32 status | u32 body bytes (≤ 4 MiB) | body
  status    0 ok (body = JPEG/PNG) · 1 not ours · 2 no thumbnail · 3 failed · 4 busy · 5 bad request
  ```

- **The app** resolves the path to its final form (a drive-letter alias
  included), refuses anything not inside one of its sync roots, not a file,
  not an image by extension, not a placeholder, or not in sync (local
  changes: the panel's picture would be stale), takes the `fileId` from the
  placeholder's identity — opening it for attributes only, so nothing is
  hydrated — and calls `GET thumbnail` (≤ 4 at once, 4.5 s). Answers,
  including "none", are cached in memory (1024 entries / 48 MB) under
  fileId + size + the placeholder's size and modified time.
- The DLL gives up after 5 s; any failure is an error to the shell, which then
  shows the file's icon.
- An embedding app may start answering before its drives connect
  (`App::prime_thumbnails`: token + org + the registered roots, no network).

## Starting on demand (Windows, OTHCloud Companion)

Only the process that called `CfConnectSyncRoot` can serve a sync root, and
nothing in Windows restarts it. But Explorer itself loads two COM classes a
sync root names in its `SyncRootManager\<id>` key, out of process, in a
dllhost surrogate of the user: `ThumbnailProvider` (above) and
`StorageProviderStatusUISourceFactory` (the provider status, asked for when
the user NAVIGATES into the sync root's folder — any folder, pictures or
not; via the real path or the sidebar, not via a drive-letter alias). The
Companion's DLL implements both (`{8164AFD8-…}` and `{61645C88-45C0-4D54-
AA6D-6C33B74C42D0}`, same AppID) and, when the pipe isn't there:

- starts the Companion — only the exe named by the REG_SZ value
  `OTHCloudCompanion` on its own `HKCU\Software\Classes\CLSID\{8164AFD8-…}`
  key (written by the app), only an absolute local path to an existing
  `othcloud-companion.exe`, via `CreateProcessW` with the application name
  set, with fixed arguments `--background --by=thumbnails|status`; not if
  the single-instance mutex `Local\OTHCloudDrive-<user>` exists; at most once
  per 20 s per surrogate;
- thumbnails wait up to 20 s for the pipe (measured: ~1.8 s for the first
  picture from cold); the status reports *Starting OTHCloud Companion…* and
  raises `StatusUIChanged` once it runs (*Offline / isn't running* if it
  can't be started).

The DLL still holds no token. Once the app is up the drives connect (~2–4 s)
and opening cloud-only files works again; a file opened before that, or from
outside Explorer (an app's recent files, a script) with the app not running,
still gets `0x8007016A`.

## Backup status in File Explorer (Windows, OTHCloud Companion)

Backed-up folders are ordinary local folders, not sync roots, so the Cloud
Files states (and `StorageProviderItemProperties`) don't apply to them. The
Companion shows a mark on their files and folders with three **shell icon
overlay handlers** in its shell DLL (`crates/thumbnails/src/overlay.rs`):
*backed up* (uploaded, unchanged), *backing up* (new or changed: settling,
queued, uploading, paused, waiting for the panel), *problem* (failed, in use
after retries, over the size limit, the drive is full). Files skipped by rule
(hidden/system, scratch names, excludes, `.git`, `node_modules`, links) get
no mark. A folder shows the worst mark of what the backup knows is inside it.

- **Registration — two halves.** Explorer reads the LIST of overlay handlers
  only from `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\
  ShellIconOverlayIdentifiers` (checked on Windows 11 26200: entries under
  HKCU are never enumerated — the handler is not even asked for its icon).
  So, once and only when the user asks (UAC): three entries there,
  `"  OTHCloud1Problem"`, `"  OTHCloud2Pending"`, `"  OTHCloud3BackedUp"`
  (two leading spaces: ahead of most other apps; Windows shows about 11
  handler overlays, in name order) → `{3C0E5A51-6B3F-4E0B-9B57-2D3A1F0C7E01…03}`,
  written by Windows' own `reg.exe` with fixed arguments. The classes
  themselves are per user (`HKCU\Software\Classes\CLSID\{…}`, the staged
  DLL, `ThreadingModel` Both) and exist only while the setting is on and a
  backup folder is on; without them the HKLM entries are inert. Explorer
  reads the list when it starts: new entries show after it restarts.
- **`IsMemberOf` never waits.** Explorer (and any program showing a file
  dialog) loads the handlers in process and asks for every item it draws.
  The DLL answers from (1) the **status board**, a 64 KiB shared-memory
  section `Local\othcloud-backup-status-<user SID>` (DACL: this user and
  SYSTEM) the app writes:

  ```
  0   "OTHB"
  4   u32 seq          odd while being written (seqlock)
  8   u64 generation   bumped whenever an answer may have changed
  16  u32 roots bytes
  20  roots            UTF-16LE, lower-case, each ending in '\', NUL-separated
  ```

  — no board, or a path outside every root: "no" at once (~0.7 µs); and (2)
  an in-process cache of directory → marks (512 directories). A miss or a
  stale entry (generation moved on, or older than 10 s) answers from what it
  has and queues the directory for a background thread, which asks the app
  and calls `SHChangeNotify(SHCNE_UPDATEITEM)` for each entry whose mark
  changed (past 200: `SHCNE_UPDATEDIR`), so Explorer asks again. It never
  starts the app; no answer (no pipe) = no marks until the board moves on.
- **The request**, on the thumbnail pipe (same DACL and checks):

  ```
  request   "OTHS" | u32 version = 1 | u32 flags = 0 | u32 path bytes | directory, UTF-16LE
  response  "OTHR" | u32 status | u32 body bytes | body
  body      u64 generation | u32 count | count × (u8 mark | u8 0 | u16 name units | name UTF-16LE)
  mark      1 backed up · 2 backing up · 3 problem   (unmarked entries are left out)
  status    0 ok · 1 not inside a backed-up folder (or the status is off) · 4 busy · 5 bad request
  ```

  The directory may be a backed-up folder, anything inside one, or the
  folder one is in (then only the backed-up folder itself is listed). Body
  ≤ 4 MiB, ≤ 20 000 entries looked at.
- **The app's answer** (`shellstatus.rs`) is local only: one listing of the
  directory and what the backup published at its save points (relative path
  → uploaded size + mtime; the files waiting; the files that failed; per
  folder the sets of folders with something waiting / failing inside). A
  file is *backed up* when its size and modified time are what was
  uploaded — so a file changed a second ago is *backing up* before any pass
  has seen it. Changes are batched once a second: the generation is bumped,
  then `SHChangeNotify` for the changed items and their folders (≤ 300, else
  the folders). On quit the board lists nothing.

## Folder backup (devices, profiles, versions)

The user picks local folders (Desktop, Documents, any folder) and a drive; the
app backs them up **one way, PC → drive**. Photo backup becomes one of these
folders (images only), keeping its existing remote folder and upload state so
nothing is uploaded twice.

### Rules
- **Never deletes remotely.** A file removed on the PC stays in the drive and
  is marked *deleted on this PC* (`deletedOnDeviceAt`); if it reappears with
  the same relative path the mark is cleared.
- **Versions.** Replacing a backed-up file keeps the previous content as a
  version; the newest **5** versions per file are kept (older ones are removed
  by the panel). Versions count toward the drive's quota. Any file can have
  versions — the web Versions menu is generic — but only backup uploads
  create them for now.
- **Layout.** `<drive>/Backups/<device name>/<folder label>/<same sub-paths>`.
  Photo backup keeps `<drive>/Photo backup/<device name>`.
- **Skipped**: hidden/system files, `desktop.ini`, `Thumbs.db`, `~$*`, `*.tmp`,
  `.git/`, `node_modules/`, files the OS won't let us read (locked — retried,
  then reported), symlinks/junctions (never followed), and anything over the
  drive's per-file limit. Users can add exclude patterns per folder.
- Files are uploaded when they have been unchanged for ~10 s (no half-written
  files), at most 3 at a time; a rescan runs at start and every 6 h to catch
  missed watcher events.

### Endpoints — under `/api/desktop/backup/` (desktop bearer + `X-OTH-Org`)

| method / path | body → response |
|---|---|
| `PUT device` | `{ deviceKey, name, platform: "windows", osVersion, appVersion }` → `{ deviceId }`. `deviceKey` is a random id the app creates once per install (kept in its settings); same key = same device, so reinstalling with the settings kept, or renaming the PC, updates the row. |
| `GET profile?deviceId=` | → `{ profile: Profile \| null }` |
| `PUT profile` | `{ deviceId, storageId, paused, folders: [{ folderKey, label, localPath, kind: "folder"\|"photos", enabled, excludes: string[], remoteFolderId?: string\|null }] }` → `{ profile }` with each folder's `remoteFolderId` filled in (the panel creates the remote folders). `localPath` is display-only for the web. |
| `POST status` | `{ deviceId, folders: [{ folderKey, files, bytes, pending, lastBackupAt, lastError }] }` → 204. Sent after each run and at most once a minute while busy. |
| `POST deleted` | `{ deviceId, fileIds: string[], deleted: boolean }` → 204 (marks / unmarks *deleted on this PC*). Only files under that device's backup folders. |

Uploads use the normal drive endpoints. A replace (`PUT file?fileId=`) with
header **`X-OTH-Keep-Version: 1`** makes the panel move the current content to
a version before replacing it (only allowed for files under one of the
caller's backup folders; otherwise 403).

`Profile = { profileId, deviceId, storageId, paused, folders: [{ folderKey, label, localPath, kind, enabled, excludes, remoteFolderId, status }], updatedAt }`.

`status` is the folder's last `POST status`, or `null` if it never reported:
`{ files, bytes, pending, lastBackupAt: ISO|null, lastError: string|null, reportedAt: ISO }`.

### Details the panel enforces (panel side, as built)

- **A device is per user AND organization.** `deviceId` is unique for
  (user, `X-OTH-Org`, `deviceKey`): drives belong to an org, and a device whose
  profile pointed at a drive in another org could be neither shown nor
  checked. So the app gets a DIFFERENT `deviceId` (and its own profile) in each
  org it is used in. Call `PUT device` at every start and after switching org,
  and use the `deviceId` it returns; a `deviceId` from another org is 404
  `not_found` on every endpoint here. `PUT device` also links the device to
  the token the request came with (versions record "which device").
- **Validation** (400 `bad_request` unless noted): `deviceKey` 8–128 of
  `A–Z a–z 0–9 _ -`; `name` 1–100 chars; `platform` one of `windows`, `macos`,
  `linux`, `ios`, `android` (default `windows`); `osVersion` / `appVersion` ≤ 64.
  Profile: ≤ 50 folders; `folderKey` 1–64 of `A–Z a–z 0–9 _ -`, unique;
  `label` 1–120 chars without `/`, `\`, control characters, `.`/`..`, unique
  case-insensitively (400 `invalid_name` — it becomes a drive folder name);
  `localPath` ≤ 1024; `excludes` ≤ 100 patterns of 1–260 chars (duplicates
  dropped). No control characters in any of these. Body ≤ 2 MB.
- **`storageId`** must be a drive the caller can write (404 otherwise, as on
  every drive endpoint). Changing it starts fresh folders in the new drive;
  what was backed up to the old one stays there.
- **How `remoteFolderId` is chosen**, per folder, first match wins:
  1. the drive folder this `folderKey` already has (same drive, still exists);
     a changed `label` renames it when the panel made it and the name is free;
  2. `kind: "photos"` only: the `remoteFolderId` the app SENT (the folder its
     existing photo backup already uploads to, so nothing is uploaded twice) —
     ignored for `kind: "folder"`, or when it isn't a folder in that drive;
  3. `kind: "photos"` only: an existing `Photo backup/<device name>`;
  4. `Backups/<device name>/<label>`, made if missing (an existing folder of
     that name is adopted). The device folder is remembered, so a renamed PC
     keeps writing into the one it has.
  A drive folder serves ONE backup folder of ONE device: if another device
  already uses `Backups/<name>` (two PCs with the same name) or a folder, the
  panel takes `<name> (2)` instead. A file in the way is treated the same.
- **A deleted drive folder**: if someone deletes a backup's drive folder in
  the web app, its `remoteFolderId` becomes `null` in `GET profile`. Uploads
  into it fail 404; the app should `PUT profile` again, which makes a new one.
- **`POST status`**: unknown `folderKey`s are ignored; `files`/`bytes`/`pending`
  whole numbers ≥ 0; `lastBackupAt` ISO or null; `lastError` is kept up to
  2000 chars with line breaks folded to spaces. Also refreshes "last seen".
- **`POST deleted`**: ≤ 1000 ids per call. Ids that are unknown, in another
  drive, or not under one of THAT device's backup folders are skipped silently
  (still 204), so a file moved or deleted in the web app can't wedge retries.
  A mark keeps its first date.
- **`X-OTH-Keep-Version`**: `1` keeps a version, absent or `0` is an ordinary
  replace, anything else 400. Allowed for files under any backup folder of
  any of the CALLER's devices in that drive; otherwise **403
  `not_backup_file`**, sent before the body is read. Quota: the whole new body
  counts (the old content stays, as a version) less whatever pruning to 5
  frees — 409 `quota_exceeded` otherwise. The new content goes to a new object
  and the old one becomes the version, so nothing is copied and an aborted
  upload leaves the file as it was. The version's date is when that content
  was written (the file's previous `updatedAt`).
- **Any desktop replace** (`PUT file?fileId=`, with or without the header)
  clears the file's *deleted on this PC* mark — it was just written from a PC.
- Deleting a file or folder (web or desktop) deletes its versions too, and
  frees their bytes from the drive's size.

### Web (Drives app)
- **Backups** view: every device that has signed in to a desktop app for this
  user/org (from `desktop_token` + backup devices: name, app, platform, first
  and last seen, signed in / revoked), each with its backup profile (drive,
  folders, last backup, files/size, errors, paused) and a browser of its backed-
  up files with a "Show files deleted on the PC" filter.
- **Versions** menu on any file that has versions: list (date, size, device),
  download, restore (current becomes a version; the chosen one becomes current),
  delete a version.
- Panel side: `api.driveBackup.*` (`server/api/routers/drive-backup.ts`),
  `server/drive-backup.ts`, `lib/drive-backup.ts` (pure rules, tested in
  `__test__/drive-backup/`); a version downloads through the same-origin,
  session-checked `/api/storage/version?versionId=…&download=1`. Restoring
  swaps object keys (no copy, no quota change). DDL
  `apps/othcloud/drizzle/0193_drive_backup.sql`, `npm run db:drive-backup`.
