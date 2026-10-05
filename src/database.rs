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
//! - 面向面板/管理功能：`names` / `default_name` / `connection_label`（摘要展示，避免泄露口令）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
}

impl DatabaseFile {
    /// 加载约定文件；**文件不存在返回 `None`**。
    pub fn load(base: &Path) -> Result<Option<Self>, String> {
        let path = file_path(base);
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取 {FILE_NAME} 失败：{e}"))?;
        let doc = dhrust::config::toml::parse_document(&text)?;
        let json = dhrust::config::toml::document_to_json(&doc)?;
        let mut file: DatabaseFile =
            serde_json::from_value(json).map_err(|e| format!("{FILE_NAME} 解析失败：{e}"))?;
        file.normalize();
        Ok(Some(file))
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
}

/// 约定文件完整路径（`{base}/Config/Database.toml`）。
pub fn file_path(base: &Path) -> PathBuf {
    base.join("Config").join(FILE_NAME)
}

/// 连接串摘要标签（如 `Provider=MySql`；面板/日志展示用，避免泄露口令）。
pub fn connection_label(conn: &str) -> String {
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
    for seg in c.split(';') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some((k, v)) = seg.split_once('=')
            && k.trim().eq_ignore_ascii_case("data source")
        {
            let p = Path::new(v.trim());
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            };
            if let Some(dir) = abs.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("创建数据目录失败：{e}"))?;
            }
            parts.push(format!("Data Source={}", abs.display()));
        } else {
            parts.push(seg.to_string());
        }
    }
    if parts.is_empty() {
        return Err("连接串无效".to_string());
    }
    Ok(parts.join(";"))
}

/// 多数据源容器：按 `Config/Database.toml` 惰性打开并复用各连接。
///
/// 线程安全；同一连接名全进程复用同一 [`SharedStore`]（单飞打开 + 写路径串行锁）。
pub struct Databases {
    base: PathBuf,
    file: DatabaseFile,
    /// `Some` = 按模型打开并增量同步表结构（与各项目现有行为一致）。
    model: Option<EntityModel>,
    /// 驱动分发管理器（需要时按 `[DriverStore]` 惰性构建；仅 driver-pack）。
    #[cfg(feature = "driver-pack")]
    manager: Mutex<Option<Arc<DriverManager>>>,
}

impl Databases {
    /// 构建容器（`model` 提供时：`Dal::open_with_model` + `sync_schema`）。
    pub fn new(base: &Path, file: DatabaseFile, model: Option<EntityModel>) -> Self {
        Self {
            base: base.to_path_buf(),
            file,
            model,
            #[cfg(feature = "driver-pack")]
            manager: Mutex::new(None),
        }
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
    pub fn connection_label(&self, name: &str) -> String {
        self.file
            .connections
            .get(name.trim())
            .map(|e| connection_label(e.connection_string()))
            .unwrap_or_else(|| "未知连接".to_string())
    }

    /// 打开（或复用）指定连接的共享存储。
    ///
    /// 需要驱动包的连接：按 `[DriverStore]` 分发（未配置组件源时给出明确错误）。
    pub fn store(&self, name: &str) -> Result<Arc<SharedStore>, String> {
        let name = name.trim();
        let entry = self.file.connections.get(name).ok_or_else(|| {
            format!(
                "连接不存在：{name}（可用：{}）",
                self.names().join(", ")
            )
        })?;
        let raw = resolve_connection(&self.base, entry.connection_string())?;
        let (store, _) = store::get_or_open_scoped(&self.base, name, || {
            let conn = self.prepare_conn(&raw)?;
            match &self.model {
                Some(model) => {
                    let dal = Dal::open_with_model(&conn, model.clone())
                        .map_err(|e| format!("打开数据库失败：{e}"))?;
                    dal.sync_schema()
                        .map_err(|e| format!("同步表结构失败：{e}"))?;
                    Ok(dal)
                }
                None => Dal::open(&conn).map_err(|e| format!("打开数据库失败：{e}")),
            }
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
    fn manager(&self) -> Result<Arc<DriverManager>, String> {
        let mut slot = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(existing));
        }
        let Some(spec) = self
            .file
            .driver_store
            .as_ref()
            .filter(|s| !s.url.trim().is_empty())
        else {
            return Err(format!(
                "连接需要驱动包（本程序未内置该驱动），但未配置驱动组件源：\
                 请在 {FILE_NAME} 增加 [DriverStore]（Url/PubKey）"
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
        assert_eq!(file.driver_store.as_ref().unwrap().url, "http://127.0.0.1:5502");
        assert_eq!(file.driver_store.as_ref().unwrap().pub_key, "abc");
        assert_eq!(
            connection_label("Server=x;Provider=MySQL"),
            "Provider=MySQL"
        );
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
        // 已是绝对路径：幂等
        let abs = resolve_connection(&dir, &out).unwrap();
        assert_eq!(abs, out);
        // 非 SQLite 原样；空串报错
        assert_eq!(
            resolve_connection(&dir, "Server=x;Provider=MySql").unwrap(),
            "Server=x;Provider=MySql"
        );
        assert!(resolve_connection(&dir, "  ").is_err());
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
}
