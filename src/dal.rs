//! 数据访问层：对应 DH.NCode 的 `DAL`（连接管理）与实体表操作。
//!
//! 职责：
//! - 解析 XCode 风格的连接串（`Data Source=..;Provider=SQLite;ShowSql=false`）
//! - 按模型同步数据库结构（建表 / 补列，对应 XCode 的反向工程与迁移）
//! - 提供实体表的增删改查（与 `SqlSession` 组合使用，会话可复用可独立）

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{Arc, Mutex, OnceLock},
};

use crate::cache::{EntityCache, SingleCache};
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::migration::Migration;
use crate::model::{EntityModel, TableMeta};
use crate::pool::{PoolOptions, PoolStats, SessionPool};
use crate::query::{Query, Where};
use crate::session::{DbRow, RowSet, SqlSession};
use crate::sqlbuild;
use crate::sqlite::SqliteSession;
use crate::value::DbValue;

/// 连接串：大小写无关的键值对（`key=value` 以 `;` 分隔）。
#[derive(Debug, Clone)]
pub struct ConnectionString {
    /// 原始连接串
    raw: String,
    /// 小写键 → (原键, 值)
    items: BTreeMap<String, (String, String)>,
}

impl ConnectionString {
    /// 解析连接串。
    pub fn parse(raw: &str) -> Self {
        let mut items = BTreeMap::new();
        for part in raw.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((key, value)) = part.split_once('=')
                && !key.trim().is_empty()
            {
                let key = key.trim();
                items.insert(
                    key.to_ascii_lowercase(),
                    (key.to_string(), value.trim().to_string()),
                );
            }
        }
        Self {
            raw: raw.to_string(),
            items,
        }
    }

    /// 原始连接串。
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// 设置项（插入或覆盖，键大小写不敏感，对齐 `ConnectionStringBuilder` 索引器）。
    /// <param name="key">键</param>
    /// <param name="value">值</param>
    pub fn set(&mut self, key: &str, value: &str) {
        self.items.insert(
            key.to_ascii_lowercase(),
            (key.to_string(), value.to_string()),
        );
    }

    /// 尝试添加项，已存在则失败（对齐 `TryAdd`）。
    /// <param name="key">键</param>
    /// <param name="value">值</param>
    /// <returns>是否添加成功</returns>
    pub fn try_add(&mut self, key: &str, value: &str) -> bool {
        if self.items.contains_key(&key.to_ascii_lowercase()) {
            return false;
        }
        self.set(key, value);
        true
    }

    /// 删除项（对齐 `Remove`）。
    /// <param name="key">键</param>
    /// <returns>是否存在并删除</returns>
    pub fn remove(&mut self, key: &str) -> bool {
        self.items.remove(&key.to_ascii_lowercase()).is_some()
    }

    /// 获取并删除项（对齐 `TryGetAndRemove`）。
    /// <param name="key">键</param>
    /// <returns>项的值（含空值时也返回，以对齐 C# 的 out 语义）；不存在时为 None</returns>
    pub fn try_get_and_remove(&mut self, key: &str) -> Option<String> {
        self.items
            .remove(&key.to_ascii_lowercase())
            .map(|(_, value)| value)
    }

    /// 重新组装连接串（小写键排序、保留键的原始大小写；对齐 `ConnectionString` 属性）。
    /// <returns>连接串文本</returns>
    pub fn to_connection_string(&self) -> String {
        self.items
            .values()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// 取值（忽略键大小写）。
    pub fn get(&self, key: &str) -> Option<&str> {
        match self.items.get(&key.to_ascii_lowercase()) {
            Some((_, v)) if !v.is_empty() => Some(v),
            _ => None,
        }
    }

    /// `provider=` 值。
    pub fn provider(&self) -> Option<&str> {
        self.get("provider")
    }

    /// 数据源（文件路径/数据库名），兼容多种写法。
    pub fn data_source(&self) -> Option<&str> {
        ["data source", "datasource", "filename", "file", "database"]
            .iter()
            .find_map(|k| self.get(k))
    }

    /// 是否开启 SQL 输出（`ShowSql=true`，与 XCode 行为一致）。
    pub fn show_sql(&self) -> bool {
        self.get("showsql")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    }

    /// 探测数据库类型：
    /// 1. 有 `provider` 时按其解析
    /// 2. 否则根据数据源后缀推断 SQLite（`.db`/`.sqlite`/`:memory:`）
    pub fn kind(&self) -> Result<DatabaseKind> {
        if let Some(provider) = self.provider() {
            return DatabaseKind::from_provider(provider);
        }

        if let Some(source) = self.data_source() {
            let lower = source.to_ascii_lowercase();
            if lower == ":memory:"
                || lower.ends_with(".db")
                || lower.ends_with(".sqlite")
                || lower.ends_with(".sqlite3")
                || lower.ends_with(".db3")
            {
                return Ok(DatabaseKind::Sqlite);
            }
        }

        Err(Error::Unsupported(
            "无法识别数据库类型，请在连接串中指定 provider=sqlite/mysql/sqlserver/postgresql/oracle".into(),
        ))
    }
}

/// 数据访问层入口。
pub struct Dal {
    /// 连接串
    conn_str: ConnectionString,
    /// 数据库类型
    kind: DatabaseKind,
    /// 数据模型（可选；结构迁移与表操作需要）
    model: Option<Arc<EntityModel>>,
    /// 是否输出执行的 SQL
    show_sql: bool,
    /// 迁移档位（生效值：连接串 > 模型 Option > 缺省 On，与 DH.NCode 一致）
    migration: Migration,
    /// 连接串显式指定的档位（用于与模型级配置的优先级判定）
    conn_migration: Option<Migration>,
    /// 实体缓存注册表（按表共享，对应 DH.NCode 的 `Meta.Cache`）
    pub(crate) entity_caches: Mutex<HashMap<String, Arc<EntityCache>>>,
    /// 单对象缓存注册表（按表共享，对应 DH.NCode 的 `Meta.SingleCache`）
    pub(crate) single_caches: Mutex<HashMap<String, Arc<SingleCache>>>,
    /// 会话连接池（懒创建；`Pooling=false` 时不使用）
    pool: OnceLock<Arc<SessionPool>>,
    /// 是否启用连接池
    pool_enabled: bool,
}

/// 直接创建会话（不走连接池；供 `Pooling=false` 与池工厂使用）。
fn create_session(kind: DatabaseKind, conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    // network：SQL 转发到远端 XCode/DbServer（远端类型由 `Dal::open` 登录探明）
    if crate::network::is_network(conn_str) {
        return Ok(Box::new(crate::network::NetworkSession::new(conn_str, kind)?));
    }
    match kind {
        DatabaseKind::Sqlite => {
            let path = conn_str.data_source().ok_or_else(|| {
                Error::Model("SQLite 连接串缺少 Data Source（数据库文件路径）".into())
            })?;
            Ok(Box::new(SqliteSession::open(path)?))
        }
        DatabaseKind::MySql => open_mysql(conn_str),
        DatabaseKind::SqlServer => open_sqlserver(conn_str),
        DatabaseKind::PostgreSql => open_postgres(conn_str),
        DatabaseKind::Oracle => open_oracle(conn_str),
        DatabaseKind::DuckDb => {
            #[cfg(feature = "duckdb")]
            {
                Ok(Box::new(crate::duckdb::DuckDbSession::open(conn_str)?))
            }
            #[cfg(not(feature = "duckdb"))]
            {
                Err(Error::Unsupported(
                    "DuckDB 驱动未随本次构建编译：请使用 `cargo build --features duckdb` 启用\
                     （内嵌 DuckDB 需要 CMake 构建，见 README）"
                        .into(),
                ))
            }
        }
        DatabaseKind::Firebird => open_firebird(conn_str),
        DatabaseKind::ClickHouse => open_clickhouse(conn_str),
        DatabaseKind::TDengine => open_tdengine(conn_str),
        DatabaseKind::InfluxDb => open_influxdb(conn_str),
        DatabaseKind::Hana => open_hana(conn_str),
        DatabaseKind::MongoDb => open_mongodb(conn_str),
        // ODBC 桥：DB2 / 达梦 / IRIS / Access 共用一套通用驱动
        DatabaseKind::Db2 | DatabaseKind::DaMeng | DatabaseKind::Iris | DatabaseKind::Access => {
            open_odbc(kind, conn_str)
        }
    }
}

// ———— 驱动打开辅助：未编译的驱动返回可操作的提示（对应 driver-* 特性）————

/// 未启用某驱动时的统一错误提示。
#[allow(dead_code)]
fn driver_missing(driver: &str, feature: &str) -> Error {
    Error::Unsupported(format!(
        "本构建未包含 {driver} 驱动：请在 Cargo.toml 启用 Pek.RCode 特性 `{feature}`（或 `all-drivers`）后重新构建"
    ))
}

#[cfg(feature = "driver-mysql")]
fn open_mysql(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::mysql::MysqlSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-mysql"))]
fn open_mysql(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("MySQL", "driver-mysql"))
}

#[cfg(feature = "driver-sqlserver")]
fn open_sqlserver(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::mssql::MssqlSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-sqlserver"))]
fn open_sqlserver(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("SQL Server", "driver-sqlserver"))
}

#[cfg(feature = "driver-postgresql")]
fn open_postgres(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::postgres::PostgresSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-postgresql"))]
fn open_postgres(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("PostgreSQL", "driver-postgresql"))
}

#[cfg(feature = "driver-oracle")]
fn open_oracle(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::oracle::OracleSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-oracle"))]
fn open_oracle(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("Oracle", "driver-oracle"))
}

#[cfg(feature = "driver-firebird")]
fn open_firebird(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::firebird::FirebirdSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-firebird"))]
fn open_firebird(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("Firebird", "driver-firebird"))
}

#[cfg(feature = "driver-clickhouse")]
fn open_clickhouse(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::clickhouse::ClickHouseSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-clickhouse"))]
fn open_clickhouse(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("ClickHouse", "driver-clickhouse"))
}

#[cfg(feature = "driver-tdengine")]
fn open_tdengine(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::tdengine::TDengineSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-tdengine"))]
fn open_tdengine(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("TDengine", "driver-tdengine"))
}

#[cfg(feature = "driver-influxdb")]
fn open_influxdb(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::influxdb::InfluxDbSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-influxdb"))]
fn open_influxdb(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("InfluxDB", "driver-influxdb"))
}

#[cfg(feature = "driver-hana")]
fn open_hana(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::hana::HanaSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-hana"))]
fn open_hana(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("SAP HANA", "driver-hana"))
}

#[cfg(feature = "driver-mongodb")]
fn open_mongodb(conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::mongodb::MongoSession::open(conn_str)?))
}
#[cfg(not(feature = "driver-mongodb"))]
fn open_mongodb(_conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("MongoDB", "driver-mongodb"))
}

#[cfg(feature = "driver-odbc")]
fn open_odbc(kind: DatabaseKind, conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Ok(Box::new(crate::odbc::OdbcSession::open(kind, conn_str)?))
}
#[cfg(not(feature = "driver-odbc"))]
fn open_odbc(_kind: DatabaseKind, _conn_str: &ConnectionString) -> Result<Box<dyn SqlSession>> {
    Err(driver_missing("ODBC（DB2/达梦/IRIS/Access）", "driver-odbc"))
}

impl Dal {
    /// 仅按连接串创建（不加载模型，不做任何连接）。
    pub fn open(conn_str: &str) -> Result<Self> {
        let conn_str = ConnectionString::parse(conn_str);
        // network 驱动：远端类型登录后才能确定（对齐 XCode Network 的 Login → RawType）
        let kind = if crate::network::is_network(&conn_str) {
            crate::network::probe_remote_kind(&conn_str)?
        } else {
            conn_str.kind()?
        };
        let show_sql = conn_str.show_sql();
        // 连接池默认开启；`Pooling=false` 关闭（对齐 XCode 的连接池默认行为）
        let pool_enabled = conn_str
            .get("pooling")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "no" | "off"
                )
            })
            .unwrap_or(true);
        // 迁移档位：连接串显式值优先（对应 DbBase 从连接串解析 Migration），否则默认 On
        //（与 XCodeSetting.Migration 默认值一致；模型级配置在 open_with_model 中补充）
        let conn_migration = conn_str.get("migration").and_then(Migration::parse);
        let migration = conn_migration.unwrap_or_default();
        Ok(Self {
            conn_str,
            kind,
            model: None,
            show_sql,
            migration,
            conn_migration,
            entity_caches: Mutex::new(HashMap::new()),
            single_caches: Mutex::new(HashMap::new()),
            pool: OnceLock::new(),
            pool_enabled,
        })
    }

    /// 创建并绑定数据模型（后续可执行建表迁移与表操作）。
    pub fn open_with_model(conn_str: &str, model: EntityModel) -> Result<Self> {
        let mut dal = Self::open(conn_str)?;
        // 模型级迁移档位：仅当连接串未显式指定时生效（对应 XCodeSetting.Migration）
        if dal.conn_migration.is_none()
            && let Some(m) = model.options.migration()
        {
            dal.migration = m;
        }
        dal.model = Some(Arc::new(model));
        Ok(dal)
    }

    /// 数据库类型。
    pub fn kind(&self) -> DatabaseKind {
        self.kind
    }

    /// 连接串。
    pub fn connection_string(&self) -> &ConnectionString {
        &self.conn_str
    }

    /// 数据模型。
    pub fn model(&self) -> Option<&Arc<EntityModel>> {
        self.model.as_ref()
    }

    /// 替换数据模型。
    pub fn set_model(&mut self, model: EntityModel) {
        self.model = Some(Arc::new(model));
    }

    /// 设置 SQL 输出开关（覆盖连接串中的 `ShowSql`）。
    pub fn set_show_sql(&mut self, value: bool) {
        self.show_sql = value;
    }

    /// 迁移档位（生效值）。
    ///
    /// 来源优先级：连接串 `Migration=...` > 模型 `<Option><Migration>...` > 缺省 [`Migration::On`]。
    /// 表级档位（`<Table Migration="...">`）只能在此基础上收紧（`min(表级, 全局)`）。
    /// <returns>生效档位</returns>
    pub fn migration(&self) -> Migration {
        self.migration
    }

    /// 设置迁移档位（覆盖连接串与模型配置）。
    /// <param name="migration">档位</param>
    pub fn set_migration(&mut self, migration: Migration) {
        self.migration = migration;
    }

    /// 输出一条 SQL（开启 ShowSql 时）。
    pub fn log_sql(&self, sql: &str) {
        if self.show_sql {
            println!("[SQL] {sql}");
        }
    }

    /// 打开数据库会话（默认走连接池；`Pooling=false` 时每次新建）。
    ///
    /// 连接池按连接串（当前 `Dal`）共享，对应 C# 的 `ConnectionPool`：
    /// 优先复用空闲会话；空闲超时（默认 30 秒）或调用出错的会话在归还时关闭。
    pub fn open_session(&self) -> Result<Box<dyn SqlSession>> {
        if !self.pool_enabled {
            return create_session(self.kind, &self.conn_str);
        }
        self.session_pool().checkout()
    }

    /// 取（或懒创建）会话池。
    fn session_pool(&self) -> Arc<SessionPool> {
        self.pool
            .get_or_init(|| {
                let kind = self.kind;
                let conn_str = self.conn_str.clone();
                let factory: crate::pool::SessionFactory =
                    Arc::new(move || create_session(kind, &conn_str));
                Arc::new(SessionPool::new(PoolOptions::default(), factory))
            })
            .clone()
    }

    /// 连接池统计（未启用池时返回缺省值）。
    pub fn pool_stats(&self) -> PoolStats {
        self.pool.get().map(|p| p.stats()).unwrap_or_default()
    }

    /// 清空连接池（关闭全部空闲连接；`Dal` 仍可继续使用）。
    ///
    /// 删除/移动 SQLite 数据库文件前可先调用，确保文件句柄已释放。
    pub fn clear_pool(&self) {
        if let Some(pool) = self.pool.get() {
            pool.clear();
        }
    }

    /// 是否启用连接池（连接串 `Pooling` 键，默认 true）。
    pub fn pooling_enabled(&self) -> bool {
        self.pool_enabled
    }

    /// 获取表操作句柄。
    pub fn table(&self, name: &str) -> Result<TableRef<'_>> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，请使用 open_with_model 或 set_model".into()))?;
        let table = model
            .table(name)
            .ok_or_else(|| Error::Model(format!("模型中不存在表/实体 {name}")))?;
        Ok(TableRef {
            dal: self,
            table,
            table_name: None,
        })
    }

    /// 获取表操作句柄（**指定物理表名**）：列元数据仍取模型定义，SQL 操作落到指定表。
    ///
    /// 用于分表场景（如 `Log2` → `Log2_20260927`，见 [`crate::shards`]）与手工别名表操作；
    /// 不会自动建表，分表建表用 [`Dal::ensure_shard_table`]。
    pub fn table_as(&self, name: &str, table_name: &str) -> Result<TableRef<'_>> {
        let mut handle = self.table(name)?;
        handle.table_name = Some(table_name.to_string());
        Ok(handle)
    }

    /// 创建分表物理表（不存在时），结构照抄模型表（含索引，单表 DDL 与 `sync_schema` 一致）。
    ///
    /// 对齐 C# `EntitySession.CheckTable` → `dal.SetTables` 的"新表名自动建表"行为：
    /// - 已存在 / 迁移档位为 `Off` / 只读档（`ReadOnly`）→ 返回 `false` 不做任何事；
    /// - `On` / `Full` → 建表 + 建索引，返回 `true`。
    pub fn ensure_shard_table(&self, name: &str, table_name: &str) -> Result<bool> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，无法创建分表".into()))?;
        let table = model
            .table(name)
            .ok_or_else(|| Error::Model(format!("模型中不存在表/实体 {name}")))?;

        // 时序/文档库无建表 DDL（写入时自动创建）
        if !self.kind.supports_ddl() {
            return Ok(false);
        }

        let mode = self.migration.tighten(table.migration);
        if mode == Migration::Off || mode.is_readonly() {
            return Ok(false);
        }

        let mut session = self.open_session()?;
        if session.table_exists(table_name)? {
            return Ok(false);
        }

        // 克隆表元数据并替换物理表名（对应 C# `table.Clone()` + `TableName = name`）
        let mut cloned = table.clone();
        cloned.table_name = table_name.to_string();

        for stmt in self.kind.create_table_sql(&cloned) {
            self.log_sql(&stmt);
            session.execute(&stmt, &[])?;
        }
        for index in &cloned.indexes {
            if let Some(sql) = self.kind.create_index_sql(&cloned, index) {
                self.log_sql(&sql);
                session.execute(&sql, &[])?;
            }
        }
        // 序列型自增（Oracle/DB2/Firebird/DuckDB）：补建 `SEQ_{表名}` 序列
        if cloned.identity().is_some()
            && matches!(
                self.kind,
                DatabaseKind::Oracle
                    | DatabaseKind::Db2
                    | DatabaseKind::Firebird
                    | DatabaseKind::DuckDb
            )
        {
            let mut report = SchemaReport {
                mode,
                ..Default::default()
            };
            self.ensure_identity_sequence(&mut *session, mode, &mut report, table_name)?;
        }
        Ok(true)
    }

    /// 按模型同步数据库结构（建表 / 补列 / 补索引；`Full` 档含修改与删除），返回本次变更清单。
    ///
    /// 档位语义（与 DH.NCode 的 [`Migration`] 一致）：
    /// - `Off`：跳过（返回空报告）
    /// - `ReadOnly`：只检查、不执行；将 DDL 收集到 [`SchemaReport::pending_sql`] 供人工执行
    /// - `On`（缺省）：只做创建类（建表 / 补列 / 补索引），不修改、不删除
    /// - `Full`：在 `On` 基础上允许修改列类型与删除多余列/索引（**删除类动作仅此档允许**）
    ///
    /// 表级档位（`<Table Migration="...">`）只能收紧、不能放大：生效档 = `min(表级, 全局)`。
    ///
    /// **network 连接默认不建表/改表**（对齐 C# `NetworkMetaData.OnSetTables` 空实现）；
    /// 由驱动组件分发的**自有远端库**（本机 dbserver 驱动宿主）请用
    /// [`Dal::sync_schema_including_network`] 显式启用。
    pub fn sync_schema(&self) -> Result<SchemaReport> {
        self.sync_schema_inner(false)
    }

    /// 同 [`Dal::sync_schema`]，但**对 network 连接也执行**结构同步（建表 / 补列 / 补索引）。
    ///
    /// 适用场景：`provider=network`（驱动组件自举的 dbserver 宿主）接入**全新空库**时
    /// 自动建表，对齐原生连接的 `SyncSchema` 行为；远端为共享/他人维护的数据库时请勿使用。
    pub fn sync_schema_including_network(&self) -> Result<SchemaReport> {
        self.sync_schema_inner(true)
    }

    /// 结构同步实现：`include_network = false` 时保持 C# 对齐的网络空实现。
    fn sync_schema_inner(&self, include_network: bool) -> Result<SchemaReport> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，无法同步结构".into()))?;

        let mut report = SchemaReport {
            mode: self.migration,
            ..Default::default()
        };

        // 时序/文档库无建表 DDL（measurement/collection 写入时自动创建）
        if !self.kind.supports_ddl() {
            return Ok(report);
        }

        // 网络库默认不在本端建表/改表（对齐 C# `NetworkMetaData.OnSetTables` 空实现：远端结构由远端维护）；
        // 自有远端库（dbserver 驱动宿主 + 空库初始化）经 sync_schema_including_network 显式放行
        if !include_network && crate::network::is_network(&self.conn_str) {
            return Ok(report);
        }

        // Off：完全跳过结构检查（对齐 C# `SetTables`：mode == Off 直接返回）
        if self.migration == Migration::Off {
            return Ok(report);
        }

        let mut session = self.open_session()?;

        for table in &model.tables {
            // 表级收紧：只能比全局更保守（对应 XCode ResolveMigration）
            let mode = self.migration.tighten(table.migration);
            if mode == Migration::Off {
                continue;
            }

            let table_name = table.effective_table_name();

            if !session.table_exists(table_name)? {
                for stmt in self.kind.create_table_sql(table) {
                    self.exec_ddl(&mut *session, mode, &mut report, &stmt)?;
                }
                report.created_tables.push(table_name.to_string());
                continue;
            }

            // 已存在的表：补齐缺失的列
            let existing = session.table_columns(table_name)?;
            for col in &table.columns {
                let col_name = table.effective_column_name(col);
                if !existing.iter().any(|c| c.eq_ignore_ascii_case(col_name)) {
                    let sql = self.kind.add_column_sql(table, col);
                    self.exec_ddl(&mut *session, mode, &mut report, &sql)?;
                    report
                        .added_columns
                        .push((table_name.to_string(), col_name.to_string()));
                }
            }

            // 既存表：补齐模型定义中缺失的索引（同列且同序即视为已存在，避免与其它工具命名差异重复建）
            if crate::catalog::supports_index_catalog(self.kind) {
                match crate::catalog::read_indexes(session.as_mut(), self.kind, table_name) {
                    Ok(existing_indexes) => {
                        for idx in &table.indexes {
                            if idx.columns.is_empty()
                                || existing_indexes
                                    .iter()
                                    .any(|e| same_index(&e.columns, &idx.columns))
                            {
                                continue;
                            }
                            if let Some(sql) = self.kind.create_index_sql(table, idx) {
                                self.exec_ddl(&mut *session, mode, &mut report, &sql)?;
                                report.added_indexes.push((
                                    table_name.to_string(),
                                    self.kind.index_name(table, idx),
                                ));
                            }
                        }
                    }
                    // 目录不可用：跳过索引补齐（不阻断结构同步）
                    Err(Error::Unsupported(_)) => {}
                    Err(e) => return Err(e),
                }
            }

            // 序列型自增（Oracle/DB2/Firebird/DuckDB）：补齐历史表缺失的序列
            if table.identity().is_some()
                && matches!(
                    self.kind,
                    DatabaseKind::Oracle
                        | DatabaseKind::Db2
                        | DatabaseKind::Firebird
                        | DatabaseKind::DuckDb
                )
            {
                self.ensure_identity_sequence(&mut *session, mode, &mut report, table_name)?;
            }
        }

        // Full 档：修改列类型 + 删除多余列/索引（删除类动作仅此档允许；对应 C# 的 onlyCreate=false 分支）
        if self.migration == Migration::Full {
            self.apply_full_changes(&mut report)?;
        }

        Ok(report)
    }

    /// 执行（或只读收集）一条 DDL：统一 ShowSql 输出与档位处理。
    /// <param name="session">会话</param>
    /// <param name="mode">该表生效档位</param>
    /// <param name="report">同步报告</param>
    /// <param name="sql">DDL 语句</param>
    fn exec_ddl(
        &self,
        session: &mut dyn SqlSession,
        mode: Migration,
        report: &mut SchemaReport,
        sql: &str,
    ) -> Result<()> {
        self.log_sql(sql);
        if mode.is_readonly() {
            // 只读档：不执行，收集“将执行”的 DDL 供人工处理（对齐 C# `DDL模式[ReadOnly]，请手工创建表`）
            report.pending_sql.push(sql.to_string());
        } else {
            session.execute(sql, &[])?;
        }
        Ok(())
    }

    /// `Full` 档的修改/删除执行：基于 [`Dal::diff_schema`] 的预检结果。
    ///
    /// 顺序与 XCode 一致：**先删多余索引，再删多余列**（否则索引引用会阻止删列），最后修改列类型。
    /// 说明：
    /// - 表级档位收紧到 `Full` 以下的表不动（只能收紧、不能放大）
    /// - 模型外的多余表**从不自动删除**（与 XCode 相同：迁移只处理模型中声明的表）
    /// - 单条失败不中断整体（记入 [`SchemaReport::notes`]，对齐 C# `CheckAllTables` 的逐表容错）
    fn apply_full_changes(&self, report: &mut SchemaReport) -> Result<()> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，无法同步结构".into()))?;
        let diff = self.diff_schema()?;
        if diff.extra_indexes.is_empty()
            && diff.extra_columns.is_empty()
            && diff.type_mismatches.is_empty()
        {
            return Ok(());
        }

        let mut session = self.open_session()?;

        // 该表生效档位须为 Full（表级只能收紧）
        let allowed = |tname: &str| -> bool {
            model
                .table(tname)
                .map(|t| self.migration.tighten(t.migration) == Migration::Full)
                .unwrap_or(false)
        };

        // 1) 删除多余索引（先删索引，后面才有可能删字段——对齐 XCode 的既有注释）
        for (tname, iname) in &diff.extra_indexes {
            if !allowed(tname) || is_primary_index_name(iname) {
                continue;
            }
            let Some(sql) = self.kind.drop_index_sql(iname, Some(tname)) else {
                report
                    .notes
                    .push(format!("{tname}.{iname}：该数据库不支持直接删除索引，请人工处理"));
                continue;
            };
            match self.exec_ddl(&mut *session, Migration::Full, report, &sql) {
                Ok(()) => report.dropped_indexes.push((tname.clone(), iname.clone())),
                Err(e) => report
                    .notes
                    .push(format!("{tname}.{iname}：删除索引失败（{e}）")),
            }
        }

        // 2) 删除多余列（数据库中存在、模型未声明的列）
        for (tname, cname) in &diff.extra_columns {
            if !allowed(tname) {
                continue;
            }
            let Some(sql) = self.kind.drop_column_sql(tname, cname) else {
                report
                    .notes
                    .push(format!("{tname}.{cname}：该数据库不支持直接删除列，请人工处理"));
                continue;
            };
            match self.exec_ddl(&mut *session, Migration::Full, report, &sql) {
                Ok(()) => report.dropped_columns.push((tname.clone(), cname.clone())),
                Err(e) => report
                    .notes
                    .push(format!("{tname}.{cname}：删除列失败（{e}）")),
            }
        }

        // 3) 修改列类型（仅基础类型不同时触发；长度/精度差异见 diff_schema 的宽松比较）
        for mismatch in &diff.type_mismatches {
            let tname = &mismatch.table;
            if !allowed(tname) {
                continue;
            }
            let Some(table) = model.table(tname) else {
                continue;
            };
            let Some(col) = table.column(&mismatch.column) else {
                continue;
            };
            let Some(sql) = self.kind.alter_column_sql(table, col) else {
                report.notes.push(format!(
                    "{tname}.{}：类型 {} → {} 需人工处理（该数据库不支持直接修改列类型，如 SQLite 需重建表）",
                    mismatch.column, mismatch.actual, mismatch.expected
                ));
                continue;
            };
            match self.exec_ddl(&mut *session, Migration::Full, report, &sql) {
                Ok(()) => report
                    .altered_columns
                    .push((tname.clone(), mismatch.column.clone())),
                Err(e) => report
                    .notes
                    .push(format!("{tname}.{}：修改列类型失败（{e}）", mismatch.column)),
            }
        }

        Ok(())
    }

    /// 结构比对：模型 vs 数据库（只读），并生成可直接执行的 ALTER 脚本（dry-run 输出）。
    ///
    /// 对应 DH.NCode 的迁移预检：缺失的表/列/索引会生成 `CREATE TABLE`/`ADD COLUMN`/
    /// `CREATE INDEX` 语句；多余的对象与类型差异（可能涉及数据变化）**只报告不生成 DDL**。
    /// 索引以“同列且同序”判定存在性（避免与其它工具的命名差异误报）。
    pub fn diff_schema(&self) -> Result<SchemaDiff> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，无法比对结构".into()))?;
        let mut diff = SchemaDiff::default();
        if !self.kind.supports_ddl() {
            return Ok(diff);
        }
        let mut session = self.open_session()?;

        match crate::catalog::read_tables(session.as_mut(), self.kind, None) {
            Ok(db_tables) => {
                for table in &model.tables {
                    let tname = table.effective_table_name();
                    match db_tables.iter().find(|t| t.name.eq_ignore_ascii_case(tname)) {
                        None => {
                            diff.missing_tables.push(tname.to_string());
                            diff.alter_sql.extend(self.kind.create_table_sql(table));
                        }
                        Some(db) => {
                            // 列：缺失 / 类型差异 / 多余
                            for col in &table.columns {
                                let cname = table.effective_column_name(col);
                                match db
                                    .columns
                                    .iter()
                                    .find(|c| c.name.eq_ignore_ascii_case(cname))
                                {
                                    None => {
                                        diff.missing_columns
                                            .push((tname.to_string(), cname.to_string()));
                                        diff.alter_sql.push(self.kind.add_column_sql(table, col));
                                    }
                                    Some(db_col) => {
                                        let expected = self.kind.field_type(col);
                                        if !same_base_type(&expected, &db_col.raw_type) {
                                            // `Full` 档专用：修改类预检脚本（dry-run 导出；执行前请确认数据影响）
                                            if let Some(sql) = self.kind.alter_column_sql(table, col) {
                                                diff.full_sql.push(sql);
                                            }
                                            diff.type_mismatches.push(ColumnTypeMismatch {
                                                table: tname.to_string(),
                                                column: cname.to_string(),
                                                expected,
                                                actual: db_col.raw_type.clone(),
                                            });
                                        }
                                    }
                                }
                            }
                            for db_col in &db.columns {
                                if table.column(&db_col.name).is_none() {
                                    // `Full` 档专用：删除类预检脚本
                                    if let Some(sql) = self.kind.drop_column_sql(tname, &db_col.name) {
                                        diff.full_sql.push(sql);
                                    }
                                    diff.extra_columns
                                        .push((tname.to_string(), db_col.name.clone()));
                                }
                            }
                            // 索引：缺失 / 多余（同列且同序视为一致）
                            if crate::catalog::supports_index_catalog(self.kind) {
                                for idx in &table.indexes {
                                    if idx.columns.is_empty()
                                        || db
                                            .indexes
                                            .iter()
                                            .any(|e| same_index(&e.columns, &idx.columns))
                                    {
                                        continue;
                                    }
                                    diff.missing_indexes.push((
                                        tname.to_string(),
                                        self.kind.index_name(table, idx),
                                    ));
                                    if let Some(sql) = self.kind.create_index_sql(table, idx) {
                                        diff.alter_sql.push(sql);
                                    }
                                }
                                for e in &db.indexes {
                                    let in_model = table
                                        .indexes
                                        .iter()
                                        .any(|idx| same_index(&idx.columns, &e.columns));
                                    if !in_model {
                                        // `Full` 档专用：删除类预检脚本（主键/自动索引不可删）
                                        if !is_primary_index_name(&e.name)
                                            && let Some(sql) =
                                                self.kind.drop_index_sql(&e.name, Some(tname))
                                        {
                                            diff.full_sql.push(sql);
                                        }
                                        diff.extra_indexes
                                            .push((tname.to_string(), e.name.clone()));
                                    }
                                }
                            }
                        }
                    }
                }
                for db in &db_tables {
                    if model.table(&db.name).is_none() {
                        diff.extra_tables.push(db.name.clone());
                    }
                }
            }
            Err(Error::Unsupported(_)) => {
                // 目录读取未覆盖的库：退回“存在性”级比对（表/列）
                for table in &model.tables {
                    let tname = table.effective_table_name();
                    if !session.table_exists(tname)? {
                        diff.missing_tables.push(tname.to_string());
                        diff.alter_sql.extend(self.kind.create_table_sql(table));
                        continue;
                    }
                    let existing = session.table_columns(tname)?;
                    for col in &table.columns {
                        let cname = table.effective_column_name(col);
                        if !existing.iter().any(|c| c.eq_ignore_ascii_case(cname)) {
                            diff.missing_columns
                                .push((tname.to_string(), cname.to_string()));
                            diff.alter_sql.push(self.kind.add_column_sql(table, col));
                        }
                    }
                }
            }
            Err(e) => return Err(e),
        }
        Ok(diff)
    }

    /// 补齐自增序列（XCode 约定 `SEQ_{表名}`）：不存在时创建。
    ///
    /// 不同数据库的序列目录不同（Oracle/DB2 的 `USER_SEQUENCES`、Firebird 的 `RDB$GENERATORS`、
    /// DuckDB 的 `duckdb_sequences()`），统一探测两种存储大小写。
    fn ensure_identity_sequence(
        &self,
        session: &mut dyn SqlSession,
        mode: Migration,
        report: &mut SchemaReport,
        table_name: &str,
    ) -> Result<()> {
        let sequence = crate::dialect::oracle_identity_sequence(table_name);
        let (probe, create) = match self.kind {
            DatabaseKind::Oracle => (
                "SELECT COUNT(*) FROM USER_SEQUENCES WHERE SEQUENCE_NAME IN (:1, :2)".to_string(),
                format!(
                    "CREATE SEQUENCE {} START WITH 1 INCREMENT BY 1 CACHE 20",
                    self.kind.quote(&sequence)
                ),
            ),
            DatabaseKind::Db2 => (
                "SELECT COUNT(*) FROM USER_SEQUENCES WHERE SEQUENCE_NAME = ? OR SEQUENCE_NAME = ?"
                    .to_string(),
                format!("CREATE SEQUENCE {sequence} START WITH 1 INCREMENT BY 1"),
            ),
            DatabaseKind::Firebird => (
                "SELECT COUNT(*) FROM RDB$GENERATORS WHERE RDB$GENERATOR_NAME = ? OR RDB$GENERATOR_NAME = ?"
                    .to_string(),
                format!("CREATE SEQUENCE {}", self.kind.quote(&sequence)),
            ),
            DatabaseKind::DuckDb => (
                "SELECT COUNT(*) FROM duckdb_sequences() WHERE sequence_name = ? OR sequence_name = ?"
                    .to_string(),
                format!("CREATE SEQUENCE {}", self.kind.quote(&sequence)),
            ),
            _ => return Ok(()),
        };

        let set = session.query(
            &probe,
            &[
                DbValue::Text(sequence.clone()),
                DbValue::Text(sequence.to_uppercase()),
            ],
        )?;
        let exists = set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0)
            > 0;
        if !exists {
            self.exec_ddl(session, mode, report, &create)?;
            report.created_sequences.push(sequence);
        }
        Ok(())
    }
}

/// 结构同步结果。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SchemaReport {
    /// 本次生效的迁移档位（对应 DH.NCode 的 `Db.Migration`）
    pub mode: Migration,
    /// 新建的表（只读档为“将新建”）
    pub created_tables: Vec<String>,
    /// 补充的列（表名, 列名）
    pub added_columns: Vec<(String, String)>,
    /// 补建的序列（Oracle 自增序列 SEQ_{表名}）
    pub created_sequences: Vec<String>,
    /// 补充的索引（表名, 索引名）
    pub added_indexes: Vec<(String, String)>,
    /// 修改的列（`Full` 档：表名, 列名）
    pub altered_columns: Vec<(String, String)>,
    /// 删除的多余列（`Full` 档：表名, 列名）
    pub dropped_columns: Vec<(String, String)>,
    /// 删除的多余索引（`Full` 档：表名, 索引名）
    pub dropped_indexes: Vec<(String, String)>,
    /// 只读档（`ReadOnly`）收集的“将执行”DDL，供人工执行
    pub pending_sql: Vec<String>,
    /// 无法自动处理项的说明（如某数据库不支持修改列类型）
    pub notes: Vec<String>,
}

impl SchemaReport {
    /// 是否没有任何变更（只读档含待执行 DDL 时同样视为有变更）。
    /// <returns>是否为空报告</returns>
    pub fn is_empty(&self) -> bool {
        self.created_tables.is_empty()
            && self.added_columns.is_empty()
            && self.created_sequences.is_empty()
            && self.added_indexes.is_empty()
            && self.altered_columns.is_empty()
            && self.dropped_columns.is_empty()
            && self.dropped_indexes.is_empty()
            && self.pending_sql.is_empty()
    }
}

impl fmt::Display for SchemaReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("数据库结构已是最新");
        }
        write!(
            f,
            "新建表 {} 张，补充列 {} 个，补充索引 {} 个",
            self.created_tables.len(),
            self.added_columns.len(),
            self.added_indexes.len()
        )?;
        if !self.altered_columns.is_empty() {
            write!(f, "，修改列 {} 个", self.altered_columns.len())?;
        }
        if !self.dropped_columns.is_empty() {
            write!(f, "，删除列 {} 个", self.dropped_columns.len())?;
        }
        if !self.dropped_indexes.is_empty() {
            write!(f, "，删除索引 {} 个", self.dropped_indexes.len())?;
        }
        if !self.pending_sql.is_empty() {
            write!(
                f,
                "（只读档：另有 {} 条待执行 DDL 见 pending_sql）",
                self.pending_sql.len()
            )?;
        }
        if !self.created_tables.is_empty() {
            write!(f, "；新建：{}", self.created_tables.join(", "))?;
        }
        if !self.added_columns.is_empty() {
            let list: Vec<String> = self
                .added_columns
                .iter()
                .map(|(t, c)| format!("{t}.{c}"))
                .collect();
            write!(f, "；补列：{}", list.join(", "))?;
        }
        if !self.added_indexes.is_empty() {
            let list: Vec<String> = self
                .added_indexes
                .iter()
                .map(|(t, i)| format!("{t}.{i}"))
                .collect();
            write!(f, "；补索引：{}", list.join(", "))?;
        }
        if !self.created_sequences.is_empty() {
            write!(
                f,
                "；补建序列：{}",
                self.created_sequences.join(", ")
            )?;
        }
        if !self.altered_columns.is_empty() {
            let list: Vec<String> = self
                .altered_columns
                .iter()
                .map(|(t, c)| format!("{t}.{c}"))
                .collect();
            write!(f, "；改列：{}", list.join(", "))?;
        }
        if !self.dropped_columns.is_empty() {
            let list: Vec<String> = self
                .dropped_columns
                .iter()
                .map(|(t, c)| format!("{t}.{c}"))
                .collect();
            write!(f, "；删列：{}", list.join(", "))?;
        }
        if !self.dropped_indexes.is_empty() {
            let list: Vec<String> = self
                .dropped_indexes
                .iter()
                .map(|(t, i)| format!("{t}.{i}"))
                .collect();
            write!(f, "；删索引：{}", list.join(", "))?;
        }
        for note in &self.notes {
            write!(f, "；注意：{note}")?;
        }
        Ok(())
    }
}

/// 结构差异（对应 DH.NCode 的迁移预检；`alter_sql` 为 dry-run 导出的可执行脚本）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaDiff {
    /// 模型有、数据库缺的表
    pub missing_tables: Vec<String>,
    /// 数据库有、模型没声明的表
    pub extra_tables: Vec<String>,
    /// 缺失的列（表名, 列名）
    pub missing_columns: Vec<(String, String)>,
    /// 多余的列（表名, 列名）
    pub extra_columns: Vec<(String, String)>,
    /// 缺失的索引（表名, 索引名）
    pub missing_indexes: Vec<(String, String)>,
    /// 多余的索引（表名, 索引名）
    pub extra_indexes: Vec<(String, String)>,
    /// 类型差异（只报告，不生成 DDL）
    pub type_mismatches: Vec<ColumnTypeMismatch>,
    /// 可直接执行的补齐类脚本（建表 / 加列 / 建索引；任何非 `Off` 档均可用）
    pub alter_sql: Vec<String>,
    /// 修改/删除类脚本（**仅 `Full` 档可执行**：改列类型、删多余列/索引；dry-run 预览用，执行前请确认数据影响）
    pub full_sql: Vec<String>,
}

impl SchemaDiff {
    /// 是否完全一致（无差异）。
    pub fn is_empty(&self) -> bool {
        self.missing_tables.is_empty()
            && self.extra_tables.is_empty()
            && self.missing_columns.is_empty()
            && self.extra_columns.is_empty()
            && self.missing_indexes.is_empty()
            && self.extra_indexes.is_empty()
            && self.type_mismatches.is_empty()
    }
}

/// 列类型差异（模型预期 vs 数据库实际）。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnTypeMismatch {
    /// 表名
    pub table: String,
    /// 列名
    pub column: String,
    /// 模型预期类型（按方言生成）
    pub expected: String,
    /// 数据库实际原始类型
    pub actual: String,
}

/// 是否为主键/自动索引名（不允许删除：SQLite 的 `sqlite_autoindex_*`、PostgreSQL 的 `*_pkey`、
/// MySQL/SQLServer 的 `PRIMARY` 等——这些由主键约束隐式维护）。
fn is_primary_index_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("sqlite_autoindex")
        || lower.ends_with("_pkey")
        || lower == "primary"
        || lower.starts_with("primary_key")
}

/// 索引列集合是否一致（同列且同序，忽略大小写）。
fn same_index(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// 宽松比较列类型：只比较基础类型名（忽略长度/精度/大小写），并处理常见等价别名。
fn same_base_type(expected: &str, actual: &str) -> bool {
    /// 取类型主名：去掉括号参数与后续修饰（如 `timestamp without time zone` → `timestamp`）。
    fn base(text: &str) -> String {
        let lower = text.trim().to_ascii_lowercase();
        lower
            .split(['(', ' '])
            .next()
            .unwrap_or("")
            .trim_end_matches(')')
            .to_string()
    }
    let (e, a) = (base(expected), base(actual));
    if e == a {
        return true;
    }
    matches!(
        (e.as_str(), a.as_str()),
        ("integer", "int")
            | ("int", "integer")
            | ("serial", "integer")
            | ("serial", "int4")
            | ("serial8", "bigint")
            | ("serial8", "int8")
            | ("boolean", "bool")
            | ("bool", "boolean")
            | ("single", "float4")
            | ("float4", "single")
            | ("double", "float8")
            | ("float8", "double")
            | ("double", "float")
            | ("varchar", "character")
            | ("character", "varchar")
            | ("nvarchar", "varchar")
            | ("varchar", "nvarchar")
            | ("nvarchar", "character")
            | ("character", "nvarchar")
            | ("datetime", "timestamp")
            | ("timestamp", "datetime")
            | ("decimal", "numeric")
            | ("numeric", "decimal")
            | ("blob", "bytea")
            | ("bytea", "blob")
    )
}

/// 表操作句柄（绑定模型中的某张表）。
pub struct TableRef<'a> {
    /// 所属数据访问层
    dal: &'a Dal,
    /// 表定义
    table: &'a TableMeta,
    /// 物理表名覆盖（分表场景：模型表 → 分表物理表；None 时用模型的物理名）
    table_name: Option<String>,
}

impl<'a> TableRef<'a> {
    /// 表定义。
    pub fn meta(&self) -> &'a TableMeta {
        self.table
    }

    /// 实际操作的物理表名（分表覆盖优先，否则取模型物理名）。
    pub fn physical_name(&self) -> &str {
        self.table_name
            .as_deref()
            .unwrap_or_else(|| self.table.effective_table_name())
    }

    /// 所属数据访问层。
    pub(crate) fn dal(&self) -> &'a Dal {
        self.dal
    }

    /// 插入一行，返回自增主键（无自增列时返回 0）。
    ///
    /// 自增回写策略按数据库区分：
    /// - PostgreSQL 系：`INSERT ... RETURNING 列`（与 DH.NCode 的 `RETURNING *` 一致），直接回读
    /// - Oracle：插入语句携带 `SEQ_{表名}.NEXTVAL`（由 [`sqlbuild::insert_sql`] 注入），随后读取序列 CURRVAL
    /// - 其余：插入后通过会话读取自增函数（`last_insert_rowid()` / `LAST_INSERT_ID()` / `SCOPE_IDENTITY()`）
    pub fn insert(&self, session: &mut dyn SqlSession, fields: &[(&str, DbValue)]) -> Result<i64> {
        let identity = self.table.identity();
        // 拦截器补全审计字段（对应实体拦截器 OnValid）
        let values = crate::interceptor::prepare(
            self.table,
            crate::interceptor::DataMethod::Insert,
            fields,
        );
        let fields: Vec<(&str, DbValue)> =
            values.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let (mut sql, params) =
            sqlbuild::insert_sql_named(self.dal.kind, self.table, self.physical_name(), &fields)?;

        if let Some(id_col) = identity
            && matches!(
                self.dal.kind,
                DatabaseKind::PostgreSql | DatabaseKind::DuckDb
            )
        {
            sql.push_str(" RETURNING ");
            sql.push_str(&self.dal.kind.quote(self.table.effective_column_name(id_col)));
            self.dal.log_sql(&sql);
            let set = session.query(&sql, &params)?;
            // 写入使缓存失效（对应 DH.NCode：任何添删改都让缓存马上过期）
            self.dal.invalidate_cache(self.physical_name());
            return Ok(set
                .first()
                .and_then(|row| row.get(0))
                .and_then(DbValue::as_i64)
                .unwrap_or(0));
        }

        self.dal.log_sql(&sql);
        let id = if identity.is_some() {
            session.insert_and_get_identity(&sql, &params, Some(self.physical_name()))?
        } else {
            session.execute(&sql, &params)?;
            0
        };
        // 写入使缓存失效（对应 DH.NCode：任何添删改都让缓存马上过期）
        self.dal.invalidate_cache(self.physical_name());
        Ok(id)
    }

    /// 按主键查找（主键值按 `TableMeta::primary_keys()` 顺序传入）。
    pub fn find_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<Option<DbRow>> {
        let filter = self.pk_filter(pk)?;
        let query = Query::new().filter(filter).take(1);
        let (sql, params) =
            sqlbuild::select_sql_named(self.dal.kind, self.table, self.physical_name(), &query);
        self.dal.log_sql(&sql);
        let set = session.query(&sql, &params)?;
        Ok(set.rows.into_iter().next())
    }

    /// 按主键更新，返回受影响行数。
    pub fn update_by_pk(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&str, DbValue)],
        pk: &[DbValue],
    ) -> Result<u64> {
        let filter = self.pk_filter(pk)?;
        // 拦截器刷新审计字段（对应实体拦截器 OnValid）
        let values = crate::interceptor::prepare(
            self.table,
            crate::interceptor::DataMethod::Update,
            sets,
        );
        let sets: Vec<(&str, DbValue)> =
            values.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let (sql, params) = sqlbuild::update_sql_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            &sets,
            &filter,
        )?;
        self.dal.log_sql(&sql);
        let affected = session.execute(&sql, &params)?;
        if affected > 0 {
            // 写入使缓存失效（对应 DH.NCode：任何添删改都让缓存马上过期）
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// 按主键删除，返回受影响行数。
    pub fn delete_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<u64> {
        let filter = self.pk_filter(pk)?;
        self.delete_where(session, &filter)
    }

    /// 按条件删除，返回受影响行数（空条件会清空全表，请先用 [`Where::is_empty`] 防护）。
    ///
    /// 对应 C# `Entity.Delete(Expression)` 的单表部分；分表删除见 [`TableRef::delete_sharded`](crate::shards::TableRef::delete_sharded)。
    pub fn delete_where(&self, session: &mut dyn SqlSession, filter: &Where) -> Result<u64> {
        // 拦截器通知（对应 OnValid/Delete；默认拦截器不处理删除，保留扩展点）
        let mut notify: Vec<(String, DbValue)> = Vec::new();
        crate::interceptor::apply_registered(
            self.table,
            crate::interceptor::DataMethod::Delete,
            &mut notify,
        );
        let (sql, params) =
            sqlbuild::delete_sql_named(self.dal.kind, self.physical_name(), filter);
        self.dal.log_sql(&sql);
        let affected = session.execute(&sql, &params)?;
        if affected > 0 {
            // 写入使缓存失效（对应 DH.NCode：任何添删改都让缓存马上过期）
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// 主键是否存在。
    pub fn exists_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<bool> {
        Ok(self.find_by_pk(session, pk)?.is_some())
    }

    /// 按保存模式保存一行数据（对齐 C# `Entity.Save(SaveModes)` 的核心语义）。
    ///
    /// - [`crate::data_access::SaveModes::Insert`]：直接插入；
    /// - [`crate::data_access::SaveModes::Upsert`]：主键已存在则按主键更新，否则插入；
    /// - [`crate::data_access::SaveModes::InsertIgnore`]：主键已存在则忽略（返回 0）；
    /// - [`crate::data_access::SaveModes::Replace`]：主键已存在则先删除再插入。
    ///
    /// 非插入模式下 `fields` 必须包含全部主键列；写入路径中的拦截器刷新与缓存失效由
    /// [`TableRef::insert`]/[`TableRef::update_by_pk`]/[`TableRef::delete_by_pk`] 各自处理。
    /// <param name="session">数据库会话</param>
    /// <param name="fields">字段值集合</param>
    /// <param name="mode">保存模式</param>
    /// <returns>受影响行数</returns>
    pub fn save(
        &self,
        session: &mut dyn SqlSession,
        fields: &[(&str, DbValue)],
        mode: crate::data_access::SaveModes,
    ) -> Result<u64> {
        use crate::data_access::SaveModes;
        if mode == SaveModes::Insert {
            self.insert(session, fields)?;
            return Ok(1);
        }

        let pk = self.pk_values_from_fields(fields)?;
        match mode {
            SaveModes::Insert => unreachable!(),
            SaveModes::Upsert => {
                if self.exists_by_pk(session, &pk)? {
                    self.update_by_pk(session, fields, &pk)
                } else {
                    self.insert(session, fields)?;
                    Ok(1)
                }
            }
            SaveModes::InsertIgnore => {
                if self.exists_by_pk(session, &pk)? {
                    Ok(0)
                } else {
                    self.insert(session, fields)?;
                    Ok(1)
                }
            }
            SaveModes::Replace => {
                if self.exists_by_pk(session, &pk)? {
                    self.delete_by_pk(session, &pk)?;
                }
                self.insert(session, fields)?;
                Ok(1)
            }
        }
    }

    /// 从字段集中按主键顺序提取主键值（缺失时报错）。
    fn pk_values_from_fields(&self, fields: &[(&str, DbValue)]) -> Result<Vec<DbValue>> {
        let keys = self.table.primary_keys();
        if keys.is_empty() {
            return Err(Error::Model(format!(
                "表 {} 没有主键，无法按主键保存",
                self.table.name
            )));
        }
        let mut values = Vec::with_capacity(keys.len());
        for key in keys.iter() {
            let value = fields
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&key.name))
                .map(|(_, value)| value.clone());
            match value {
                Some(v) => values.push(v),
                None => {
                    return Err(Error::Argument(format!(
                        "表 {} 缺少主键列 {} 的值",
                        self.table.name, key.name
                    )));
                }
            }
        }
        Ok(values)
    }

    /// 统计行数。
    pub fn count(&self, session: &mut dyn SqlSession, filter: Option<&Where>) -> Result<i64> {
        let (sql, params) =
            sqlbuild::count_sql_named(self.dal.kind, self.physical_name(), filter);
        self.dal.log_sql(&sql);
        let set = session.query(&sql, &params)?;
        Ok(set
            .first()
            .and_then(|r| r.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0))
    }

    /// 查询。
    pub fn query(&self, session: &mut dyn SqlSession, query: &Query) -> Result<RowSet> {
        let (sql, params) =
            sqlbuild::select_sql_named(self.dal.kind, self.table, self.physical_name(), query);
        self.dal.log_sql(&sql);
        session.query(&sql, &params)
    }

    /// 组装主键过滤条件。
    fn pk_filter(&self, pk: &[DbValue]) -> Result<Where> {
        let keys = self.table.primary_keys();
        if keys.is_empty() {
            return Err(Error::Model(format!(
                "表 {} 没有主键，无法按主键操作",
                self.table.name
            )));
        }
        if keys.len() != pk.len() {
            return Err(Error::Model(format!(
                "表 {} 主键需要 {} 个值，实际传入 {} 个",
                self.table.name,
                keys.len(),
                pk.len()
            )));
        }

        let mut filter = Where::new();
        for (key, value) in keys.iter().zip(pk.iter()) {
            filter = filter.eq(key.name.clone(), value.clone());
        }
        Ok(filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order" Description="订单">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
        <Column Name="Status" DataType="Int32" />
        <Column Name="CreateTime" DataType="DateTime" />
      </Columns>
      <Indexes><Index Columns="Code" Unique="True" /></Indexes>
    </Table></Tables></EntityModel>"#;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let stamp = chrono::Local::now().format("%H%M%S%.6f").to_string().replace('.', "");
        let dir = std::env::temp_dir().join(format!("rcode-{}-{}-{name}", std::process::id(), stamp));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn connection_string_parsing() {
        let cs = ConnectionString::parse("Data Source=..\\..\\Data\\DG.db;ShowSql=false;Provider=SQLite");
        assert_eq!(cs.kind().unwrap(), DatabaseKind::Sqlite);
        assert_eq!(cs.data_source(), Some("..\\..\\Data\\DG.db"));
        assert!(!cs.show_sql());
        assert_eq!(cs.get("PROVIDER"), Some("SQLite"));

        let cs = ConnectionString::parse("Server=localhost;Port=3307;Database=mes;Uid=root;Pwd=123456;provider=mysql");
        assert_eq!(cs.kind().unwrap(), DatabaseKind::MySql);

        // 无 provider 但后缀为 .db → SQLite
        let cs = ConnectionString::parse("Data Source=demo.sqlite");
        assert_eq!(cs.kind().unwrap(), DatabaseKind::Sqlite);

        // 无法识别时给出明确错误
        assert!(ConnectionString::parse("Server=x").kind().is_err());
    }

    #[test]
    fn sync_schema_creates_and_extends() {
        let dir = temp_dir("sync");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert_eq!(report.created_tables, vec!["DH_Order"]);
        assert!(report.added_columns.is_empty());

        // 再次同步应无变更
        assert!(dal.sync_schema().unwrap().is_empty());

        // 模型新增一列 → 补列
        let mut model = EntityModel::parse(MODEL).unwrap();
        model.tables[0].columns.push(crate::model::ColumnMeta {
            name: "Remark".into(),
            column_name: None,
            data_type: crate::types::DataType::String,
            raw_type: None,
            length: 100,
            precision: 0,
            scale: 0,
            identity: false,
            primary_key: false,
            master: false,
            nullable: true,
            default_value: None,
            description: String::new(),
            enum_type: None,
            data_scale: None,
            map: None,
            show_in: None,
            model: None,
        });
        let dal2 = Dal::open_with_model(&conn, model).unwrap();
        let report = dal2.sync_schema().unwrap();
        assert_eq!(report.added_columns, vec![("DH_Order".to_string(), "Remark".to_string())]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_schema_adds_missing_index() {
        let dir = temp_dir("index");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();

        // 手动删掉索引后再次同步：应补建
        let mut session = dal.open_session().unwrap();
        session
            .execute("DROP INDEX \"ix_DH_Order_Code\"", &[])
            .unwrap();
        drop(session);

        let report = dal.sync_schema().unwrap();
        assert_eq!(
            report.added_indexes,
            vec![("DH_Order".to_string(), "ix_DH_Order_Code".to_string())]
        );

        // 再次同步无变更
        assert!(dal.sync_schema().unwrap().is_empty());

        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_schema_reports_and_exports_alter() {
        let dir = temp_dir("diff");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();

        // 全新建库：缺表，导出建表 + 建索引脚本
        let diff = dal.diff_schema().unwrap();
        assert_eq!(diff.missing_tables, vec!["DH_Order"]);
        assert!(diff.alter_sql.iter().any(|s| s.starts_with("CREATE TABLE")));
        assert!(
            diff.alter_sql
                .iter()
                .any(|s| s.starts_with("CREATE UNIQUE INDEX"))
        );

        // 同步后无差异
        dal.sync_schema().unwrap();
        let diff = dal.diff_schema().unwrap();
        assert!(diff.is_empty(), "同步后应无差异：{diff:?}");

        // 手工制造差异：删索引、加多余列、加多余表
        let mut session = dal.open_session().unwrap();
        session
            .execute("DROP INDEX \"ix_DH_Order_Code\"", &[])
            .unwrap();
        session
            .execute("ALTER TABLE \"DH_Order\" ADD COLUMN \"Extra\" text", &[])
            .unwrap();
        session
            .execute("CREATE TABLE \"DH_Other\" (\"X\" int)", &[])
            .unwrap();
        drop(session);

        let diff = dal.diff_schema().unwrap();
        assert_eq!(
            diff.missing_indexes,
            vec![("DH_Order".to_string(), "ix_DH_Order_Code".to_string())]
        );
        assert_eq!(
            diff.extra_columns,
            vec![("DH_Order".to_string(), "Extra".to_string())]
        );
        assert_eq!(diff.extra_tables, vec!["DH_Other"]);
        assert!(
            diff.alter_sql
                .iter()
                .any(|s| s.contains("CREATE UNIQUE INDEX"))
        );

        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_schema_detects_type_mismatch() {
        const MODEL2: &str = r#"<EntityModel><Tables><Table Name="T2" TableName="DH_T2">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="50" />
          </Columns>
        </Table></Tables></EntityModel>"#;

        let dir = temp_dir("diff-type");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        // 手工建一张与模型类型不符的表（Code 为 int，模型为 nvarchar(50)）
        let setup = Dal::open(&conn).unwrap();
        let mut session = setup.open_session().unwrap();
        session
            .execute(
                "CREATE TABLE \"DH_T2\" (\"Id\" integer PRIMARY KEY AUTOINCREMENT, \"Code\" int)",
                &[],
            )
            .unwrap();
        drop(session);
        setup.clear_pool();
        drop(setup);

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL2).unwrap()).unwrap();
        let diff = dal.diff_schema().unwrap();
        assert_eq!(diff.type_mismatches.len(), 1, "{diff:?}");
        let m = &diff.type_mismatches[0];
        assert_eq!(m.table, "DH_T2");
        assert_eq!(m.column, "Code");
        assert_eq!(m.expected, "nvarchar(50)");
        // 注意：SQLite 的 pragma_table_info 会把声明类型规范化为大写（int → INT）
        assert!(m.actual.eq_ignore_ascii_case("int"), "diff={diff:?}");
        // 类型差异只报告，不生成 ALTER
        assert!(diff.alter_sql.is_empty());

        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_off_skips_and_readonly_collects() {
        let dir = temp_dir("migration-off");
        let db = dir.join("test.db");

        // Off：完全跳过——新库不会建表（对齐 C# `SetTables`：mode == Off 直接返回）
        let conn = format!("Data Source={};Provider=SQLite;Migration=Off", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        assert_eq!(dal.migration(), Migration::Off);
        let report = dal.sync_schema().unwrap();
        assert!(report.is_empty());
        assert_eq!(report.mode, Migration::Off);
        let mut session = dal.open_session().unwrap();
        assert!(!session.table_exists("DH_Order").unwrap(), "Off 档不应建表");
        drop(session);
        dal.clear_pool();
        drop(dal);

        // ReadOnly：只收集待执行 DDL，不执行（对齐 C# `DDL模式[ReadOnly]，请手工创建表`）
        let conn = format!("Data Source={};Provider=SQLite;Migration=ReadOnly", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        assert_eq!(dal.migration(), Migration::ReadOnly);
        let report = dal.sync_schema().unwrap();
        assert_eq!(report.created_tables, vec!["DH_Order"], "只读档应报告“将新建”");
        assert!(
            report.pending_sql.iter().any(|s| s.starts_with("CREATE TABLE")),
            "只读档应收集待执行 DDL：{report:?}"
        );
        let mut session = dal.open_session().unwrap();
        assert!(!session.table_exists("DH_Order").unwrap(), "只读档不应执行 DDL");
        drop(session);
        dal.clear_pool();
        drop(dal);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_option_and_table_level_migration() {
        let dir = temp_dir("migration-levels");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        // 模型级 Migration 作为缺省（对应 XCodeSetting.Migration）
        let mut model = EntityModel::parse(MODEL).unwrap();
        model.options.raw.insert("Migration".into(), "ReadOnly".into());
        let dal = Dal::open_with_model(&conn, model).unwrap();
        assert_eq!(dal.migration(), Migration::ReadOnly, "模型级档位应生效");
        drop(dal);

        // 连接串显式指定优先于模型级（对应 DbBase 从连接串解析 Migration）
        let conn2 = format!("Data Source={};Provider=SQLite;Migration=Off", db.display());
        let mut model = EntityModel::parse(MODEL).unwrap();
        model.options.raw.insert("Migration".into(), "ReadOnly".into());
        let dal = Dal::open_with_model(&conn2, model).unwrap();
        assert_eq!(dal.migration(), Migration::Off, "连接串优先于模型级");

        // 表级只能收紧、不能放大：全局 Off + 表级 Full → 实际 Off
        let xml = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order" Migration="Full">
          <Columns><Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" /></Columns>
        </Table></Tables></EntityModel>"#;
        let model = EntityModel::parse(xml).unwrap();
        assert_eq!(model.tables[0].migration, Some(Migration::Full));
        assert_eq!(
            dal.migration().tighten(model.tables[0].migration),
            Migration::Off,
            "表级只能收紧"
        );

        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn table_level_off_blocks_schema_sync() {
        let dir = temp_dir("migration-table");
        let db = dir.join("test.db");
        // 全局 Full + 表级 Off：该表完全跳过
        let xml = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order" Migration="Off">
          <Columns><Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" /></Columns>
        </Table></Tables></EntityModel>"#;
        let conn = format!("Data Source={};Provider=SQLite;Migration=Full", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(xml).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert!(report.is_empty(), "表级 Off 不应做任何变更：{report:?}");
        let mut session = dal.open_session().unwrap();
        assert!(!session.table_exists("DH_Order").unwrap(), "表级 Off 不应建表");
        drop(session);
        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_drops_extra_column_and_index_but_keeps_other_tables() {
        let dir = temp_dir("migration-full");
        let db = dir.join("test.db");
        let plain = format!("Data Source={};Provider=SQLite", db.display());

        // 先 On 档建库
        let setup = Dal::open_with_model(&plain, EntityModel::parse(MODEL).unwrap()).unwrap();
        setup.sync_schema().unwrap();
        setup.clear_pool();
        drop(setup);

        // 手工制造“多余物”：多余列 Extra、多余索引 ix_extra、模型外表 DH_Other
        let raw = Dal::open(&plain).unwrap();
        let mut session = raw.open_session().unwrap();
        session
            .execute("ALTER TABLE \"DH_Order\" ADD COLUMN \"Extra\" text", &[])
            .unwrap();
        session
            .execute("CREATE INDEX \"ix_extra\" ON \"DH_Order\" (\"Status\")", &[])
            .unwrap();
        session
            .execute("CREATE TABLE \"DH_Other\" (\"X\" int)", &[])
            .unwrap();
        drop(session);
        raw.clear_pool();
        drop(raw);

        // Full 档同步：删多余列与索引；模型外表保留（对齐 XCode：从不触碰模型未声明的表）
        let conn = format!("Data Source={};Provider=SQLite;Migration=Full", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert_eq!(report.mode, Migration::Full);
        assert_eq!(
            report.dropped_columns,
            vec![("DH_Order".to_string(), "Extra".to_string())],
            "{report:?}"
        );
        assert!(
            report
                .dropped_indexes
                .contains(&("DH_Order".to_string(), "ix_extra".to_string())),
            "{report:?}"
        );
        assert_eq!(report.dropped_indexes.len(), 1, "{report:?}");
        assert!(report.notes.is_empty(), "{report:?}");

        // 库中确认
        let mut session = dal.open_session().unwrap();
        let cols = session.table_columns("DH_Order").unwrap();
        assert!(!cols.iter().any(|c| c.eq_ignore_ascii_case("Extra")));
        assert!(session.table_exists("DH_Other").unwrap(), "模型外的表从不自动删除");
        drop(session);

        // 再次同步无变更
        assert!(dal.sync_schema().unwrap().is_empty());

        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_reports_unsupported_column_type_change() {
        const TT: &str = r#"<EntityModel><Tables><Table Name="T2" TableName="DH_T2">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="50" />
          </Columns>
        </Table></Tables></EntityModel>"#;

        let dir = temp_dir("migration-full-type");
        let db = dir.join("test.db");
        let plain = format!("Data Source={};Provider=SQLite", db.display());

        // 手工建一张与模型类型不符的表（Code 为 int，模型为 nvarchar(50)）
        let setup = Dal::open(&plain).unwrap();
        let mut session = setup.open_session().unwrap();
        session
            .execute(
                "CREATE TABLE \"DH_T2\" (\"Id\" integer PRIMARY KEY AUTOINCREMENT, \"Code\" int)",
                &[],
            )
            .unwrap();
        drop(session);
        setup.clear_pool();
        drop(setup);

        // Full 档：SQLite 不支持直接改列类型 → 记入 notes 并继续，不中断
        let conn = format!("Data Source={};Provider=SQLite;Migration=Full", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(TT).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert!(report.altered_columns.is_empty());
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("不支持直接修改列类型")),
            "{report:?}"
        );

        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn table_crud_roundtrip() {
        let dir = temp_dir("crud");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        let table = dal.table("Order").unwrap();
        let mut session = dal.open_session().unwrap();

        // 插入
        let id = table
            .insert(
                session.as_mut(),
                &[
                    ("Code", "HLT-001".into()),
                    ("Status", 1.into()),
                    ("CreateTime", chrono::NaiveDate::from_ymd_opt(2026, 9, 26).unwrap().and_hms_opt(18, 0, 0).unwrap().into()),
                ],
            )
            .unwrap();
        assert!(id > 0, "应返回自增主键");

        // 查询
        let row = table.find_by_pk(session.as_mut(), &[id.into()]).unwrap().unwrap();
        assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("HLT-001"));
        assert!(table.exists_by_pk(session.as_mut(), &[id.into()]).unwrap());

        // 更新
        let affected = table
            .update_by_pk(session.as_mut(), &[("Status", 9.into())], &[id.into()])
            .unwrap();
        assert_eq!(affected, 1);
        let row = table.find_by_pk(session.as_mut(), &[id.into()]).unwrap().unwrap();
        assert_eq!(row.get_by_name("Status").unwrap().as_i64(), Some(9));

        // 统计与条件查询
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 1);
        let filter = Where::new().eq("Status", 9);
        assert_eq!(table.count(session.as_mut(), Some(&filter)).unwrap(), 1);

        // 删除
        assert_eq!(table.delete_by_pk(session.as_mut(), &[id.into()]).unwrap(), 1);
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn table_save_modes_roundtrip() {
        let dir = temp_dir("save");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        let table = dal.table("Order").unwrap();
        let mut session = dal.open_session().unwrap();

        use crate::data_access::SaveModes;

        let created = chrono::NaiveDate::from_ymd_opt(2026, 9, 26)
            .unwrap()
            .and_hms_opt(18, 0, 0)
            .unwrap();

        // 先插入一行
        let id = table
            .insert(
                session.as_mut(),
                &[
                    ("Code", "S-001".into()),
                    ("Status", 1.into()),
                    ("CreateTime", created.into()),
                ],
            )
            .unwrap();
        let pk = [DbValue::Int(id)];

        // Upsert：存在则更新
        let affected = table
            .save(
                session.as_mut(),
                &[
                    ("Id", id.into()),
                    ("Code", "S-001".into()),
                    ("Status", 2.into()),
                    ("CreateTime", created.into()),
                ],
                SaveModes::Upsert,
            )
            .unwrap();
        assert_eq!(affected, 1);
        let row = table.find_by_pk(session.as_mut(), &pk).unwrap().unwrap();
        assert_eq!(row.get_by_name("Status").unwrap().as_i64(), Some(2));

        // InsertIgnore：存在则忽略
        let affected = table
            .save(
                session.as_mut(),
                &[
                    ("Id", id.into()),
                    ("Code", "S-001".into()),
                    ("Status", 9.into()),
                    ("CreateTime", created.into()),
                ],
                SaveModes::InsertIgnore,
            )
            .unwrap();
        assert_eq!(affected, 0);
        let row = table.find_by_pk(session.as_mut(), &pk).unwrap().unwrap();
        assert_eq!(row.get_by_name("Status").unwrap().as_i64(), Some(2));

        // Replace：删除后插入
        let affected = table
            .save(
                session.as_mut(),
                &[
                    ("Id", id.into()),
                    ("Code", "S-001".into()),
                    ("Status", 7.into()),
                    ("CreateTime", created.into()),
                ],
                SaveModes::Replace,
            )
            .unwrap();
        assert_eq!(affected, 1);
        let row = table.find_by_pk(session.as_mut(), &pk).unwrap().unwrap();
        assert_eq!(row.get_by_name("Status").unwrap().as_i64(), Some(7));

        // Upsert 主键不存在：插入
        let affected = table
            .save(
                session.as_mut(),
                &[
                    ("Id", 999.into()),
                    ("Code", "S-999".into()),
                    ("Status", 3.into()),
                    ("CreateTime", created.into()),
                ],
                SaveModes::Upsert,
            )
            .unwrap();
        assert_eq!(affected, 1);
        assert!(table.exists_by_pk(session.as_mut(), &[999.into()]).unwrap());

        // 缺少主键列时报错
        assert!(
            table
                .save(session.as_mut(), &[("Code", "X".into())], SaveModes::Upsert)
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
