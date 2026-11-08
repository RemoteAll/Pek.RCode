<#
.SYNOPSIS
  本地绿色版数据库实例（MySQL 协议端到端测试用）：默认 MySQL 8.0，可切 MariaDB。

.DESCRIPTION
  为什么要两个 vendor：
  - MySQL 8.0.19+ 支持 `INSERT ... VALUES (...) AS _new ON DUPLICATE KEY UPDATE col=_new.col`
    （本库 `SqlFeatures.upsert_row_alias` 的启用分支）；
  - MariaDB 不支持行别名，只能走 `VALUES(col)`（禁用分支）。
  两条分支都必须在真实实例上各跑一遍，否则 `upsert_batch_by_unique` 只验证了一半。

  目录约定（程序全局共享、数据每仓库独立）：
  - 程序包缓存：`$env:RCODE_MYSQL_DIST` → `E:\Soft\mysql-dist` → `<repo>\target\mysql-test\dist`
  - 数据目录：  `<repo>\target\mysql-test\data`（不入库，`clean` 可一键删除）
  - 端口：      3399（与 `tests/mysql_e2e.rs`、`tests/mysql_perf.rs` 文件头一致）

  镜像实测（2026-11-08）：阿里云 ≈1.58 MB/s 可用，且只有到 MySQL-8.0；
  官方 `dev.mysql.com` 404 / Oracle 技术故障页，清华/中科大/华为云/腾讯云均无 8.4+ 的 winx64。
  要换 8.4/9.x：把 `mysql-<版本>-winx64.zip` 放进缓存目录，用 `-ZipName` 指定即可（不改脚本）。

.PARAMETER Action
  start | stop | status | clean

.PARAMETER Vendor
  MySQL（默认）| MariaDB

.PARAMETER ZipName
  覆盖默认包名（换版本时用，如 `mysql-8.4.6-winx64.zip`）。

.PARAMETER PurgeDist
  `clean` 时一并删除全局程序缓存（默认保留，供其它仓库复用）。

.EXAMPLE
  pwsh scripts\mysql-test-env.ps1 start
  $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None"
  cargo test --test mysql_e2e
  pwsh scripts\mysql-test-env.ps1 stop

.EXAMPLE
  # 换 MariaDB 验证 `VALUES(col)` 分支（需自行准备 mariadb zip）
  pwsh scripts\mysql-test-env.ps1 start -Vendor MariaDB -ZipName mariadb-11.8.9-winx64.zip
#>
param(
    [ValidateSet('start', 'stop', 'status', 'clean')]
    [string]$Action = 'start',

    [ValidateSet('MySQL', 'MariaDB')]
    [string]$Vendor = 'MySQL',

    [string]$ZipName,

    [switch]$PurgeDist
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$base = Join-Path $root 'target\mysql-test'
$data = Join-Path $base 'data'
$log = Join-Path $base 'db.err'

$port = 3399
$password = 'root123'
$database = 'rcode_test'
$conn = "Server=127.0.0.1;Port=$port;Database=$database;Uid=root;Pwd=$password;provider=mysql;SslMode=None"

# 各 vendor 的程序名与默认包（包名可用 -ZipName 覆盖）
$specs = @{
    MySQL   = @{
        Zip    = 'mysql-8.0.28-winx64.zip'
        Url    = 'https://mirrors.aliyun.com/mysql/MySQL-8.0/mysql-8.0.28-winx64.zip'
        Server = 'mysqld.exe'
        Admin  = 'mysqladmin.exe'
        Client = 'mysql.exe'
    }
    MariaDB = @{
        Zip    = 'mariadb-11.8.9-winx64.zip'
        Url    = ''
        Server = 'mariadbd.exe'
        Admin  = 'mariadb-admin.exe'
        Client = 'mariadb.exe'
    }
}
$spec = $specs[$Vendor]
if ($ZipName) { $spec.Zip = $ZipName }
# 解压目录 = 包名去掉 .zip
$distDirName = [System.IO.Path]::GetFileNameWithoutExtension($spec.Zip)

# 程序包全局缓存（多仓库共享，避免每仓库各存一份 1.5GB）
$distCache = if ($env:RCODE_MYSQL_DIST) { $env:RCODE_MYSQL_DIST } else { 'E:\Soft\mysql-dist' }

function Test-DbPort {
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $client.Connect('127.0.0.1', $port)
        return $true
    } catch {
        return $false
    } finally {
        $client.Dispose()
    }
}

# PS 5.1 下 $ErrorActionPreference='Stop' 会把外部程序的 stderr 当终止错误，必须临时降级并取退出码。
# 注意：外部程序的输出必须先吞掉（否则与退出码一起被当作返回值，调用方拿到的就是数组）。
$script:LastExeOutput = ''
function Invoke-Exe {
    param([string]$FilePath, [string[]]$Arguments)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = & $FilePath @Arguments 2>&1
        $code = $LASTEXITCODE
        $script:LastExeOutput = ($output | Out-String)
        return $code
    } finally {
        $ErrorActionPreference = $prev
    }
}

# 已解压可用的程序根目录；找不到返回 $null
function Resolve-Dist {
    $candidates = @((Join-Path $distCache $distDirName), (Join-Path $base "dist\$distDirName"))
    foreach ($dir in $candidates) {
        if (Test-Path (Join-Path $dir "bin\$($spec.Server)")) { return $dir }
    }
    return $null
}

# 下载（缺包时）并解压，返回程序根目录
function Install-Dist {
    $target = $distCache
    try {
        New-Item -ItemType Directory -Force $target | Out-Null
    } catch {
        # 全局目录不可写时回退到仓库 target（例如移动盘只读）
        $target = Join-Path $base 'dist'
        New-Item -ItemType Directory -Force $target | Out-Null
    }
    $zipPath = Join-Path $target $spec.Zip
    if (-not (Test-Path $zipPath)) {
        if (-not $spec.Url) {
            throw "未找到 $Vendor 绿色包：$zipPath（$Vendor 无自动下载源，请手动放入该路径）"
        }
        Write-Host "下载 $Vendor 绿色包：$($spec.Url)"
        $code = Invoke-Exe 'curl.exe' @('-L', '--ssl-no-revoke', '--fail', '-sS', '-o', $zipPath, $spec.Url)
        if ($code -ne 0 -or -not (Test-Path $zipPath)) {
            if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
            throw "下载失败（curl 退出码 $code）：$($spec.Url)"
        }
        $mb = [math]::Round((Get-Item $zipPath).Length / 1MB, 1)
        Write-Host "下载完成：$zipPath（$mb MB）"
    }
    Write-Host "解压到：$target"
    Expand-Archive -Path $zipPath -DestinationPath $target -Force
    $dir = Join-Path $target $distDirName
    $server = Join-Path $dir "bin\$($spec.Server)"
    if (-not (Test-Path $server)) {
        throw "解压后未找到 $server，请确认包名与目录名一致（当前包：$($spec.Zip)）"
    }
    return $dir
}

# 用客户端执行 SQL（密码走 MYSQL_PWD，避免命令行明文告警）
function Invoke-Sql {
    param([string]$Dist, [string]$Sql, [switch]$NoPassword)
    $client = Join-Path $Dist "bin\$($spec.Client)"
    $args = @('-h', '127.0.0.1', '-P', "$port", '-u', 'root')
    if ($NoPassword) { $args += '--skip-password' }
    $args += @('-e', $Sql)
    $prev = $env:MYSQL_PWD
    if (-not $NoPassword) { $env:MYSQL_PWD = $password }
    try {
        return Invoke-Exe $client $args
    } finally {
        if ($null -eq $prev) { Remove-Item Env:\MYSQL_PWD -ErrorAction SilentlyContinue } else { $env:MYSQL_PWD = $prev }
    }
}

# 服务端版本串（失败返回空串）
function Get-ServerVersion {
    param([string]$Dist)
    $client = Join-Path $Dist "bin\$($spec.Client)"
    $prevEap = $ErrorActionPreference
    $prevPwd = $env:MYSQL_PWD
    $ErrorActionPreference = 'Continue'
    $env:MYSQL_PWD = $password
    try {
        $out = & $client -h 127.0.0.1 -P $port -u root --skip-column-names -e 'SELECT VERSION()' 2>$null
        return ($out | Where-Object { $_ } | Select-Object -First 1)
    } catch {
        return ''
    } finally {
        $ErrorActionPreference = $prevEap
        if ($null -eq $prevPwd) { Remove-Item Env:\MYSQL_PWD -ErrorAction SilentlyContinue } else { $env:MYSQL_PWD = $prevPwd }
    }
}

# 优雅关闭（密码走 MYSQL_PWD）
function Stop-DbServer {
    param([string]$Admin)
    $prevPwd = $env:MYSQL_PWD
    $env:MYSQL_PWD = $password
    try {
        $null = Invoke-Exe $Admin @('-h', '127.0.0.1', '-P', "$port", '-u', 'root', 'shutdown')
    } finally {
        if ($null -eq $prevPwd) { Remove-Item Env:\MYSQL_PWD -ErrorAction SilentlyContinue } else { $env:MYSQL_PWD = $prevPwd }
    }
    Start-Sleep -Milliseconds 800
}

# MySQL 官方 zip 首次需初始化数据目录（root 无密码），随后设密码
function Initialize-Data {
    param([string]$Dist)
    New-Item -ItemType Directory -Force $base | Out-Null
    $server = Join-Path $Dist "bin\$($spec.Server)"
    if ($Vendor -eq 'MySQL') {
        Write-Host "首次初始化数据目录：$data"
        $code = Invoke-Exe $server @("--initialize-insecure", "--basedir=$Dist", "--datadir=$data", '--console')
        if ($code -ne 0) {
            throw "初始化失败（mysqld 退出码 $code）：$($script:LastExeOutput)"
        }
    } else {
        $init = Join-Path $Dist 'bin\mariadb-install-db.exe'
        Write-Host "首次初始化数据目录：$data"
        $code = Invoke-Exe $init @("--datadir=$data", "--port=$port", "--password=$password")
        if ($code -ne 0) { throw "初始化失败（mariadb-install-db 退出码 $code）" }
    }
}

Write-Host "[$Vendor] 数据目录：$data"

switch ($Action) {
    'start' {
        if (Test-DbPort) {
            Write-Host "已在运行：127.0.0.1:$port"
            break
        }
        $dist = Resolve-Dist
        if (-not $dist) {
            $dist = Install-Dist
        }
        if (-not (Test-Path $data)) {
            Initialize-Data $dist
        }
        New-Item -ItemType Directory -Force $base | Out-Null
        $server = Join-Path $dist "bin\$($spec.Server)"
        Start-Process -FilePath $server `
            -ArgumentList "--basedir=$dist", "--datadir=$data", "--port=$port", '--bind-address=127.0.0.1' `
            -RedirectStandardError $log -WindowStyle Hidden | Out-Null
        for ($i = 0; $i -lt 120; $i++) {
            Start-Sleep -Milliseconds 250
            if (Test-DbPort) { break }
        }
        if (-not (Test-DbPort)) {
            throw "启动失败，详见日志：$log"
        }

        # 首启（MySQL）root 还没有密码：先设密码，再建测试库；已有密码时直接用密码连
        $pingCode = Invoke-Sql -Dist $dist -Sql 'SELECT 1' -NoPassword
        if ($pingCode -eq 0) {
            $sql = "ALTER USER 'root'@'localhost' IDENTIFIED BY '$password';" +
                   "CREATE USER IF NOT EXISTS 'root'@'127.0.0.1' IDENTIFIED BY '$password';" +
                   "GRANT ALL PRIVILEGES ON *.* TO 'root'@'127.0.0.1' WITH GRANT OPTION;" +
                   "FLUSH PRIVILEGES;"
            $code = Invoke-Sql -Dist $dist -Sql $sql -NoPassword
            if ($code -ne 0) { $code = Invoke-Sql -Dist $dist -Sql $sql }
        } else {
            $code = Invoke-Sql -Dist $dist -Sql 'SELECT 1'
        }
        if ($code -ne 0) {
            throw "root 认证失败（退出码 $code）：请确认数据目录 $data 与此前实例一致，或执行 clean 后重建"
        }
        $null = Invoke-Sql -Dist $dist -Sql "CREATE DATABASE IF NOT EXISTS $database CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"

        $version = Get-ServerVersion $dist
        Write-Host "已启动：127.0.0.1:$port（$Vendor $version，库 $database，root/$password）"
        Write-Host "连接串环境变量：`$env:RCODE_MYSQL = `"$conn`""
        Write-Host '运行端到端测试：cargo test --test mysql_e2e / mysql_perf / mysql_perf_roundtrip'
    }
    'stop' {
        if (-not (Test-DbPort)) {
            Write-Host '未在运行'
            break
        }
        $dist = Resolve-Dist
        if ($dist) {
            $admin = Join-Path $dist "bin\$($spec.Admin)"
            if (Test-Path $admin) { Stop-DbServer $admin }
        }
        if (Test-DbPort) {
            Get-Process -Name ($spec.Server -replace '\.exe$', '') -ErrorAction SilentlyContinue | Stop-Process -Force
        }
        Write-Host '已停止'
    }
    'status' {
        $dist = Resolve-Dist
        Write-Host "vendor    : $Vendor"
        Write-Host "程序目录  : $(if ($dist) { $dist } else { '（未安装，start 时自动下载）' })"
        Write-Host "数据目录  : $data"
        if (Test-DbPort) {
            Write-Host "状态      : 运行中 127.0.0.1:$port"
            Write-Host "连接串    : $conn"
            if ($dist) { Write-Host "版本      : $(Get-ServerVersion $dist)" }
        } else {
            Write-Host '状态      : 未运行'
        }
    }
    'clean' {
        if (Test-DbPort) {
            $dist = Resolve-Dist
            if ($dist) {
                $admin = Join-Path $dist "bin\$($spec.Admin)"
                if (Test-Path $admin) { Stop-DbServer $admin }
            }
            Get-Process -Name ($spec.Server -replace '\.exe$', '') -ErrorAction SilentlyContinue | Stop-Process -Force
        }
        if (Test-Path $base) {
            Remove-Item $base -Recurse -Force
        }
        Write-Host "已清理数据目录：$base"
        if ($PurgeDist) {
            $dir = Join-Path $distCache $distDirName
            if (Test-Path $dir) { Remove-Item $dir -Recurse -Force }
            $zip = Join-Path $distCache $spec.Zip
            if (Test-Path $zip) { Remove-Item $zip -Force }
            Write-Host "已清理程序缓存：$dir"
        } else {
            Write-Host "程序缓存保留（$distCache），其它仓库可直接复用；要一并删除加 -PurgeDist"
        }
    }
}
