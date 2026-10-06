//! 驱动包按需分发消费端（DriverManager）。
//!
//! 场景：应用**编译期不包含**某个数据库驱动（按 `driver-*` 特性裁剪），运行时按需从
//! Pek.RPanlServer 的**组件源**（管理员「下载管理」）拉取对应驱动包，本机拉起
//! `dbserver` 驱动宿主，再以 `provider=network` 连接——即"用到哪个驱动就下载哪个"。
//!
//! 流程：
//!
//! ```text
//! 本机连接串（Provider=MySql）→ 解析类型 → 组件 id（dbserver-mysql）
//!   → 组件源目录 catalog.json（Ed25519 验签，公钥来自平台面板）
//!   → 下载驱动包（SHA-256 强制校验）
//!   → 解压到本地缓存（{cache}/{组件}/{版本}/）
//!   → 拉起 dbserver（回环绑定 + 端口 0 自动分配 + 一次性令牌）
//!   → 解析就绪行（{"event":"ready","addr":"127.0.0.1:<port>",...}）
//!   → 返回 network 连接串（Server=http://127.0.0.1:<port>;Database=...;Password=<令牌>;provider=network）
//! ```
//!
//! 说明：
//!
//! - 组件源协议见 Pek.RPanlServer「下载管理」：`/components/catalog.json`（同址 `.sig`
//!   为 base64 Ed25519 签名，与插件源同一把平台密钥）；公钥为 32 字节裸 hex（或 44 字节 SPKI DER）。
//! - 配置 `pubkey` 非空时**强制验签**（此时允许内网 http 组件源）；为空时仅允许 https 或回环 http。
//! - https 组件源默认用 rustls **WebPki 内置根**（不读系统证书库）；自签/内网 CA 场景经
//!   [`DriverManagerConfig::ca_pem`] 指定根证书（PEM，可多张），无需改动系统信任。
//!   **自签证书要点**：须为叶子证书（`CA:FALSE`）或标准 CA+叶子链（信任 CA）；
//!   直接用 `CA:TRUE` 证书当服务器证书会被 rustls 拒绝（`CaUsedAsEndEntity`）。
//!   生成示例（SAN 按实际地址改）：`openssl req -x509 -newkey rsa:2048 -keyout key.pem
//!   -out ca.pem -days 365 -nodes -subj "/CN=drivers.local" -addext "subjectAltName=IP:127.0.0.1"
//!   -addext "basicConstraints=critical,CA:FALSE"`
//! - 相同连接串（文本）复用同一宿主进程；[`DriverManager::ensure_updated`] 会联网检查新版本。
//!   [`DriverManager::ensure`] 则本地缓存优先（离线可用），仅本地无驱动时才联网。
//! - [`DriverManager`] 析构时停止全部宿主进程；长驻应用可配置 `idle_timeout` 并用
//!   [`DriverManager::reap_idle`] 按空闲阈值回收。
//! - 拉起时附加 `--watch-stdin` 并保持 stdin 打开：调用方进程消失（stdin EOF）时
//!   dbserver 自退，防止强杀场景下的孤儿进程；旧版 dbserver 忽略该参数（向后兼容）。
//! - https 组件源需同时启用 `http-tls` 特性。
//!
//! 用法：
//!
//! ```no_run
//! use pek_rcode::driver_pack::{DriverManager, DriverManagerConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mgr = DriverManager::new(DriverManagerConfig {
//!     store_url: "http://192.168.1.10:5502".into(),
//!     pubkey: "<64 位 hex 公钥>".into(),
//!     ..Default::default()
//! })?;
//! // 本机连接串：按需下载 MySql 驱动包并拉起宿主，返回 network 连接串
//! let conn = mgr.ensure("Server=192.168.1.5;Database=Demo;Provider=MySql")?;
//! let dal = pek_rcode::dal::Dal::open(&conn)?;
//! # let _ = dal;
//! # Ok(())
//! # }
//! ```

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};

/// 组件源目录大小上限（1MB，与插件源一致）。
const MAX_CATALOG_SIZE: usize = 1024 * 1024;
/// 驱动包大小上限（32MB）。
const MAX_PACKAGE_SIZE: usize = 32 * 1024 * 1024;
/// 目录/包下载超时。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// 就绪行默认等待超时。
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// 驱动宿主可执行文件名（随平台）。
#[cfg(windows)]
const DBSERVER_BIN: &str = "dbserver.exe";
#[cfg(not(windows))]
const DBSERVER_BIN: &str = "dbserver";

/// 当前运行平台标识（与 `scripts/pack-drivers.ps1` 的 `-Target` 命名对齐）。
pub fn current_target() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "riscv64") => "linux-riscv64",
        ("linux", "loongarch64") => "linux-loongarch64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        _ => return None,
    })
}

/// [`DriverManager`] 配置。
#[derive(Clone, Debug, Default)]
pub struct DriverManagerConfig {
    /// 平台组件源地址（如 `http://192.168.1.10:5502`；也接受完整 `.../components/catalog.json`）。
    pub store_url: String,
    /// 平台 Ed25519 公钥（32 字节裸 hex 或 44 字节 SPKI DER）。空 = 不验签（仅允许 https/回环 http）。
    pub pubkey: String,
    /// 组件源 https 自定义根证书（PEM 内容，可含多张；自签/内网 CA 场景）。
    /// `None` = rustls WebPki 内置根（不读系统证书库）；配置后该组根证书**替换**默认根。
    pub ca_pem: Option<Vec<u8>>,
    /// 驱动缓存目录；缺省为系统临时目录下 `pek-rcode-drivers`。
    pub cache_dir: Option<PathBuf>,
    /// 宿主空闲回收阈值（配合 [`DriverManager::reap_idle`]；`None` = 不按空闲回收）。
    pub idle_timeout: Option<Duration>,
    /// 拉起后等待就绪行的超时（缺省 15 秒）。
    pub ready_timeout: Option<Duration>,
}

/// 运行中的驱动宿主快照（状态展示用）。
#[derive(Clone, Debug)]
pub struct HostStatus {
    /// 组件 id（如 `dbserver-mysql`）
    pub component: String,
    /// 组件版本
    pub version: String,
    /// 就绪地址（`127.0.0.1:<port>`）
    pub addr: String,
    /// 生成的 network 连接串
    pub connection: String,
    /// 进程 id
    pub pid: u32,
    /// 空闲秒数
    pub idle_secs: u64,
}

/// 组件源目录条目（「下载管理」字段；见 Pek.RPanlServer `store::build_components_catalog`）。
#[derive(Clone, Debug)]
pub struct ComponentEntry {
    /// 组件 id（如 `dbserver-mysql`）
    pub id: String,
    /// 显示名
    pub name: String,
    /// 版本号
    pub version: String,
    /// 目标平台（如 `win-x64`）
    pub target: String,
    /// 包内容 SHA-256（小写 hex）
    pub sha256: String,
    /// 包下载地址（绝对）
    pub url: String,
}

impl ComponentEntry {
    /// 解析校验单个目录条目（无效即错误；调用方选择忽略并记录）。
    fn from_json(item: &Json) -> std::result::Result<Self, String> {
        let field = |k: &str| {
            item.get(k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or("")
                .to_string()
        };
        let id = field("id");
        if id.is_empty() {
            return Err("条目缺少 id".to_string());
        }
        let url = field("url");
        if url.is_empty() {
            return Err(format!("{id}：缺少 url"));
        }
        let sha256 = field("sha256").to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{id}：sha256 缺失或格式错误"));
        }
        Ok(Self {
            id,
            name: field("name"),
            version: field("version"),
            target: field("target"),
            sha256,
            url,
        })
    }
}

struct RunningHost {
    component: String,
    version: String,
    addr: String,
    connection: String,
    child: Child,
    /// 保持打开：调用方退出 → EOF → 宿主（`--watch-stdin`）自退（防孤儿）
    _stdin: Option<std::process::ChildStdin>,
    last_used: Instant,
}

/// 驱动包管理器：按需下载、缓存、拉起与回收 `dbserver` 驱动宿主。
///
/// 线程安全（内部加锁）；`ensure*` 的网络/解压阶段在锁外执行，典型用法为应用启动时
/// 依需求调用一次，之后直接使用返回的 network 连接串。
pub struct DriverManager {
    cfg: DriverManagerConfig,
    cache_dir: PathBuf,
    catalog_url: String,
    /// 自定义根证书（解析自 `ca_pem`；`None` = WebPki 默认根）
    ca_certs: Option<ureq::tls::RootCerts>,
    hosts: Mutex<HashMap<String, RunningHost>>,
}

impl DriverManager {
    /// 创建管理器（校验组件源地址与公钥格式；`store_url` 为空 = **仅本地缓存模式**：
    /// 不联网，消费方可从本机组件库等来源预置缓存后直接使用）。
    pub fn new(cfg: DriverManagerConfig) -> Result<Self> {
        let store = cfg.store_url.trim().trim_end_matches('/');
        let has_pubkey = !cfg.pubkey.trim().is_empty();
        if !store.is_empty() && !url_allowed(store, has_pubkey) {
            return Err(Error::Argument(format!(
                "驱动管理器：组件源地址不被允许（需 https；配置公钥后可放行内网 http）：{store}"
            )));
        }
        if has_pubkey {
            // 提前解析，配置错误快速暴露
            dhrust::plugin::parse_pubkey(cfg.pubkey.trim())
                .map_err(|e| Error::Argument(format!("驱动管理器：公钥无效（{e}）")))?;
        }
        let catalog_url = if store.is_empty() {
            String::new()
        } else {
            catalog_url_of(store)
        };
        let ca_certs = match cfg.ca_pem.as_deref() {
            Some(pem) => Some(parse_ca_pem(pem).map_err(Error::Argument)?),
            None => None,
        };
        let cache_dir = cfg
            .cache_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("pek-rcode-drivers"));
        Ok(Self {
            cfg,
            cache_dir,
            catalog_url,
            ca_certs,
            hosts: Mutex::new(HashMap::new()),
        })
    }

    /// 确保连接串对应驱动就绪并拉起宿主，返回 `provider=network` 连接串。
    ///
    /// **本地缓存优先**（已缓存则离线可用）；缓存缺失时才访问组件源下载。
    /// 相同连接串（文本）复用同一宿主进程。
    pub fn ensure(&self, conn_str: &str) -> Result<String> {
        self.ensure_inner(conn_str, false)
    }

    /// 同 [`DriverManager::ensure`]，但**总是联网检查目录**，有更高版本则下载并替换宿主；
    /// 组件源不可用时回退本地缓存（有缓存则继续，无则报错）；未配置组件源时直接用本地缓存。
    pub fn ensure_updated(&self, conn_str: &str) -> Result<String> {
        self.ensure_inner(conn_str, true)
    }

    /// 准备连接串（消费方“一条线”入口）：内嵌驱动（SQLite/DuckDB）与 network 连接串原样返回；
    /// 需要驱动包的类型经组件源分发（[`Self::ensure_updated`]）后返回 `provider=network` 连接串。
    pub fn prepare(&self, conn_str: &str) -> Result<String> {
        match driver_pack_need(conn_str)? {
            DriverPackNeed::Direct => Ok(conn_str.trim().to_string()),
            DriverPackNeed::Component(_) => self.ensure_updated(conn_str),
        }
    }

    /// 仅准备驱动包（下载/校验/解压到缓存，**不拉起宿主**）：供消费方在切换/重启前预热，
    /// 使后续启动直接命中缓存。返回 `(组件 id, 版本, 缓存目录)`；内嵌驱动报错。
    pub fn prepare_package(&self, conn_str: &str) -> Result<(String, String, PathBuf)> {
        let key = conn_str.trim();
        if key.is_empty() {
            return Err(Error::Argument("驱动管理器：连接串为空".to_string()));
        }
        let cs = ConnectionString::parse(key);
        let kind = resolve_provider_kind(&cs)?;
        let component = component_id_for(kind).ok_or_else(|| {
            Error::Argument(format!(
                "驱动管理器：{} 为内嵌驱动（本地直接打开即可），无需下载驱动包",
                kind.name()
            ))
        })?;
        let (version, dir) = self.resolve_driver(&component, true)?;
        Ok((component, version, dir))
    }

    /// 停止指定连接串对应的宿主；返回是否存在并已停止。
    pub fn stop(&self, conn_str: &str) -> bool {
        let key = conn_str.trim();
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut host) = hosts.remove(key) {
            kill_host(&mut host);
            true
        } else {
            false
        }
    }

    /// 停止全部宿主（析构时也会调用）。
    pub fn stop_all(&self) {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        for (_, mut host) in hosts.drain() {
            kill_host(&mut host);
        }
    }

    /// 空闲回收：停止空闲超过 `idle_timeout` 的宿主（含已退出的），返回停止数量。
    /// 未配置 `idle_timeout` 时仅清理已退出进程。
    pub fn reap_idle(&self) -> usize {
        let timeout = self.cfg.idle_timeout;
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let mut stop_keys: Vec<String> = Vec::new();
        for (key, host) in hosts.iter_mut() {
            let dead = !matches!(host.child.try_wait(), Ok(None));
            let idle = timeout.map(|t| host.last_used.elapsed() >= t).unwrap_or(false);
            if dead || idle {
                stop_keys.push(key.clone());
            }
        }
        let count = stop_keys.len();
        for key in stop_keys {
            if let Some(mut host) = hosts.remove(&key) {
                kill_host(&mut host);
            }
        }
        count
    }

    /// 运行中宿主快照（顺带清理已退出进程）。
    pub fn hosts(&self) -> Vec<HostStatus> {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        hosts.retain(|_, host| matches!(host.child.try_wait(), Ok(None)));
        let now = Instant::now();
        hosts
            .values()
            .map(|host| HostStatus {
                component: host.component.clone(),
                version: host.version.clone(),
                addr: host.addr.clone(),
                connection: host.connection.clone(),
                pid: host.child.id(),
                idle_secs: now.duration_since(host.last_used).as_secs(),
            })
            .collect()
    }

    // ————— 内部实现 —————

    fn ensure_inner(&self, conn_str: &str, update: bool) -> Result<String> {
        let key = conn_str.trim();
        if key.is_empty() {
            return Err(Error::Argument("驱动管理器：连接串为空".to_string()));
        }
        let cs = ConnectionString::parse(key);
        let kind = resolve_provider_kind(&cs)?;
        let component = component_id_for(kind).ok_or_else(|| {
            Error::Argument(format!(
                "驱动管理器：{} 为内嵌驱动（本地直接打开即可），无需下载驱动包",
                kind.name()
            ))
        })?;
        let (version, dir) = self.resolve_driver(&component, update)?;
        let database = cs.get("database").unwrap_or("default").to_string();
        self.start_host(key, &component, &version, &dir, key, &database)
    }

    /// 解析可用驱动版本与本地目录：本地缓存优先 / 联网取目录并校验下载。
    fn resolve_driver(&self, component: &str, update: bool) -> Result<(String, PathBuf)> {
        let local = self.local_versions(component);
        if !update
            && let Some((version, dir)) = local.last()
        {
            return Ok((version.clone(), dir.clone()));
        }
        // 未配置组件源：仅本地缓存模式（消费方可预先从本机组件库等来源安装）
        if self.catalog_url.is_empty() {
            if let Some((version, dir)) = local.last() {
                return Ok((version.clone(), dir.clone()));
            }
            return Err(Error::Model(format!(
                "缺少驱动包 {component}：本地缓存为空，且未配置组件源\
                 （可在 [DriverStore] 填 Url，或由消费方从本机组件库预置到缓存）"
            )));
        }
        // 联网取目录（update = 总是查；无本地缓存 = 必须查）
        let entries = match self.load_catalog() {
            Ok(entries) => entries,
            Err(e) => {
                if let Some((version, dir)) = local.last() {
                    eprintln!(
                        "[pek-rcode] 驱动管理器：组件源不可用（{e}），改用本地驱动 {component} {version}"
                    );
                    return Ok((version.clone(), dir.clone()));
                }
                return Err(e);
            }
        };
        let target = current_target().ok_or_else(|| {
            Error::Unsupported(format!(
                "驱动管理器：当前平台（{} / {}）暂无驱动包支持",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;
        let newest = entries
            .iter()
            .filter(|e| e.id == component && e.target == target)
            .max_by(|a, b| version_cmp(&a.version, &b.version));
        let Some(entry) = newest else {
            if let Some((version, dir)) = local.last() {
                return Ok((version.clone(), dir.clone()));
            }
            return Err(Error::Model(format!(
                "组件源中不存在驱动包：{component}（平台 {target}）"
            )));
        };
        // 目标版本已在本地
        if let Some((_, dir)) = local.iter().find(|(v, _)| v == &entry.version) {
            return Ok((entry.version.clone(), dir.clone()));
        }
        // 下载 → SHA-256 校验 → 解压（原子落到 {组件}/{版本}）
        let data = self.fetch(&entry.url, MAX_PACKAGE_SIZE)?;
        let got = dhrust::sign::sha256_hex(&data);
        if !got.eq_ignore_ascii_case(&entry.sha256) {
            return Err(Error::Db(format!(
                "驱动包 SHA-256 校验失败，已拒绝使用（期望 {}，实际 {got}）",
                entry.sha256
            )));
        }
        let dir = self.extract_package(&data, component, &entry.version)?;
        Ok((entry.version.clone(), dir))
    }

    /// 本地缓存中已下载的版本（升序；仅含带可执行文件的目录）。
    fn local_versions(&self, component: &str) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.cache_dir.join(component)) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_dir() || !path.join(DBSERVER_BIN).is_file() {
                    continue;
                }
                out.push((entry.file_name().to_string_lossy().to_string(), path));
            }
        }
        out.sort_by(|a, b| version_cmp(&a.0, &b.0));
        out
    }

    /// 加载组件源目录（可选强制验签）。
    fn load_catalog(&self) -> Result<Vec<ComponentEntry>> {
        let bytes = self.fetch(&self.catalog_url, MAX_CATALOG_SIZE)?;
        let pubkey = self.cfg.pubkey.trim();
        if !pubkey.is_empty() {
            let sig = self.fetch(&format!("{}.sig", self.catalog_url), 4096)?;
            dhrust::plugin::verify_base64(pubkey, &bytes, &String::from_utf8_lossy(&sig))
                .map_err(|e| Error::Model(format!("驱动组件源签名校验失败：{e}")))?;
        }
        parse_catalog(&bytes).map_err(Error::Model)
    }

    /// 下载（大小上限保护；地址放行规则见 [`url_allowed`]）。
    fn fetch(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        if !url_allowed(url, !self.cfg.pubkey.trim().is_empty()) {
            return Err(Error::Model(format!(
                "驱动组件源地址不被允许（需 https，或配置公钥后的 http）：{url}"
            )));
        }
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .http_status_as_error(false);
        if let Some(roots) = &self.ca_certs {
            // 自签/内网 CA：以配置的根证书集合替换 WebPki 默认根
            builder = builder
                .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots.clone()).build());
        }
        let agent = ureq::Agent::new_with_config(builder.build());
        let mut resp = agent
            .get(url)
            .call()
            .map_err(|e| Error::Db(format!("下载失败（{url}）：{e}")))?;
        let status = resp.status();
        let bytes = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| Error::Db(format!("读取响应失败（{url}）：{e}")))?;
        if !status.is_success() {
            return Err(Error::Db(format!(
                "下载失败（{url}）：HTTP {}",
                status.as_u16()
            )));
        }
        if bytes.len() > max {
            return Err(Error::Model(format!(
                "内容超出大小上限（{} > {max} 字节）：{url}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// 解压驱动包到缓存（实现见模块函数 [`extract_package_to_cache`]）。
    fn extract_package(&self, data: &[u8], component: &str, version: &str) -> Result<PathBuf> {
        extract_package_to_cache(&self.cache_dir, data, component, version)
    }

    /// 拉起（或复用）驱动宿主，返回 network 连接串。
    fn start_host(
        &self,
        key: &str,
        component: &str,
        version: &str,
        dir: &Path,
        conn: &str,
        database: &str,
    ) -> Result<String> {
        // 1) 快路径：复用存活且版本一致的宿主
        {
            let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(host) = live_host(&mut hosts, key, version) {
                host.last_used = Instant::now();
                return Ok(host.connection.clone());
            }
        }
        // 2) 拉起 + 等待就绪（锁外；并发时先到者胜出）
        let exe = dir.join(DBSERVER_BIN);
        let token = dhrust::random::hex(16);
        let mut cmd = Command::new(&exe);
        cmd.arg(conn)
            .arg("0")
            .arg(&token)
            .arg("--watch-stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：后台运行时不弹控制台
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Db(format!("拉起驱动宿主失败（{}）：{e}", exe.display())))?;
        let child_stdin = child.stdin.take(); // 保持打开：调用方退出 → EOF → 宿主自退
        let stdout = child.stdout.take().expect("stdout 已管道化");
        let stderr = child.stderr.take().expect("stderr 已管道化");
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        {
            let sink = Arc::clone(&stderr_tail);
            std::thread::spawn(move || collect_tail(stderr, sink, 2000));
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || read_until_ready(stdout, tx));
        let timeout = self.cfg.ready_timeout.unwrap_or(DEFAULT_READY_TIMEOUT);
        let addr = match rx.recv_timeout(timeout) {
            Ok(Ok((addr, _kind))) => addr,
            Ok(Err(msg)) => {
                let tail = stderr_tail.lock().map(|s| s.clone()).unwrap_or_default();
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Db(format!(
                    "驱动宿主提前退出：{msg}{}",
                    format_tail(&tail)
                )));
            }
            Err(_) => {
                let tail = stderr_tail.lock().map(|s| s.clone()).unwrap_or_default();
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Db(format!(
                    "驱动宿主 {timeout:?} 内未就绪{}",
                    format_tail(&tail)
                )));
            }
        };
        let connection = build_network_conn(&addr, database, &token);
        // 3) 锁内落位：并发时保留先到者，丢弃本次多余进程
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(host) = live_host(&mut hosts, key, version) {
            host.last_used = Instant::now();
            let existing = host.connection.clone();
            drop(hosts);
            let _ = child.kill();
            let _ = child.wait();
            return Ok(existing);
        }
        hosts.insert(
            key.to_string(),
            RunningHost {
                component: component.to_string(),
                version: version.to_string(),
                addr,
                connection: connection.clone(),
                child,
                _stdin: child_stdin,
                last_used: Instant::now(),
            },
        );
        Ok(connection)
    }
}

impl Drop for DriverManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

// ————— 纯函数（独立可测） —————

/// 解析连接串 provider → 数据库类型（DriverManager 场景的约束检查）。
/// 从本地 zip 包安装驱动到 `{cache_dir}/{组件}/{版本}/`（供消费方从本机组件库预置；返回安装目录）。
///
/// 与组件源下载路径同语义：大小上限、临时目录 + 原子改名、校验包含 `dbserver`、Unix 置 0o755。
pub fn install_from_zip(
    cache_dir: &Path,
    component_id: &str,
    version: &str,
    zip_path: &Path,
) -> Result<PathBuf> {
    let safe = |s: &str| !s.is_empty() && !s.contains(['/', '\\']) && s != "." && s != "..";
    if !safe(component_id) || !safe(version) {
        return Err(Error::Argument(
            "驱动管理器：组件 id / 版本号不合法".to_string(),
        ));
    }
    let data = std::fs::read(zip_path)?;
    if data.len() > MAX_PACKAGE_SIZE {
        return Err(Error::Model(format!(
            "驱动包超出大小上限（{} > {MAX_PACKAGE_SIZE} 字节）：{}",
            data.len(),
            zip_path.display()
        )));
    }
    extract_package_to_cache(cache_dir, &data, component_id, version)
}

/// 解压驱动包字节到 `{cache}/{组件}/{版本}/`（临时目录 + 原子改名；并发安全）。
fn extract_package_to_cache(
    cache_dir: &Path,
    data: &[u8],
    component: &str,
    version: &str,
) -> Result<PathBuf> {
    let root = cache_dir.join(component);
    std::fs::create_dir_all(&root)?;
    let final_dir = root.join(version);
    if final_dir.join(DBSERVER_BIN).is_file() {
        return Ok(final_dir); // 竞态：另一进程已完成
    }
    let tmp = root.join(format!("{version}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let cleanup = |e: Error| {
        let _ = std::fs::remove_dir_all(&tmp);
        e
    };
    extract_zip_bytes(data, &tmp).map_err(cleanup)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            tmp.join(DBSERVER_BIN),
            std::fs::Permissions::from_mode(0o755),
        );
    }
    if !tmp.join(DBSERVER_BIN).is_file() {
        return Err(cleanup(Error::Model(format!(
            "驱动包缺少 {DBSERVER_BIN}（{component} {version}）"
        ))));
    }
    if final_dir.join(DBSERVER_BIN).is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(final_dir);
    }
    match std::fs::rename(&tmp, &final_dir) {
        Ok(()) => Ok(final_dir),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            if final_dir.join(DBSERVER_BIN).is_file() {
                Ok(final_dir) // 竞态：另一进程已完成
            } else {
                Err(Error::Io(e))
            }
        }
    }
}

fn resolve_provider_kind(cs: &ConnectionString) -> Result<DatabaseKind> {
    let provider = cs.provider().map(str::trim).unwrap_or_default();
    if provider.is_empty() {
        return Err(Error::Argument(
            "驱动管理器：连接串缺少 provider 字段".to_string(),
        ));
    }
    if matches!(provider.to_ascii_lowercase().as_str(), "network" | "net") {
        return Err(Error::Argument(
            "驱动管理器：该连接串已是 network 驱动；请在普通（本机驱动）连接串上调用".to_string(),
        ));
    }
    DatabaseKind::from_provider(provider)
}

/// 数据库类型 → 平台组件 id（与 `scripts/pack-drivers.ps1` 的命名对齐）；
/// 内嵌驱动（SQLite/DuckDB）返回 `None`。
fn component_id_for(kind: DatabaseKind) -> Option<String> {
    let slug = match kind {
        DatabaseKind::Sqlite | DatabaseKind::DuckDb => return None,
        // ODBC 桥一盘棋：DB2 / 达梦 / IRIS / Access 共用 dbserver-odbc 包
        DatabaseKind::Db2 | DatabaseKind::DaMeng | DatabaseKind::Iris | DatabaseKind::Access => {
            "odbc".to_string()
        }
        other => other.name().to_ascii_lowercase(),
    };
    Some(format!("dbserver-{slug}"))
}

/// 连接串的驱动包需求分类（供消费方判断是否需要 [`DriverManager`]）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverPackNeed {
    /// 可直接打开：内嵌驱动（SQLite/DuckDB）或已是 network 连接串。
    Direct,
    /// 需要驱动包；值为组件 id（如 `dbserver-mysql`）。
    Component(String),
}

/// 判断连接串是否需要驱动包（纯本地判断，不访问网络）。
pub fn driver_pack_need(conn_str: &str) -> Result<DriverPackNeed> {
    let cs = ConnectionString::parse(conn_str.trim());
    let provider = cs.provider().map(str::trim).unwrap_or_default();
    if provider.is_empty() {
        return Err(Error::Argument("连接串缺少 provider 字段".to_string()));
    }
    if matches!(provider.to_ascii_lowercase().as_str(), "network" | "net") {
        return Ok(DriverPackNeed::Direct);
    }
    let kind = DatabaseKind::from_provider(provider)?;
    Ok(match component_id_for(kind) {
        None => DriverPackNeed::Direct,
        Some(id) => DriverPackNeed::Component(id),
    })
}

/// 版本号比较（段拆分：数字段按数值、非数字段按字典序；
/// 缺段 = 数字 0（`1.0` == `1.0.0`）；数字段 > 非数字段（`1.0.0` > `1.0.0-beta`））。
fn version_cmp(a: &str, b: &str) -> Ordering {
    fn key(seg: Option<&str>) -> (u8, u64, String) {
        match seg {
            None => (2, 0, String::new()),
            Some(s) => match s.parse::<u64>() {
                Ok(n) => (2, n, String::new()),
                Err(_) => (0, 0, s.to_ascii_lowercase()),
            },
        }
    }
    let sa: Vec<&str> = a.split(['.', '-', '_']).collect();
    let sb: Vec<&str> = b.split(['.', '-', '_']).collect();
    for i in 0..sa.len().max(sb.len()) {
        let ka = key(sa.get(i).copied());
        let kb = key(sb.get(i).copied());
        let ord = ka
            .0
            .cmp(&kb.0)
            .then_with(|| ka.1.cmp(&kb.1))
            .then_with(|| ka.2.cmp(&kb.2));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// 解析 dbserver 就绪行（`{"event":"ready","addr":"...","kind":"...","protocol":1}`）。
fn parse_ready_line(line: &str) -> Option<(String, String)> {
    let v: Json = serde_json::from_str(line.trim()).ok()?;
    if v.get("event").and_then(Json::as_str) != Some("ready") {
        return None;
    }
    let addr = v.get("addr").and_then(Json::as_str)?.trim();
    if addr.is_empty() {
        return None;
    }
    let kind = v
        .get("kind")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    Some((addr.to_string(), kind))
}

/// 由就绪地址/库名/令牌组装 `provider=network` 连接串。
fn build_network_conn(addr: &str, database: &str, token: &str) -> String {
    format!("Server=http://{addr};Database={database};Password={token};provider=network")
}

/// 组件源地址放行规则：https 一律允许；http 需配置公钥（签名保护链路）或为回环地址。
fn url_allowed(url: &str, has_pubkey: bool) -> bool {
    let u = url.trim();
    if u.starts_with("https://") {
        return true;
    }
    if !u.starts_with("http://") {
        return false;
    }
    if has_pubkey {
        return true;
    }
    let host = &u["http://".len()..];
    host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]")
}

/// 解析 PEM 根证书（可含多张 `BEGIN CERTIFICATE` 块；供自签/内网 CA 场景）。
fn parse_ca_pem(pem: &[u8]) -> std::result::Result<ureq::tls::RootCerts, String> {
    let text = String::from_utf8_lossy(pem);
    let mut certs = Vec::new();
    for block in text.split("-----BEGIN CERTIFICATE-----").skip(1) {
        let Some(end) = block.find("-----END CERTIFICATE-----") else {
            return Err("根证书 PEM 缺少 END 标记".to_string());
        };
        let full = format!(
            "-----BEGIN CERTIFICATE-----{}-----END CERTIFICATE-----",
            &block[..end]
        );
        let cert = ureq::tls::Certificate::from_pem(full.as_bytes())
            .map_err(|e| format!("根证书 PEM 解析失败：{e}"))?;
        certs.push(cert);
    }
    if certs.is_empty() {
        return Err("根证书 PEM 未包含 BEGIN CERTIFICATE 块".to_string());
    }
    Ok(ureq::tls::RootCerts::new_with_certs(&certs))
}

/// 组件源根地址 → 目录地址（已是 `.../catalog.json` 则原样）。
fn catalog_url_of(store: &str) -> String {
    let s = store.trim().trim_end_matches('/');
    if s.ends_with(".json") {
        s.to_string()
    } else {
        format!("{s}/components/catalog.json")
    }
}

/// 解析目录 JSON（`{"components":[...]}`；无效条目忽略并提示）。
fn parse_catalog(bytes: &[u8]) -> std::result::Result<Vec<ComponentEntry>, String> {
    let json: Json =
        serde_json::from_slice(bytes).map_err(|e| format!("catalog.json 解析失败：{e}"))?;
    let Some(list) = json.get("components").and_then(|v| v.as_array()) else {
        return Err("catalog.json 缺少 components 数组".to_string());
    };
    let mut out = Vec::new();
    for item in list {
        match ComponentEntry::from_json(item) {
            Ok(entry) => out.push(entry),
            Err(e) => eprintln!("[pek-rcode] 组件源：忽略无效条目（{e}）"),
        }
    }
    Ok(out)
}

/// 内存 zip 解压（防目录穿越；`enclosed_name` 拦截 `..`/绝对路径）。
fn extract_zip_bytes(data: &[u8], dest: &Path) -> Result<()> {
    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| Error::Model(format!("驱动包不是有效 zip：{e}")))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::Model(format!("读取 zip 条目失败：{e}")))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(Error::Model(format!(
                "驱动包含非法路径：{}",
                entry.name()
            )));
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut file)?;
    }
    Ok(())
}

/// 取"存活且版本一致"的宿主；已退出者顺带清理，版本不符者停止让位。
fn live_host<'a>(
    hosts: &'a mut HashMap<String, RunningHost>,
    key: &str,
    version: &str,
) -> Option<&'a mut RunningHost> {
    let alive = {
        let host = hosts.get_mut(key)?;
        matches!(host.child.try_wait(), Ok(None))
    };
    if !alive {
        hosts.remove(key);
        return None;
    }
    let stale = hosts.get(key).map(|h| h.version != version).unwrap_or(false);
    if stale {
        if let Some(mut old) = hosts.remove(key) {
            kill_host(&mut old);
        }
        return None;
    }
    hosts.get_mut(key)
}

/// 停止并回收宿主进程。
fn kill_host(host: &mut RunningHost) {
    let _ = host.child.kill();
    let _ = host.child.wait();
}

/// 读取宿主 stdout 直到出现就绪行（之后继续消耗输出防管道堵塞）。
fn read_until_ready(
    stdout: impl Read,
    tx: mpsc::Sender<std::result::Result<(String, String), String>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut sent = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if !sent
                    && let Some(ready) = parse_ready_line(&line)
                {
                    sent = true;
                    let _ = tx.send(Ok(ready));
                }
            }
            Err(_) => break,
        }
    }
    if !sent {
        let _ = tx.send(Err("进程提前退出（未输出就绪行）".to_string()));
    }
}

/// 收集 stderr 尾部（错误报告用，上限按字符边界截断）。
fn collect_tail(stderr: impl Read, sink: Arc<Mutex<String>>, max: usize) {
    let mut buf = String::new();
    if BufReader::new(stderr).read_to_string(&mut buf).is_err() {
        return;
    }
    if buf.len() > max {
        let start = buf.len() - max;
        let start = (0..=start)
            .rev()
            .find(|&i| buf.is_char_boundary(i))
            .unwrap_or(0);
        buf = buf[start..].to_string();
    }
    if let Ok(mut text) = sink.lock() {
        *text = buf;
    }
}

/// 组装错误信息的 stderr 尾巴（换行折叠为 ` | `）。
fn format_tail(tail: &str) -> String {
    let text = tail.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("；stderr：{}", text.replace(['\r', '\n'], " | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pek-driver-pack-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 起一个"固定响应"的 https 静态服务（rustls 自签）；接受 `accepts` 个连接（失败忽略）。
    fn spawn_https_static(
        cert: rcgen::Certificate,
        key: rcgen::KeyPair,
        body: Vec<u8>,
        accepts: usize,
    ) -> (String, std::thread::JoinHandle<()>) {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

        let cfg = std::sync::Arc::new(
            rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for _ in 0..accepts {
                let Ok((tcp, _)) = listener.accept() else { break };
                let Ok(conn) = rustls::ServerConnection::new(cfg.clone()) else {
                    continue;
                };
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                let mut buf = [0u8; 2048];
                // 读到请求（校验失败的连接会读错/断开，忽略）
                let _ = tls.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(head.as_bytes());
                let _ = tls.write_all(&body);
                let _ = tls.flush();
            }
        });
        (format!("https://127.0.0.1:{port}"), handle)
    }

    #[test]
    fn self_signed_https_source_needs_ca_pem() {
        // 自签证书（SAN=127.0.0.1）
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "pek-test-ca");
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();
        let body = br#"{"components":[]}"#.to_vec();
        let (url, handle) = spawn_https_static(cert, key, body.clone(), 2);

        // ① 配置根证书（PEM）→ 信任自签证书，下载成功
        let mgr = DriverManager::new(DriverManagerConfig {
            store_url: url.clone(),
            ca_pem: Some(pem.clone().into_bytes()),
            cache_dir: Some(temp_dir("https-ca")),
            ..Default::default()
        })
        .unwrap();
        let got = mgr
            .fetch(&format!("{url}/components/catalog.json"), 4096)
            .unwrap();
        assert_eq!(got, body, "配置根证书后应能下载自签 https 源");

        // ② 不配置 → rustls 默认 WebPki 根不含自签证书，校验失败
        let mgr = DriverManager::new(DriverManagerConfig {
            store_url: url.clone(),
            cache_dir: Some(temp_dir("https-noca")),
            ..Default::default()
        })
        .unwrap();
        let err = mgr.fetch(&format!("{url}/components/catalog.json"), 4096);
        assert!(err.is_err(), "未配置根证书时自签源应校验失败：{err:?}");

        let _ = handle.join();
    }

    #[test]
    fn component_id_mapping_rules() {
        assert_eq!(
            component_id_for(DatabaseKind::MySql).as_deref(),
            Some("dbserver-mysql")
        );
        assert_eq!(
            component_id_for(DatabaseKind::PostgreSql).as_deref(),
            Some("dbserver-postgresql")
        );
        assert_eq!(
            component_id_for(DatabaseKind::SqlServer).as_deref(),
            Some("dbserver-sqlserver")
        );
        assert_eq!(
            component_id_for(DatabaseKind::MongoDb).as_deref(),
            Some("dbserver-mongodb")
        );
        assert_eq!(
            component_id_for(DatabaseKind::InfluxDb).as_deref(),
            Some("dbserver-influxdb")
        );
        assert_eq!(
            component_id_for(DatabaseKind::ClickHouse).as_deref(),
            Some("dbserver-clickhouse")
        );
        assert_eq!(
            component_id_for(DatabaseKind::TDengine).as_deref(),
            Some("dbserver-tdengine")
        );
        // ODBC 桥：DB2/达梦/IRIS/Access 共用
        assert_eq!(
            component_id_for(DatabaseKind::Db2).as_deref(),
            Some("dbserver-odbc")
        );
        assert_eq!(
            component_id_for(DatabaseKind::DaMeng).as_deref(),
            Some("dbserver-odbc")
        );
        // 内嵌驱动无需驱动包
        assert_eq!(component_id_for(DatabaseKind::Sqlite), None);
        assert_eq!(component_id_for(DatabaseKind::DuckDb), None);
    }

    #[test]
    fn provider_resolution_rules() {
        let ok = ConnectionString::parse("Server=x;Database=d;Provider=MySql");
        assert_eq!(resolve_provider_kind(&ok).unwrap(), DatabaseKind::MySql);
        let alias = ConnectionString::parse("Server=x;Provider=postgres");
        assert_eq!(resolve_provider_kind(&alias).unwrap(), DatabaseKind::PostgreSql);
        let missing = ConnectionString::parse("Server=x");
        assert!(resolve_provider_kind(&missing).is_err());
        let net = ConnectionString::parse("Server=http://127.0.0.1:1;Password=t;provider=network");
        assert!(resolve_provider_kind(&net).is_err());
        let unknown = ConnectionString::parse("Server=x;Provider=NoSuchDb");
        assert!(resolve_provider_kind(&unknown).is_err());
    }

    #[test]
    fn version_compare_rules() {
        assert_eq!(version_cmp("1.0.1", "1.0.0"), Ordering::Greater);
        assert_eq!(version_cmp("0.10.0", "0.9.0"), Ordering::Greater);
        assert_eq!(version_cmp("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(version_cmp("2.0.0", "10.0.0"), Ordering::Less);
        // 预发布低于正式版
        assert_eq!(version_cmp("1.0.0-beta", "1.0.0"), Ordering::Less);
        assert_eq!(version_cmp("1.0.0-alpha", "1.0.0-beta"), Ordering::Less);
    }

    #[test]
    fn cache_only_manager_resolves_local_and_reports_missing() {
        let cache = temp_dir("cacheonly");
        let mgr = DriverManager::new(DriverManagerConfig {
            cache_dir: Some(cache.clone()),
            ..Default::default()
        })
        .unwrap();
        // 空缓存 + 未配置组件源：明确报错
        let err = mgr.resolve_driver("dbserver-mysql", true).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("本地缓存为空") && msg.contains("组件源"), "{msg}");
        // 预置版本目录（含 dbserver 占位）→ 直接解析成功，且不联网
        let dir = cache.join("dbserver-mysql").join("1.2.3");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(DBSERVER_BIN), b"stub").unwrap();
        let (version, resolved) = mgr.resolve_driver("dbserver-mysql", true).unwrap();
        assert_eq!(version, "1.2.3");
        assert_eq!(resolved, dir);
    }

    #[test]
    fn install_from_zip_places_package() {
        let cache = temp_dir("instzip");
        let zip = build_zip(&[
            ("driver.json", br#"{"id":"dbserver-demo"}"# as &[u8]),
            (DBSERVER_BIN, b"stub-binary"),
        ]);
        let path = cache.join("pkg.zip");
        std::fs::write(&path, &zip).unwrap();
        let dir = install_from_zip(&cache, "dbserver-demo", "0.9.0", &path).unwrap();
        assert!(dir.join(DBSERVER_BIN).is_file());
        assert!(dir.join("driver.json").is_file());
        // 非法 id/版本拒绝
        assert!(install_from_zip(&cache, "../evil", "1.0", &path).is_err());
        assert!(install_from_zip(&cache, "dbserver-demo", "a/b", &path).is_err());
    }

    #[test]
    fn prepare_package_primes_cache_without_host() {
        let cache = temp_dir("prime");
        let mgr = DriverManager::new(DriverManagerConfig {
            cache_dir: Some(cache.clone()),
            ..Default::default()
        })
        .unwrap();
        // 空缓存：报错（不得静默）
        assert!(mgr.prepare_package("Server=x;Provider=MySql").is_err());
        // 预置后：返回位置且不拉起宿主
        let dir = cache.join("dbserver-mysql").join("0.1.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(DBSERVER_BIN), b"stub").unwrap();
        let (component, version, got) = mgr.prepare_package("Server=x;Provider=MySql").unwrap();
        assert_eq!(component, "dbserver-mysql");
        assert_eq!(version, "0.1.0");
        assert_eq!(got, dir);
        assert!(mgr.hosts().is_empty(), "准备阶段不应拉起宿主");
        // 内嵌驱动：明确报错
        assert!(mgr.prepare_package("Data Source=x.db;Provider=SQLite").is_err());
    }

    #[test]
    fn ready_line_parsing_rules() {
        let line = r#"{"event":"ready","addr":"127.0.0.1:57064","kind":"MySql","protocol":1}"#;
        assert_eq!(
            parse_ready_line(line),
            Some(("127.0.0.1:57064".to_string(), "MySql".to_string()))
        );
        assert_eq!(parse_ready_line("hello"), None);
        assert_eq!(parse_ready_line(r#"{"event":"other","addr":"x"}"#), None);
        assert_eq!(parse_ready_line(r#"{"event":"ready"}"#), None);
    }

    #[test]
    fn driver_pack_need_classification() {
        // 内嵌驱动 / network：直接打开
        assert_eq!(
            driver_pack_need("Data Source=x.db;Provider=SQLite").unwrap(),
            DriverPackNeed::Direct
        );
        // 需要驱动包（含别名与 ODBC 桥）
        assert_eq!(
            driver_pack_need("Server=x;Provider=MySql").unwrap(),
            DriverPackNeed::Component("dbserver-mysql".to_string())
        );
        assert_eq!(
            driver_pack_need("Server=x;Provider=postgres").unwrap(),
            DriverPackNeed::Component("dbserver-postgresql".to_string())
        );
        assert_eq!(
            driver_pack_need("Server=x;Provider=dm").unwrap(),
            DriverPackNeed::Component("dbserver-odbc".to_string())
        );
        // 无效输入
        assert!(driver_pack_need("Server=x").is_err());
        assert!(driver_pack_need("Server=x;Provider=NoSuch").is_err());
    }

    #[test]
    fn prepare_passes_direct_through() {
        let mgr = DriverManager::new(DriverManagerConfig {
            store_url: "http://127.0.0.1:1".into(),
            ..Default::default()
        })
        .unwrap();
        let sqlite = "Data Source=x.db;Provider=SQLite";
        assert_eq!(mgr.prepare(sqlite).unwrap(), sqlite);
        let net = "Server=http://127.0.0.1:1;Password=t;provider=network";
        assert_eq!(mgr.prepare(net).unwrap(), net);
        assert!(mgr.prepare("Server=x").is_err());
    }

    #[test]
    fn network_connection_and_url_rules() {
        assert_eq!(
            build_network_conn("127.0.0.1:5000", "Demo", "tok"),
            "Server=http://127.0.0.1:5000;Database=Demo;Password=tok;provider=network"
        );
        assert!(url_allowed("https://x/y", false));
        assert!(url_allowed("http://127.0.0.1:5502/x", false));
        assert!(url_allowed("http://localhost/x", false));
        assert!(!url_allowed("http://10.0.0.5/x", false));
        assert!(url_allowed("http://10.0.0.5/x", true));
        assert!(!url_allowed("ftp://x/y", true));
        assert_eq!(
            catalog_url_of("http://h:1"),
            "http://h:1/components/catalog.json"
        );
        assert_eq!(
            catalog_url_of("http://h:1/"),
            "http://h:1/components/catalog.json"
        );
        assert_eq!(
            catalog_url_of("http://h:1/components/catalog.json"),
            "http://h:1/components/catalog.json"
        );
    }

    #[test]
    fn catalog_signature_verify_and_parse() {
        let key = dhrust::plugin::SigningKey::from_bytes(&[9u8; 32]);
        let pubkey = dhrust::plugin::pubkey_hex(&key);
        let catalog = format!(
            r#"{{"name":"t","version":1,"components":[{{"id":"dbserver-mysql","name":"MySQL","category":"driver","version":"1.0.0","target":"win-x64","sha256":"{}","notes":"","url":"http://127.0.0.1:1/x.zip"}}]}}"#,
            "a".repeat(64)
        );
        let sig = dhrust::plugin::sign_base64(&key, catalog.as_bytes());
        assert!(dhrust::plugin::verify_base64(&pubkey, catalog.as_bytes(), &sig).is_ok());
        assert!(dhrust::plugin::verify_base64(&pubkey, b"tampered", &sig).is_err());
        let entries = parse_catalog(catalog.as_bytes()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "dbserver-mysql");
        // 无效条目（缺 sha256）被忽略
        let bad = br#"{"components":[{"id":"x","url":"http://127.0.0.1:1/a.zip"}]}"#;
        assert!(parse_catalog(bad).unwrap().is_empty());
    }

    #[test]
    fn ensure_rejects_invalid_inputs() {
        let mgr = DriverManager::new(DriverManagerConfig {
            store_url: "http://127.0.0.1:1".into(),
            cache_dir: Some(temp_dir("neg")),
            ..Default::default()
        })
        .unwrap();
        assert!(mgr.ensure("").is_err());
        assert!(mgr.ensure("Server=x").is_err());
        assert!(mgr
            .ensure("Server=http://x;Password=t;provider=network")
            .is_err());
        assert!(mgr.ensure("Data Source=x.db;Provider=SQLite").is_err());
        // 公钥无效 → 构造即报错
        assert!(DriverManager::new(DriverManagerConfig {
            store_url: "http://127.0.0.1:1".into(),
            pubkey: "not-hex".into(),
            ..Default::default()
        })
        .is_err());
        // 无公钥时非回环 http 源被拒
        assert!(DriverManager::new(DriverManagerConfig {
            store_url: "http://10.0.0.5:5502".into(),
            ..Default::default()
        })
        .is_err());
    }

    // ————— 端到端：假组件源 + 真签名 + 假 dbserver（rustc 现场编译） —————

    const FAKE_DBSERVER_SRC: &str = r##"
fn main() {
    println!(r#"{{"event":"ready","addr":"127.0.0.1:59987","kind":"Fake","protocol":1}}"#);
    std::thread::sleep(std::time::Duration::from_secs(120));
}
"##;

    /// 现场编译假 dbserver（打印就绪行后常驻）；rustc 不可用返回 None（跳过）。
    fn build_fake_dbserver() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("pek-rcode-fake-server-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let bin = dir.join(if cfg!(windows) {
            "fake_dbserver.exe"
        } else {
            "fake_dbserver"
        });
        if bin.is_file() {
            return Some(bin);
        }
        let src = dir.join("fake_dbserver.rs");
        std::fs::write(&src, FAKE_DBSERVER_SRC).ok()?;
        let status = Command::new("rustc")
            .arg("--edition=2021")
            .arg("-O")
            .arg(&src)
            .arg("-o")
            .arg(&bin)
            .status()
            .ok()?;
        if status.success() { Some(bin) } else { None }
    }

    fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default();
            for (name, data) in files {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }

    /// 极简 HTTP 服务器（固定路由，每连接一请求；仅测试用）。
    fn serve(listener: TcpListener, routes: HashMap<String, Vec<u8>>) {
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(clone) = stream.try_clone() else { continue };
                let mut reader = BufReader::new(clone);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) => break,
                        Ok(_) if header.trim().is_empty() => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let (status, body) = match routes.get(&path) {
                    Some(bytes) => ("200 OK", bytes.clone()),
                    None => ("404 Not Found", Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
    }

    #[test]
    fn ensure_downloads_extracts_starts_and_reaps() {
        let Some(fake_bin) = build_fake_dbserver() else {
            eprintln!("跳过：rustc 不可用");
            return;
        };
        let work = temp_dir("e2e");
        let cache = work.join("cache");

        // 驱动包 zip：假 dbserver + driver.json
        let zip = build_zip(&[
            (DBSERVER_BIN, &std::fs::read(&fake_bin).unwrap()),
            (
                "driver.json",
                br#"{"id":"dbserver-mysql","kind":"MySql","version":"1.0.0","protocol":1}"#,
            ),
        ]);
        let sha = dhrust::sign::sha256_hex(&zip);

        // 假组件源（先 bind 拿地址，再构造签名目录）
        let key = dhrust::plugin::SigningKey::from_bytes(&[5u8; 32]);
        let pubkey = dhrust::plugin::pubkey_hex(&key);
        let target = current_target().expect("当前平台应有标识");
        let component = "dbserver-mysql";
        let version = "1.0.0";
        let zip_path = format!("/components/{component}-{version}-{target}.zip");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let catalog = serde_json::json!({
            "name": "测试组件源",
            "version": 1,
            "components": [{
                "id": component,
                "name": "MySQL",
                "category": "driver",
                "version": version,
                "target": target,
                "sizeKB": zip.len() as u64 / 1024,
                "sha256": sha,
                "notes": "",
                "url": format!("{base}{zip_path}"),
            }]
        })
        .to_string();
        let sig = dhrust::plugin::sign_base64(&key, catalog.as_bytes());
        let routes = HashMap::from([
            (
                "/components/catalog.json".to_string(),
                catalog.into_bytes(),
            ),
            (
                "/components/catalog.json.sig".to_string(),
                sig.into_bytes(),
            ),
            (zip_path, zip),
        ]);
        serve(listener, routes);

        let mgr = DriverManager::new(DriverManagerConfig {
            store_url: base,
            pubkey,
            cache_dir: Some(cache.clone()),
            idle_timeout: Some(Duration::from_millis(300)),
            ready_timeout: Some(Duration::from_secs(10)),
            ca_pem: None,
        })
        .unwrap();

        let real = "Server=127.0.0.1;Database=demo;Provider=MySql";
        let conn = mgr.ensure(real).expect("ensure 应成功");
        assert!(conn.starts_with("Server=http://127.0.0.1:59987;"), "{conn}");
        assert!(conn.contains("Database=demo;"));
        assert!(conn.ends_with("provider=network"));
        // 解压产物落盘
        assert!(
            cache
                .join(component)
                .join(version)
                .join(DBSERVER_BIN)
                .is_file()
        );
        // 相同连接串复用同一宿主
        let conn2 = mgr.ensure(real).unwrap();
        assert_eq!(conn, conn2);
        let hosts = mgr.hosts();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].component, component);
        // ensure_updated：同版本仍复用
        let conn3 = mgr.ensure_updated(real).unwrap();
        assert_eq!(conn, conn3);
        // 空闲回收
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(mgr.reap_idle(), 1);
        assert!(mgr.hosts().is_empty());
        // 显式停止（无命中返回 false）
        assert!(!mgr.stop(real));
    }
}
