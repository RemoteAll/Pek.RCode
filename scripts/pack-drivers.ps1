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
  powershell -ExecutionPolicy Bypass -File .\scripts\pack-drivers.ps1 -Drivers all -Target linux-x64

Targets:
  win-x64（本机构建） / linux-x64 / linux-arm64 / linux-riscv64 / linux-loongarch64
  （linux 交叉编译需 cargo-zigbuild + zig；zig 经 -ZigPath 或环境变量 CARGO_ZIGBUILD_ZIG_PATH 指定）

Outputs:
  dist\drivers\dbserver-<name>-<version>-<target>.zip  (+ SHA256SUMS.txt)
#>
param(
    [string[]]$Drivers = @('mysql', 'postgresql'),
    [string]$Target = 'win-x64',
    [string]$Version = '0.1.0',
    [string]$OutDir = 'dist\drivers',
    [string]$ZigPath = ''
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
    'odbc'       = @{ kind = 'ODBC';       title = 'ODBC (DB2/DaMeng/IRIS/Access)'; features = 'driver-odbc'; notes = '运行时需数据库厂商 ODBC 驱动（odbcinst.ini 注册）；Linux 版为 glibc 构建（内置 unixODBC、无系统库依赖），musl/Alpine 不支持' }
}

if ($Drivers -contains 'all') { $Drivers = $defs.Keys | Sort-Object }

# 目标平台 → Rust triple（命名与 Pek.RCode `driver_pack::current_target()` 对齐）
$triples = @{
    'linux-x64'         = 'x86_64-unknown-linux-musl'
    'linux-arm64'       = 'aarch64-unknown-linux-musl'
    'linux-riscv64'     = 'riscv64gc-unknown-linux-musl'
    'linux-loongarch64' = 'loongarch64-unknown-linux-musl'
}
$triple = $triples[$Target]
if ($Target -notin @('win-x64', 'win-arm64') -and -not $triple) { throw ("unsupported target: {0}" -f $Target) }
if ($triple) {
    if (-not $ZigPath) { $ZigPath = $env:CARGO_ZIGBUILD_ZIG_PATH }
    if (-not $ZigPath) { $ZigPath = 'G:\Tools\zig\zig-0.16.0\zig.exe' }
    if (-not (Test-Path $ZigPath)) { throw ("zig not found: {0}（用 -ZigPath 指定）" -f $ZigPath) }
    $env:CARGO_ZIGBUILD_ZIG_PATH = $ZigPath
}
# gnu 侧目标（odbc 专用；幂等，已装则秒过）
if ($Target -in @('linux-x64', 'linux-arm64')) {
    $null = cmd /c "rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu >nul 2>&1"
}

if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null

$failed = @()
$skipped = @()
foreach ($name in $Drivers) {
    $def = $defs[$name]
    if (-not $def) { Write-Host "!! unknown driver: $name (skip)"; continue }
    $features = $def.features
    $drvTriple = $triple
    if ($name -eq 'odbc') {
        # ODBC：驱动管理器运行时必须 dlopen 厂商驱动，而 musl 静态二进制的 dlopen 是桩函数
        # （"Dynamic loading not supported"）——odbc 必须构建为 glibc 目标；内置 unixODBC
        # （vendored 静态库）使其除厂商驱动外无系统依赖。win 目标不需要：非静态链接走系统 odbc32。
        if ($drvTriple -like 'riscv64*' -or $drvTriple -like 'loongarch64*') {
            Write-Host ("SKIP  {0}: musl static cannot dlopen (odbc needs a glibc target)" -f $name)
            $skipped += $name
            continue
        }
        if ($Target -eq 'linux-x64') { $drvTriple = 'x86_64-unknown-linux-gnu'; $features = 'driver-odbc,odbc-vendored' }
        elseif ($Target -eq 'linux-arm64') { $drvTriple = 'aarch64-unknown-linux-gnu'; $features = 'driver-odbc,odbc-vendored' }
        elseif ($Target -like 'win-*') { $features = 'driver-odbc' }
    }
    if ($drvTriple) {
        # Linux cross-build: native-tls on Unix pulls OpenSSL (openssl-sys cannot be
        # zig-cross-compiled) -> switch to the rustls backend for these packages.
        $features = $features -replace 'tls-native', 'tls-rustls'
        Write-Host ("==> [{0}] cargo zigbuild --target {1} --release --example dbserver --no-default-features --features {2}" -f $name, $drvTriple, $features)
        cargo zigbuild --release --example dbserver --no-default-features --features $features --target $drvTriple
    } else {
        Write-Host ("==> [{0}] cargo build --release --example dbserver --no-default-features --features {1}" -f $name, $features)
        cargo build --release --example dbserver --no-default-features --features $features
    }
    if ($LASTEXITCODE -ne 0) {
        Write-Host ("FAILED {0}: build error (exit {1})" -f $name, $LASTEXITCODE)
        $failed += $name
        continue
    }

    if ($drvTriple) {
        $bin = Join-Path $root ("target\{0}\release\examples\dbserver" -f $drvTriple)
        $binName = 'dbserver'
    } else {
        $bin = Join-Path $root 'target\release\examples\dbserver.exe'
        if (-not (Test-Path $bin)) { $bin = Join-Path $root 'target\release\examples\dbserver' }
        $binName = 'dbserver.exe'
    }
    if (-not (Test-Path $bin)) { throw ("dbserver binary not found for {0}" -f $name) }

    $stage = Join-Path $env:TEMP ("rcdbserver-{0}" -f $name)
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $stage | Out-Null
    Copy-Item $bin (Join-Path $stage $binName)
    $manifest = [ordered]@{
        id       = "dbserver-$name"
        kind     = $def.kind
        name     = $def.title
        version  = $Version
        protocol = 1
        target   = $Target
    }
    if ($def.notes) { $manifest['notes'] = $def.notes }
    $manifest = $manifest | ConvertTo-Json -Compress
    [IO.File]::WriteAllText((Join-Path $stage 'driver.json'), $manifest)

    $zipName = "dbserver-$name-$Version-$Target.zip"
    $zipPath = Join-Path $root (Join-Path $OutDir $zipName)
    Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
    [IO.Compression.ZipFile]::CreateFromDirectory($stage, $zipPath, [IO.Compression.CompressionLevel]::Optimal, $false)
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue

    $sha = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $sizeKB = [int]((Get-Item $zipPath).Length / 1024)
    Write-Host ("    -> {0}  ({1} KB, sha256 {2})" -f $zipName, $sizeKB, $sha)
}
# 全目录扫描生成 SHA256SUMS（跨批次一致，而非仅本轮）
$outFull = Join-Path $root $OutDir
$zipFiles = @(Get-ChildItem $outFull -Filter *.zip | Sort-Object Name)
[IO.File]::WriteAllLines((Join-Path $outFull 'SHA256SUMS.txt'), @($zipFiles | ForEach-Object {
    "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $_.Name
}))
if ($skipped.Count -gt 0) { Write-Host ("== skipped: {0}" -f ($skipped -join ', ')) }
if ($failed.Count -gt 0) {
    Write-Host ("== FAILED drivers: {0}" -f ($failed -join ', '))
    exit 1
}
Write-Host ("== done. outputs in {0}" -f $OutDir)
