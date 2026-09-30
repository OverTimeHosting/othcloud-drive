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
| 403 | `not_member` (X-OTH-Org), `wrong_client` (a game/remote token) |
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
| GET | `drives` | `[{ storageId, name, sizeBytes, quotaBytes, hasCover, updatedAt }]` |
| GET | `cover?storageId=` | the drive's cover image bytes (`Content-Type` image/*); 404 if none |
| GET | `tree?storageId=` | the WHOLE drive, flat (see below). Honors `If-None-Match` → 304 |
| GET/HEAD | `file?fileId=` | file bytes. Honors `Range` (single `bytes=a-b` / `a-` / `-n`) → 206 + `Content-Range`; past the end → 416. Sends `ETag` (changes with the contents) and honors `If-None-Match` → 304 |
| PUT | `file?storageId=&folderId=&name=` | create a NEW file from the raw body → 201. `folderId` omitted/empty = drive root. 409 `name_taken` if a file OR folder with that name exists in that folder |
| PUT | `file?fileId=` | REPLACE the contents of an existing file (same id, name, shares) |
| POST | `folder` | JSON `{ storageId, parentId: string\|null, name }` → the folder row. 409 `name_taken` |
| POST | `move` | JSON `{ type: "file"\|"folder", id, name?: string, parentId?: string\|null }` — rename and/or move (parentId null = root; omitted = unchanged). 409 `name_taken`; moving a folder into itself/descendant = 400 |
| DELETE | `item?type=file\|folder&id=` | delete (folder = recursive) |

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
string. The client polls `tree` every ~15 s with `If-None-Match` and diffs
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

## Windows app behaviour (v1)

- Each OTHCloud drive the user picks is mounted as its OWN drive letter
  (WinFsp), starting at `O:` and skipping taken letters. Volume label = drive
  name. The drive's cover image becomes that letter's Explorer icon
  (converted to .ico, registered per-user under
  `HKCU\Software\Classes\Applications\Explorer.exe\Drives\<L>\DefaultIcon`).
- Two-way: reads fetch on demand (ranges) into a local cache
  (`%LOCALAPPDATA%\OTHCloud Drive\cache`); writes land in the cache and are
  uploaded on close/flush; create/rename/move/delete call the API immediately.
- Photo backup: optional — watch a chosen local folder (default the user's
  Pictures) and upload new images into a chosen drive/folder
  (default `<drive>/Photo backup/<PC name>`), never deleting anything remotely.
- Tray icon: status, pause sync, open drive, settings, sign out.
