<#
End-to-end test of one synced drive: starts the headless `mount` command
(a Cloud Files sync root in a scratch folder + a drive letter aliasing it),
drives it from PowerShell like a user would — through BOTH the letter and
the folder path — and checks every effect on the server through the
protocol API (so it works against the mock server and a real panel alike).

  # against the mock (started and stopped by the script):
  powershell -File scripts\mount-test.ps1 -Mock
  # against a panel:
  powershell -File scripts\mount-test.ps1 -Origin http://127.0.0.1:3900 -Token … -Org … -Storage …

Everything it creates on the server lives in a fresh top-level folder
"mt-<random>", deleted at the end. Locally, the sync root is created under
%TEMP% and removed (unregistered, placeholders deleted) on exit.
#>
param(
    [string]$Origin,
    [string]$Token,
    [string]$Org,
    [string]$Storage,
    [switch]$Mock,
    [string]$Exe = "$PSScriptRoot\..\target\release\othcloud-drive.exe",
    [int]$PollSecs = 3,
    # Real panel only: psql.exe + connection args for its LOCAL test database,
    # used to set/clear the drive's cover and colour for the icon checks.
    [string]$Psql,
    [string]$PgArgs = "-h 127.0.0.1 -p 55432 -U postgres -d othcloud_test"
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot\.."
$work = Join-Path $env:TEMP ("othcloud-mt-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force $work | Out-Null
$env:OTHCLOUD_DRIVE_DATA_DIR = "$work\data"
$env:OTHCLOUD_DRIVE_ROOT_BASE = "$work\roots"
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
function Hash($p) { (Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash }
# cfapi attribute bits
$RECALL = 0x00400000; $OFFLINE = 0x1000; $PINNED = 0x00080000; $UNPINNED = 0x00100000
function Attr($p) { [int](Get-Item -LiteralPath $p -Force).Attributes }
function Dehydrated($p) { ((Attr $p) -band $RECALL) -ne 0 }
$SRM = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager"

Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class K32 {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern bool MoveFileEx(string from, string to, int flags);
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern uint QueryDosDevice(string dev, StringBuilder target, int max);
}
"@
function DosTarget($letter) {
    $sb = New-Object System.Text.StringBuilder 1024
    if ([K32]::QueryDosDevice("${letter}:", $sb, 1024) -eq 0) { return $null }
    $sb.ToString()
}

# ---------------------------------------------------------------- mount
$stop = "$work\stop"
# Whatever fails below, never leave a drive mounted or the mock running.
trap {
    Write-Host "ABORTED: $_" -ForegroundColor Red
    Set-Content $stop "x"; Start-Sleep 5
    if ($mountProc) { Stop-Process -Id $mountProc.Id -Force -ErrorAction SilentlyContinue }
    Start-Process $Exe -ArgumentList "cleanup-test-roots" -Wait -WindowStyle Hidden -RedirectStandardOutput "$work\cleanup.out"
    if ($mockProc) { Stop-Process -Id $mockProc.Id -ErrorAction SilentlyContinue }
    exit 99
}
$mountProc = Start-Process $Exe -ArgumentList "mount", "--origin", $Origin, "--token", $Token, "--org", $Org, `
    "--storage", $Storage, "--poll-secs", $PollSecs, "--stop-file", $stop, "--remove-on-exit", `
    "--provider-id", "OTHCloudDriveTest", "--cover-secs", "3" `
    -RedirectStandardOutput "$work\mount.out" -RedirectStandardError "$work\mount.err" -PassThru -WindowStyle Hidden
$mounted = Wait-Until { @(Get-Content "$work\mount.out" -ErrorAction SilentlyContinue) -match 'MOUNTED' } 90
if (-not $mounted) {
    Write-Host "mount failed:"; Get-Content "$work\mount.err"; if ($mockProc) { Stop-Process $mockProc.Id }; exit 1
}
# "MOUNTED <storageId> <L>: <path>" — @(…): a one-line file comes back as a
# string, and `string -match` is a bool.
$line = (@(Get-Content "$work\mount.out") -match 'MOUNTED' | Select-Object -First 1)
$parts = $line.Split(' ', 4)
$L = $parts[2].TrimEnd(':')
$F = $parts[3]
$D = "${L}:\"
Write-Host "sync root $F, letter $D"
$drive = (Invoke-RestMethod -Headers $H "$api/drives") | Where-Object { $_.storageId -eq $Storage }
$T = "mt-" + [guid]::NewGuid().ToString('N').Substring(0, 6)
$TD = Join-Path $D $T     # test dir via the LETTER
$TF = Join-Path $F $T     # the same dir via the FOLDER path
$rootId = $null

Check "sync root registered with the shell" {
    $k = Get-ChildItem $SRM | Where-Object { $_.PSChildName -like "OTHCloudDriveTest!*!$Storage" }
    if (-not $k) { throw "no SyncRootManager entry" }
    $script:rootId = $k.PSChildName
    $p = (Get-ItemProperty "$($k.PSPath)\UserSyncRoots").PSObject.Properties | Where-Object { $_.Name -like 'S-1-*' } | Select-Object -First 1
    if ($p.Value -ne $F) { throw "registered path $($p.Value) != $F" }
    $name = (Get-ItemProperty $k.PSPath).DisplayNameResource
    if ($name -ne $drive.name) { throw "display name $name" }
    "$($k.PSChildName) -> $($p.Value)"
}
Check "drive letter aliases the sync root (subst semantics)" {
    $t = DosTarget $L
    if (-not $t -or $t.TrimStart('\??\'.ToCharArray()) -ne $F.TrimStart('\??\'.ToCharArray())) { throw "QueryDosDevice ${L}: = $t" }
    if (-not (Test-Path $D)) { throw "$D not accessible" }
    "${L}: -> $t"
}
Check "Explorer shows the letter with the drive's label and icon keys" {
    $k = "HKCU:\Software\Classes\Applications\Explorer.exe\Drives\$L"
    $label = (Get-ItemProperty "$k\DefaultLabel").'(default)'
    if ($label -ne $drive.name) { throw "DefaultLabel $label" }
    $icon = (Get-ItemProperty "$k\DefaultIcon" -ErrorAction SilentlyContinue).'(default)'
    if ($drive.hasCover -and -not (Test-Path $icon)) { throw "icon missing: $icon" }
    $pc = (New-Object -ComObject Shell.Application).NameSpace(17)   # This PC
    $item = @($pc.Items()) | Where-Object { $_.Path -eq $D }
    if (-not $item) { throw "This PC doesn't list $D" }
    "This PC: '$($item.Name)'; DefaultLabel=$label; DefaultIcon=$icon"
}
Check "dir root matches server (letter and folder)" {
    $srv = (Get-ServerPaths).Keys | Where-Object { $_ -notmatch '/.' } | ForEach-Object { $_.TrimEnd('/') } | Sort-Object
    foreach ($base in @($D, $F)) {
        $local = (Get-ChildItem $base -Force | ForEach-Object Name) | Sort-Object
        $diff = Compare-Object @($local) @($srv)
        if ($diff) { throw "$base differs: $($diff | Out-String)" }
    }
    "$(@($srv).Count) entries"
}
# A remote file to hydrate: the largest one at the root, or one we put there.
$probe = Get-ChildItem $F -File | Sort-Object Length -Descending | Select-Object -First 1
if (-not $probe) {
    $tmp = "$work\probe.bin"; [IO.File]::WriteAllBytes($tmp, (1..200000 | ForEach-Object { [byte]($_ % 251) }))
    Invoke-RestMethod -Method Put -Headers $H -InFile $tmp -ContentType 'application/octet-stream' "$api/file?storageId=$Storage&name=mt-probe-$T.bin" | Out-Null
    Wait-Until { Test-Path "$F\mt-probe-$T.bin" } ($PollSecs * 4 + 10) | Out-Null
    $probe = Get-Item "$F\mt-probe-$T.bin"
}
Check "remote file is a dehydrated placeholder until read" {
    $a = Attr $probe.FullName
    if (($a -band $RECALL) -eq 0 -or ($a -band $OFFLINE) -eq 0) { throw ("attributes 0x{0:X}" -f $a) }
    "{0}: 0x{1:X} (RECALL_ON_DATA_ACCESS|OFFLINE), size {2}" -f $probe.Name, $a, $probe.Length
}
Check "reading THROUGH THE LETTER hydrates the placeholder (seen via the folder)" {
    $hh = Hash (Join-Path $D $probe.Name)
    if ($hh -ne (Get-ServerHash $probe.Name)) { throw "hash differs from server" }
    if (Dehydrated $probe.FullName) { throw ("still dehydrated: 0x{0:X}" -f (Attr $probe.FullName)) }
    "sha256 $($hh.Substring(0,16))…, now 0x{0:X}" -f (Attr $probe.FullName)
}
Check "free up space (attrib +U) dehydrates; pin (attrib +P) hydrates" {
    & attrib -P +U $probe.FullName | Out-Null
    if (-not (Wait-Until { Dehydrated $probe.FullName } 20)) { throw ("not dehydrated: 0x{0:X}" -f (Attr $probe.FullName)) }
    $afterFree = Attr $probe.FullName
    & attrib -U +P $probe.FullName | Out-Null
    if (-not (Wait-Until { -not (Dehydrated $probe.FullName) } 30)) { throw ("pin didn't hydrate: 0x{0:X}" -f (Attr $probe.FullName)) }
    $pinned = Attr $probe.FullName
    if (($pinned -band $PINNED) -eq 0) { throw "not pinned" }
    & attrib -P $probe.FullName | Out-Null
    if ((Hash $probe.FullName) -ne (Get-ServerHash $probe.Name)) { throw "content changed" }
    "freed 0x{0:X} -> pinned 0x{1:X}" -f $afterFree, $pinned
}

Check "mkdir (via letter)" {
    New-Item -ItemType Directory $TD | Out-Null
    if (-not (Wait-Until { (Get-ServerPaths).ContainsKey("$T/") } 20)) { throw "folder not on server" }
    "created $T"
}
$big = "$work\big.bin"
$rng = New-Object byte[] (9 * 1024 * 1024 + 12345); (New-Object Random 42).NextBytes($rng)
[IO.File]::WriteAllBytes($big, $rng)
$bigHash = Hash $big
Check "Copy-Item 9 MB in via LETTER; server + folder read-back identical" {
    Copy-Item $big "$TD\big.bin"
    if (-not (Wait-Until { (Get-ServerHash "$T/big.bin") -eq $bigHash } 90)) { throw "server copy differs/missing" }
    if ((Hash "$TF\big.bin") -ne $bigHash) { throw "folder read-back differs" }
    "sha256 $($bigHash.Substring(0,16))…"
}
Check "Copy-Item 9 MB in via FOLDER; server + letter read-back identical" {
    Copy-Item $big "$TF\big2.bin"
    if (-not (Wait-Until { (Get-ServerHash "$T/big2.bin") -eq $bigHash } 90)) { throw "server copy differs/missing" }
    if ((Hash "$TD\big2.bin") -ne $bigHash) { throw "letter read-back differs" }
    "ok"
}
Check "copy OUT after freeing space: re-hydrates via letter AND via folder" {
    foreach ($pair in @(@("$TF\big.bin", "$TD\big.bin"), @("$TF\big2.bin", "$TF\big2.bin"))) {
        & attrib +U $pair[0] | Out-Null
        if (-not (Wait-Until { Dehydrated $pair[0] } 30)) { throw "not dehydrated: $($pair[0])" }
        & attrib -U $pair[0] | Out-Null
        Copy-Item $pair[1] "$work\out.bin" -Force
        if ((Hash "$work\out.bin") -ne $bigHash) { throw "copy out of $($pair[1]) differs" }
    }
    "ok"
}
Check "create + write text file" {
    Set-Content -Path "$TD\notes.txt" -Value "line one" -NoNewline
    if (-not (Wait-Until { (Get-ServerPaths)["$T/notes.txt"].sizeBytes -eq 8 } 30)) { throw "not uploaded" }
    Get-Content "$TF\notes.txt"
}
Check "overwrite file (Set-Content truncates) keeps server id" {
    $id = (Get-ServerPaths)["$T/notes.txt"].fileId
    Set-Content -Path "$TD\notes.txt" -Value "rewritten, longer content" -NoNewline
    $ok = Wait-Until { $r = (Get-ServerPaths)["$T/notes.txt"]; $r.fileId -eq $id -and $r.sizeBytes -eq 25 } 30
    if (-not $ok) { throw "server not updated" }
    "id $id kept"
}
Check "append" {
    Add-Content -Path "$TF\notes.txt" -Value "!" -NoNewline
    if (-not (Wait-Until { (Get-ServerPaths)["$T/notes.txt"].sizeBytes -eq 26 } 30)) { throw "not appended" }
    Get-Content "$TD\notes.txt"
}
Check "editor save: write temp + MoveFileEx(REPLACE_EXISTING) over original" {
    $id = (Get-ServerPaths)["$T/notes.txt"].fileId
    [IO.File]::WriteAllText("$TD\notes.txt.tmp~", "saved by editor")
    if (-not [K32]::MoveFileEx("$TD\notes.txt.tmp~", "$TD\notes.txt", 1)) { throw "MoveFileEx failed $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
    $ok = Wait-Until { $r = (Get-ServerPaths)["$T/notes.txt"]; $r.fileId -eq $id -and $r.sizeBytes -eq 15 } 30
    if (-not $ok) { throw "server: $((Get-ServerPaths)["$T/notes.txt"] | ConvertTo-Json -Compress)" }
    Start-Sleep 5   # past the delete grace period
    if ((Get-ServerPaths).Keys -match 'tmp~') { throw "temp leaked" }
    "id kept, content: " + (Get-Content "$TF\notes.txt")
}
Check "Word-style save (~WRD.tmp / ~WRL.tmp dance) keeps id, no temp on server" {
    Set-Content "$TD\Report.docx" "v1" -NoNewline
    if (-not (Wait-Until { (Get-ServerPaths).ContainsKey("$T/Report.docx") } 30)) { throw "v1 not uploaded" }
    Start-Sleep 2
    $id = (Get-ServerPaths)["$T/Report.docx"].fileId
    Set-Content "$TD\~`$Report.docx" "owner" -NoNewline
    Set-Content "$TD\~WRD0000.tmp" "version two" -NoNewline
    if (-not [K32]::MoveFileEx("$TD\Report.docx", "$TD\~WRL0001.tmp", 0)) { throw "rename away failed" }
    if (-not [K32]::MoveFileEx("$TD\~WRD0000.tmp", "$TD\Report.docx", 0)) { throw "rename in failed" }
    Remove-Item "$TD\~WRL0001.tmp"; Remove-Item -Force "$TD\~`$Report.docx"
    $ok = Wait-Until { $r = (Get-ServerPaths)["$T/Report.docx"]; $r.fileId -eq $id -and $r.sizeBytes -eq 11 } 30
    if (-not $ok) { throw "server: $((Get-ServerPaths)["$T/Report.docx"] | ConvertTo-Json -Compress)" }
    Start-Sleep 5
    $leak = (Get-ServerPaths).Keys | Where-Object { $_ -like "$T/*" -and ($_ -match '\.tmp$' -or $_ -match '~\$') }
    if ($leak) { throw "temp leaked: $leak" }
    if ((Get-ServerPaths)["$T/Report.docx"].fileId -ne $id) { throw "id lost after grace" }
    "id $id kept"
}
Check "rename file" {
    Rename-Item "$TD\notes.txt" "renamed.txt"
    if (-not (Wait-Until { $p = Get-ServerPaths; $p.ContainsKey("$T/renamed.txt") -and -not $p.ContainsKey("$T/notes.txt") } 20)) { throw "server not renamed" }
    "ok"
}
Check "move file into subfolder" {
    New-Item -ItemType Directory "$TF\Sub" | Out-Null
    Start-Sleep 3
    Move-Item "$TD\renamed.txt" "$TD\Sub\renamed.txt"
    if (-not (Wait-Until { (Get-ServerPaths).ContainsKey("$T/Sub/renamed.txt") } 20)) { throw "not moved" }
    Get-Content "$TF\Sub\renamed.txt"
}
Check "rename + move folder" {
    Rename-Item "$TD\Sub" "Sub2"
    New-Item -ItemType Directory "$TD\Outer" | Out-Null
    Start-Sleep 3
    Move-Item "$TF\Sub2" "$TF\Outer\Sub2"
    if (-not (Wait-Until { (Get-ServerPaths).ContainsKey("$T/Outer/Sub2/renamed.txt") } 20)) { throw "not moved" }
    "ok"
}
Check "case-insensitive open, case preserved" {
    $c = Get-Content "$TD\OUTER\sub2\RENAMED.TXT"
    $n = (Get-ChildItem "$TF\Outer").Name
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
    if ((Get-ServerHash "$T/tree/d/blob.bin") -ne (Hash "$src\d\blob.bin")) { throw "blob differs" }
    "$($want.Count) items"
}
Check "delete file" {
    Remove-Item "$TD\tree\a\f1.txt"
    if (-not (Wait-Until { -not (Get-ServerPaths).ContainsKey("$T/tree/a/f1.txt") } 20)) { throw "still on server" }
    "ok"
}
Check "delete non-empty folder recursively" {
    Remove-Item -Recurse "$TF\tree\a"
    if (-not (Wait-Until { -not ((Get-ServerPaths).Keys -like "$T/tree/a*") } 30)) { throw "still on server" }
    if (Test-Path "$TD\tree\a") { throw "still local" }
    "ok"
}
Check "server-side change appears after a poll (dehydrated, new bytes on read)" {
    $tmp = "$work\srv.txt"; Set-Content $tmp "changed on the server" -NoNewline
    $id = (Get-ServerPaths)["$T/Report.docx"].fileId
    Invoke-RestMethod -Method Put -Headers $H -InFile $tmp -ContentType 'application/octet-stream' "$api/file?fileId=$id" | Out-Null
    $f2 = "$work\srvnew.txt"; Set-Content $f2 "brand new" -NoNewline
    $folderId = (Get-ServerPaths)["$T/"].folderId
    Invoke-RestMethod -Method Put -Headers $H -InFile $f2 -ContentType 'application/octet-stream' "$api/file?storageId=$Storage&folderId=$folderId&name=from-server.txt" | Out-Null
    $ok = Wait-Until { (Get-Item "$TF\Report.docx").Length -eq 21 -and (Test-Path "$TF\from-server.txt") } ($PollSecs * 4 + 10)
    if (-not $ok) { throw "not applied" }
    $wasDehydrated = Dehydrated "$TF\Report.docx"
    $c = Get-Content "$TD\Report.docx" -Raw
    if ($c -ne "changed on the server") { throw "local: '$c'" }
    if ((Get-Content "$TD\from-server.txt" -Raw) -ne "brand new") { throw "new file content" }
    "dehydrated before read: $wasDehydrated"
}
Check "server-side delete disappears after a poll" {
    $id = (Get-ServerPaths)["$T/from-server.txt"].fileId
    Invoke-RestMethod -Method Delete -Headers $H "$api/item?type=file&id=$id" | Out-Null
    if (-not (Wait-Until { -not (Test-Path "$TF\from-server.txt") } ($PollSecs * 4 + 10))) { throw "still visible" }
    "ok"
}
# ------------------------------------------------ drive icon: cover/colour
function Set-DriveColor($c) {
    if ($Mock) {
        $q = if ($c) { "&color=" + [uri]::EscapeDataString($c) } else { "" }
        Invoke-RestMethod -Method Post "$Origin/__mock/color?storageId=$Storage$q" | Out-Null
    } elseif ($Psql) {
        $v = if ($c) { $c } else { "" }
        & $Psql $PgArgs.Split(' ') -c "update storage_space set color='$v' where storage_id='$Storage'" | Out-Null
    } else { throw "needs -Mock or -Psql" }
}
function Set-DriveCover($pngPath) {
    if ($Mock) {
        if ($pngPath) { Invoke-RestMethod -Method Post -InFile $pngPath "$Origin/__mock/cover?storageId=$Storage" | Out-Null }
        else { Invoke-RestMethod -Method Post -Body ([byte[]]@()) "$Origin/__mock/cover?storageId=$Storage" | Out-Null }
    } elseif ($Psql) {
        if ($pngPath) {
            $name = "mt-cover-" + [guid]::NewGuid().ToString('N').Substring(0, 6) + ".png"
            $row = Invoke-RestMethod -Method Put -Headers $H -InFile $pngPath -ContentType 'image/png' "$api/file?storageId=$Storage&folderId=$((Get-ServerPaths)["$T/"].folderId)&name=$name"
            & $Psql $PgArgs.Split(' ') -c "update storage_space set cover_file_id='$($row.fileId)' where storage_id='$Storage'" | Out-Null
        } else {
            & $Psql $PgArgs.Split(' ') -c "update storage_space set cover_file_id=null where storage_id='$Storage'" | Out-Null
        }
    } else { throw "needs -Mock or -Psql" }
}
function Solid-Png($r, $g, $b) {
    Add-Type -AssemblyName System.Drawing
    $bmp = New-Object System.Drawing.Bitmap 96, 64
    $gr = [System.Drawing.Graphics]::FromImage($bmp); $gr.Clear([System.Drawing.Color]::FromArgb(255, $r, $g, $b)); $gr.Dispose()
    $p = "$work\cover-$r-$g-$b.png"; $bmp.Save($p, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose(); $p
}
function Icon-Pixel($ico, $x, $y) {
    Add-Type -AssemblyName System.Drawing
    $i = New-Object System.Drawing.Icon($ico, 64, 64); $bm = $i.ToBitmap(); $c = $bm.GetPixel($x, $y); $bm.Dispose(); $i.Dispose(); $c
}
$DIK = "HKCU:\Software\Classes\Applications\Explorer.exe\Drives\$L\DefaultIcon"
function Letter-Icon { (Get-ItemProperty $DIK -ErrorAction SilentlyContinue).'(default)' }
function Root-Icon { (Get-ItemProperty "$SRM\$rootId" -ErrorAction SilentlyContinue).IconResource }
function Expect-IconChange($before, [scriptblock]$pixelOk, $what) {
    if (-not (Wait-Until { (Letter-Icon) -and (Letter-Icon) -ne $before } 30)) { throw "$what`: DefaultIcon didn't change (still $(Letter-Icon))" }
    $now = Letter-Icon
    if (-not (Test-Path $now)) { throw "$what`: $now missing" }
    if ($before -and (Test-Path $before)) { throw "$what`: old icon $before not deleted" }
    if ((Root-Icon) -ne "$now,0") { throw "$what`: sync root icon is $(Root-Icon)" }
    $px = Icon-Pixel $now 32 6
    if (-not (& $pixelOk $px)) { throw "$what`: pixel $px" }
    "$what -> $(Split-Path $now -Leaf) (px $($px.R),$($px.G),$($px.B))"
}
if ($Mock -or $Psql) {
    $log = New-Object System.Collections.ArrayList
    Check "drive icon follows colour + cover changes (letter DefaultIcon + sync root icon)" {
        $i0 = Letter-Icon
        if (-not $i0 -or -not (Test-Path $i0)) { throw "no initial icon: $i0" }
        if ((Root-Icon) -ne "$i0,0") { throw "sync root icon $(Root-Icon) != $i0" }
        # Start from "no cover, no colour" (navy tile).
        Set-DriveCover $null; Set-DriveColor $null
        if ($drive.hasCover) { [void]$log.Add((Expect-IconChange $i0 { param($p) $p.B -gt $p.R } "cover cleared")) }
        $a = Letter-Icon
        Set-DriveColor "#f43f5e"
        [void]$log.Add((Expect-IconChange $a { param($p) $p.R -gt 200 -and $p.G -lt 140 } "colour #f43f5e"))
        $b = Letter-Icon
        Set-DriveCover (Solid-Png 20 200 40)
        [void]$log.Add((Expect-IconChange $b { param($p) $p.G -gt 150 -and $p.R -lt 90 } "green cover"))
        $c = Letter-Icon
        Set-DriveCover (Solid-Png 30 60 220)
        [void]$log.Add((Expect-IconChange $c { param($p) $p.B -gt 180 -and $p.G -lt 120 } "blue cover"))
        $d = Letter-Icon
        Set-DriveCover $null
        [void]$log.Add((Expect-IconChange $d { param($p) $p.R -gt 200 -and $p.G -lt 140 } "cover cleared (colour tile)"))
        $e = Letter-Icon
        Set-DriveColor $null
        [void]$log.Add((Expect-IconChange $e { param($p) $p.B -gt $p.R } "colour cleared (navy)"))
        $log -join '; '
    }
}

Check "clean up test folder" {
    Remove-Item -Recurse -Force $TD
    if (Test-Path "$F\mt-probe-$T.bin") { Remove-Item "$F\mt-probe-$T.bin" }
    if (-not (Wait-Until { -not (Get-ServerPaths).ContainsKey("$T/") } 30)) { throw "still on server" }
    "ok"
}

# -------------------------------------------------------------- unmount
Set-Content $stop "x"
[void]$mountProc.WaitForExit(120000)
if (-not $mountProc.HasExited) { Stop-Process -Id $mountProc.Id -Force -ErrorAction SilentlyContinue }
Check "exit: letter removed, registry keys removed, sync root unregistered, folder removed" {
    if (-not $mountProc.HasExited) { throw "mount process still running" }
    if (DosTarget $L) { throw "${L}: still defined -> $(DosTarget $L)" }
    if (Test-Path "HKCU:\Software\Classes\Applications\Explorer.exe\Drives\$L") { throw "drive icon keys left" }
    if ($rootId -and (Test-Path "$SRM\$rootId")) { throw "sync root still registered" }
    if (Test-Path $F) { throw "$F still exists" }
    (@(Get-Content "$work\mount.out") -join ' ')
}
# Belt and braces: remove any OTHCloudDriveTest root/letter still around.
Start-Process $Exe -ArgumentList "cleanup-test-roots" -Wait -WindowStyle Hidden -RedirectStandardOutput "$work\cleanup.out"
Check "no test sync roots / letters left" {
    $left = @(Get-ChildItem $SRM | Where-Object { $_.PSChildName -like "OTHCloudDriveTest!*" })
    if ($left.Count) { throw "left: $($left.PSChildName -join ', ')" }
    (Get-Content "$work\cleanup.out" -Raw).Trim()
}
if ($mockProc) { Stop-Process -Id $mockProc.Id -ErrorAction SilentlyContinue }
Write-Host ""
$results | Format-Table -AutoSize | Out-String -Width 220 | Write-Host
$fail = @($results | Where-Object Result -eq 'FAIL').Count
Write-Host "$(@($results).Count - $fail) passed, $fail failed. Logs: $work"
exit $fail
