#requires -Version 5.1
<#
Upload driver component zips (from pack-drivers.ps1) to a Pek.RPanlServer
"components" store (Download Center), for clients (DriverManager) to fetch.

Usage:
  powershell -ExecutionPolicy Bypass -File .\scripts\upload-drivers.ps1 -Server http://127.0.0.1:5502 -User admin -Password secret
  powershell -ExecutionPolicy Bypass -File .\scripts\upload-drivers.ps1 -Server http://host:5502 -User admin -Password secret -Dir dist\drivers
  powershell -ExecutionPolicy Bypass -File .\scripts\upload-drivers.ps1 -Server ... -User ... -Password ... -Zip dist\drivers\dbserver-mysql-0.1.0-linux-x64.zip

Notes:
  - Requires an admin account with the "components" permission;
  - Each zip must contain driver.json (id/name/version/target);
  - Duplicate (same id+version+target) is rejected by the platform -> reported as SKIP;
  - ASCII-only output (safe for PS 5.1 console encodings).
#>
param(
    [Parameter(Mandatory = $true)][string]$Server,
    [string]$User = '',
    [string]$Password = '',
    [string]$Token = '',
    [string]$Dir = 'dist\drivers',
    [string]$Zip = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$server = $Server.TrimEnd('/')

# Auth precedence: -Token (existing session, e.g. handed over from the web UI)
# > -User/-Password > interactive hidden prompt (keeps passwords out of
# command lines, shell history and chat logs).
if (-not $Token -and -not $Password) {
    if (-not $User) { throw 'provide -Token, or -User/-Password' }
    $sec = Read-Host ("password for " + $User + "@" + $server + " (hidden)") -AsSecureString
    $Password = [Runtime.InteropServices.Marshal]::PtrToStringBSTR([Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec))
}

function Api($method, $path, $token, $bodyFile, $contentType) {
    $out = Join-Path $env:TEMP 'rc-up-out.bin'
    $a = @('-s', '-X', $method, "$server$path", '-o', $out)
    if ($token) { $a += @('-H', "Authorization: Bearer $token") }
    if ($bodyFile) { $a += @('-H', "Content-Type: $contentType", '--data-binary', "@$bodyFile") }
    & curl.exe @a | Out-Null
    return [IO.File]::ReadAllText($out, [Text.Encoding]::UTF8)
}

# --- login ---
if ($Token) {
    $token = $Token
    Write-Host ("session token provided; skipping login (" + $server + ")")
} else {
    $lf = Join-Path $env:TEMP 'rc-up-login.json'
    [IO.File]::WriteAllText($lf, (@{ user = $User; password = $Password } | ConvertTo-Json -Compress), (New-Object Text.UTF8Encoding($false)))
    $r = Api 'POST' '/api/login' $null $lf 'application/json'
    if (-not ($r -match '"code":0')) { throw ("login failed: " + $r) }
    $token = ([regex]::Match($r, '"token":"([^"]+)"')).Groups[1].Value
    Write-Host ("login ok: " + $User + "@" + $server)
}

# --- collect zips ---
if ($Zip) {
    $files = @(Get-Item $Zip)
} else {
    $files = @(Get-ChildItem (Join-Path $root $Dir) -Filter '*.zip' -ErrorAction SilentlyContinue | Sort-Object Name)
}
if ($files.Count -eq 0) { throw ("no zip found (dir=" + $Dir + ")") }
Write-Host ("uploading " + $files.Count + " package(s) ...")

Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null
$okCount = 0; $skipCount = 0
foreach ($f in $files) {
    # read driver.json from zip (note: do NOT reuse the $Zip parameter name here:
    # a [string]-typed param would coerce a ZipArchive assignment back to string)
    $archive = [IO.Compression.ZipFile]::OpenRead($f.FullName)
    try {
        $entry = $archive.Entries | Where-Object { $_.Name -eq 'driver.json' } | Select-Object -First 1
        if (-not $entry) { Write-Host ("SKIP  " + $f.Name + " : no driver.json"); $skipCount++; continue }
        $sr = New-Object IO.StreamReader($entry.Open(), [Text.Encoding]::UTF8)
        $dj = $sr.ReadToEnd() | ConvertFrom-Json
        $sr.Close()
    } finally { $archive.Dispose() }

    $title = [uri]::EscapeDataString([string]$dj.name)
    $notes = if ($dj.notes) { [uri]::EscapeDataString([string]$dj.notes) } else { '' }
    $q = "componentId=" + $dj.id + "&title=" + $title + "&category=driver&version=" + $dj.version + "&target=" + $dj.target + "&notes=" + $notes
    $r = Api 'POST' ("/api/componentUpload?" + $q) $token $f.FullName 'application/zip'
    if ($r -match '"code":0') {
        Write-Host ("OK    " + $f.Name + "  (" + $dj.id + " v" + $dj.version + " " + $dj.target + ")")
        $okCount++
    } else {
        Write-Host ("SKIP  " + $f.Name + " : " + $r)
        $skipCount++
    }
}

# --- summary from platform ---
$r = Api 'GET' '/api/componentInfo' $token $null $null
$cnt = ([regex]::Match($r, '"componentCount":(\d+)')).Groups[1].Value
Write-Host ("done. uploaded=" + $okCount + " skipped/failed=" + $skipCount + " platformTotal=" + $cnt)
