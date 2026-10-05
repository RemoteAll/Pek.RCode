#requires -Version 5.1
<#
Pek.RCode driver pack builder.

Builds the `dbserver` example in per-driver trimmed form (only the target
driver is compiled in) and packages it as a downloadable driver component
(zip: dbserver(.exe) + driver.json). The zips are meant to be uploaded to the
Pek.RPanlServer "components" store so client apps can fetch drivers on demand.

Usage:
  powershell -ExecutionPolicy Bypass -File .\scripts\pack-drivers.ps1 -Drivers mysql,postgresql
  powershell -ExecutionPolicy Bypass -File .\scripts\pack-drivers.ps1 -Drivers all

Outputs:
  dist\drivers\dbserver-<name>-<version>-<target>.zip  (+ SHA256SUMS.txt)
#>
param(
    [string[]]$Drivers = @('mysql', 'postgresql'),
    [string]$Target = 'win-x64',
    [string]$Version = '0.1.0',
    [string]$OutDir = 'dist\drivers'
)
$ErrorActionPreference = 'Stop'
# 兼容 `-File scripts\pack-drivers.ps1 -Drivers mysql,postgresql`（-File 模式可能整体作为单串传入）
$Drivers = @($Drivers | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' })
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$defs = @{
    'mysql'      = @{ kind = 'MySql';      title = 'MySQL';                         features = 'driver-mysql,tls-native' }
    'postgresql' = @{ kind = 'PostgreSql'; title = 'PostgreSQL';                    features = 'driver-postgresql,tls-native' }
    'sqlserver'  = @{ kind = 'SqlServer';  title = 'SQL Server';                    features = 'driver-sqlserver,tls-native' }
    'oracle'     = @{ kind = 'Oracle';     title = 'Oracle';                        features = 'driver-oracle,tls-native' }
    'firebird'   = @{ kind = 'Firebird';   title = 'Firebird';                      features = 'driver-firebird,tls-native' }
    'hana'       = @{ kind = 'Hana';       title = 'SAP HANA';                      features = 'driver-hana,tls-native' }
    'mongodb'    = @{ kind = 'MongoDb';    title = 'MongoDB';                       features = 'driver-mongodb,tls-native' }
    'clickhouse' = @{ kind = 'ClickHouse'; title = 'ClickHouse';                    features = 'driver-clickhouse,http-tls' }
    'tdengine'   = @{ kind = 'TDengine';   title = 'TDengine';                      features = 'driver-tdengine,http-tls' }
    'influxdb'   = @{ kind = 'InfluxDb';   title = 'InfluxDB';                      features = 'driver-influxdb,http-tls' }
    'odbc'       = @{ kind = 'ODBC';       title = 'ODBC (DB2/DaMeng/IRIS/Access)'; features = 'driver-odbc' }
}

if ($Drivers -contains 'all') { $Drivers = $defs.Keys | Sort-Object }

if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null

$sums = @()
foreach ($name in $Drivers) {
    $def = $defs[$name]
    if (-not $def) { Write-Host "!! unknown driver: $name (skip)"; continue }
    Write-Host ("==> [{0}] cargo build --release --example dbserver --no-default-features --features {1}" -f $name, $def.features)
    cargo build --release --example dbserver --no-default-features --features $def.features
    if ($LASTEXITCODE -ne 0) { throw ("build failed: {0}" -f $name) }

    $bin = Join-Path $root 'target\release\examples\dbserver.exe'
    if (-not (Test-Path $bin)) { $bin = Join-Path $root 'target\release\examples\dbserver' }
    if (-not (Test-Path $bin)) { throw ("dbserver binary not found for {0}" -f $name) }

    $stage = Join-Path $env:TEMP ("rcdbserver-{0}" -f $name)
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $stage | Out-Null
    Copy-Item $bin (Join-Path $stage 'dbserver.exe')
    $manifest = [ordered]@{
        id       = "dbserver-$name"
        kind     = $def.kind
        name     = $def.title
        version  = $Version
        protocol = 1
        target   = $Target
    } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText((Join-Path $stage 'driver.json'), $manifest)

    $zipName = "dbserver-$name-$Version-$Target.zip"
    $zipPath = Join-Path $root (Join-Path $OutDir $zipName)
    Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
    [IO.Compression.ZipFile]::CreateFromDirectory($stage, $zipPath, [IO.Compression.CompressionLevel]::Optimal, $false)
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue

    $sha = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $sizeKB = [int]((Get-Item $zipPath).Length / 1024)
    Write-Host ("    -> {0}  ({1} KB, sha256 {2})" -f $zipName, $sizeKB, $sha)
    $sums += ("{0}  {1}" -f $sha, $zipName)
}
if ($sums.Count -gt 0) {
    $sumFile = Join-Path $root (Join-Path $OutDir 'SHA256SUMS.txt')
    [IO.File]::WriteAllLines($sumFile, $sums)
}
Write-Host ("== done. outputs in {0}" -f $OutDir)
