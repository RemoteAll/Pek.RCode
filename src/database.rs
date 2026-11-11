//! 数据库连接约定文件与多数据源容器（`Config/Database.toml`）。
//!
//! **约定**（项目间统一）：各项目在 `Config/Database.toml` 声明数据库连接，
//! 支持多数据源（DH.NCode 特色：一个项目可同时使用多个库）：
//!
//! ```toml
//! Default = "main"                       # 默认连接名
//!
//! [DriverStore]                          # 驱动组件源（可选）：需要未内置驱动时自动下载分发
//! Url = "http://192.168.1.10:5502"
//! PubKey = ""                            # Ed25519 hex；非空强制验签
//!
//! [Connections.main]
//! ConnectionString = "Data Source=Data/main.db;Provider=SQLite"
//! Note = "业务主库"                       # 可选备注
//!
//! # 简写也支持：Connections.log = "Server=...;Provider=MySql"
//! ```
//!
//! - [`DatabaseFile::load`]：加载约定文件；**文件缺失返回 `None`**（消费方可回退自有旧配置，
//!   便于平滑迁移）；[`TEMPLATE`] 为带注释的模板；
//! - [`Databases`]：多数据源容器——按连接名**惰性打开并复用**（单飞 + 串行锁经 [`crate::store`]），
//!   需要驱动包的连接自动经 [`DriverManager`] 分发（驱动缓存 `{base}/Data/Drivers`），
//!   可用 `model` 提供时按模型打开并增量同步表结构；
//! - 自动化能力（对齐 DH.NCode `Entity<T>.InitData`——"初始化数据，执行反向工程检查，建库建表"）：
//!   [`Databases::with_remote_schema_sync`]（network/驱动宿主连接也按模型建表）与
//!   [`Databases::with_init_data`]（连接首次打开后初始化数据）；
//! - 面向面板/管理功能：`names` / `default_name` / `connection_label`（摘要展示，避免泄露口令）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "driver-pack")]
use std::sync::Mutex;

use serde::Deserialize;

use crate::dal::{ConnectionString, Dal};
#[cfg(feature = "driver-pack")]
use crate::driver_pack::{self, DriverManager, DriverManagerConfig, DriverPackNeed};
use crate::model::EntityModel;
use crate::store::{self, SharedStore};

/// 约定文件名（`{base}/Config/Database.toml`）。
pub const FILE_NAME: &str = "Database.toml";

/// 约定文件模板（带注释；项目首启/文档引用）。
pub const TEMPLATE: &str = r#"# 数据库连接约定文件（各项目统一：Config/Database.toml）
# - 支持多数据源：Connections 下每个名字一个连接；Default 指定默认连接名
# - 连接串与 XCode 一致（Provider 决定驱动）；SQLite 始终内置；
#   MySql/PostgreSQL 等未内置驱动可在 [DriverStore] 配置组件源后运行时自动下载分发
#   （组件源 = Pek.RPanlServer「下载管理」页地址与公钥）

# 默认连接名（程序/面板未指定时使用）
Default = "main"

# 驱动组件源（可选；需要未内置驱动时自动下载 dbserver 驱动包并拉起）
# [DriverStore]
# Url = "http://192.168.1.10:5502"
# PubKey = ""
# CaFile = "Config/store-ca.pem"   # 组件源为 https 自签/内网 CA 时指定根证书（PEM 文件路径）
#   自签证书须为叶子（CA:FALSE）或标准 CA+叶子链（信任 CA）；SAN 需含访问地址

# 连接定义（表形态；也支持简写：Connections.main = "Data Source=Data/main.db;Provider=SQLite"）
# 注：Windows 绝对路径请用**单引号**字符串（如 'Data Source=C:\App\Data\x.db;Provider=SQLite'）
#     或正斜杠（C:/App/Data/x.db）；双引号串中 \U、\x 等会被 TOML 当作转义序列。
[Connections.main]
ConnectionString = "Data Source=Data/main.db;Provider=SQLite"
Note = "业务主库"

# 多数据源示例（取消注释启用）：
# [Connections.log]
# ConnectionString = "Server=127.0.0.1;Database=log;Uid=root;Pwd=***;Provider=MySql"
# Note = "日志库（首次使用自动从组件源下载 MySql 驱动包）"
"#;

/// 驱动组件源配置（`[DriverStore]` 段）。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct DriverStore {
    /// 组件源地址（平台地址或完整 catalog 地址）
    pub url: String,
    /// Ed25519 公钥（hex；非空强制验签）
    pub pub_key: String,
    /// 组件源 https 根证书（PEM 文件路径；自签/内网 CA 场景；空 = WebPki 内置根）
    pub ca_file: String,
}

/// 连接条目：字符串简写或表形态。
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ConnectionEntry {
    /// 简写：`Connections.main = "Server=...;Provider=MySql"`
    Simple(String),
    /// 表形态：`[Connections.main] ConnectionString = "..." + Note = "..."`
    Full(FullConnection),
}

/// 表形态连接。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FullConnection {
    /// 连接串
    pub connection_string: String,
    /// 备注（可选）
    #[serde(default)]
    pub note: String,
}

impl ConnectionEntry {
    /// 连接串。
    pub fn connection_string(&self) -> &str {
        match self {
            ConnectionEntry::Simple(s) => s,
            ConnectionEntry::Full(f) => &f.connection_string,
        }
    }

    /// 备注（无则空串）。
    pub fn note(&self) -> &str {
        match self {
            ConnectionEntry::Simple(_) => "",
            ConnectionEntry::Full(f) => &f.note,
        }
    }
}

/// 数据库约定文件（`Config/Database.toml`）。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct DatabaseFile {
    /// 默认连接名（缺省 `main`）
    pub default: String,
    /// 连接表：名字 → 连接
    pub connections: BTreeMap<String, ConnectionEntry>,
    /// 驱动组件源（可选）
    pub driver_store: Option<DriverStore>,
    /// 外部数据域绑定（可选）：本项目自身数据之外的、属于主站（C#）的库
    pub external: Option<ExternalLinks>,
}

/// 外部数据域绑定。
///
/// 口径（与 C# 一致）：**本项目自己产生的数据**（用量、上传、文章、面板账号等）放本项目库；
/// **主站（C#）的数据**（如用户表 `DH_UserEx`——中转站只是读它来校验 Key）绑到主站自己的库，
/// 不能混入本项目库。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ExternalLinks {
    /// 用户/成员库连接名（`DH_UserEx` 所在库）；空 = 与本项目库相同
    pub user_connection: String,
}

impl DatabaseFile {
    /// 主站用户库连接名（未配置或为空时返回 `None` = 与本项目库相同）。
    pub fn user_connection(&self) -> Option<&str> {
        self.external
            .as_ref()
            .map(|e| e.user_connection.trim())
            .filter(|s| !s.is_empty())
    }
}

impl DatabaseFile {
    /// 加载约定文件；**文件不存在返回 `None`**。
    pub fn load(base: &Path) -> Result<Option<Self>, String> {
        let path = file_path(base);
        if !path.is_file() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("读取 {FILE_NAME} 失败：{e}"))?;
        Self::parse(&text).map(Some)
    }

    /// 从 TOML 文本解析（与 [`DatabaseFile::load`] 同口径）。
    ///
    /// 供**热重载**使用：`dhrust::config::ConfigWatcher` 的 parse 闭包要直接吃文件文本，
    /// 不必为了解析再读一次盘（也不会有「两次读到不同内容」的窗口）。
    pub fn parse(text: &str) -> Result<Self, String> {
        let doc = dhrust::config::toml::parse_document(text)?;
        let json = dhrust::config::toml::document_to_json(&doc)?;
        let mut file: DatabaseFile =
            serde_json::from_value(json).map_err(|e| format!("{FILE_NAME} 解析失败：{e}"))?;
        file.normalize();
        Ok(file)
    }

    /// 渲染为 TOML 文本（与 [`DatabaseFile::load`] 同口径，可被自身解析回来）。
    ///
    /// 供面板/部署脚本回写 `Config/Database.toml`；`ConnectionEntry::Simple`（字符串简写）
    /// 一律按表形态 `[Connections.<名>]` 写出——语义等价，且便于运维逐项加备注。
    /// 键名/取值转义复用 [`dhrust::config::toml`]，含引号、换行、控制字符也能写出合法 TOML。
    pub fn render(&self) -> String {
        let mut out = String::from("# 数据库约定文件（与生态其它项目一致；改完需重启生效）\n");
        out.push_str(&format!(
            "Default = \"{}\"\n\n",
            toml_escape(self.default_name())
        ));

        for (name, entry) in &self.connections {
            out.push_str(&format!("[Connections.{}]\n", toml_quote_key(name)));
            out.push_str(&format!(
                "ConnectionString = \"{}\"\n",
                toml_escape(entry.connection_string())
            ));
            out.push_str(&format!("Note = \"{}\"\n\n", toml_escape(entry.note())));
        }

        if let Some(store) = &self.driver_store {
            out.push_str("[DriverStore]\n");
            out.push_str(&format!("Url = \"{}\"\n", toml_escape(&store.url)));
            out.push_str(&format!("PubKey = \"{}\"\n", toml_escape(&store.pub_key)));
            out.push_str(&format!("CaFile = \"{}\"\n", toml_escape(&store.ca_file)));
        }

        // 外部数据域绑定（非空才写，避免默认文件多一段无用配置）
        if let Some(ext) = &self.external
            && !ext.user_connection.trim().is_empty()
        {
            out.push_str("[External]\n");
            out.push_str(&format!(
                "UserConnection = \"{}\"\n",
                toml_escape(&ext.user_connection)
            ));
        }

        out
    }

    /// 归一化：修剪键/值空白、丢弃空名连接。
    pub fn normalize(&mut self) {
        self.default = self.default.trim().to_string();
        let mut rebuilt = BTreeMap::new();
        for (name, mut entry) in std::mem::take(&mut self.connections) {
            let name = name.trim().to_string();
            if name.is_empty() {
                continue;
            }
            match &mut entry {
                ConnectionEntry::Simple(s) => *s = s.trim().to_string(),
                ConnectionEntry::Full(f) => {
                    f.connection_string = f.connection_string.trim().to_string();
                    f.note = f.note.trim().to_string();
                }
            }
            rebuilt.insert(name, entry);
        }
        self.connections = rebuilt;
        if let Some(store) = &mut self.driver_store {
            store.url = store.url.trim().trim_end_matches('/').to_string();
            store.pub_key = store.pub_key.trim().to_string();
        }
    }

    /// 默认连接名（未配置时 `main`）。
    pub fn default_name(&self) -> &str {
        if self.default.is_empty() {
            "main"
        } else {
            &self.default
        }
    }

    /// 连接名清单（字典序）。
    pub fn names(&self) -> Vec<&str> {
        self.connections.keys().map(String::as_str).collect()
    }

    /// 解析指定连接名（SQLite 相对路径按 `base` 绝对化）。
    ///
    /// 供读写分离等“按连接名取实际连接串”的场景使用；支持 C# 的 `MapTo=` 别名
    /// （见 [`DatabaseFile::follow_map_to`]）。
    pub fn resolve_connection(&self, base: &Path, name: &str) -> Result<String, String> {
        let (_, raw) = self.follow_map_to(name)?;
        resolve_connection(base, &raw)
    }

    /// 跟随 C# 的 `MapTo=<连接名>` 别名，返回 `(最终连接名, 真实连接串)`。
    ///
    /// 实现见 [`DatabaseFile::map_to_chain`]（本函数只取链尾）。
    pub fn follow_map_to(&self, name: &str) -> Result<(String, String), String> {
        let chain = self.map_to_chain(name)?;
        let last = chain.last().cloned().unwrap_or_default();
        let (key, entry) = self
            .lookup(&last)
            .ok_or_else(|| format!("连接 {last} 不存在（可用：{}）", self.names().join(", ")))?;
        Ok((key.to_string(), entry.connection_string().trim().to_string()))
    }

    /// 返回 `MapTo=` 别名链上的**全部连接名**（起点 → 终点；不是别名时只有起点本身）。
    ///
    /// 例：`DH = "MapTo=Pek"` → `["DH", "Pek"]`。
    ///
    /// 用途：别名与目标指向**同一个物理库**，所以「按连接名过滤」的场景（如建表）
    /// 必须同时认链上所有名字 —— 只认终点会把 `DH` 的表漏掉（真实缺陷：
    /// `Data/Pek.db` 里没有 `DH_UserEx`，因为模型里该表的 `ConnName` 写的是 `DH`）。
    pub fn map_to_chain(&self, name: &str) -> Result<Vec<String>, String> {
        let start = name.trim();
        let mut current = start.to_string();
        let mut seen: Vec<String> = Vec::new();
        loop {
            if seen.iter().any(|s| s.eq_ignore_ascii_case(&current)) {
                let mut chain: Vec<String> = seen.clone();
                chain.push(current.clone());
                return Err(format!("连接 {start} 的 MapTo 出现循环：{}", chain.join(" → ")));
            }
            if seen.len() >= 8 {
                return Err(format!("连接 {start} 的 MapTo 跳转超过 8 层（疑似循环）：{}", seen.join(" → ")));
            }
            // 连接名查找**大小写不敏感**（对齐 .NET `ConnectionStrings` 语义；
            // 线上 C# 配置里 MapTo=Pek 与连接名 Pek 的大小写不一定一致）
            let (key, entry) = self.lookup(&current).ok_or_else(|| {
                format!("连接 {current} 不存在（可用：{}）", self.names().join(", "))
            })?;
            let key = key.to_string();
            let raw = entry.connection_string().trim();
            match map_to_of(raw) {
                Some(target) if !target.is_empty() => {
                    seen.push(key);
                    current = target;
                }
                Some(_) => return Err(format!("连接 {current} 的 MapTo 目标为空")),
                None => {
                    seen.push(key);
                    return Ok(seen);
                }
            }
        }
    }

    /// 按名字查连接（先精确、再大小写不敏感），返回真实键名与条目。
    fn lookup(&self, name: &str) -> Option<(&str, &ConnectionEntry)> {
        if let Some((k, v)) = self.connections.get_key_value(name) {
            return Some((k.as_str(), v));
        }
        self.connections
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(k, v)| (k.as_str(), v))
    }
}

/// 从连接串里取 `MapTo=<连接名>`（大小写不敏感；C# DH.NCode 的多库别名写法）。
/// 例：`MapTo=Pek`。
fn map_to_of(conn: &str) -> Option<String> {
    for seg in conn.split(';') {
        let seg = seg.trim();
        if let Some((k, v)) = seg.split_once('=')
            && k.trim().eq_ignore_ascii_case("MapTo")
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// 约定文件完整路径（`{base}/Config/Database.toml`）。
pub fn file_path(base: &Path) -> PathBuf {
    base.join("Config").join(FILE_NAME)
}

/// TOML 基本字符串转义（含引号/换行/控制字符）
fn toml_escape(value: &str) -> String {
    dhrust::config::toml::escape_basic(value)
}

/// TOML 裸键不安全时加引号
fn toml_quote_key(value: &str) -> String {
    dhrust::config::toml::quote_key(value)
}

/// 连接串摘要标签（如 `Provider=MySql` / `MapTo=Pek`；面板/日志展示用，避免泄露口令）。
pub fn connection_label(conn: &str) -> String {
    // 别名连接（`MapTo=Pek`）没有 Provider，直接显示映射目标，别显示成「Provider=未知」
    if let Some(target) = map_to_of(conn) {
        return format!("MapTo={target}");
    }
    let cs = ConnectionString::parse(conn);
    cs.provider()
        .map(|p| format!("Provider={p}"))
        .unwrap_or_else(|| "Provider=未知".to_string())
}

/// SQLite 连接的 `Data Source` 相对路径按 `base` 解析为绝对路径（并创建父目录）。
///
/// 其他 Provider 原样返回（非 SQLite 不做路径处理）；以解决“配置写 `Data/xxx.db`、
/// 运行目录不确定”的常见部署问题（各项目统一行为）。
pub fn resolve_connection(base: &Path, conn: &str) -> Result<String, String> {
    let c = conn.trim();
    if c.is_empty() {
        return Err("连接串未配置".to_string());
    }
    if !c.to_ascii_lowercase().contains("provider=sqlite") {
        return Ok(c.to_string());
    }
    let mut parts: Vec<String> = Vec::new();
    let mut has_source = false;
    for seg in c.split(';') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some((k, v)) = seg.split_once('=')
            && k.trim().eq_ignore_ascii_case("data source")
        {
            // **路径分隔符归一（跨平台必须）**：配置里常写 Windows 风格 `Data\DH.db`，
            // 而 Linux 上反斜杠**不是**路径分隔符 —— `Path::new("Data\\DH.db")` 是「一个文件名」，
            // 于是 `base.join(...)` 会在**部署根**生成一个名字里带反斜杠的库（`Data\DH.db`），
            // `Data/` 目录里反而没有（线上实测踩到）。统一按 `/` 解析，
            // Windows / Linux 都得到正确的目录层级。
            let normalized = v.trim().replace('\\', "/");
            let p = Path::new(&normalized);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            };
            if let Some(dir) = abs.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
            }
            parts.push(format!("Data Source={}", abs.display()));
            has_source = true;
        } else {
            parts.push(seg.to_string());
        }
    }
    if !has_source {
        return Err("SQLite 连接串缺少 Data Source".to_string());
    }
    Ok(parts.join(";"))
}

/// 连接串的 SQLite 数据文件路径（`Data Source`；相对路径按 `base` 解析）。
///
/// 非 SQLite 或缺少 `Data Source` 时返回 `None`（面板/管理功能展示库文件用）。
pub fn sqlite_file_of(base: &Path, conn: &str) -> Option<PathBuf> {
    let cs = ConnectionString::parse(conn);
    if !cs.provider()?.eq_ignore_ascii_case("sqlite") {
        return None;
    }
    let ds = cs.data_source()?;
    // 与 `resolve_connection` 同口径：反斜杠按 `/` 解析（Linux 上反斜杠不是分隔符）
    let normalized = ds.replace('\\', "/");
    let path = Path::new(&normalized);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

/// 多数据源容器：按 `Config/Database.toml` 惰性打开并复用各连接。
///
/// 线程安全；同一连接名全进程复用同一 [`SharedStore`]（单飞打开 + 写路径串行锁）。
pub struct Databases {
    base: PathBuf,
    file: DatabaseFile,
    /// `Some` = 按模型打开并增量同步表结构（与各项目现有行为一致）。
    model: Option<EntityModel>,
    /// 对 network 连接也执行建表结构同步（默认 false，对齐 C# `NetworkMetaData.OnSetTables` 空实现）。
    /// 自有远端库（驱动组件分发的 dbserver 宿主）场景经 [`Databases::with_remote_schema_sync`] 启用。
    remote_schema_sync: bool,
    /// 连接首次打开成功后执行的**初始化数据**（对齐 DH.NCode `Entity<T>.InitData`；默认无）。
    /// 见 [`Databases::with_init_data`]。
    init_data: Option<InitDataFn>,
    /// 驱动分发管理器（需要时按 `[DriverStore]` 惰性构建；仅 driver-pack）。
    #[cfg(feature = "driver-pack")]
    manager: Mutex<Option<DriverManagerSlot>>,
}

impl Databases {
    /// 构建容器（`model` 提供时：`Dal::open_with_model` + `sync_schema`）。
    ///
    /// **模型级连接名优先**：C# 那边实体取连接是「`Model.xml` 的 `<Option><ConnName>` → 同名连接」，
    /// 不是一律用约定文件的 `Default`。这里对齐：模型 `ConnName` 在约定文件里有同名连接时，
    /// 它就成为默认连接；没有同名连接则保持 `Default`（部署文件是权威，模型与它不一致时不让启动失败，
    /// `PEK_PROXY_DB` 这类"只给一个连接"的覆盖场景也不会因为找不到 ConnName 而报错）。
    pub fn new(base: &Path, file: DatabaseFile, model: Option<EntityModel>) -> Self {
        let mut file = file;
        if let Some(wanted) = model
            .as_ref()
            .and_then(|m| m.options.conn_name())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| file.connections.contains_key(*s))
        {
            file.default = wanted.to_string();
        }
        Self {
            base: base.to_path_buf(),
            file,
            model,
            remote_schema_sync: false,
            init_data: None,
            #[cfg(feature = "driver-pack")]
            manager: Mutex::new(None),
        }
    }

    /// 启用 "network 连接结构同步"（builder 链式；默认关闭）。
    ///
    /// 开启后，`provider=network` 的**自有远端库**（驱动组件自举的 dbserver 宿主）在首次
    /// 打开时会像原生连接一样按模型建表（对齐 [`Dal::sync_schema_including_network`]）；
    /// 未开启时保持 C# 对齐的"网络连接不建表"行为。
    pub fn with_remote_schema_sync(mut self, yes: bool) -> Self {
        self.remote_schema_sync = yes;
        self
    }

    /// 设置 "首次打开连接时初始化数据"（builder 链式；对齐 DH.NCode `Entity<T>.InitData`）。
    ///
    /// 回调在数据源打开成功（含结构同步）后调用，参数为连接名与共享 [`Dal`]；
    /// **消费方自行判空并保证幂等**（进程重启会再次触发，与 XCode"首次连接时初始化"语义一致）。
    /// 回调失败视同打开失败：不注册、可重试。
    pub fn with_init_data<F>(mut self, f: F) -> Self
    where
        F: Fn(&str, &Dal) -> Result<(), String> + Send + Sync + 'static,
    {
        self.init_data = Some(Arc::new(f));
        self
    }

    /// 加载约定文件并构建；**文件缺失返回 `None`**。
    pub fn load(base: &Path, model: Option<EntityModel>) -> Result<Option<Self>, String> {
        match DatabaseFile::load(base)? {
            Some(file) => Ok(Some(Self::new(base, file, model))),
            None => Ok(None),
        }
    }

    /// 约定文件内容。
    pub fn file(&self) -> &DatabaseFile {
        &self.file
    }

    /// 连接名清单。
    pub fn names(&self) -> Vec<String> {
        self.file.names().into_iter().map(str::to_string).collect()
    }

    /// 默认连接名。
    pub fn default_name(&self) -> String {
        self.file.default_name().to_string()
    }

    /// 连接备注（无则空串）。
    pub fn note(&self, name: &str) -> String {
        self.file
            .connections
            .get(name.trim())
            .map(|e| e.note().to_string())
            .unwrap_or_default()
    }

    /// 连接串摘要（避免泄露口令）。
    ///
    /// - 本条是别名（`MapTo=Pek`）→ 显示 `MapTo=Pek（Provider=MySql）`：
    ///   既让人看出「它指向哪条」，也知道实际用的是哪种库；
    /// - 否则显示自己的 `Provider=xxx`。
    pub fn connection_label(&self, name: &str) -> String {
        let Some((_, entry)) = self.file.lookup(name) else {
            return "未知连接".to_string();
        };
        let raw = entry.connection_string().trim();
        if let Some(target) = map_to_of(raw) {
            return match self.file.follow_map_to(name) {
                Ok((_, resolved)) => format!("MapTo={target}（{}）", connection_label(&resolved)),
                Err(_) => format!("MapTo={target}"),
            };
        }
        connection_label(raw)
    }

    /// 打开（或复用）指定连接的共享存储。
    ///
    /// 需要驱动包的连接：按 `[DriverStore]` 分发（未配置且无缓存/预置时给出明确错误）。
    pub fn store(&self, name: &str) -> Result<Arc<SharedStore>, String> {
        // `MapTo=` 别名与目标指向同一个库 → 用**最终连接名**做缓存 key，共享同一个连接池
        let (target, raw) = self.file.follow_map_to(name)?;
        // 建表按**整条别名链**过滤：`DH = MapTo=Pek` 时 `DH_UserEx`（表级 ConnName=DH）
        // 必须也能建到 `Pek` 的库里，否则新部署的 `Pek.db` 会缺这些表。
        let chain = self.file.map_to_chain(name)?;
        let chain_refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        let raw = resolve_connection(&self.base, &raw)?;
        let (store, _) = store::get_or_open_scoped(&self.base, &target, || {
            let conn = self.prepare_conn(&raw)?;
            let dal = match &self.model {
                Some(model) => {
                    let dal = Dal::open_with_model(&conn, model.clone())
                        .map_err(|e| format!("打开数据库失败：{e}"))?;
                    // **按连接名建表**：多库模型下每个连接只建属于自己的表
                    //（表级 `ConnName` 优先，没有则归模型级 `ConnName`）。
                    // 「无表级 ConnName」的表只在本连接就是模型级连接（或模型级连接没配置、本连接代它）时建。
                    let own_model_tables = match model.options.conn_name() {
                        Some(model_conn) => {
                            chain
                                .iter()
                                .any(|c| c.eq_ignore_ascii_case(model_conn.trim()))
                                || !self
                                    .file
                                    .connections
                                    .keys()
                                    .any(|k| k.eq_ignore_ascii_case(model_conn.trim()))
                        }
                        None => true,
                    };
                    dal.sync_schema_for_any(&chain_refs, own_model_tables, false)
                        .map_err(|e| format!("同步表结构失败：{e}"))?;
                    // 自有远端库：network 连接默认空实现（对齐 C#），显式启用时补做结构同步
                    if self.remote_schema_sync
                        && crate::network::is_network(&ConnectionString::parse(&conn))
                    {
                        dal.sync_schema_for_any(&chain_refs, own_model_tables, true)
                            .map_err(|e| format!("同步远端表结构失败：{e}"))?;
                    }
                    dal
                }
                None => Dal::open(&conn).map_err(|e| format!("打开数据库失败：{e}"))?,
            };
            // 首次打开时初始化数据（对齐 DH.NCode `Entity<T>.InitData`：首次连接数据库时初始化数据；
            // 消费方自行判空、保证幂等；失败视同打开失败——不注册、可重试）
            if let Some(init) = &self.init_data {
                init(name, &dal).map_err(|e| format!("初始化数据失败：{e}"))?;
            }
            Ok(dal)
        })?;
        Ok(store)
    }

    /// 默认连接的共享存储。
    pub fn default_store(&self) -> Result<Arc<SharedStore>, String> {
        let name = self.default_name();
        self.store(&name)
    }

    /// 连接串 → 可打开连接串（内嵌/network 原样；需要驱动包时经组件源分发）。
    fn prepare_conn(&self, conn: &str) -> Result<String, String> {
        #[cfg(feature = "driver-pack")]
        {
            match driver_pack::driver_pack_need(conn) {
                Err(e) => Err(format!("连接串无效：{e}")),
                Ok(DriverPackNeed::Direct) => Ok(conn.to_string()),
                Ok(DriverPackNeed::Component(_)) => {
                    let manager = self.manager()?;
                    manager.prepare(conn).map_err(|e| format!("{e}"))
                }
            }
        }
        #[cfg(not(feature = "driver-pack"))]
        {
            // 未启用驱动分发：直连驱动照常打开；组件驱动由 Dal 在打开时报错
            Ok(conn.to_string())
        }
    }

    /// 取（或按 `[DriverStore]` 惰性构建）驱动分发管理器。
    #[cfg(feature = "driver-pack")]
    pub fn driver_manager(&self) -> Option<Arc<DriverManager>> {
        self.manager().ok()
    }

    /// 取（或按 `[DriverStore]` 惰性构建）驱动分发管理器。
    #[cfg(feature = "driver-pack")]
    fn manager(&self) -> Result<Arc<DriverManager>, String> {
        let mut slot = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(existing));
        }
        let Some(spec) = self.file.driver_store.as_ref() else {
            return Err(format!(
                "连接需要驱动包（本程序未内置该驱动），但未配置驱动组件源：\
                 请在 {FILE_NAME} 增加 [DriverStore]（Url/PubKey；Url 可空 = 仅用本机缓存）"
            ));
        };
        let ca_pem = if spec.ca_file.trim().is_empty() {
            None
        } else {
            let p = Path::new(spec.ca_file.trim());
            let path = if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.base.join(p)
            };
            Some(
                std::fs::read(&path)
                    .map_err(|e| format!("读取组件源根证书失败（{}）：{e}", path.display()))?,
            )
        };
        let manager = DriverManager::new(DriverManagerConfig {
            store_url: spec.url.clone(),
            pubkey: spec.pub_key.clone(),
            cache_dir: Some(self.base.join("Data").join("Drivers")),
            ca_pem,
            ..Default::default()
        })
        .map_err(|e| format!("驱动组件源配置无效：{e}"))?;
        let manager = Arc::new(manager);
        *slot = Some(Arc::clone(&manager));
        Ok(manager)
    }
}

/// 驱动分发管理器槽（惰性构建；`[DriverStore]` 配置后才存在）。
#[cfg(feature = "driver-pack")]
type DriverManagerSlot = Arc<DriverManager>;

/// 初始化数据回调（连接首次打开成功后执行；对齐 DH.NCode `Entity<T>.InitData`）。
type InitDataFn = Arc<dyn Fn(&str, &Dal) -> Result<(), String> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pek-database-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Config")).unwrap();
        dir
    }

    fn write_file(base: &Path, text: &str) {
        std::fs::write(file_path(base), text).unwrap();
    }

    #[test]
    fn template_parses() {
        let dir = temp_dir("tpl");
        write_file(&dir, TEMPLATE);
        let file = DatabaseFile::load(&dir).unwrap().expect("模板应可解析");
        assert_eq!(file.default_name(), "main");
        assert!(file.connections.contains_key("main"));
        assert_eq!(
            file.connections["main"].connection_string(),
            "Data Source=Data/main.db;Provider=SQLite"
        );
        assert!(file.driver_store.is_none());
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = temp_dir("none");
        assert!(DatabaseFile::load(&dir).unwrap().is_none());
        assert!(Databases::load(&dir, None).unwrap().is_none());
    }

    #[test]
    fn parses_full_and_simple_forms() {
        let dir = temp_dir("forms");
        write_file(
            &dir,
            r#"
Default = "main"
Connections.simple = "Data Source=Data/s.db;Provider=SQLite"

[DriverStore]
Url = "http://127.0.0.1:5502/"
PubKey = " abc "
[Connections.main]
ConnectionString = "Data Source=Data/a.db;Provider=SQLite"
Note = " 主库 "
[Connections.log]
ConnectionString = "Server=x;Provider=MySql"
"#,
        );
        let file = DatabaseFile::load(&dir).unwrap().unwrap();
        assert_eq!(file.default_name(), "main");
        assert_eq!(file.names(), vec!["log", "main", "simple"]);
        assert_eq!(file.connections["main"].note(), "主库");
        assert_eq!(
            file.driver_store.as_ref().unwrap().url,
            "http://127.0.0.1:5502"
        );
        assert_eq!(file.driver_store.as_ref().unwrap().pub_key, "abc");
        assert_eq!(
            connection_label("Server=x;Provider=MySQL"),
            "Provider=MySQL"
        );
    }

    #[test]
    fn render_round_trips_through_load() {
        let dir = temp_dir("render");
        write_file(
            &dir,
            r#"
Default = "main"
Connections.simple = "Server=y;Provider=MySql"

[DriverStore]
Url = "http://127.0.0.1:5502"
PubKey = "abc"
CaFile = "/etc/ca.pem"

[Connections.main]
ConnectionString = "Data Source=Data/a.db;Provider=SQLite"
Note = "主库 \"引号\" 与\n换行"
"#,
        );
        let loaded = DatabaseFile::load(&dir).unwrap().expect("应可解析");
        let rendered = loaded.render();

        // 回写后必须能被自己解析回来（面板改配置不会把文件写坏）
        write_file(&dir, &rendered);
        let again = DatabaseFile::load(&dir).unwrap().expect("render 结果应可解析");

        assert_eq!(again.default_name(), "main");
        assert_eq!(again.names(), loaded.names());
        for name in loaded.names() {
            assert_eq!(
                again.connections[name].connection_string(),
                loaded.connections[name].connection_string(),
                "连接串在往返后必须一致：{name}"
            );
            assert_eq!(
                again.connections[name].note(),
                loaded.connections[name].note(),
                "备注在往返后必须一致：{name}"
            );
        }
        let store = again.driver_store.expect("DriverStore 不应丢");
        assert_eq!(store.url, "http://127.0.0.1:5502");
        assert_eq!(store.pub_key, "abc");
        assert_eq!(store.ca_file, "/etc/ca.pem");

        // 简写形态回写为表形态（语义等价、便于运维逐项加备注）
        assert!(
            rendered.contains("[Connections.simple]"),
            "简写应写成表形态：{rendered}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_to_alias_resolves_to_target_and_detects_cycle() {
        let dir = temp_dir("mapto");
        write_file(
            &dir,
            r#"
Default = "DH"
[Connections.Pek]
ConnectionString = "Server=db;Port=3306;Database=pek;provider=mysql"
Note = "线上主库"
[Connections.Membership]
ConnectionString = "MapTo=Pek"
[Connections.DH]
ConnectionString = "mapto=pek"
"#,
        );
        let file = DatabaseFile::load(&dir).unwrap().expect("应可解析");

        // 别名跟随到真实连接串（大小写不敏感）
        let (target, raw) = file.follow_map_to("DH").unwrap();
        assert_eq!(target, "Pek");
        assert!(raw.contains("Database=pek"), "{raw}");
        assert_eq!(
            file.resolve_connection(&dir, "Membership").unwrap(),
            file.resolve_connection(&dir, "Pek").unwrap(),
            "Membership 与 Pek 必须解析成同一个库"
        );

        // 循环必须明确报错，不能静默当成普通连接串
        let mut cyclic = DatabaseFile::default();
        cyclic.connections.insert(
            "A".to_string(),
            ConnectionEntry::Simple("MapTo=B".to_string()),
        );
        cyclic.connections.insert(
            "B".to_string(),
            ConnectionEntry::Simple("MapTo=A".to_string()),
        );
        let err = cyclic.follow_map_to("A").unwrap_err();
        assert!(err.contains("循环"), "{err}");

        // 目标不存在 → 报错里带可用连接名
        let mut orphan = DatabaseFile::default();
        orphan.connections.insert(
            "X".to_string(),
            ConnectionEntry::Simple("MapTo=Missing".to_string()),
        );
        let err = orphan.follow_map_to("X").unwrap_err();
        assert!(err.contains("Missing"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_to_chain_lists_every_alias_name() {
        // 建表要按**整条链**过滤（别名与目标同库）：`DH = MapTo=Pek` → ["DH", "Pek"]，
        // 否则模型里表级 `ConnName=DH` 的表不会建到 `Pek` 的库里。
        let mut file = DatabaseFile::default();
        file.connections.insert(
            "Pek".to_string(),
            ConnectionEntry::Simple("Data Source=Data/Pek.db;Provider=SQLite".to_string()),
        );
        file.connections.insert(
            "DH".to_string(),
            ConnectionEntry::Simple("MapTo=Pek".to_string()),
        );
        file.connections.insert(
            "Membership".to_string(),
            ConnectionEntry::Simple("MapTo=DH".to_string()),
        );

        assert_eq!(file.map_to_chain("DH").unwrap(), vec!["DH", "Pek"]);
        // 多跳：起点与终点都要在链上，顺序为「起点 → 终点」
        assert_eq!(
            file.map_to_chain("Membership").unwrap(),
            vec!["Membership", "DH", "Pek"]
        );
        // 直接连接：链上只有自己（不产生额外行为变化）
        assert_eq!(file.map_to_chain("Pek").unwrap(), vec!["Pek"]);
    }

    #[test]
    fn model_conn_name_wins_over_default_when_connection_exists() {
        // C# 口径：实体取连接是「Model.xml 的 <Option><ConnName> → 同名连接」，
        // 不是一律用 Database.toml 的 Default。
        let model = EntityModel::parse(
            r#"<EntityModel><Option><ConnName>DH</ConnName></Option><Tables></Tables></EntityModel>"#,
        )
        .expect("模型应可解析");

        let mut file = DatabaseFile::default();
        file.connections.insert(
            "main".to_string(),
            ConnectionEntry::Simple("Data Source=Data/main.db;Provider=SQLite".to_string()),
        );
        file.connections.insert(
            "DH".to_string(),
            ConnectionEntry::Simple("Data Source=Data/dh.db;Provider=SQLite".to_string()),
        );
        file.default = "main".to_string();

        let dbs = Databases::new(Path::new("."), file.clone(), Some(model.clone()));
        assert_eq!(dbs.default_name(), "DH", "模型 ConnName 应优先于 Default");

        // 约定文件里没有同名连接 → 保持 Default（部署文件权威，不让启动因模型不一致而失败）
        let mut only_main = DatabaseFile::default();
        only_main.connections.insert(
            "main".to_string(),
            ConnectionEntry::Simple("Data Source=Data/main.db;Provider=SQLite".to_string()),
        );
        only_main.default = "main".to_string();
        let dbs = Databases::new(Path::new("."), only_main, Some(model));
        assert_eq!(dbs.default_name(), "main");
    }

    #[test]
    fn multiple_sources_open_and_reuse() {
        let dir = temp_dir("multi");
        write_file(
            &dir,
            r#"
Default = "a"
[Connections.a]
ConnectionString = "Data Source=Data/a.db;Provider=SQLite"
[Connections.b]
ConnectionString = "Data Source=Data/b.db;Provider=SQLite"
"#,
        );
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        let dbs = Databases::load(&dir, None).unwrap().unwrap();
        // SQLite 相对路径由库统一按 base 解析（并建目录），无需测试重写连接
        let sa = dbs.store("a").unwrap();
        let sb = dbs.store("b").unwrap();
        assert!(!Arc::ptr_eq(&sa, &sb));
        // 各自建表互不影响：a 建 Demo 后 b 无此表
        sa.with_session(|_dal, session| {
            session.execute("CREATE TABLE \"Demo\" (\"Id\" INTEGER PRIMARY KEY)", &[])
        })
        .unwrap();
        let has_in_b = sb
            .with_session(|_dal, session| Ok(session.table_exists("Demo").unwrap_or(false)))
            .unwrap();
        assert!(!has_in_b, "b 库不应看到 a 库的表");
        // 复用
        let sa2 = dbs.store("a").unwrap();
        assert!(Arc::ptr_eq(&sa, &sa2));
        assert_eq!(dbs.default_name(), "a");
        assert!(dbs.store("none").is_err());
        store::drop_for_test_scoped(&dir, "a");
        store::drop_for_test_scoped(&dir, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_connection_absolutizes_sqlite_and_keeps_others() {
        let dir = temp_dir("resolve");
        let out = resolve_connection(&dir, "Data Source=Data/x.db;Provider=SQLite;ShowSql=false")
            .unwrap();
        assert!(out.contains(&dir.display().to_string()), "{out}");
        assert!(out.contains("ShowSql=false"), "其余段保留：{out}");
        assert!(dir.join("Data").is_dir(), "父目录自动创建");
        // 已是绝对路径：幂等（分隔符形式会被归一到 `/`，语义等价）
        let abs = resolve_connection(&dir, &out).unwrap();
        assert_eq!(
            abs.replace('\\', "/"),
            out.replace('\\', "/"),
            "已是绝对路径时应幂等"
        );
        // 非 SQLite 原样；空串报错
        assert_eq!(
            resolve_connection(&dir, "Server=x;Provider=MySql").unwrap(),
            "Server=x;Provider=MySql"
        );
        assert!(resolve_connection(&dir, "  ").is_err());
        // SQLite 缺 Data Source：明确报错
        let err = resolve_connection(&dir, "Provider=SQLite").unwrap_err();
        assert!(err.contains("Data Source"), "{err}");
        // SQLite 数据文件路径推导
        let p = sqlite_file_of(&dir, "Data Source=Data/x.db;Provider=SQLite").unwrap();
        assert_eq!(p, dir.join("Data").join("x.db"));
        assert!(sqlite_file_of(&dir, "Server=x;Provider=MySql").is_none());

        // **Windows 风格分隔符必须跨平台正确**：Linux 上反斜杠不是分隔符，
        // 不归一会生成「文件名带反斜杠」的库（`Data\DH.db` 落在部署根，Data/ 里反而没有）。
        // 入参是 TOML **解析后**的值，即单个反斜杠。
        let expected = dir.join("Data").join("DH.db");
        let win = resolve_connection(&dir, "Data Source=Data\\DH.db;Provider=SQLite").unwrap();
        assert!(
            win.replace('\\', "/").contains(&expected.display().to_string().replace('\\', "/")),
            "反斜杠必须按目录分隔解析：{win}"
        );
        assert_eq!(
            sqlite_file_of(&dir, "Data Source=Data\\DH.db;Provider=SQLite").unwrap(),
            expected,
            "sqlite_file_of 也要按目录解析"
        );
        assert!(dir.join("Data").is_dir(), "应建出 Data 目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "driver-pack")]
    fn component_connection_without_driver_store_errors_clearly() {
        let dir = temp_dir("nostore");
        write_file(
            &dir,
            r#"
[Connections.main]
ConnectionString = "Server=127.0.0.1;Database=x;Provider=MySql"
"#,
        );
        let dbs = Databases::load(&dir, None).unwrap().unwrap();
        let err = match dbs.store("main") {
            Ok(_) => panic!("应报错"),
            Err(e) => e,
        };
        assert!(err.contains("DriverStore"), "{err}");
        assert!(err.contains(FILE_NAME), "{err}");
    }

    #[test]
    #[cfg(feature = "driver-pack")]
    fn empty_driver_store_url_is_cache_only() {
        let dir = temp_dir("cacheonly");
        write_file(
            &dir,
            r#"
[Connections.main]
ConnectionString = "Server=127.0.0.1;Database=x;Provider=MySql"

[DriverStore]
Url = ""
"#,
        );
        let dbs = Databases::load(&dir, None).unwrap().unwrap();
        let err = match dbs.store("main") {
            Ok(_) => panic!("空缓存 + 空 Url 应报错"),
            Err(e) => e,
        };
        assert!(err.contains("本地缓存为空"), "{err}");
        assert!(err.contains("组件源"), "{err}");
    }

    #[test]
    fn init_data_runs_on_first_open_per_connection() {
        use std::sync::Mutex;
        let dir = temp_dir("initdata");
        write_file(
            &dir,
            r#"
Default = "a"
[Connections.a]
ConnectionString = "Data Source=Data/a.db;Provider=SQLite"
[Connections.b]
ConnectionString = "Data Source=Data/b.db;Provider=SQLite"
"#,
        );
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        let dbs = Databases::load(&dir, None)
            .unwrap()
            .unwrap()
            .with_init_data(move |name, _dal| {
                sink.lock().unwrap().push(name.to_string());
                Ok(())
            });
        dbs.store("a").unwrap();
        dbs.store("a").unwrap(); // 复用已打开连接：不重复初始化
        dbs.store("b").unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), ["a", "b"]);
        store::drop_for_test_scoped(&dir, "a");
        store::drop_for_test_scoped(&dir, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_data_error_fails_open_and_allows_retry() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = temp_dir("initfail");
        write_file(
            &dir,
            "Default = \"main\"\n[Connections.main]\nConnectionString = \"Data Source=Data/m.db;Provider=SQLite\"\n",
        );
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let dbs = Databases::load(&dir, None)
            .unwrap()
            .unwrap()
            .with_init_data(move |_name, _dal| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("模拟初始化失败".to_string())
                } else {
                    Ok(())
                }
            });
        let err = match dbs.store("main") {
            Ok(_) => panic!("首次应失败"),
            Err(e) => e,
        };
        assert!(err.contains("初始化数据失败"), "{err}");
        // 未注册：重试时重新打开并再次执行初始化
        assert!(dbs.store("main").is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        store::drop_for_test_scoped(&dir, "main");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
