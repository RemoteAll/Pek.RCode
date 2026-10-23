<#
.SYNOPSIS
  本地 MariaDB 绿色实例（MySQL 协议端到端测试用），全部文件在 target\mysql-test（不入库、可一键清理）。

.DESCRIPTION
  start  : 首次运行自动初始化数据目录（root 密码 root123），随后在 127.0.0.1:3399 启动并建库 rcode_test
  stop   : 优雅停止（mariadb-admin shutdown，失败则强制结束进程）
  status : 显示运行状态与连接串
  clean  : 停止并删除整个 target\mysql-test（含数据目录与发行包）

.EXAMPLE
  pwsh scripts\mysql-test-env.ps1 start
  $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None"
  cargo test --test mysql_e2e
  pwsh scripts\mysql-test-env.ps1 clean
#>
param(
    [ValidateSet('start', 'stop', 'status', 'clean')]
    [string]$Action = 'start'
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$base = Join-Path $root 'target\mysql-test'
$dist = Join-Path $base 'mariadb-11.8.9-winx64'
$data = Join-Path $base 'data'
$log = Join-Path $base 'db.err'
$port = 3399
$conn = "Server=127.0.0.1;Port=$port;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None"

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

switch ($Action) {
    'start' {
        $server = Join-Path $dist 'bin\mariadbd.exe'
        if (-not (Test-Path $server)) {
            throw "未找到 MariaDB：$server（下载 mariadb-11.8.9-winx64.zip 解压到 $base 即可）"
        }
        if (Test-DbPort) {
            "已在运行：127.0.0.1:$port"
            break
        }
        if (-not (Test-Path $data)) {
            "首次初始化数据目录：$data"
            & (Join-Path $dist 'bin\mariadb-install-db.exe') "--datadir=$data" "--port=$port" '--password=root123' | Out-Null
        }
        New-Item -ItemType Directory -Force $base | Out-Null
        Start-Process -FilePath $server `
            -ArgumentList "--datadir=$data", "--port=$port", '--bind-address=127.0.0.1' `
            -RedirectStandardError $log -WindowStyle Hidden | Out-Null
        for ($i = 0; $i -lt 60; $i++) {
            Start-Sleep -Milliseconds 250
            if (Test-DbPort) { break }
        }
        if (-not (Test-DbPort)) {
            throw "启动失败，详见日志：$log"
        }
        & (Join-Path $dist 'bin\mariadb.exe') -h 127.0.0.1 -P $port -u root -proot123 `
            -e 'CREATE DATABASE IF NOT EXISTS rcode_test CHARACTER SET utf8mb4' 2>$null
        "已启动：127.0.0.1:$port（库 rcode_test，root/root123）"
        "连接串环境变量：`$env:RCODE_MYSQL = `"$conn`""
        "运行端到端测试：cargo test --test mysql_e2e"
    }
    'stop' {
        if (-not (Test-DbPort)) {
            '未在运行'
            break
        }
        $admin = Join-Path $dist 'bin\mariadb-admin.exe'
        if (Test-Path $admin) {
            & $admin -h 127.0.0.1 -P $port -u root -proot123 shutdown 2>$null
            Start-Sleep -Milliseconds 800
        }
        Get-Process mariadbd -ErrorAction SilentlyContinue | Stop-Process -Force
        '已停止'
    }
    'status' {
        if (Test-DbPort) {
            "运行中：127.0.0.1:$port"
            "连接串：$conn"
        } else {
            "未运行（数据目录：$data）"
        }
    }
    'clean' {
        if (Test-DbPort) {
            $admin = Join-Path $dist 'bin\mariadb-admin.exe'
            if (Test-Path $admin) {
                & $admin -h 127.0.0.1 -P $port -u root -proot123 shutdown 2>$null
                Start-Sleep -Milliseconds 800
            }
            Get-Process mariadbd -ErrorAction SilentlyContinue | Stop-Process -Force
        }
        if (Test-Path $base) {
            Remove-Item $base -Recurse -Force
        }
        "已清理：$base"
    }
}
