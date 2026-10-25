//! 数据访问层：对应 DH.NCode 的 `DAL`（连接管理）与实体表操作。
//!
//! 职责：
//! - 解析 XCode 风格的连接串（`Data Source=..;Provider=SQLite;ShowSql=false`）
//! - 按模型同步数据库结构（建表 / 补列，对应 XCode 的反向工程与迁移）
//! - 提供实体表的增删改查（与 `SqlSession` 组合使用，会话可复用可独立）

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::cache::{EntityCache, SingleCache};
use crate::catalog::ColumnInfo;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::migration::Migration;
use crate::model::{ColumnMeta, EntityModel, TableMeta};
use crate::pool::{PoolOptions, PoolStats, SessionPool};
use crate::query::{Query, Where};
use crate::readwrite::ReadWriteStrategy;
use crate::session::{DbRow, RowSet, SqlSession};
use crate::sqlbuild;
use crate::sqlbuild::BatchWriteMode;
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
    /// 实体缓存注册表（按表共享，对应 DH.NCode 的 `Meta.Cache`；读多写少 → RwLock）
    pub(crate) entity_caches: RwLock<HashMap<String, Arc<EntityCache>>>,
    /// 单对象缓存注册表（按表共享，对应 DH.NCode 的 `Meta.SingleCache`）
    pub(crate) single_caches: RwLock<HashMap<String, Arc<SingleCache>>>,
    /// 字段缓存注册表（表（小写）→ 列（小写）→ 共享实例；对应 DH.NCode 的 `FieldCache<T>`）
    pub(crate) field_caches:
        RwLock<HashMap<String, HashMap<String, Arc<crate::cache::FieldCache>>>>,
    /// 是否注册过任一类缓存（写入路径快路：无缓存时零锁零分配）
    pub(crate) has_caches: AtomicBool,
    /// 表行数缓存（表名→（行数, 时刻）；对应 C# `EntitySession.Count` 的 60 秒缓存；读多写少用 RwLock）
    pub(crate) count_cache: RwLock<HashMap<String, (i64, std::time::Instant)>>,
    /// 模型表名索引（小写实体名/物理表名 → 表下标；避免每次操作线性扫 176 表）
    table_index: OnceLock<HashMap<String, usize>>,
    /// 时间分表规格（表名 → 规格；解析期算一次，未命中即"不分表"）
    shard_specs: OnceLock<HashMap<String, Arc<crate::model::ShardSpec>>>,
    /// 会话连接池（懒创建；`Pooling=false` 时不使用）
    pool: OnceLock<Arc<SessionPool>>,
    /// 是否启用连接池
    pool_enabled: bool,
    /// 连接是否只读（连接串 `Readonly=true`；对应 C# `DbBase.Readonly`）
    readonly: AtomicBool,
    /// 只读副本（对应 C# `DAL.Reads`；读操作轮询分流）
    read_replicas: RwLock<Arc<Vec<Arc<Dal>>>>,
    /// 是否有可用副本（读路径快路：false 时直接走主库，零锁零分配）
    has_replicas: AtomicBool,
    /// 读写分离策略是否需要判定（有忽略表或全局切主库）；false 时读路径免锁
    rw_needs_check: AtomicBool,
    /// 读写分离策略（运行期可调）
    rw_strategy: RwLock<ReadWriteStrategy>,
    /// 副本轮询计数器
    read_cursor: AtomicUsize,
}

/// 只读连接上的会话包装：写操作直接报错、不发 SQL。
///
/// 对应 C# `DAL_DbOperate` 的 `if (Db.Readonly) throw new InvalidOperationException(..)`。
/// 只在**只读连接**上包装（主库连接零额外开销）。
struct ReadOnlySession {
    inner: Box<dyn SqlSession>,
    kind: DatabaseKind,
}

impl ReadOnlySession {
    fn deny(&self, action: &str) -> Error {
        Error::Unsupported(format!(
            "数据连接[{:?}]只读（Readonly=true），禁止执行{action}",
            self.kind
        ))
    }
}

impl SqlSession for ReadOnlySession {
    fn kind(&self) -> DatabaseKind {
        self.kind
    }

    fn execute(&mut self, _sql: &str, _params: &[DbValue]) -> Result<u64> {
        Err(self.deny("写入语句"))
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        self.inner.query(sql, params)
    }

    fn begin(&mut self) -> Result<()> {
        // 事务必然意味着写入意图，只读连接直接拒绝（对齐"只读连接禁止写"语义）
        Err(self.deny("事务"))
    }

    fn commit(&mut self) -> Result<()> {
        self.inner.commit()
    }

    fn rollback(&mut self) -> Result<()> {
        self.inner.rollback()
    }

    fn last_identity(&mut self) -> Result<i64> {
        self.inner.last_identity()
    }

    fn last_identity_of(&mut self, table: &str) -> Result<i64> {
        self.inner.last_identity_of(table)
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        self.inner.table_exists(table)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        self.inner.table_columns(table)
    }

    fn catalog_tables(&mut self) -> Result<Option<Vec<crate::catalog::TableInfo>>> {
        self.inner.catalog_tables()
    }
}

/// 批量写入的列项视图：让 owned（`String` 列名）与借用（`&str` 列名）两种行共用同一套执行逻辑。
pub(crate) trait BatchField {
    /// 列名。
    fn name(&self) -> &str;
    /// 列值。
    fn value(&self) -> &DbValue;
}

impl BatchField for (String, DbValue) {
    fn name(&self) -> &str {
        &self.0
    }

    fn value(&self) -> &DbValue {
        &self.1
    }
}

impl BatchField for (&str, DbValue) {
    fn name(&self) -> &str {
        self.0
    }

    fn value(&self) -> &DbValue {
        &self.1
    }
}

impl BatchField for (&str, &DbValue) {
    fn name(&self) -> &str {
        self.0
    }

    fn value(&self) -> &DbValue {
        self.1
    }
}

/// 批量写入的行视图（`Vec<列项>`）：owned 与借用两种行共用。
pub(crate) trait BatchRow {
    /// 按位置取 `(列名, 值)`。
    fn get(&self, index: usize) -> Option<(&str, &DbValue)>;
    /// 按列名取值（忽略大小写）。
    fn find_value(&self, column: &str) -> Option<&DbValue>;
}

impl<F: BatchField> BatchRow for Vec<F> {
    fn get(&self, index: usize) -> Option<(&str, &DbValue)> {
        self.as_slice()
            .get(index)
            .map(|field| (field.name(), field.value()))
    }

    fn find_value(&self, column: &str) -> Option<&DbValue> {
        self.iter()
            .find(|field| field.name().eq_ignore_ascii_case(column))
            .map(BatchField::value)
    }
}

/// 连接串 → 池配置（`PoolMinSize` / `PoolMaxSize` / `PoolIdleTime`，单位秒；缺省对齐 C#）。
///
/// 说明：`PoolMaxSize` 是**空闲上限**（不是并发上限）；`PoolIdleTime` 过小会让低频 IoT 服务
/// 每次请求都重新建连（建连 ≈ 3~4 次往返），建议 300 秒以上。
fn pool_options_from(conn: &ConnectionString) -> (PoolOptions, usize) {
    let mut options = PoolOptions::default();
    // 仅当连接串**显式**指定 `PoolMinSize` 时才预热（默认保持按需创建，不改变既有行为）
    let mut prewarm = 0usize;
    if let Some(value) = conn.get("poolmaxsize").or(conn.get("maxpoolsize"))
        && let Ok(size) = value.trim().parse::<usize>()
    {
        options.max = size;
    }
    if let Some(value) = conn.get("poolminsize").or(conn.get("minpoolsize"))
        && let Ok(size) = value.trim().parse::<usize>()
    {
        options.min = size;
        prewarm = size;
    }
    if let Some(value) = conn.get("poolidletime").or(conn.get("idletimeout"))
        && let Ok(seconds) = value.trim().parse::<u64>()
    {
        options.idle_time = Duration::from_secs(seconds);
    }
    (options, prewarm)
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
            let options = crate::sqlite::SqliteOptions::from_connection_string(conn_str);
            Ok(Box::new(SqliteSession::open_with_options(path, &options)?))
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
        // 连接只读：`Readonly=true` / `ReadOnly=1`（对齐 C# `DbBase` 从连接串解析）
        let readonly = conn_str
            .get("readonly")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        Ok(Self {
            conn_str,
            kind,
            model: None,
            show_sql,
            migration,
            conn_migration,
            entity_caches: RwLock::new(HashMap::new()),
            single_caches: RwLock::new(HashMap::new()),
            has_caches: AtomicBool::new(false),
            field_caches: RwLock::new(HashMap::new()),
            count_cache: RwLock::new(HashMap::new()),
            table_index: OnceLock::new(),
            shard_specs: OnceLock::new(),
            pool: OnceLock::new(),
            pool_enabled,
            readonly: AtomicBool::new(readonly),
            read_replicas: RwLock::new(Arc::new(Vec::new())),
        has_replicas: AtomicBool::new(false),
        rw_needs_check: AtomicBool::new(false),
            rw_strategy: RwLock::new(ReadWriteStrategy::new()),
            read_cursor: AtomicUsize::new(0),
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
        // 模型变了 → 表名索引重建（OnceLock 可整体替换）
        self.table_index = OnceLock::new();
    }

    /// 模型表名索引（小写实体名与物理表名都指向同一个表下标）。
    fn table_index(&self) -> Result<&HashMap<String, usize>> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型".into()))?;
        Ok(self.table_index.get_or_init(|| {
            let mut map = HashMap::with_capacity(model.tables.len() * 4);
            for (i, table) in model.tables.iter().enumerate() {
                // 原名（生成代码传入字面量 = 零分配精确匹配）
                map.entry(table.name.clone()).or_insert(i);
                map.entry(table.effective_table_name().to_string()).or_insert(i);
                // 小写（兼容大小写不一致的调用）
                map.entry(table.name.to_ascii_lowercase()).or_insert(i);
                map.entry(table.effective_table_name().to_ascii_lowercase())
                    .or_insert(i);
            }
            map
        }))
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
        let session = if !self.pool_enabled {
            create_session(self.kind, &self.conn_str)?
        } else {
            self.session_pool().checkout()?
        };
        // 只读连接：写操作直接报错（对齐 C# `if (Db.Readonly) throw`），不发 SQL
        if self.is_readonly() {
            return Ok(Box::new(ReadOnlySession {
                inner: session,
                kind: self.kind,
            }));
        }
        Ok(session)
    }

    /// 连接是否只读（连接串 `Readonly=true`；对应 C# `DbBase.Readonly`）。
    pub fn is_readonly(&self) -> bool {
        self.readonly.load(Ordering::Relaxed)
    }

    /// 运行期切换只读（对应 C# `SuspendReadOnly` / `ResumeReadOnly`）。
    ///
    /// 只影响**后续** `open_session()`；已取出的会话保持创建时的保护状态。
    pub fn set_readonly(&self, value: bool) {
        self.readonly.store(value, Ordering::Relaxed);
    }

    /// 挂载只读副本（对应 C# `DAL.Reads`；读操作轮询分流）。
    ///
    /// 典型来源是连接名约定 `X.readonly` / `X.readonly.rd0..`（见
    /// [`open_replicas_from_file`](Self::open_replicas_from_file)）。
    pub fn attach_read_replicas(&self, replicas: Vec<Arc<Dal>>) {
        let has = !replicas.is_empty();
        // 先发布副本列表，再置标志（读路径先查标志）
        *self
            .read_replicas
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(replicas);
        self.has_replicas.store(has, Ordering::Release);
    }

    /// 当前只读副本（**每次调用分配一个 `Vec`**；热路径请用 [`Dal::read_replicas_arc`]）。
    pub fn read_replicas(&self) -> Vec<Arc<Dal>> {
        self.read_replicas_arc().as_ref().clone()
    }

    /// 当前只读副本快照（`Arc` 共享，零拷贝；读路径内部使用）。
    pub fn read_replicas_arc(&self) -> Arc<Vec<Arc<Dal>>> {
        self.read_replicas
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 是否有可用副本。
    pub fn has_read_replicas(&self) -> bool {
        self.has_replicas.load(Ordering::Acquire)
    }

    /// 读写分离策略快照（忽略表 + 当前开关）。
    pub fn read_write_strategy(&self) -> ReadWriteStrategy {
        self.rw_strategy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    /// 设置忽略表（这些表的读操作强制走主库）。
    pub fn set_ignore_tables(&self, tables: &[&str]) {
        let mut guard = self.rw_strategy.write().unwrap_or_else(|e| e.into_inner());
        *guard = guard.snapshot().ignore_tables(tables);
        drop(guard);
        self.refresh_rw_needs_check();
    }

    /// 当前忽略表（小写）。
    pub fn ignored_tables(&self) -> Vec<String> {
        self.rw_strategy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .ignored_tables()
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// 全局切主库（`true`）或恢复读写分离（`false`）。
    pub fn set_primary_only(&self, value: bool) {
        self.rw_strategy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .set_primary_only(value);
        self.refresh_rw_needs_check();
    }

    /// 是否已全局切主库。
    pub fn is_primary_only(&self) -> bool {
        self.rw_strategy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_primary_only()
    }

    /// 取只读会话（对应 C# `ReadWriteStrategy.TryGet`）。
    ///
    /// - 有副本：按轮询分配（与 C# 的 `Interlocked.Increment % count` 一致）；
    /// - 无副本：回退主库（业务无需关心是否配置了从库）。
    ///
    /// 只读查询应该用本方法；写入必须用 [`open_session`](Self::open_session)。
    pub fn read_session(&self) -> Result<Box<dyn SqlSession>> {
        // 常见情形（未配从库）：一个原子读即回主库，不取锁、不分配
        if !self.has_replicas.load(Ordering::Acquire) {
            return self.open_session();
        }
        let replicas = self.read_replicas_arc();
        if replicas.is_empty() {
            return self.open_session();
        }
        let index = self.read_cursor.fetch_add(1, Ordering::Relaxed) % replicas.len();
        match replicas[index].open_session() {
            Ok(session) => Ok(session),
            // 副本不可用不阻断业务：退回主库（可用性优先，与只读场景语义兼容）
            Err(_) => self.open_session(),
        }
    }

    /// 按表名策略取只读会话：命中忽略表或全局切主库时走主库。
    pub fn read_session_for(&self, table: &str) -> Result<Box<dyn SqlSession>> {
        // 常见情形（无忽略表、未切主库）：直接进读路径，不碰策略锁
        if !self.rw_needs_check.load(Ordering::Acquire) || self.allows_replica(table) {
            return self.read_session();
        }
        self.open_session()
    }

    /// 指定表是否允许走从库（内部使用）。
    fn allows_replica(&self, table: &str) -> bool {
        self.rw_strategy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .allows_replica(table)
    }

    /// 重算并发布「读写分离是否需要判定」标志（配置变更后调用）。
    fn refresh_rw_needs_check(&self) {
        let needs = self
            .rw_strategy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .needs_check();
        self.rw_needs_check.store(needs, Ordering::Release);
    }

    /// 从 `Config/Database.toml` 按 C# 连接名约定创建副本集合。
    ///
    /// 约定（与 DH.NCode `DAL.cs` 一致）：主连接名 `X` 对应
    /// `X.readonly`（单副本）或 `X.readonly.rd0` / `.rd1`（多副本）。
    /// 未配置任何副本时返回空集合（不影响主库使用）。
    #[cfg(feature = "database")]
    pub fn read_replicas_from_file(
        file: &crate::database::DatabaseFile,
        base: &std::path::Path,
        conn_name: &str,
    ) -> Result<Vec<Arc<Dal>>> {
        let mut names = Vec::new();
        let single = format!("{conn_name}.readonly");
        if file.connections.contains_key(&single) {
            names.push(single);
        } else {
            for i in 0..32 {
                let name = format!("{conn_name}.readonly.rd{i}");
                if file.connections.contains_key(&name) {
                    names.push(name);
                }
            }
        }
        let mut replicas = Vec::with_capacity(names.len());
        for name in names {
            // 副本连接串不完整时忽略该副本（主库仍可用），不阻断启动
            let Ok(conn) = file.resolve_connection(base, &name) else {
                continue;
            };
            replicas.push(Arc::new(Dal::open(&conn)?));
        }
        Ok(replicas)
    }

    /// 取（或懒创建）会话池。
    fn session_pool(&self) -> Arc<SessionPool> {
        self.pool
            .get_or_init(|| {
                let kind = self.kind;
                let conn_str = self.conn_str.clone();
                let factory: crate::pool::SessionFactory =
                    Arc::new(move || create_session(kind, &conn_str));
                let (options, prewarm) = pool_options_from(&self.conn_str);
                let pool = Arc::new(SessionPool::new(options, factory));
                // `PoolMinSize>0` 时在首次建池就预热（失败静默：连接不可用时不阻断 Dal 创建）
                if prewarm > 0 {
                    pool.prewarm(prewarm);
                }
                pool
            })
            .clone()
    }

    /// 预热连接池：预建 `count` 个连接放入空闲池，返回成功数。
    ///
    /// 建连要「TCP + 认证 + 会话初始化」约 3~4 次往返（MySQL 本地回环 ≈ 300~400 µs）；
    /// 建议服务启动时调用，避免首批并发请求各自承担这笔成本。
    pub fn warmup_pool(&self, count: usize) -> usize {
        if !self.pooling_enabled() {
            return 0;
        }
        self.session_pool().prewarm(count)
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
        let table = self
            .dal_table_meta(name)
            .ok_or_else(|| Error::Model(format!("模型中不存在表/实体 {name}")))?;
        Ok(TableRef {
            dal: self,
            table,
            table_name: None,
            shard: self.shard_spec_of(table),
        })
    }

    /// 表的时间分表规格（`None` = 不分表）。
    ///
    /// 模型解析期算一次并缓存（表名 → 规格）；非分表表命中空表，仅一次 HashMap 查找。
    fn shard_spec_of(&self, table: &TableMeta) -> Option<Arc<crate::model::ShardSpec>> {
        let specs = self.shard_specs.get_or_init(|| {
            let mut map: HashMap<String, Arc<crate::model::ShardSpec>> = HashMap::new();
            let Some(model) = self.model.as_ref() else {
                return map;
            };
            for item in &model.tables {
                if let Some(spec) = item.shard_spec() {
                    map.insert(item.name.clone(), Arc::new(spec));
                }
            }
            map
        });
        specs.get(&table.name).cloned()
    }

    /// 按实体名/物理表名取表定义（**走表名索引**，O(1)）。
    ///
    /// 快路径用**原名精确匹配**（生成代码传入的都是模型里的字面量，零分配）；
    /// 未命中时才做小写化再查（兼容大小写不一致/外部调用）。
    pub(crate) fn dal_table_meta(&self, name: &str) -> Option<&TableMeta> {
        let model = self.model.as_ref()?;
        let index = self.table_index().ok()?;
        if let Some(i) = index.get(name) {
            return model.tables.get(*i);
        }
        index
            .get(&name.to_ascii_lowercase())
            .and_then(|i| model.tables.get(*i))
    }

    /// 获取表操作句柄（**指定物理表名**）：列元数据仍取模型定义，SQL 操作落到指定表。
    ///
    /// 用于分表场景（如 `Log2` → `Log2_20260927`，见 [`crate::shards`]）与手工别名表操作；
    /// 不会自动建表，分表建表用 [`Dal::ensure_shard_table`]。
    pub fn table_as(&self, name: &str, table_name: &str) -> Result<TableRef<'_>> {
        let mut handle = self.table(name)?;
        handle.table_name = Some(table_name.to_string());
        // 显式指定物理表 = 调用方已自行路由（分表引擎内部走这里），**不再自动分表**（防递归）
        handle.shard = None;
        Ok(handle)
    }

    /// 表级 `ConnName` 解析出的目标连接（对应 C# `Meta.ConnName`；`None` = 与当前连接相同）。
    ///
    /// 规则见 [`crate::shards::resolve_table_dal`]：已注册连接直接复用，未注册则按 C#
    /// `DAL.Init` 规则自动创建 `{自动目录}/{连接名}.db`（SQLite）并缓存。
    pub fn table_conn_dal(&self, name: &str) -> Result<Option<Arc<Dal>>> {
        let table = self
            .dal_table_meta(name)
            .ok_or_else(|| Error::Model(format!("模型中不存在表/实体 {name}")))?;
        let base_conn = self.model.as_ref().and_then(|m| m.options.conn_name());
        crate::shards::resolve_table_dal(table.conn_name.as_deref(), base_conn, self)
    }

    /// 在「表级 `ConnName` 解析出的连接」上打开会话并执行闭包（自动分库路由）。
    ///
    /// 这是 C# `Entity`（`Meta.ConnName` 隐式选连接）在本库（会话显式）下的等价入口：
    ///
    /// ```no_run
    /// # use pek_rcode::{Dal, DbValue, Query};
    /// # fn demo(dal: &Dal) -> pek_rcode::Result<()> {
    /// // 表在模型里配了 ConnName="IoTData"：自动落到 IoTData 连接（未注册则 Data/IoTData.db）
    /// dal.with_table_session("StandardData", |_dal, table, session| {
    ///     table.insert(session, &[("Name", DbValue::from("x"))])?;
    ///     let _rows = table.query(session, &Query::new().column("Id"))?;
    ///     Ok(())
    /// })?;
    /// # Ok(()) }
    /// ```
    pub fn with_table_session<R, F>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Dal, TableRef<'_>, &mut dyn SqlSession) -> Result<R>,
    {
        let target = self.table_conn_dal(name)?;
        let dal: &Dal = target.as_deref().unwrap_or(self);
        let table = dal.table(name)?;
        let mut session = dal.open_session()?;
        f(dal, table, session.as_mut())
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
        // 视图不存在"分表建表"语义（对齐 C#：视图跳过结构迁移）
        if model.table(name).is_some_and(|t| t.is_view) {
            return Ok(false);
        }
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

    /// 按模型同步数据库结构（建表 / 补列 / 补索引 / 放宽字符串长度；`Full` 档含修改与删除），返回本次变更清单。
    ///
    /// 档位语义（与 DH.NCode 的 [`Migration`] 一致）：
    /// - `Off`：跳过（返回空报告）
    /// - `ReadOnly`：只检查、不执行；将 DDL 收集到 [`SchemaReport::pending_sql`] 供人工执行
    /// - `On`（缺省）：创建类（建表 / 补列 / 补索引）+ **字符串列长度放宽**（不缩窄、不改类型、不删除）
    /// - `Full`：在 `On` 基础上允许修改列类型与长度（含缩窄）、删除多余列/索引（**删除类动作仅此档允许**）
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

    /// **只同步索引**（不改表、不改列，只按模型补/升级索引）。
    ///
    /// 设计目的：索引变更属于**低风险、可自动**的结构项（不改数据、不动列），
    /// 不应要求上线时关掉整个 `sync_schema` 再手工执行 DDL。消费方可以在
    /// `sync_schema = false` 的常规部署里单独调用它，让「新增索引 / 普通索引→唯一索引」
    /// 随服务启动自动生效。
    ///
    /// 行为（对齐 [`Dal::sync_schema`] 的 `On` 档语义）：
    /// - 表不存在 → **跳过**（交给 `sync_schema` 建表，避免这里误建空表）；
    /// - 索引缺失 → `CREATE INDEX`；
    /// - 模型要唯一、库里同列只有普通索引 → 先 `DROP` 同名索引再 `CREATE UNIQUE INDEX`；
    /// - 库里唯一、模型写普通 → **不降级**（避免悄悄丢约束）；
    /// - 多余索引 → **不删除**（删除属 `Full` 档，且可能影响其它查询）。
    ///
    /// # 返回值
    /// 结构报告（`added_indexes` / `dropped_indexes` 为本轮变更项；`pending_sql` 在只读档下给出脚本）。
    pub fn sync_indexes(&self) -> Result<SchemaReport> {
        let mut report = SchemaReport {
            mode: Migration::On,
            ..Default::default()
        };
        let Some(model) = self.model.as_ref() else {
            report.notes.push("尚未加载数据模型，跳过索引同步".into());
            return Ok(report);
        };
        if !self.kind.supports_ddl() || !crate::catalog::supports_index_catalog(self.kind) {
            report
                .notes
                .push(format!("{:?} 不支持索引目录，跳过索引同步", self.kind));
            return Ok(report);
        }
        let mut session = self.open_session()?;
        let db_tables = match crate::catalog::read_tables(session.as_mut(), self.kind, None) {
            Ok(tables) => tables,
            Err(Error::Unsupported(_)) => {
                report.notes.push("索引目录不可用，跳过索引同步".into());
                return Ok(report);
            }
            Err(e) => return Err(e),
        };

        for table in &model.tables {
            let table_name = table.effective_table_name();
            // 表还不存在：由 sync_schema 建表时一并建索引
            if !db_tables
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(table_name))
            {
                continue;
            }
            let existing = match crate::catalog::read_indexes(
                session.as_mut(),
                self.kind,
                table_name,
            ) {
                Ok(indexes) => indexes,
                Err(Error::Unsupported(_)) => continue,
                Err(e) => return Err(e),
            };
            for idx in &table.indexes {
                if idx.columns.is_empty() {
                    continue;
                }
                let unique_exists = existing
                    .iter()
                    .any(|e| e.unique && same_index(&e.columns, &idx.columns));
                if unique_exists
                    || (!idx.unique
                        && existing
                            .iter()
                            .any(|e| same_index(&e.columns, &idx.columns)))
                {
                    continue;
                }
                let want_name = self.kind.index_name(table, idx);
                if idx.unique
                    && let Some(plain) = existing.iter().find(|e| {
                        !e.unique
                            && same_index(&e.columns, &idx.columns)
                            && e.name.eq_ignore_ascii_case(&want_name)
                    })
                    && let Some(drop_sql) =
                        self.kind.drop_index_sql(&plain.name, Some(table_name))
                {
                    self.exec_ddl(session.as_mut(), Migration::On, &mut report, &drop_sql)?;
                    report
                        .dropped_indexes
                        .push((table_name.to_string(), plain.name.clone()));
                }
                if let Some(sql) = self.kind.create_index_sql(table, idx) {
                    self.exec_ddl(session.as_mut(), Migration::On, &mut report, &sql)?;
                    report
                        .added_indexes
                        .push((table_name.to_string(), want_name));
                }
            }
            // 清理多余索引：**按模型为准，该清的清**
            // - 模型里没有该列组合的索引 → 删除；
            // - 同列已有唯一索引时，残留的普通同列索引属于冗余 → 删除；
            // - 主键/系统索引（`sqlite_autoindex*`、`*_pkey` 等）跳过。
            // 索引增删不改变数据，放在默认档（`On`）执行。
            for e in &existing {
                if is_primary_index_name(&e.name) {
                    continue;
                }
                let in_model = table
                    .indexes
                    .iter()
                    .any(|idx| same_index(&idx.columns, &e.columns));
                let redundant = !e.unique
                    && existing
                        .iter()
                        .any(|o| o.unique && same_index(&o.columns, &e.columns));
                if in_model && !redundant {
                    continue;
                }
                if let Some(sql) = self.kind.drop_index_sql(&e.name, Some(table_name)) {
                    self.exec_ddl(session.as_mut(), Migration::On, &mut report, &sql)?;
                    report
                        .dropped_indexes
                        .push((table_name.to_string(), e.name.clone()));
                }
            }
        }
        Ok(report)
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
            // 视图不参与结构迁移（对齐 C# `EntitySession` 的 `if (DataTable.IsView) return`）
            if table.is_view {
                continue;
            }
            // 表级收紧：只能比全局更保守（对应 XCode ResolveMigration）
            let mode = self.migration.tighten(table.migration);
            if mode == Migration::Off {
                continue;
            }

            let table_name = table.effective_table_name();

            if !session.table_exists(table_name)? {
                // MySQL 单行 65535 字节预算：建表前先做可读校验（否则会得到难懂的 ERROR 1118）
                if self.kind == DatabaseKind::MySql
                    && !mode.is_readonly()
                    && let Some(issue) = table.mysql_row_size_issue()
                {
                    return Err(Error::Model(issue));
                }
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
                    // 给已有表加“超宽 varchar”极易把整行推过 MySQL 65535 字节（实际事故：
                    // 16000 字符的 SiteSummary 叠加大表原有列 → ERROR 1118）。提前拒绝并给出改法。
                    if self.kind == DatabaseKind::MySql
                        && !mode.is_readonly()
                        && col.is_wide_varchar()
                    {
                        return Err(Error::Model(format!(
                            "新增列 {table_name}.{col_name} 的长度 {} 超过安全阈值 {} 字符：\
                             请改用 DataType=\"Text\"（MySQL→LONGTEXT）。\
                             MySQL 单行上限 65535 字节，utf8mb4 下 varchar(n) 占 4n 字节，\
                             叠加已有列会报 ERROR 1118 Row size too large。",
                            col.length,
                            crate::model::MYSQL_SAFE_VARCHAR_CHARS
                        )));
                    }
                    let sql = self.kind.add_column_sql(table, col);
                    self.exec_ddl(&mut *session, mode, &mut report, &sql)?;
                    report
                        .added_columns
                        .push((table_name.to_string(), col_name.to_string()));
                }
            }

            // 既存表：补齐模型定义中缺失的索引（同列且同序即视为已存在，避免与其它工具命名差异重复建）
            //
            // **唯一性升级**：模型要求唯一、库里同列只有普通索引时，同列判断会误以为"已存在"
            // 而永远不建唯一约束（历史坑：改模型 Unique=True 后 `sync_schema` 静默跳过）。
            // 这里额外比较 `unique`：不一致则先删同名索引再建唯一索引；反向（库里唯一、模型普通）不降级。
            if crate::catalog::supports_index_catalog(self.kind) {
                match crate::catalog::read_indexes(session.as_mut(), self.kind, table_name) {
                    Ok(existing_indexes) => {
                        for idx in &table.indexes {
                            if idx.columns.is_empty() {
                                continue;
                            }
                            let unique_exists = existing_indexes
                                .iter()
                                .any(|e| e.unique && same_index(&e.columns, &idx.columns));
                            if unique_exists || (!idx.unique
                                && existing_indexes
                                    .iter()
                                    .any(|e| same_index(&e.columns, &idx.columns)))
                            {
                                // 已有同列唯一索引；或模型不要求唯一而库里已有同列索引（不降级）
                                continue;
                            }
                            // 模型要求唯一但库里只有同列普通索引 → 同名时先删再建
                            let want_name = self.kind.index_name(table, idx);
                            if idx.unique
                                && let Some(existing) = existing_indexes.iter().find(|e| {
                                    !e.unique
                                        && same_index(&e.columns, &idx.columns)
                                        && e.name.eq_ignore_ascii_case(&want_name)
                                })
                                && let Some(drop_sql) =
                                    self.kind.drop_index_sql(&existing.name, Some(table_name))
                            {
                                self.exec_ddl(&mut *session, mode, &mut report, &drop_sql)?;
                                report
                                    .dropped_indexes
                                    .push((table_name.to_string(), existing.name.clone()));
                            }
                            if let Some(sql) = self.kind.create_index_sql(table, idx) {
                                self.exec_ddl(&mut *session, mode, &mut report, &sql)?;
                                report
                                    .added_indexes
                                    .push((table_name.to_string(), want_name));
                            }
                        }
                        // 清理多余索引：**按模型为准，该清的清**
                        // - 模型里没有该列组合的索引 → 删除；
                        // - 同列已有唯一索引时，残留的普通同列索引属冗余 → 删除；
                        // - 主键/系统索引（`sqlite_autoindex*`、`*_pkey` 等）跳过；
                        // - 索引增删不改数据，默认档（`On`）即执行，只读档收集 SQL。
                        for e in &existing_indexes {
                            if is_primary_index_name(&e.name) {
                                continue;
                            }
                            let in_model = table
                                .indexes
                                .iter()
                                .any(|idx| same_index(&idx.columns, &e.columns));
                            let redundant = !e.unique
                                && existing_indexes
                                    .iter()
                                    .any(|o| o.unique && same_index(&o.columns, &e.columns));
                            if in_model && !redundant {
                                continue;
                            }
                            if let Some(sql) =
                                self.kind.drop_index_sql(&e.name, Some(table_name))
                            {
                                self.exec_ddl(&mut *session, mode, &mut report, &sql)?;
                                report
                                    .dropped_indexes
                                    .push((table_name.to_string(), e.name.clone()));
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

        // 先归还本轮的会话，后面的比对/对齐会各开一次（顺序执行，连接池可复用同一物理连接）
        drop(session);

        // 列长度对齐（字符串列）：**`On` 档即允许放宽**——对齐 C# `CheckColumnsChange` 里
        // `IsColumnLengthChanged` 传的是 `@readonly`，只有类型变化/删除才带 `onlyCreate`。
        // 缩窄（含“明确长度 → 不限/大文本”）仍需 `Full` 档。
        if self.migration != Migration::Off {
            let diff = self.diff_schema()?;
            self.apply_length_changes(&mut report, &diff)?;

            // Full 档：修改列类型 + 删除多余列/索引（删除类动作仅此档允许；对应 C# 的 onlyCreate=false 分支）
            if self.migration == Migration::Full {
                self.apply_full_changes(&mut report, &diff)?;
            }
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

    /// 字符串列长度对齐（**`On` 档可放宽、`Full` 档可缩窄**）。
    ///
    /// 与 DH.NCode 对齐：C# `CheckColumnsChange` 里长度扩大走 `PerformSchema(sb, @readonly, …)`，
    /// 不带 `onlyCreate`，即**默认 `On` 档也会执行**；缩窄/任意差异（`IsColumnChanged`）只在 `Full`。
    ///
    /// 不支持 `ALTER COLUMN` 的数据库（SQLite/Firebird/Access 等）只记 `notes` 留给人工——
    /// C# 对这些库走 `ReBuildTable` 重建表，本库暂不自动重建（避免动数据）。
    fn apply_length_changes(&self, report: &mut SchemaReport, diff: &SchemaDiff) -> Result<()> {
        if diff.length_mismatches.is_empty() {
            return Ok(());
        }
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，无法同步结构".into()))?;
        let mut session = self.open_session()?;

        for m in &diff.length_mismatches {
            let Some(table) = model.table(&m.table) else {
                continue;
            };
            // 表级档位只能收紧：Off 的表不动
            let mode = self.migration.tighten(table.migration);
            if mode == Migration::Off {
                continue;
            }
            // 缩窄（含“明确长度 → 不限”）仅 Full 档允许（对齐 C# `IsColumnChanged` 的 onlyCreate 闸门）
            if !m.widening && mode != Migration::Full {
                continue;
            }
            let Some(col) = table.column(&m.column) else {
                continue;
            };
            // MySQL 单行 65535 字节预算：放宽到超宽 varchar 与补列同样会炸行（ERROR 1118）
            if self.kind == DatabaseKind::MySql && !mode.is_readonly() && col.is_wide_varchar() {
                report.notes.push(format!(
                    "{}.{}：长度 {} → {} 超过 MySQL 安全阈值 {} 字符，请改用 DataType=\"Text\" 或人工调整",
                    m.table,
                    m.column,
                    m.actual,
                    m.expected,
                    crate::model::MYSQL_SAFE_VARCHAR_CHARS
                ));
                continue;
            }
            let Some(sql) = self.kind.alter_column_sql(table, col) else {
                report.notes.push(format!(
                    "{}.{}：长度 {} → {} 需人工处理（该数据库不支持直接改列，如 SQLite 需重建表）",
                    m.table, m.column, m.actual, m.expected
                ));
                continue;
            };
            match self.exec_ddl(&mut *session, mode, report, &sql) {
                Ok(()) => report
                    .altered_columns
                    .push((m.table.clone(), m.column.clone())),
                Err(e) => report.notes.push(format!(
                    "{}.{}：调整列长度失败（{e}）",
                    m.table, m.column
                )),
            }
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
    fn apply_full_changes(&self, report: &mut SchemaReport, diff: &SchemaDiff) -> Result<()> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，无法同步结构".into()))?;
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

    /// 字符串列长度差异判定（对齐 C# `IsColumnLengthChanged` / `IsColumnChanged`）。
    ///
    /// 返回 `(模型长度, 库长度, 是否放宽)`；无差异或无需处理时返回 `None`。
    /// - 只比较字符串列（其余类型交给 `type_mismatches`）；
    /// - 两侧都是大文本（长度 `0`/超 `LongTextLength`）→ 长度无意义，忽略；
    /// - 库侧原始类型为 `text`/`ntext`/`sysname` → 忽略（C# 同款白名单）；
    /// - `0`（不限）视为最大长度，参与比较方向判定。
    fn string_length_change(
        &self,
        col: &ColumnMeta,
        db_col: &ColumnInfo,
    ) -> Option<(i32, i32, bool)> {
        if col.data_type != crate::types::DataType::String
            || db_col.data_type != crate::types::DataType::String
        {
            return None;
        }
        if col.length == db_col.length {
            return None;
        }
        let limit = self.kind.long_text_length();
        let model_big = col.length <= 0 || col.length > limit;
        let db_big = db_col.length <= 0 || db_col.length > limit;
        let db_base = db_col
            .raw_type
            .trim()
            .to_ascii_lowercase()
            .split(['(', ' '])
            .next()
            .unwrap_or("")
            .to_string();
        if (model_big && db_big) || matches!(db_base.as_str(), "ntext" | "text" | "sysname") {
            return None;
        }
        // 放宽判定与 C# 一致：仅“模型明确长度 > 库明确长度”算放宽（`On` 档就会执行）；
        // 其余（含 `0` = 不限的任一方向）归 `Full` 档，避免默认档误缩窄。
        let widening = col.length > 0 && db_col.length > 0 && col.length > db_col.length;
        Some((col.length, db_col.length, widening))
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
                                        } else if let Some((model_len, db_len, widening)) =
                                            self.string_length_change(col, db_col)
                                        {
                                            // 长度差异：**扩大在 `On` 档即执行**（对齐 C# `IsColumnLengthChanged`），
                                            // 缩窄只在 `Full` 档（`IsColumnChanged`）
                                            if let Some(sql) = self.kind.alter_column_sql(table, col) {
                                                if widening {
                                                    diff.alter_sql.push(sql);
                                                } else {
                                                    diff.full_sql.push(sql);
                                                }
                                            }
                                            diff.length_mismatches.push(ColumnLengthMismatch {
                                                table: tname.to_string(),
                                                column: cname.to_string(),
                                                expected: model_len,
                                                actual: db_len,
                                                widening,
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
                            // 索引：缺失 / 多余（同列且同序视为一致；唯一性差异按"需升级"报告）
                            if crate::catalog::supports_index_catalog(self.kind) {
                                for idx in &table.indexes {
                                    if idx.columns.is_empty() {
                                        continue;
                                    }
                                    let unique_exists = db
                                        .indexes
                                        .iter()
                                        .any(|e| e.unique && same_index(&e.columns, &idx.columns));
                                    if unique_exists
                                        || (!idx.unique
                                            && db
                                                .indexes
                                                .iter()
                                                .any(|e| same_index(&e.columns, &idx.columns)))
                                    {
                                        continue;
                                    }
                                    // 模型要唯一、库里只有同列普通索引 → 需要先删同名再建
                                    let want_name = self.kind.index_name(table, idx);
                                    if idx.unique
                                        && let Some(existing) =
                                            db.indexes.iter().find(|e| {
                                                !e.unique
                                                    && same_index(&e.columns, &idx.columns)
                                                    && e.name.eq_ignore_ascii_case(&want_name)
                                            })
                                        && let Some(drop_sql) =
                                            self.kind.drop_index_sql(&existing.name, Some(tname))
                                    {
                                        diff.alter_sql.push(drop_sql);
                                    }
                                    diff.missing_indexes
                                        .push((tname.to_string(), want_name));
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
    /// 字符串列长度差异（扩大可由 `On` 档直接执行，缩窄仅 `Full` 档）
    pub length_mismatches: Vec<ColumnLengthMismatch>,
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
            && self.length_mismatches.is_empty()
    }
}

/// 字符串列长度差异（模型预期 vs 数据库实际；`0` 表示不限长度/大文本）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnLengthMismatch {
    /// 表名
    pub table: String,
    /// 列名
    pub column: String,
    /// 模型预期长度（`0` 均为不限）
    pub expected: i32,
    /// 数据库实际长度（`0` 均为不限）
    pub actual: i32,
    /// 是否为放宽（模型长度 > 库长度；`true` 时 `On` 档即会执行，`false` 需 `Full`）
    pub widening: bool,
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
    /// 时间分表规格（`Some` = 表级 `DataScale="timeShard:..."`，CRUD 自动路由；
    /// 显式物理表（[`Dal::table_as`]）时为 `None`，防递归）
    shard: Option<Arc<crate::model::ShardSpec>>,
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

    /// 时间分表规格（`None` = 不分表；[`Dal::table_as`] 显式物理表恒为 `None`）。
    pub(crate) fn shard_spec(&self) -> Option<&crate::model::ShardSpec> {
        self.shard.as_deref()
    }

    /// 所属数据访问层。
    pub(crate) fn dal(&self) -> &'a Dal {
        self.dal
    }

    /// 是否为视图表（对应 DH.NCode `TableItem.IsView`）。
    pub fn is_view(&self) -> bool {
        self.table.is_view
    }

    /// 视图表拒绝写入（对齐 C# `if (DataTable.IsView) throw new NotSupportedException("视图无法添删改！")`）。
    fn ensure_writable(&self) -> Result<()> {
        if self.table.is_view {
            return Err(Error::Unsupported(format!(
                "视图无法添删改！（{}）",
                self.table.name
            )));
        }
        Ok(())
    }

    /// 插入一行，返回自增主键（无自增列时返回 0）。
    ///
    /// 自增回写策略按数据库区分：
    /// - PostgreSQL 系：`INSERT ... RETURNING 列`（与 DH.NCode 的 `RETURNING *` 一致），直接回读
    /// - Oracle：插入语句携带 `SEQ_{表名}.NEXTVAL`（由 [`sqlbuild::insert_sql`] 注入），随后读取序列 CURRVAL
    /// - 其余：插入后通过会话读取自增函数（`last_insert_rowid()` / `LAST_INSERT_ID()` / `SCOPE_IDENTITY()`）
    pub fn insert(&self, session: &mut dyn SqlSession, fields: &[(&str, DbValue)]) -> Result<i64> {
        self.ensure_writable()?;
        // 无拦截器且非分表：借用列集直接构建，省每列一次 String 分配
        if !crate::interceptor::has_registered() && self.shard_spec().is_none() {
            return self.insert_borrowed_single(session, fields);
        }
        // 拦截器补全审计字段（对应实体拦截器 OnValid）
        let values = crate::interceptor::prepare(
            self.table,
            crate::interceptor::DataMethod::Insert,
            fields,
        );
        self.insert_prepared_owned(session, values)
    }

    /// 插入一行（列集合已拥有：值直接移入 SQL 参数，省一次克隆）。
    pub fn insert_owned(
        &self,
        session: &mut dyn SqlSession,
        fields: Vec<(String, DbValue)>,
    ) -> Result<i64> {
        self.ensure_writable()?;
        let values = crate::interceptor::prepare_owned(
            self.table,
            crate::interceptor::DataMethod::Insert,
            fields,
        );
        self.insert_prepared_owned(session, values)
    }

    /// 插入一行（**已执行拦截器补全**的字段）。
    ///
    /// 供批量写入的回退路径复用，避免拦截器重复执行；单行 [`TableRef::insert`] 亦委托至此。
    pub(crate) fn insert_prepared(
        &self,
        session: &mut dyn SqlSession,
        fields: &[(&str, DbValue)],
    ) -> Result<i64> {
        // 已执行拦截器；非分表时直接借用构建（批量回退路径复用）
        if self.shard_spec().is_none() {
            return self.insert_borrowed_single(session, fields);
        }
        let owned: Vec<(String, DbValue)> = fields
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        self.insert_prepared_owned(session, owned)
    }

    /// 插入一行（**已执行拦截器补全**且列集合已拥有：值移入 SQL 参数）。
    pub(crate) fn insert_prepared_owned(
        &self,
        session: &mut dyn SqlSession,
        fields: Vec<(String, DbValue)>,
    ) -> Result<i64> {
        // 透明分表：表带 `DataScale="timeShard:..."` 时按分表字段值路由
        if let Some(spec) = self.shard_spec().cloned() {
            return self.insert_auto_sharded(session, &spec, fields);
        }
        self.insert_prepared_owned_single(session, fields)
    }

    /// 插入一行（**单表**实现：不做分表路由，分表引擎内部直接调用）。
    pub(crate) fn insert_prepared_owned_single(
        &self,
        session: &mut dyn SqlSession,
        fields: Vec<(String, DbValue)>,
    ) -> Result<i64> {
        let (sql, params) = sqlbuild::insert_sql_owned_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            fields,
        )?;
        self.execute_insert_sql(session, sql, params)
    }

    /// 无拦截器/非分表快路：借用列集直接构建 INSERT（跳过 owned 转换）。
    fn insert_borrowed_single(
        &self,
        session: &mut dyn SqlSession,
        fields: &[(&str, DbValue)],
    ) -> Result<i64> {
        let (sql, params) = sqlbuild::insert_sql_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            fields,
        )?;
        self.execute_insert_sql(session, sql, params)
    }

    /// 执行已构建的 INSERT：自增回写（PG/DuckDB `RETURNING`、其余会话函数）与缓存失效。
    fn execute_insert_sql(
        &self,
        session: &mut dyn SqlSession,
        mut sql: String,
        params: Vec<DbValue>,
    ) -> Result<i64> {
        let identity = self.table.identity();

        if let Some(id_col) = identity
            && matches!(
                self.dal.kind,
                DatabaseKind::PostgreSql | DatabaseKind::DuckDb
            )
        {
            sql.push_str(" RETURNING ");
            sql.push_str(&self.dal.kind.quote(self.table.effective_column_name(id_col)));
            self.dal.log_sql(&sql);
            let set = session.query_owned(&sql, params)?;
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
            session.insert_and_get_identity_owned(&sql, params, Some(self.physical_name()))?
        } else {
            session.execute_owned(&sql, params)?;
            0
        };
        // 写入使缓存失效（对应 DH.NCode：任何添删改都让缓存马上过期）
        self.dal.invalidate_cache(self.physical_name());
        Ok(id)
    }

    /// 批量插入（对应 C# `EntityExtension.Insert(list)` → `IDbSession.Insert(table, columns, list)`）。
    ///
    /// - `rows`：每行一组 `(字段, 值)`；列集以首行为准，其余行按列名（大小写不敏感）投影，缺失补 `NULL`；
    /// - 每行先执行拦截器补全（与单行 [`TableRef::insert`] 一致）；
    /// - **首行自增列为 0 或缺失 → 整批排除自增列**（由数据库生成；对齐 C# `BuildInsertColumns`，
    ///   需要显式指定自增值时请按"含/不含自增值"拆批调用）；
    /// - 支持多行 `VALUES` 的数据库整批写入（`batch_size` 默认按方言：MySQL `500`、其余 `5000`，
    ///   再按数据库单语句参数上限收紧）；
    ///   其余数据库回退逐行执行（语义一致，仅少一次往返的优化不生效）；
    /// - **不保证回写自增主键**（与 C# `BatchInsert` 一致）；无自增的雪花主键请先填值
    ///   （分表批量 [`crate::entity::Entity::insert_batch_sharded`] 会自动生成）；
    /// - 返回受影响行数；任一分块失败即中止（已执行的分块不回滚，需要原子性请自行包裹事务）。
    pub fn insert_batch(
        &self,
        session: &mut dyn SqlSession,
        rows: &[Vec<(&str, DbValue)>],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        self.write_batch(session, rows, BatchWriteMode::Insert, batch_size)
    }

    /// 通用批量写入（对应 C# `IDbSession.Insert / InsertIgnore / Replace / Upsert` 的多行实现）。
    ///
    /// - `rows`：每行一组 `(字段, 值)`；列集以首行为准，其余行按列名（大小写不敏感）投影，缺失补 `NULL`；
    /// - 每行先执行拦截器补全（与单行 [`TableRef::insert`] 一致）；
    /// - **首行自增列为 0 或缺失 → 整批排除自增列**（由数据库生成；对齐 C# `BuildInsertColumns`，
    ///   需要显式指定自增值时请按"含/不含自增值"拆批调用）；
    /// - 模式支持：`Insert`（不支持多行的方言回退逐行）、`InsertIgnore`/`Replace`（方言不支持时报错，
    ///   无安全回退）、`Upsert`（SQLite/MySQL/PostgreSQL/DuckDB 多行 `On Conflict`；其余方言回退逐行
    ///   ——查主键后更新/插入）；
    /// - **`Upsert`/`Replace` 是整行语义**（冲突时覆盖全部非主键列；`Replace` 为先删后插），
    ///   多个应用共享同一张表时禁用，改用 [`TableRef::update_by_pk`] / [`TableRef::update_where`]
    ///   只写变化的列（详见 `docs/使用规范.md` §4.5）；
    /// - `batch_size` 默认 [`crate::batch::DEFAULT_INSERT_BATCH_SIZE`] = 5000，再按数据库单语句参数上限收紧；
    /// - **不保证回写自增主键**（与 C# `BatchInsert` 一致）；无自增的雪花主键请先填值
    ///   （分表批量 [`crate::entity::Entity::insert_batch_sharded`] / [`crate::entity::Entity::upsert_batch_sharded`] 会自动生成）；
    /// - 返回受影响行数；任一分块失败即中止（已执行的分块不回滚，需要原子性请自行包裹事务）。
    pub fn write_batch(
        &self,
        session: &mut dyn SqlSession,
        rows: &[Vec<(&str, DbValue)>],
        mode: BatchWriteMode,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        self.ensure_writable()?;
        // 无拦截器：直接用借用行执行（分表也走借用行，省每行每列一次 String 分配 + 值 clone）
        if !crate::interceptor::has_registered() {
            if let Some(spec) = self.shard_spec().cloned() {
                return self.write_batch_auto_sharded(session, &spec, rows, mode, batch_size);
            }
            return self.write_batch_borrowed_single(session, rows, mode, batch_size);
        }
        let prepared: Vec<Vec<(String, DbValue)>> = rows
            .iter()
            .map(|fields| {
                crate::interceptor::prepare(
                    self.table,
                    crate::interceptor::DataMethod::Insert,
                    fields,
                )
            })
            .collect();
        self.write_batch_prepared(session, &prepared, mode, batch_size)
    }

    /// 通用批量写入（**已执行拦截器补全**的行；分表批量路径复用，避免拦截器重复执行）。
    pub(crate) fn write_batch_prepared(
        &self,
        session: &mut dyn SqlSession,
        rows: &[Vec<(String, DbValue)>],
        mode: BatchWriteMode,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        // 透明分表：按分表字段逐行分组后分组写入（对齐 C# `BatchInsert` 的分表分支）
        if let Some(spec) = self.shard_spec().cloned() {
            return self.write_batch_auto_sharded(session, &spec, rows, mode, batch_size);
        }
        self.write_batch_prepared_single(session, rows, mode, batch_size)
    }

    /// 通用批量写入（**单表**实现：不分组）。
    pub(crate) fn write_batch_prepared_single(
        &self,
        session: &mut dyn SqlSession,
        rows: &[Vec<(String, DbValue)>],
        mode: BatchWriteMode,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        let refs: Vec<&Vec<(String, DbValue)>> =
            rows.iter().filter(|row| !row.is_empty()).collect();
        self.write_batch_rows_core(session, &refs, mode, batch_size)
    }

    /// 借用行版本的批量写入（无拦截器/非分表快路）。
    fn write_batch_borrowed_single(
        &self,
        session: &mut dyn SqlSession,
        rows: &[Vec<(&str, DbValue)>],
        mode: BatchWriteMode,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        let refs: Vec<&Vec<(&str, DbValue)>> =
            rows.iter().filter(|row| !row.is_empty()).collect();
        self.write_batch_rows_core(session, &refs, mode, batch_size)
    }

    /// 批量写入核心（泛型行：owned 与借用共用；分表路由在调用方处理）。
    pub(crate) fn write_batch_rows_core<F: BatchField>(
        &self,
        session: &mut dyn SqlSession,
        rows: &[&Vec<F>],
        mode: BatchWriteMode,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        self.ensure_writable()?;
        let Some(first) = rows.first() else {
            return Ok(0);
        };
        let batch = batch_size
            .unwrap_or_else(|| crate::batch::default_insert_batch_size(self.dal.kind))
            .max(1);

        // 列集 = 首行列集；首行自增列为 0/缺失 → 整批排除自增列（对齐 C# `BuildInsertColumns`）
        let identity = self.table.identity().map(|c| c.name.clone());
        let drop_identity = identity
            .as_ref()
            .is_some_and(|name| first.find_value(name).and_then(DbValue::as_i64).unwrap_or(0) == 0);
        let columns: Vec<&str> = (0..first.len())
            .filter_map(|index| first.get(index))
            .map(|(name, _)| name)
            .filter(|name| {
                !(drop_identity && identity.as_deref().is_some_and(|id| name.eq_ignore_ascii_case(id)))
            })
            .collect();
        if columns.is_empty() {
            return Err(Error::Model(format!(
                "表 {} 的批量写入没有可写字段",
                self.table.name
            )));
        }

        let mut affected = 0u64;
        match mode {
            BatchWriteMode::Insert => {
                if self.dal.kind.supports_multi_row_insert() {
                    affected += self.exec_multi_write(session, rows, &columns, batch, mode)?;
                } else {
                    // 回退逐行（拦截器已补全，不再重复执行）
                    for row in rows {
                        let fields: Vec<(&str, DbValue)> = (0..row.len())
                            .filter_map(|index| row.get(index))
                            .map(|(name, value)| (name, value.clone()))
                            .collect();
                        let _ = self.insert_prepared(session, &fields)?;
                        affected += 1;
                    }
                }
            }
            BatchWriteMode::InsertIgnore | BatchWriteMode::Replace => {
                // 方言不支持时由 SQL 组装报错（无安全回退）
                affected += self.exec_multi_write(session, rows, &columns, batch, mode)?;
            }
            BatchWriteMode::Upsert => {
                if self.dal.kind.supports_multi_upsert() {
                    affected += self.exec_multi_write(session, rows, &columns, batch, mode)?;
                } else {
                    // 回退：逐行查主键后更新/插入（与队列 Upsert 同语义）
                    affected += self.upsert_rows_fallback(session, rows)?;
                }
            }
        }
        // MySQL 的 `ON DUPLICATE KEY UPDATE` 对「插入」行返回 1、对「更新」行返回 **2**，
        // 把驱动返回值直接当行数用会让重复上报的统计翻倍（实测 20 万行 Upsert 记成 40 万）。
        // 只要本批有任意行发生变化，其逻辑行数就是行数本身；全部无变化时才是 0。
        if mode == BatchWriteMode::Upsert
            && self.dal.kind == DatabaseKind::MySql
            && affected > 0
        {
            affected = rows.len() as u64;
        }
        self.dal.invalidate_cache(self.physical_name());
        Ok(affected)
    }

    /// 多行写入执行（按参数上限与批大小分块；供各模式复用）。
    fn exec_multi_write<F: BatchField>(
        &self,
        session: &mut dyn SqlSession,
        rows: &[&Vec<F>],
        columns: &[&str],
        batch: usize,
        mode: BatchWriteMode,
    ) -> Result<u64> {
        self.exec_multi_write_upsert(session, rows, columns, batch, mode, None)
    }

    /// 多行写入执行（可指定 Upsert 的「只更新列」）。
    fn exec_multi_write_upsert<F: BatchField>(
        &self,
        session: &mut dyn SqlSession,
        rows: &[&Vec<F>],
        columns: &[&str],
        batch: usize,
        mode: BatchWriteMode,
        update_only: Option<&[&str]>,
    ) -> Result<u64> {
        let max_params = self.dal.kind.max_statement_params();
        let per_statement = if max_params == 0 {
            batch
        } else {
            (max_params / columns.len()).clamp(1, batch)
        };
        let mut affected = 0u64;
        // 列 → 行内位置：同一批的行来自同一套字段（列集与顺序一致），
        // 只需按首行算一次位置，后续行按位置取值（原来每行每列都要线性比较一遍列名）。
        let Some(first) = rows.first() else {
            return Ok(0);
        };
        let positions: Vec<Option<usize>> = columns
            .iter()
            .map(|column| {
                (0..first.len()).find(|index| {
                    first
                        .get(*index)
                        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(column))
                })
            })
            .collect();
        let aligned = rows.iter().all(|row| {
            !row.is_empty() && positions.iter().all(|p| p.is_some_and(|i| i < row.len()))
        });
        for chunk in rows.chunks(per_statement) {
            let sql = match (mode, update_only) {
                (BatchWriteMode::Upsert, Some(only)) => sqlbuild::multi_write_sql_upsert_named(
                    self.dal.kind,
                    self.table,
                    self.physical_name(),
                    columns,
                    chunk.len(),
                    Some(only),
                )?,
                _ => sqlbuild::multi_write_sql_named(
                    self.dal.kind,
                    self.table,
                    self.physical_name(),
                    columns,
                    chunk.len(),
                    mode,
                )?,
            };
            let mut params = Vec::with_capacity(chunk.len() * columns.len());
            if aligned {
                for row in chunk {
                    for position in &positions {
                        // `aligned` 已保证位置有效
                        if let Some((_, value)) = row.get(position.unwrap_or(0)) {
                            params.push(value.clone());
                        }
                    }
                }
            } else {
                // 防御路径：行内列集与首行不一致时按列名查找（缺失补 NULL）
                for row in chunk {
                    for column in columns {
                        params.push(row.find_value(column).cloned().unwrap_or(DbValue::Null));
                    }
                }
            }
            self.dal.log_sql(&sql);
            affected += session.execute_owned(&sql, params)?;
        }
        Ok(affected)
    }

    /// 多行 Upsert（可指定「只更新哪些列」）：脏列追踪的批量路径。
    ///
    /// `update_only = Some([..])` 时，冲突只更新这些列（插入仍写全列），
    /// 于是「脏列集合相同的行」可以合并成一条语句，而不用逐条 `find + update`。
    /// 方言不支持多行 Upsert 时返回 [`Error::Unsupported`]（调用方回退逐条）。
    pub(crate) fn write_batch_rows_upsert<F: BatchField>(
        &self,
        session: &mut dyn SqlSession,
        rows: &[&Vec<F>],
        update_only: Option<&[&str]>,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        self.ensure_writable()?;
        let Some(first) = rows.first() else {
            return Ok(0);
        };
        if !self.dal.kind.supports_multi_upsert() {
            return Err(Error::Unsupported(format!(
                "{:?} 不支持多行 Upsert 批量写入",
                self.dal.kind
            )));
        }
        let batch = batch_size
            .unwrap_or_else(|| crate::batch::default_insert_batch_size(self.dal.kind))
            .max(1);
        // 列集 = 首行列集；首行自增列为 0/缺失 → 整批排除自增列（对齐 C# `BuildInsertColumns`）
        let identity = self.table.identity().map(|c| c.name.clone());
        let drop_identity = identity.as_ref().is_some_and(|name| {
            first.find_value(name).and_then(DbValue::as_i64).unwrap_or(0) == 0
        });
        let columns: Vec<&str> = (0..first.len())
            .filter_map(|index| first.get(index))
            .map(|(name, _)| name)
            .filter(|name| {
                !(drop_identity
                    && identity
                        .as_deref()
                        .is_some_and(|id| name.eq_ignore_ascii_case(id)))
            })
            .collect();
        if columns.is_empty() {
            return Err(Error::Model(format!(
                "表 {} 的批量写入没有可写字段",
                self.table.name
            )));
        }
        let affected = self.exec_multi_write_upsert(
            session,
            rows,
            &columns,
            batch,
            BatchWriteMode::Upsert,
            update_only,
        )?;
        self.dal.invalidate_cache(self.physical_name());
        Ok(affected)
    }

    /// Upsert 的逐行回退（方言不支持多行 `On Conflict` 时）：查主键存在则更新、否则插入。
    fn upsert_rows_fallback<F: BatchField>(
        &self,
        session: &mut dyn SqlSession,
        rows: &[&Vec<F>],
    ) -> Result<u64> {
        let pks: Vec<String> = self
            .table
            .primary_keys()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        if pks.is_empty() {
            return Err(Error::Model(format!(
                "表 {} 没有主键，无法批量 Upsert",
                self.table.name
            )));
        }
        let identity = self.table.identity().map(|c| c.name.clone());
        let mut affected = 0u64;
        for row in rows {
            let mut pk_values = Vec::with_capacity(pks.len());
            for pk in &pks {
                let value = row.find_value(pk).cloned().ok_or_else(|| {
                    Error::Model(format!("批量 Upsert 行缺少主键列 {pk}"))
                })?;
                pk_values.push(value);
            }
            let exists = self.find_by_pk(session, &pk_values)?.is_some();
            if exists {
                let sets: Vec<(&str, DbValue)> = (0..row.len())
                    .filter_map(|index| row.get(index))
                    .filter(|(k, _)| {
                        !pks.iter().any(|p| p.eq_ignore_ascii_case(k))
                            && !identity.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(k))
                    })
                    .map(|(k, v)| (k, v.clone()))
                    .collect();
                if !sets.is_empty() {
                    affected += self.update_by_pk(session, &sets, &pk_values)?;
                }
            } else {
                let fields: Vec<(&str, DbValue)> = (0..row.len())
                    .filter_map(|index| row.get(index))
                    .map(|(k, v)| (k, v.clone()))
                    .collect();
                let _ = self.insert_prepared(session, &fields)?;
                affected += 1;
            }
        }
        Ok(affected)
    }

    /// 按单列主键批量删除（主键 `IN` 分批；对应 C# `EntityExtension.Delete(list)` 的批量分支）。
    ///
    /// - `pk_field`：主键字段名（由实体层保证是单一主键）；
    /// - `values`：主键值列表（空返回 0；调用方应先过滤 `NULL` 值）；
    /// - `batch_size` 默认 [`crate::batch::DEFAULT_DELETE_BATCH_SIZE`] = 1000（对齐 C#）。
    pub fn delete_by_pk_values(
        &self,
        session: &mut dyn SqlSession,
        pk_field: &str,
        values: &[DbValue],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        self.ensure_writable()?;
        if values.is_empty() {
            return Ok(0);
        }
        let batch = batch_size
            .unwrap_or(crate::batch::DEFAULT_DELETE_BATCH_SIZE)
            .max(1);
        let mut affected = 0u64;
        for chunk in values.chunks(batch) {
            let filter = Where::new().in_(pk_field, chunk.iter().cloned());
            affected += self.delete_where(session, &filter)?;
        }
        Ok(affected)
    }

    /// 分批删除（对应 C# `EntityPersistence.Delete(session, whereClause, maximumRows)`）：
    /// 每批最多 `batch_size` 行（默认 [`crate::batch::DEFAULT_DELETE_WHERE_BATCH_SIZE`] = 10000，
    /// 对齐 C# `Dal.GetBatchSize(10_000)`），批间按 `interval` 等待
    /// （`None` = [`crate::batch::DEFAULT_BATCH_INTERVAL_MS`] 100ms，对齐 C# `XCodeSetting.BatchInterval`；
    /// `Some(0)` = 不等待），避免一次性删除压垮数据库 IO。
    ///
    /// - 方言不支持分批删除时回退为一次性删除（与 C# 相同）；
    /// - `max_rows`：最多删除行数（`None`/`0` = 全部），达到后停止；
    /// - 批内语句不经过拦截器（与 C# 分批删除一致，直接执行 SQL）；返回实际删除行数。
    pub fn delete_where_batched(
        &self,
        session: &mut dyn SqlSession,
        filter: &Where,
        batch_size: Option<usize>,
        max_rows: Option<u64>,
        interval: Option<Duration>,
    ) -> Result<u64> {
        self.ensure_writable()?;
        let batch = batch_size
            .unwrap_or(crate::batch::DEFAULT_DELETE_WHERE_BATCH_SIZE)
            .max(1);
        let wait = interval.unwrap_or_else(|| {
            Duration::from_millis(crate::batch::DEFAULT_BATCH_INTERVAL_MS)
        });
        let max = max_rows.unwrap_or(0);
        let mut total = 0u64;
        loop {
            let size = if max > 0 {
                batch.min((max - total) as usize)
            } else {
                batch
            };
            if size == 0 {
                break;
            }
            let Some((sql, params)) =
                sqlbuild::delete_batched_sql_named(self.dal.kind, self.physical_name(), filter, size)
            else {
                // 方言不支持分批删除：回退一次性删除
                return self.delete_where(session, filter);
            };
            self.dal.log_sql(&sql);
            let rows = session.execute_owned(&sql, params)?;
            total += rows;
            if rows < size as u64 {
                break;
            }
            if max > 0 && total >= max {
                break;
            }
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        self.dal.invalidate_cache(self.physical_name());
        Ok(total)
    }

    /// 按主键查找（主键值按 `TableMeta::primary_keys()` 顺序传入）。
    ///
    /// 表带 `DataScale="timeShard:..."` 且**主键为雪花 Id** 时自动路由到对应分表
    /// （无法从主键推时间时按模型默认表查，与 C# 一致）。
    pub fn find_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<Option<DbRow>> {
        if let Some(spec) = self.shard_spec().cloned()
            && let Ok(shard) = self.auto_shard_model(&spec, None, pk)
        {
            return Ok(self
                .run_on_shard(session, &shard, false, |table, session| {
                    table.find_by_pk_single(session, pk)
                })?
                .flatten());
        }
        self.find_by_pk_single(session, pk)
    }

    /// 按主键查找（**单表**实现）。
    pub(crate) fn find_by_pk_single(
        &self,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
    ) -> Result<Option<DbRow>> {
        let filter = self.pk_filter(pk)?;
        let query = Query::new().filter(filter).take(1);
        let (sql, params) =
            sqlbuild::select_sql_named(self.dal.kind, self.table, self.physical_name(), &query);
        self.dal.log_sql(&sql);
        let set = session.query_owned(&sql, params)?;
        Ok(set.rows.into_iter().next())
    }

    /// 按主键更新，返回受影响行数。
    ///
    /// 表带 `DataScale="timeShard:..."` 时**自动按分表路由**（分片字段优先取 `sets`，
    /// 其次按雪花主键解析；目标分表不存在时返回 0，不建表）。
    pub fn update_by_pk(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&str, DbValue)],
        pk: &[DbValue],
    ) -> Result<u64> {
        if let Some(spec) = self.shard_spec().cloned() {
            return self.update_by_pk_auto_sharded(session, &spec, sets, pk);
        }
        self.update_by_pk_single(session, sets, pk)
    }

    /// 按主键更新（**单表**实现）。
    pub(crate) fn update_by_pk_single(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&str, DbValue)],
        pk: &[DbValue],
    ) -> Result<u64> {
        self.ensure_writable()?;
        let pk_names = self.pk_names();
        if pk_names.len() != pk.len() {
            return Err(Error::Model(format!(
                "表 {} 主键需要 {} 个值，实际传入 {} 个",
                self.table.name,
                pk_names.len(),
                pk.len()
            )));
        }
        // 拦截器刷新审计字段（对应实体拦截器 OnValid）；未启用拦截器时走借用快路
        let (sql, params) = if crate::interceptor::has_registered() {
            let values = crate::interceptor::prepare(
                self.table,
                crate::interceptor::DataMethod::Update,
                sets,
            );
            sqlbuild::update_by_pk_sql_owned_named(
                self.dal.kind,
                self.table,
                self.physical_name(),
                values,
                &pk_names,
                pk,
                &[],
            )?
        } else {
            sqlbuild::update_by_pk_sql_named(
                self.dal.kind,
                self.table,
                self.physical_name(),
                sets,
                &pk_names,
                pk,
                &[],
            )?
        };
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        if affected > 0 {
            // 写入使缓存失效（对应 DH.NCode：任何添删改都让缓存马上过期）
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// 按主键更新（**静态列名**版本；有拦截器时用 Cow 列名，零列名分配）。
    ///
    /// 供实体层（列名来自 `rcodegen` 的 `'static` 字面量）使用；无拦截器时直接走借用快路。
    pub fn update_by_pk_static(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&'static str, DbValue)],
        pk: &[DbValue],
    ) -> Result<u64> {
        if !crate::interceptor::has_registered() {
            return self.update_by_pk_single(session, sets, pk);
        }
        self.ensure_writable()?;
        let pk_names = self.pk_names();
        if pk_names.len() != pk.len() {
            return Err(Error::Model(format!(
                "表 {} 主键需要 {} 个值，实际传入 {} 个",
                self.table.name,
                pk_names.len(),
                pk.len()
            )));
        }
        let values = crate::interceptor::prepare_static(
            self.table,
            crate::interceptor::DataMethod::Update,
            sets,
        );
        let (sql, params) = sqlbuild::update_by_pk_sql_owned_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            values,
            &pk_names,
            pk,
            &[],
        )?;
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        if affected > 0 {
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// 条件更新（静态列名版本；语义同 [`TableRef::update_if`]）。
    pub fn update_if_static(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&'static str, DbValue)],
        pk: &[DbValue],
        expected: &[(&str, DbValue)],
    ) -> Result<u64> {
        if !crate::interceptor::has_registered() {
            return self.update_if(session, sets, pk, expected);
        }
        self.ensure_writable()?;
        if sets.is_empty() {
            return Err(Error::Argument("条件更新至少需要一个待更新字段".into()));
        }
        if self.shard_spec().is_some() {
            return Err(Error::Unsupported(format!(
                "表 {} 是分表，条件更新请用具体分表句柄",
                self.table.name
            )));
        }
        let pk_names = self.pk_names();
        if pk_names.len() != pk.len() {
            return Err(Error::Model(format!(
                "表 {} 主键需要 {} 个值，实际传入 {} 个",
                self.table.name,
                pk_names.len(),
                pk.len()
            )));
        }
        let values = crate::interceptor::prepare_static(
            self.table,
            crate::interceptor::DataMethod::Update,
            sets,
        );
        let (sql, params) = sqlbuild::update_by_pk_sql_owned_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            values,
            &pk_names,
            pk,
            expected,
        )?;
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        // 冲突也要失效缓存（同 update_if，否则重试会一直读旧值）
        self.dal.invalidate_cache(self.physical_name());
        Ok(affected)
    }

    /// 插入一行（静态列名版本；有拦截器时 Cow 零列名分配）。
    pub fn insert_static(
        &self,
        session: &mut dyn SqlSession,
        fields: &[(&'static str, DbValue)],
    ) -> Result<i64> {
        if !crate::interceptor::has_registered() || self.shard_spec().is_some() {
            return self.insert(session, fields);
        }
        self.ensure_writable()?;
        let values = crate::interceptor::prepare_static(
            self.table,
            crate::interceptor::DataMethod::Insert,
            fields,
        );
        let (sql, params) = sqlbuild::insert_sql_owned_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            values,
        )?;
        self.execute_insert_sql(session, sql, params)
    }

    /// 按条件更新（`UPDATE ... SET ... WHERE ...`，一条语句改多行），返回受影响行数。
    ///
    /// 对应 C# `EntityPersistence.Update` 的"按条件改同值"用法（批量改状态、批量打标等）：
    /// 比逐行 `update_by_pk` 少 N-1 次往返。**不同行不同值仍请用 `update_batch`（逐条）**。
    ///
    /// - 视图表拒绝（与其它写操作一致）；
    /// - 拦截器按 `Update` 刷新审计字段；
    /// - 空条件会更新全表，请先用 [`Where::is_empty`] 防护（与 `delete_where` 同规矩）。
    pub fn update_where(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&str, DbValue)],
        filter: &Where,
    ) -> Result<u64> {
        self.ensure_writable()?;
        if sets.is_empty() {
            return Err(Error::Argument("按条件更新至少需要一个待更新字段".into()));
        }
        let values = crate::interceptor::prepare(
            self.table,
            crate::interceptor::DataMethod::Update,
            sets,
        );
        let (sql, params) = sqlbuild::update_sql_owned_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            values,
            filter,
        )?;
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        if affected > 0 {
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// **条件更新（CAS / 乐观锁）**：仅当库内 `expected` 各列仍等于给定旧值时才写入 `sets`。
    ///
    /// 一条 `UPDATE ... SET ... WHERE 主键 AND 列=旧值` 完成“检查 + 更新”，
    /// 多应用共享同一张表时用它防止把旧值覆盖到别的系统刚写入的数据上
    /// （见 `docs/使用规范.md` §4.5）。
    ///
    /// - 返回受影响行数：`1` = 成功；`0` = 主键不存在**或**任一期望列已被修改（统一视为冲突）；
    /// - `expected` 传**读取时的旧值**（不是“实体当前值”）；空时等价 [`TableRef::update_by_pk`]；
    /// - **零额外往返、零新线程**：条件与 `SET` 在同一条语句里；参数比 `update_by_pk` 多
    ///   `expected.len()` 个，`sets`/`expected` 均为借用切片；
    /// - 冲突（0 行）时**也会失效本表缓存**：否则调用方重试会继续命中旧缓存、永远冲突；
    /// - 分表（`DataScale="timeShard:..."`）表请用具体分表句柄调用（本方法不做自动路由）；
    /// - 视图/仅插入表拒绝。
    pub fn update_if(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&str, DbValue)],
        pk: &[DbValue],
        expected: &[(&str, DbValue)],
    ) -> Result<u64> {
        self.ensure_writable()?;
        if sets.is_empty() {
            return Err(Error::Argument("条件更新至少需要一个待更新字段".into()));
        }
        if self.shard_spec().is_some() {
            return Err(Error::Unsupported(format!(
                "表 {} 是分表，条件更新请用具体分表句柄",
                self.table.name
            )));
        }
        let pk_names = self.pk_names();
        if pk_names.len() != pk.len() {
            return Err(Error::Model(format!(
                "表 {} 主键需要 {} 个值，实际传入 {} 个",
                self.table.name,
                pk_names.len(),
                pk.len()
            )));
        }
        // 拦截器刷新审计字段（对应实体拦截器 OnValid）；未启用拦截器时走借用快路
        let (sql, params) = if crate::interceptor::has_registered() {
            let values = crate::interceptor::prepare(
                self.table,
                crate::interceptor::DataMethod::Update,
                sets,
            );
            sqlbuild::update_by_pk_sql_owned_named(
                self.dal.kind,
                self.table,
                self.physical_name(),
                values,
                &pk_names,
                pk,
                expected,
            )?
        } else {
            sqlbuild::update_by_pk_sql_named(
                self.dal.kind,
                self.table,
                self.physical_name(),
                sets,
                &pk_names,
                pk,
                expected,
            )?
        };
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        // 成功 → 本进程缓存过期；冲突（0 行）→ 说明本进程读到的值已过期，同样必须失效，
        // 否则调用方重读会继续命中旧缓存、永远冲突。
        self.dal.invalidate_cache(self.physical_name());
        Ok(affected)
    }

    /// 按主键删除，返回受影响行数。
    ///
    /// 表带 `DataScale="timeShard:..."` 时**自动按分表路由**（按雪花主键解析时间；
    /// 目标分表不存在时返回 0）。
    pub fn delete_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<u64> {
        if let Some(spec) = self.shard_spec().cloned() {
            return self.delete_by_pk_auto_sharded(session, &spec, pk);
        }
        self.delete_by_pk_single(session, pk)
    }

    /// 按主键删除（**单表**实现）。
    pub(crate) fn delete_by_pk_single(
        &self,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
    ) -> Result<u64> {
        let filter = self.pk_filter(pk)?;
        self.delete_where_single(session, &filter)
    }

    /// 按唯一键查一行（**小表自动走整表缓存，零 SQL**）。
    ///
    /// 与 [`Entity::find_cached_by`](crate::Entity::find_cached_by) 同一套语义，供
    /// **手写结构体 + 表级 API** 的消费方使用（不依赖对象实体）：
    /// - 表行数 < `Model.xml` 的 `MaxCacheCount` → 整表缓存内存匹配（字符串忽略大小写）；
    /// - 否则退化为 `WHERE ... LIMIT 1`。
    ///
    /// 返回 `None` 表示没查到（或缓存里没有匹配项）。
    pub fn find_cached_by(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        key: &[(&str, DbValue)],
    ) -> Result<Option<crate::session::DbRow>> {
        if key.is_empty() {
            return Err(Error::Argument("按唯一键查询至少需要一个键".into()));
        }
        let max = self.table.max_cache_count;
        if max > 0 && dal.cached_count(&self.table.name, session)? < i64::from(max) {
            let cache = dal.entity_cache(&self.table.name)?;
            let rows = cache.entities(dal, session)?;
            for row in rows.iter() {
                if key
                    .iter()
                    .all(|(column, value)| crate::entity::row_value_matches(row.get_by_name(column), value))
                {
                    return Ok(Some(row.clone()));
                }
            }
            return Ok(None);
        }
        let mut filter = Where::new();
        for (column, value) in key {
            filter = filter.eq(*column, value.clone());
        }
        let set = self.query(session, &Query::new().filter(filter).take(1))?;
        Ok(set.rows.into_iter().next())
    }

    /// **原子累加**（对应 DH.NCode 的 `AdditionalFields` / `Count = Count + n` 语义）。
    ///
    /// 由数据库在一条语句内完成「读-加-写」，并发安全、不会丢更新；
    /// 比「读出实体 → 改值 → Save」少一次查询且无并发风险。
    ///
    /// - `deltas`：累加列与增量（必须是数值列；空则报错）；
    /// - `filter`：条件（通常传主键；为空即全表，慎用）。
    ///
    /// 拦截器仍会补 `UpdateTime`（普通赋值），写入后相关缓存失效。
    pub fn increment(
        &self,
        session: &mut dyn SqlSession,
        deltas: &[(&str, DbValue)],
        filter: &Where,
    ) -> Result<u64> {
        self.ensure_writable()?;
        if deltas.is_empty() {
            return Err(Error::Argument("原子累加至少需要一个累加字段".into()));
        }
        // 拦截器会把 `UpdateTime` 之类补进待写列；它们应作为「普通赋值」而非累加
        let prepared = crate::interceptor::prepare(
            self.table,
            crate::interceptor::DataMethod::Update,
            deltas,
        );
        let names: Vec<String> = deltas.iter().map(|(n, _)| n.to_string()).collect();
        let (delta_values, assign_values): (Vec<_>, Vec<_>) = prepared
            .into_iter()
            .partition(|(name, _)| names.iter().any(|n| n.eq_ignore_ascii_case(name)));
        let (sql, params) = sqlbuild::increment_sql_owned_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            delta_values,
            assign_values,
            filter,
        )?;
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        if affected > 0 {
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// **按主键原子累加**（[`TableRef::increment`] 的便捷版）。
    pub fn increment_by_pk(
        &self,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
        deltas: &[(&str, DbValue)],
    ) -> Result<u64> {
        let filter = self.pk_filter(pk)?;
        self.increment(session, deltas, &filter)
    }

    /// **按业务唯一键批量 Upsert**：一条语句写入多行，冲突时更新（外部唯一键不是自增主键）。
    ///
    /// 典型场景：设备在线表「一台设备一行」——表主键是自增 `Id`，业务唯一键是 `DeviceId`
    /// （模型里已声明唯一索引）。心跳聚合后按 `DeviceId` 批量写入即可，无需先查再插/改。
    ///
    /// 方言差异：
    /// - SQLite / DuckDB / PostgreSQL：`INSERT ... ON CONFLICT (指定列) DO UPDATE SET 列=excluded.列`；
    /// - MySQL：`INSERT ... ON DUPLICATE KEY UPDATE 列=VALUES(列)`（命中外键的任意唯一键）。
    ///
    /// # 参数
    /// - `session`：会话。
    /// - `columns`：插入列（所有行共用）；自增列应放在列集里并传 `DbValue::Null` 或 0，
    ///   或干脆不包含（推荐，让数据库生成）。
    /// - `conflict_columns`：冲突判定列（通常是唯一索引列，如 `DeviceId`）。
    /// - `rows`：每行的值，顺序与 `columns` 一致。
    /// - `batch_size`：单条语句的最大行数（`None` = 500）。
    ///
    /// # 返回值
    /// 受影响行数合计。
    pub fn upsert_batch_by_unique(
        &self,
        session: &mut dyn SqlSession,
        columns: &[&str],
        conflict_columns: &[&str],
        rows: &[Vec<DbValue>],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        self.ensure_writable()?;
        if columns.is_empty() || rows.is_empty() {
            return Err(Error::Argument("批量 Upsert 至少需要一个列与一行".into()));
        }
        if conflict_columns.is_empty() {
            return Err(Error::Argument("批量 Upsert 必须指定冲突列".into()));
        }
        let batch = batch_size.unwrap_or(500).max(1);
        let mut affected = 0u64;
        for chunk in rows.chunks(batch) {
            if let Some(bad) = chunk.iter().find(|row| row.len() != columns.len()) {
                return Err(Error::Argument(format!(
                    "批量 Upsert 行值个数 {} 与列数 {} 不一致",
                    bad.len(),
                    columns.len()
                )));
            }
            let sql = sqlbuild::multi_write_sql_upsert_by_named(
                self.dal.kind,
                self.table,
                self.physical_name(),
                columns,
                conflict_columns,
                chunk.len(),
                None,
            )?;
            let mut params: Vec<DbValue> = Vec::with_capacity(chunk.len() * columns.len());
            for row in chunk {
                params.extend(row.iter().cloned());
            }
            self.dal.log_sql(&sql);
            affected += session.execute_owned(&sql, params)?;
        }
        if affected > 0 {
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// **批量按主键更新**：一条 `UPDATE` 更新多行（`SET 列 = CASE 主键 WHEN ? THEN ? ... END`）。
    ///
    /// 用途：把 N 条 `UPDATE` 合成 1 条语句——在 MySQL/PostgreSQL 等网络库上，
    /// 往返次数从 N 降到 1（往返是这类库的主要成本）；心跳/状态类高频写入的批量落库入口。
    ///
    /// 约束：
    /// - 表必须有**单列主键**（复合主键请逐行调用 [`TableRef::update_by_pk`]）；
    /// - `columns` 是所有行共用的列集，`rows` 每行为 `(主键值, 各列值)`，值顺序与 `columns` 一致；
    /// - 不经过拦截器（调用方需要的话自行把 `UpdateTime` 放进 `columns`）。
    ///
    /// # 参数
    /// - `session`：会话。
    /// - `columns`：待更新列（所有行共用）。
    /// - `rows`：`(主键值, 各列值)` 列表（不能为空）。
    ///
    /// # 返回值
    /// 受影响行数。
    pub fn update_batch_by_pk(
        &self,
        session: &mut dyn SqlSession,
        columns: &[&str],
        rows: &[(DbValue, Vec<DbValue>)],
    ) -> Result<u64> {
        self.ensure_writable()?;
        if columns.is_empty() || rows.is_empty() {
            return Err(Error::Argument("批量更新至少需要一个列与一行".into()));
        }
        let pks = self.table.primary_keys();
        if pks.len() != 1 {
            return Err(Error::Model(format!(
                "表 {} 批量按主键更新要求单列主键（当前 {} 列）",
                self.table.name,
                pks.len()
            )));
        }
        let pk_name = self.table.effective_column_name(pks[0]);
        let (sql, params) = sqlbuild::update_batch_by_pk_sql_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            columns,
            pk_name,
            rows,
        )?;
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        if affected > 0 {
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// **原子累加 + 普通赋值混合更新**：`SET 累加列 = 累加列 + n, 赋值列 = ?`（一条语句）。
    ///
    /// 用于「计数与快照一起写」的场景（例如心跳：`Pings = Pings + 1` 与内存/CPU 指标同句写入），
    /// 相比先 [`TableRef::update_where`] 再 [`TableRef::increment`] **少一次数据库往返**——
    /// 在 MySQL/PostgreSQL 这类网络库上，一次往返通常就是整次写入的主要成本。
    ///
    /// 语义说明：
    /// - 数据库在一条语句内完成「读-加-写」，并发安全、不会丢计数；
    /// - 因累加列必然改变取值，**受影响行数 > 0 即代表目标行存在**（MySQL 默认只回报"实际改变"的行），
    ///   调用方可用它替代"先查是否存在"；
    /// - 拦截器补的 `UpdateTime` 仍按普通赋值处理。
    ///
    /// # 参数
    /// - `session`：会话。
    /// - `deltas`：累加列与增量（不能为空）。
    /// - `assigns`：普通赋值列（可为空）。
    /// - `filter`：条件（通常传主键或索引列）。
    ///
    /// # 返回值
    /// 受影响行数。
    pub fn increment_with(
        &self,
        session: &mut dyn SqlSession,
        deltas: &[(&str, DbValue)],
        assigns: &[(&str, DbValue)],
        filter: &Where,
    ) -> Result<u64> {
        self.ensure_writable()?;
        if deltas.is_empty() {
            return Err(Error::Argument("原子累加至少需要一个累加字段".into()));
        }
        let mut fields: Vec<(&str, DbValue)> = Vec::with_capacity(deltas.len() + assigns.len());
        fields.extend_from_slice(deltas);
        fields.extend_from_slice(assigns);
        let prepared = crate::interceptor::prepare(
            self.table,
            crate::interceptor::DataMethod::Update,
            &fields,
        );
        let names: Vec<String> = deltas.iter().map(|(n, _)| n.to_string()).collect();
        let (delta_values, assign_values): (Vec<_>, Vec<_>) = prepared
            .into_iter()
            .partition(|(name, _)| names.iter().any(|n| n.eq_ignore_ascii_case(name)));
        let (sql, params) = sqlbuild::increment_sql_owned_named(
            self.dal.kind,
            self.table,
            self.physical_name(),
            delta_values,
            assign_values,
            filter,
        )?;
        self.dal.log_sql(&sql);
        let affected = session.execute_owned(&sql, params)?;
        if affected > 0 {
            self.dal.invalidate_cache(self.physical_name());
        }
        Ok(affected)
    }

    /// 自然键 upsert：按 `keys` 查行（取主键），命中则 `update_by_pk` 更新 `values`，
    /// 未命中则插入 `keys + values + insert_extra`（`insert_extra` 为“仅插入”初值列，
    /// 如统计计数置零；更新路径不碰这些列）；返回是否为插入（true = 新增）。
    ///
    /// `keys` 不得为空（空键会退化为“取首行更新”的危险语义，直接拒绝）。
    pub fn upsert_by_key(
        &self,
        session: &mut dyn SqlSession,
        keys: &[(&str, DbValue)],
        values: &[(&str, DbValue)],
        insert_extra: &[(&str, DbValue)],
    ) -> Result<bool> {
        if keys.is_empty() {
            return Err(Error::Argument("自然键不能为空".into()));
        }
        let mut filter = Where::new();
        for (name, value) in keys {
            filter = filter.eq(*name, value.clone());
        }
        let found = self.query(
            session,
            &Query::new().column("Id").filter(filter).take(1),
        )?;
        if let Some(row) = found.first() {
            let id = row
                .get_by_name("Id")
                .and_then(|v| v.as_i64())
                .unwrap_or_default();
            self.update_by_pk(session, values, &[id.into()])?;
            Ok(false)
        } else {
            // 合并插入字段：keys + values + insert_extra，**同名列只保留先出现者**
            // （优先级 keys > values > insert_extra）。
            //
            // 必须去重：调用方常在 `values` 里已提供某列（如代理注册时上报的
            // `DefaultPassword`），同时又在 `insert_extra` 里给了该列的仅插入初值，
            // 不去重会生成 `INSERT INTO t (.., X, .., X) VALUES (..)`，
            // MySQL 报 1110 `Column 'X' specified twice`、SQLite 报 duplicate column name。
            fn push_unique<'a>(
                fields: &mut Vec<(&'a str, DbValue)>,
                name: &'a str,
                value: &DbValue,
            ) {
                if !fields.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
                    fields.push((name, value.clone()));
                }
            }
            let mut fields: Vec<(&str, DbValue)> =
                Vec::with_capacity(keys.len() + values.len() + insert_extra.len());
            for (name, value) in keys {
                push_unique(&mut fields, name, value);
            }
            for (name, value) in values {
                push_unique(&mut fields, name, value);
            }
            for (name, value) in insert_extra {
                push_unique(&mut fields, name, value);
            }
            self.insert(session, &fields)?;
            Ok(true)
        }
    }

    /// 按条件删除，返回受影响行数（空条件会清空全表，请先用 [`Where::is_empty`] 防护）。
    ///
    /// 对应 C# `Entity.Delete(Expression)`；表带 `DataScale="timeShard:..."` 时**自动按分表路由**
    /// （语义见 [`crate::shards`]）。
    pub fn delete_where(&self, session: &mut dyn SqlSession, filter: &Where) -> Result<u64> {
        if let Some(spec) = self.shard_spec().cloned() {
            return self.delete_sharded(session, &crate::shards::policy_of(&spec), filter);
        }
        self.delete_where_single(session, filter)
    }

    /// 按条件删除（**单表**实现；分表引擎内部直接调用，不再分流）。
    pub(crate) fn delete_where_single(
        &self,
        session: &mut dyn SqlSession,
        filter: &Where,
    ) -> Result<u64> {
        self.ensure_writable()?;
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
        let affected = session.execute_owned(&sql, params)?;
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
    /// **`fields` 直接作为 `SET` 的列集**：只想改少数列就只传这些列；传全字段即整行覆盖
    /// （多个应用共享同一张表时，传全字段会把旧值写回——见 `docs/使用规范.md` §4.5）。
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

    /// 统计行数（表带 `DataScale="timeShard:..."` 时自动跨分表统计）。
    pub fn count(&self, session: &mut dyn SqlSession, filter: Option<&Where>) -> Result<i64> {
        if let Some(spec) = self.shard_spec().cloned() {
            return self.count_sharded(session, &crate::shards::policy_of(&spec), filter);
        }
        self.count_single(session, filter)
    }

    /// 统计行数（**单表**实现）。
    pub(crate) fn count_single(
        &self,
        session: &mut dyn SqlSession,
        filter: Option<&Where>,
    ) -> Result<i64> {
        let (sql, params) =
            sqlbuild::count_sql_named(self.dal.kind, self.physical_name(), filter);
        self.dal.log_sql(&sql);
        let set = session.query_owned(&sql, params)?;
        Ok(set
            .first()
            .and_then(|r| r.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0))
    }

    /// 查询（表带 `DataScale="timeShard:..."` 时自动跨分表查询 + 跨表续页）。
    pub fn query(&self, session: &mut dyn SqlSession, query: &Query) -> Result<RowSet> {
        if let Some(spec) = self.shard_spec().cloned() {
            return self.query_sharded(session, &crate::shards::policy_of(&spec), query);
        }
        self.query_single(session, query)
    }

    /// 查询（**单表**实现）。
    pub(crate) fn query_single(
        &self,
        session: &mut dyn SqlSession,
        query: &Query,
    ) -> Result<RowSet> {
        let (sql, params) =
            sqlbuild::select_sql_named(self.dal.kind, self.table, self.physical_name(), query);
        self.dal.log_sql(&sql);
        session.query_owned(&sql, params)
    }

    /// 主键物理列名（按模型顺序；数量与值列表对齐）。
    fn pk_names(&self) -> Vec<&str> {
        self.table
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| self.table.effective_column_name(column))
            .collect()
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
            category: None,
            item_type: None,
            using_cache: None,
            extra: Vec::new(),
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
    fn upsert_by_key_inserts_then_updates() {
        let dir = temp_dir("upsert");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();

        let mut session = dal.open_session().unwrap();
        let table = dal.table("Order").unwrap();
        let now = chrono::Local::now().naive_local();

        // 首次：插入（CreateTime 为“仅插入”初值列）
        let keys: [(&str, DbValue); 1] = [("Code", "A001".into())];
        assert!(table
            .upsert_by_key(
                session.as_mut(),
                &keys,
                &[("Status", 1i32.into())],
                &[("CreateTime", now.into())],
            )
            .unwrap());

        // 再次：更新（不新增行；insert_extra 不参与更新）
        let later = now + chrono::Duration::seconds(5);
        assert!(!table
            .upsert_by_key(
                session.as_mut(),
                &keys,
                &[("Status", 2i32.into())],
                &[("CreateTime", later.into())],
            )
            .unwrap());
        let rows = table
            .query(
                session.as_mut(),
                &Query::new().filter(Where::new().eq("Code", "A001")),
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "自然键不应产生重复行");
        assert_eq!(
            rows.rows[0].get_by_name("Status").and_then(|v| v.as_i32()),
            Some(2)
        );
        assert_eq!(
            rows.rows[0].get_by_name("CreateTime").and_then(|v| v.as_datetime()),
            Some(now),
            "insert_extra 不应在更新时被覆盖"
        );

        // 空自然键：直接拒绝
        assert!(table
            .upsert_by_key(session.as_mut(), &[], &[], &[])
            .is_err());

        drop(session);
        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_by_key_dedupes_columns_between_values_and_insert_extra() {
        // 回归：values 与 insert_extra 出现同名列（代理注册上报 DefaultPassword 的场景）
        // 曾生成重复列名，MySQL 报 `1110 Column 'X' specified twice`。
        let dir = temp_dir("upsert-dup");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();

        let mut session = dal.open_session().unwrap();
        let table = dal.table("Order").unwrap();
        let keys: [(&str, DbValue); 1] = [("Code", "B001".into())];
        // Status 同时出现在 values 与 insert_extra：应只插入一次，且保留 values 的值
        assert!(table
            .upsert_by_key(
                session.as_mut(),
                &keys,
                &[("Status", 7i32.into())],
                &[("Status", 0i32.into()), ("CreateTime", chrono::Local::now().naive_local().into())],
            )
            .unwrap());
        let rows = table
            .query(
                session.as_mut(),
                &Query::new().filter(Where::new().eq("Code", "B001")),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows.rows[0].get_by_name("Status").and_then(|v| v.as_i32()),
            Some(7),
            "values 应优先于 insert_extra 的仅插入初值"
        );

        drop(session);
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

    /// 测试用列定义（只关心类型与长度）。
    fn col_meta(data_type: crate::types::DataType, length: i32) -> ColumnMeta {
        ColumnMeta {
            name: "Code".into(),
            column_name: None,
            data_type,
            raw_type: None,
            length,
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
            category: None,
            item_type: None,
            using_cache: None,
            extra: Vec::new(),
        }
    }

    /// 测试用列目录信息。
    fn col_info(raw_type: &str, data_type: crate::types::DataType, length: i32) -> ColumnInfo {
        ColumnInfo {
            name: "Code".into(),
            raw_type: raw_type.into(),
            data_type,
            length,
            precision: 0,
            scale: 0,
            identity: false,
            primary_key: false,
            nullable: true,
            default_value: None,
            description: String::new(),
        }
    }

    #[test]
    fn string_length_change_rules() {
        let dir = temp_dir("len-rules");
        let db = dir.join("test.db");
        let dal = Dal::open(&format!("Data Source={};Provider=SQLite", db.display())).unwrap();
        let s = crate::types::DataType::String;

        // 扩大：On 档即可执行
        assert_eq!(
            dal.string_length_change(&col_meta(s, 100), &col_info("nvarchar(20)", s, 20)),
            Some((100, 20, true))
        );
        // 缩窄：仅 Full 档
        assert_eq!(
            dal.string_length_change(&col_meta(s, 20), &col_info("nvarchar(100)", s, 100)),
            Some((20, 100, false))
        );
        // 模型不限长度（0）→ 缩窄
        assert_eq!(
            dal.string_length_change(&col_meta(s, 0), &col_info("nvarchar(100)", s, 100)),
            Some((0, 100, false))
        );
        // 两侧都是大文本 → 长度无意义，忽略
        assert_eq!(
            dal.string_length_change(&col_meta(s, 0), &col_info("text", s, 0)),
            None
        );
        // 库侧 text/ntext/sysname 白名单 → 忽略
        assert_eq!(
            dal.string_length_change(&col_meta(s, 5000), &col_info("text", s, 0)),
            None
        );
        assert_eq!(
            dal.string_length_change(&col_meta(s, 50), &col_info("ntext", s, 0)),
            None
        );
        // 长度相同 / 非字符串列不参与
        assert_eq!(
            dal.string_length_change(&col_meta(s, 50), &col_info("nvarchar(50)", s, 50)),
            None
        );
        assert_eq!(
            dal.string_length_change(
                &col_meta(crate::types::DataType::Int32, 0),
                &col_info("int", crate::types::DataType::Int32, 0)
            ),
            None
        );

        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_schema_aligns_string_length_by_mode() {
        const M3: &str = r#"<EntityModel><Tables><Table Name="T3" TableName="DH_T3">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="100" />
          </Columns>
        </Table></Tables></EntityModel>"#;

        let dir = temp_dir("len-sync");

        // ① 库中 Code 为 nvarchar(20)，模型要求 100 → 放宽：On 档即尝试（SQLite 不支持 ALTER COLUMN，落 notes）
        let db = dir.join("widen.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        {
            let setup = Dal::open(&conn).unwrap();
            let mut session = setup.open_session().unwrap();
            session
                .execute(
                    "CREATE TABLE \"DH_T3\" (\"Id\" integer PRIMARY KEY AUTOINCREMENT, \"Code\" nvarchar(20))",
                    &[],
                )
                .unwrap();
            drop(session);
            setup.clear_pool();
            drop(setup);
        }
        let dal = Dal::open_with_model(&conn, EntityModel::parse(M3).unwrap()).unwrap();
        let diff = dal.diff_schema().unwrap();
        assert_eq!(diff.length_mismatches.len(), 1, "{diff:?}");
        let m = &diff.length_mismatches[0];
        assert_eq!((m.expected, m.actual, m.widening), (100, 20, true), "{diff:?}");
        let report = dal.sync_schema().unwrap();
        assert!(
            report.notes.iter().any(|n| n.contains("长度 20 → 100")),
            "On 档应尝试放宽长度：{report:?}"
        );
        dal.clear_pool();
        drop(dal);

        // ② 反方向（库 300、模型 100）= 缩窄：On 档跳过（无 note），Full 档才处理
        let db = dir.join("narrow.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        {
            let setup = Dal::open(&conn).unwrap();
            let mut session = setup.open_session().unwrap();
            session
                .execute(
                    "CREATE TABLE \"DH_T3\" (\"Id\" integer PRIMARY KEY AUTOINCREMENT, \"Code\" nvarchar(300))",
                    &[],
                )
                .unwrap();
            drop(session);
            setup.clear_pool();
            drop(setup);
        }
        let dal = Dal::open_with_model(&conn, EntityModel::parse(M3).unwrap()).unwrap();
        let diff = dal.diff_schema().unwrap();
        assert_eq!(diff.length_mismatches.len(), 1, "{diff:?}");
        assert!(!diff.length_mismatches[0].widening, "{diff:?}");
        let report = dal.sync_schema().unwrap();
        assert!(
            report.notes.is_empty(),
            "On 档不应缩窄列长度：{report:?}"
        );
        assert!(report.is_empty(), "{report:?}");
        dal.clear_pool();
        drop(dal);

        // ③ Full 档：缩窄会尝试执行（SQLite 不支持 ALTER COLUMN → 落 notes 提示人工处理）
        let conn = format!(
            "Data Source={};Provider=SQLite;Migration=Full",
            dir.join("narrow.db").display()
        );
        let dal = Dal::open_with_model(&conn, EntityModel::parse(M3).unwrap()).unwrap();
        assert_eq!(dal.migration(), Migration::Full);
        let report = dal.sync_schema().unwrap();
        assert!(
            report.notes.iter().any(|n| n.contains("长度 300 → 100")),
            "Full 档应处理缩窄：{report:?}"
        );
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
