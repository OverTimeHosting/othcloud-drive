<#
End-to-end mount test: mounts ONE drive with the headless `mount` command,
drives it from PowerShell like a user would, and checks every effect on the
server through the protocol API (so it works against the mock server and a
real panel alike).

  # against the mock (started and stopped by the script):
  powershell -File scripts\mount-test.ps1 -Mock
  # against a panel:
  powershell -File scripts\mount-test.ps1 -Origin http://127.0.0.1:3900 -Token … -Org … -Storage …

Everything it creates lives in a fresh top-level folder "mt-<random>", which
is deleted at the end (through the mounted drive).
#>
param(
    [string]$Origin,
    [string]$Token,
    [string]$Org,
    [string]$Storage,
    [switch]$Mock,
    [string]$Exe = "$PSScriptRoot\..\target\release\othcloud-drive.exe",
    [int]$PollSecs = 3
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot\.."
$work = Join-Path $env:TEMP ("othcloud-mt-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force $work | Out-Null
$env:OTHCLOUD_DRIVE_DATA_DIR = "$work\data"
$results = New-Object System.Collections.ArrayList
function Check($name, [scriptblock]$body) {
    try {
        $r = & $body
        if ($r -eq $false) { throw "returned false" }
        [void]$results.Add([pscustomobject]@{ Test = $name; Result = 'PASS'; Detail = "$r" })
        Write-Host "PASS  $name  $r"
    } catch {
        [void]$results.Add([pscustomobject]@{ Test = $name; Result = 'FAIL'; Detail = "$_" })
        Write-Host "FAIL  $name  $_" -ForegroundColor Red
    }
}

$mockProc = $null
if ($Mock) {
    $port = Get-Random -Minimum 20000 -Maximum 40000
    $mockProc = Start-Process "$root\target\release\examples\mock_server.exe" -ArgumentList $port `
        -RedirectStandardOutput "$work\mock.out" -PassThru -WindowStyle Hidden
    for ($i = 0; $i -lt 50 -and -not ((Test-Path "$work\mock.out") -and ((Get-Item "$work\mock.out").Length -gt 0)); $i++) { Start-Sleep -Milliseconds 100 }
    Start-Sleep -Milliseconds 300
    $info = Get-Content "$work\mock.out" -Raw | ConvertFrom-Json
    $Origin = $info.origin; $Token = $info.token; $Org = $info.org; $Storage = $info.drives[0]
}
$H = @{ Authorization = "Bearer $Token"; 'X-OTH-Org' = $Org }
$api = "$Origin/api/desktop/drive"

function Get-Tree { Invoke-RestMethod -Headers $H "$api/tree?storageId=$Storage" }
# Map of "a/b/c" -> file row (files) and "a/b/" -> folder row.
function Get-ServerPaths {
    $t = Get-Tree
    $byId = @{}; foreach ($f in $t.folders) { $byId[$f.folderId] = $f }
    function FolderPath($id) {
        $parts = @()
        while ($id) { $f = $byId[$id]; $parts = , $f.name + $parts; $id = $f.parentId }
        ($parts -join '/')
    }
    $m = @{}
    foreach ($f in $t.folders) { $m[(FolderPath $f.folderId) + '/'] = $f }
    foreach ($f in $t.files) {
        $p = if ($f.folderId) { (FolderPath $f.folderId) + '/' + $f.name } else { $f.name }
        $m[$p] = $f
    }
    $m
}
function Get-ServerHash($path) {
    $f = (Get-ServerPaths)[$path]
    if (-not $f) { throw "not on server: $path" }
    $tmp = "$work\dl.bin"
    Invoke-WebRequest -Headers $H "$api/file?fileId=$($f.fileId)" -OutFile $tmp -UseBasicParsing
    (Get-FileHash $tmp -Algorithm SHA256).Hash
}
function Wait-Until([scriptblock]$cond, [int]$secs = 30) {
    $end = (Get-Date).AddSeconds($secs)
    while ((Get-Date) -lt $end) { try { if (& $cond) { return $true } } catch {}; Start-Sleep -Milliseconds 250 }
    return $false
}

Add-Type @"
using System; using System.Runtime.InteropServices;
public static class K32 {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern bool MoveFileEx(string from, string to, int flags);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode)]
  public static extern bool GetVolumeInformation(string root, System.Text.StringBuilder label, int labelLen,
      out uint serial, out uint maxComp, out uint flags, System.Text.StringBuilder fs, int fsLen);
}
"@

# ---------------------------------------------------------------- mount
$stop = "$work\stop"
# Whatever fails below, never leave a drive mounted or the mock running.
trap {
    Write-Host "ABORTED: $_" -ForegroundColor Red
    Set-Content $stop "x"; Start-Sleep 3
    if ($mountProc) { Stop-Process -Id $mountProc.Id -Force -ErrorAction SilentlyContinue }
    if ($mockProc) { Stop-Process -Id $mockProc.Id -ErrorAction SilentlyContinue }
    exit 99
}
$mountProc = Start-Process $Exe -ArgumentList "mount", "--origin", $Origin, "--token", $Token, "--org", $Org, `
    "--storage", $Storage, "--poll-secs", $PollSecs, "--stop-file", $stop `
    -RedirectStandardOutput "$work\mount.out" -RedirectStandardError "$work\mount.err" -PassThru -WindowStyle Hidden
$mounted = Wait-Until { (Get-Content "$work\mount.out" -ErrorAction SilentlyContinue) -match 'MOUNTED' } 60
if (-not $mounted) {
    Write-Host "mount failed:"; Get-Content "$work\mount.err"; if ($mockProc) { Stop-Process $mockProc.Id }; exit 1
}
# @(…): a one-line file comes back as a string, and `string -match` is a bool.
$L = (@(Get-Content "$work\mount.out") -match 'MOUNTED' | Select-Object -First 1).Split(' ')[2].TrimEnd(':')
$D = "${L}:\"
Write-Host "mounted at $D"
$drive = (Invoke-RestMethod -Headers $H "$api/drives") | Where-Object { $_.storageId -eq $Storage }
$T = "mt-" + [guid]::NewGuid().ToString('N').Substring(0, 6)
$TD = Join-Path $D $T

Check "volume label = drive name" {
    $sb = New-Object System.Text.StringBuilder 64; $fs = New-Object System.Text.StringBuilder 64
    $s = 0; $m = 0; $f = 0
    [void][K32]::GetVolumeInformation($D, $sb, 64, [ref]$s, [ref]$m, [ref]$f, $fs, 64)
    if ($sb.ToString() -ne $drive.name.Substring(0, [Math]::Min(32, $drive.name.Length))) { throw "label '$sb' fs '$fs'" }
    "label='$sb' fs='$fs'"
}
Check "Explorer DefaultLabel/DefaultIcon registered" {
    $k = "HKCU:\Software\Classes\Applications\Explorer.exe\Drives\$L"
    $label = (Get-ItemProperty "$k\DefaultLabel").'(default)'
    if ($label -ne $drive.name) { throw "label $label" }
    $icon = (Get-ItemProperty "$k\DefaultIcon" -ErrorAction SilentlyContinue).'(default)'
    if ($drive.hasCover -and -not (Test-Path $icon)) { throw "icon missing: $icon" }
    "label=$label icon=$icon"
}
Check "dir root matches server" {
    $local = (Get-ChildItem $D -Force | ForEach-Object Name) | Sort-Object
    $srv = (Get-ServerPaths).Keys | Where-Object { $_ -notmatch '/.' } | ForEach-Object { $_.TrimEnd('/') } | Sort-Object
    $diff = Compare-Object @($local) @($srv)
    if ($diff) { throw ($diff | Out-String) }
    "$($local.Count) entries"
}
Check "type an existing file" {
    $f = Get-ChildItem $D -File | Select-Object -First 1
    if (-not $f) { return "no files at root; skipped" }
    $local = (Get-FileHash $f.FullName -Algorithm SHA256).Hash
    if ($local -ne (Get-ServerHash $f.Name)) { throw "hash mismatch" }
    cmd /c "type `"$($f.FullName)`" > nul"
    "$($f.Name) $($f.Length) bytes"
}

Check "mkdir" {
    New-Item -ItemType Directory $TD | Out-Null
    if (-not (Get-ServerPaths).ContainsKey("$T/")) { throw "folder not on server" }
    "created $T"
}
$big = "$work\big.bin"
$rng = New-Object byte[] (9 * 1024 * 1024 + 12345); (New-Object Random 42).NextBytes($rng)
[IO.File]::WriteAllBytes($big, $rng)
$bigHash = (Get-FileHash $big -Algorithm SHA256).Hash
Check "Copy-Item 9 MB in, read back identical, server identical" {
    Copy-Item $big "$TD\big.bin"
    $back = (Get-FileHash "$TD\big.bin" -Algorithm SHA256).Hash
    if ($back -ne $bigHash) { throw "local read-back differs" }
    if (-not (Wait-Until { (Get-ServerHash "$T/big.bin") -eq $bigHash } 60)) { throw "server copy differs/missing" }
    "sha256 $($bigHash.Substring(0,16))…"
}
Check "Copy-Item out of the drive (fresh cache via ranged reads)" {
    Copy-Item "$TD\big.bin" "$work\out.bin"
    if ((Get-FileHash "$work\out.bin" -Algorithm SHA256).Hash -ne $bigHash) { throw "differs" }
    "ok"
}
Check "create + write text file" {
    Set-Content -Path "$TD\notes.txt" -Value "line one" -NoNewline
    if (-not (Wait-Until { (Get-ServerPaths).ContainsKey("$T/notes.txt") -and (Get-ServerPaths)["$T/notes.txt"].sizeBytes -eq 8 } 30)) { throw "not uploaded" }
    Get-Content "$TD\notes.txt"
}
Check "overwrite file (Set-Content truncates) keeps server id" {
    $id = (Get-ServerPaths)["$T/notes.txt"].fileId
    Set-Content -Path "$TD\notes.txt" -Value "rewritten, longer content" -NoNewline
    $ok = Wait-Until { $r = (Get-ServerPaths)["$T/notes.txt"]; $r.fileId -eq $id -and $r.sizeBytes -eq 25 } 30
    if (-not $ok) { throw "server not updated" }
    "id $id kept"
}
Check "append" {
    Add-Content -Path "$TD\notes.txt" -Value "!" -NoNewline
    if (-not (Wait-Until { (Get-ServerPaths)["$T/notes.txt"].sizeBytes -eq 26 } 30)) { throw "not appended" }
    Get-Content "$TD\notes.txt"
}
Check "editor save: write temp + MoveFileEx(REPLACE_EXISTING) over original" {
    $id = (Get-ServerPaths)["$T/notes.txt"].fileId
    [IO.File]::WriteAllText("$TD\notes.txt.tmp~", "saved by editor")
    if (-not [K32]::MoveFileEx("$TD\notes.txt.tmp~", "$TD\notes.txt", 1)) { throw "MoveFileEx failed $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
    $ok = Wait-Until { $r = (Get-ServerPaths)["$T/notes.txt"]; $r.fileId -eq $id -and $r.sizeBytes -eq 15 } 30
    if (-not $ok) { throw "server: $((Get-ServerPaths)["$T/notes.txt"] | ConvertTo-Json -Compress)" }
    if ((Get-ServerPaths).Keys -match 'tmp~') { throw "temp leaked" }
    "id kept, content: " + (Get-Content "$TD\notes.txt")
}
Check "Word-style save (~WRD.tmp / ~WRL.tmp dance) keeps id, no temp on server" {
    Set-Content "$TD\Report.docx" "v1" -NoNewline
    if (-not (Wait-Until { (Get-ServerPaths).ContainsKey("$T/Report.docx") } 30)) { throw "v1 not uploaded" }
    $id = (Get-ServerPaths)["$T/Report.docx"].fileId
    Set-Content "$TD\~`$Report.docx" "owner" -NoNewline
    Set-Content "$TD\~WRD0000.tmp" "version two" -NoNewline
    if (-not [K32]::MoveFileEx("$TD\Report.docx", "$TD\~WRL0001.tmp", 0)) { throw "rename away failed" }
    if (-not [K32]::MoveFileEx("$TD\~WRD0000.tmp", "$TD\Report.docx", 0)) { throw "rename in failed" }
    Remove-Item "$TD\~WRL0001.tmp"; Remove-Item -Force "$TD\~`$Report.docx"
    $ok = Wait-Until { $r = (Get-ServerPaths)["$T/Report.docx"]; $r.fileId -eq $id -and $r.sizeBytes -eq 11 } 30
    if (-not $ok) { throw "server: $((Get-ServerPaths)["$T/Report.docx"] | ConvertTo-Json -Compress)" }
    $leak = (Get-ServerPaths).Keys | Where-Object { $_ -like "$T/*" -and ($_ -match '\.tmp$' -or $_ -match '~\$') }
    if ($leak) { throw "temp leaked: $leak" }
    "id $id kept"
}
Check "rename file" {
    Rename-Item "$TD\notes.txt" "renamed.txt"
    $p = Get-ServerPaths
    if (-not $p.ContainsKey("$T/renamed.txt") -or $p.ContainsKey("$T/notes.txt")) { throw "server not renamed" }
    "ok"
}
Check "move file into subfolder" {
    New-Item -ItemType Directory "$TD\Sub" | Out-Null
    Move-Item "$TD\renamed.txt" "$TD\Sub\renamed.txt"
    if (-not (Get-ServerPaths).ContainsKey("$T/Sub/renamed.txt")) { throw "not moved" }
    Get-Content "$TD\Sub\renamed.txt"
}
Check "rename + move folder" {
    Rename-Item "$TD\Sub" "Sub2"
    New-Item -ItemType Directory "$TD\Outer" | Out-Null
    Move-Item "$TD\Sub2" "$TD\Outer\Sub2"
    if (-not (Get-ServerPaths).ContainsKey("$T/Outer/Sub2/renamed.txt")) { throw "not moved" }
    "ok"
}
Check "case-insensitive open, case preserved" {
    $c = Get-Content "$TD\OUTER\sub2\RENAMED.TXT"
    $n = (Get-ChildItem "$TD\Outer").Name
    if ($n -ne 'Sub2') { throw "case lost: $n" }
    $c
}
Check "Copy-Item -Recurse a folder tree" {
    $src = "$work\tree"
    New-Item -ItemType Directory -Force "$src\a\b\c", "$src\d" | Out-Null
    1..5 | ForEach-Object { Set-Content "$src\a\f$_.txt" "file $_" }
    Set-Content "$src\a\b\c\deep.txt" "deep"
    [IO.File]::WriteAllBytes("$src\d\blob.bin", $rng[0..300000])
    Copy-Item -Recurse $src "$TD\tree"
    $want = @("tree/", "tree/a/", "tree/a/b/", "tree/a/b/c/", "tree/a/b/c/deep.txt", "tree/d/", "tree/d/blob.bin") + (1..5 | ForEach-Object { "tree/a/f$_.txt" })
    $ok = Wait-Until { $p = Get-ServerPaths; ($want | Where-Object { -not $p.ContainsKey("$T/$_") }).Count -eq 0 } 60
    if (-not $ok) { $p = Get-ServerPaths; throw ("missing: " + (($want | Where-Object { -not $p.ContainsKey("$T/$_") }) -join ',')) }
    if ((Get-ServerHash "$T/tree/d/blob.bin") -ne (Get-FileHash "$src\d\blob.bin" -Algorithm SHA256).Hash) { throw "blob differs" }
    "$($want.Count) items"
}
Check "delete file" {
    Remove-Item "$TD\tree\a\f1.txt"
    if ((Get-ServerPaths).ContainsKey("$T/tree/a/f1.txt")) { throw "still on server" }
    "ok"
}
Check "delete non-empty folder recursively" {
    Remove-Item -Recurse "$TD\tree\a"
    if ((Get-ServerPaths).Keys -like "$T/tree/a/*") { throw "still on server" }
    if (Test-Path "$TD\tree\a") { throw "still local" }
    "ok"
}
Check "server-side change appears after a poll" {
    $tmp = "$work\srv.txt"; Set-Content $tmp "changed on the server" -NoNewline
    $id = (Get-ServerPaths)["$T/Report.docx"].fileId
    Invoke-RestMethod -Method Put -Headers $H -InFile $tmp -ContentType 'application/octet-stream' "$api/file?fileId=$id" | Out-Null
    $f2 = "$work\srvnew.txt"; Set-Content $f2 "brand new" -NoNewline
    $folderId = (Get-ServerPaths)["$T/"].folderId
    Invoke-RestMethod -Method Put -Headers $H -InFile $f2 -ContentType 'application/octet-stream' "$api/file?storageId=$Storage&folderId=$folderId&name=from-server.txt" | Out-Null
    $ok = Wait-Until { (Get-Content "$TD\Report.docx" -Raw) -eq "changed on the server" -and (Test-Path "$TD\from-server.txt") } ($PollSecs * 4 + 10)
    if (-not $ok) { throw "local: '$(Get-Content "$TD\Report.docx" -Raw)' new file: $(Test-Path "$TD\from-server.txt")" }
    Get-Content "$TD\from-server.txt"
}
Check "server-side delete disappears after a poll" {
    $id = (Get-ServerPaths)["$T/from-server.txt"].fileId
    Invoke-RestMethod -Method Delete -Headers $H "$api/item?type=file&id=$id" | Out-Null
    if (-not (Wait-Until { -not (Test-Path "$TD\from-server.txt") } ($PollSecs * 4 + 10))) { throw "still visible" }
    "ok"
}
Check "clean up test folder" {
    Remove-Item -Recurse -Force $TD
    if ((Get-ServerPaths).ContainsKey("$T/")) { throw "still on server" }
    "ok"
}

# -------------------------------------------------------------- unmount
Set-Content $stop "x"
[void]$mountProc.WaitForExit(90000)
Check "unmount: letter gone, registry keys removed" {
    if (Test-Path $D) { throw "$D still mounted" }
    if (Test-Path "HKCU:\Software\Classes\Applications\Explorer.exe\Drives\$L") { throw "registry key left" }
    (Get-Content "$work\mount.out") -join ' '
}
if ($mockProc) { Stop-Process -Id $mockProc.Id -ErrorAction SilentlyContinue }
Write-Host ""
$results | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
$fail = @($results | Where-Object Result -eq 'FAIL').Count
Write-Host "$(@($results).Count - $fail) passed, $fail failed. Logs: $work"
exit $fail
