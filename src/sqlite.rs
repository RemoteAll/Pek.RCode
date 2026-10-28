//! SQLite 驱动：对应 DH.NCode 的 `SQLite.cs`（基于 rusqlite，内嵌 SQLite 无需外部依赖）。
//!
//! 打开连接时套用与 XCode 一致的优化参数（WAL 日志、忙等待超时），
//! 保证与 C# 版共用同一个 `DG.db` 文件时的行为一致。

use rusqlite::types::{ToSql, ToSqlOutput, Value as SqlValue, ValueRef};

use std::collections::HashMap;
use std::sync::Arc;

use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::session::{DbRow, RowSet, SqlSession};
use crate::value::{DbValue, format_datetime};

/// 预编译语句缓存容量（按 SQL 文本命中；ORM 生成的 SQL 高度同构，一次编译反复复用）。
///
/// 单张表一次 `insert`/`select` 只产生 1 条 SQL，几十张表的常规业务 ≤ 64 条即可覆盖；
/// 超出后按 LRU 淘汰，不影响正确性。
pub const STATEMENT_CACHE_CAPACITY: usize = 64;

/// 列名缓存容量（SQL → 列名列表）：超出后整表清空（动态 SQL 场景不会无界增长）。
pub const COLUMN_CACHE_CAPACITY: usize = 128;

/// 默认同步模式：**Off**（对齐 DH.NCode `SQLite.cs` 的 `Synchronous=Off`，高频写入最快）。
///
/// 代价：断电/硬重启可能损坏数据库（C# 源码注释亦明确承认）。
/// 需要强一致的项目请在连接串显式设 `Synchronous=Normal`。
pub const DEFAULT_SYNCHRONOUS: SqliteSynchronous = SqliteSynchronous::Off;

/// 默认忙等待毫秒（多连接/多进程竞争时等待而不是直接报 `database is locked`）。
pub const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5000;

/// SQLite 同步模式（连接串 `Synchronous=`，与 C# 同名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqliteSynchronous {
    /// `OFF`（默认，对齐 C#）：最快；断电可能损坏数据库
    #[default]
    Off,
    /// `NORMAL`：WAL 下推荐；掉电可能丢最近事务，但不会损坏
    Normal,
    /// `FULL`
    Full,
    /// `EXTRA`
    Extra,
}

impl SqliteSynchronous {
    /// PRAGMA 值。
    pub fn pragma(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
            Self::Extra => "EXTRA",
        }
    }

    /// 解析（忽略大小写；未知值返回 `None`，由调用方回退默认）。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Some(Self::Off),
            "normal" | "1" => Some(Self::Normal),
            "full" | "2" => Some(Self::Full),
            "extra" | "3" => Some(Self::Extra),
            _ => None,
        }
    }
}

/// SQLite 日志模式（连接串 `Journal Mode=`，与 C# 同名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqliteJournalMode {
    /// `WAL`（默认，对齐 C#）：读写并发更好
    #[default]
    Wal,
    /// `DELETE`（回滚日志）
    Delete,
    /// `TRUNCATE`
    Truncate,
    /// `PERSIST`
    Persist,
    /// `MEMORY`（风险高，不推荐）
    Memory,
    /// `OFF`（无日志，风险最高）
    Off,
}

impl SqliteJournalMode {
    /// PRAGMA 值。
    pub fn pragma(self) -> &'static str {
        match self {
            Self::Wal => "WAL",
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
            Self::Persist => "PERSIST",
            Self::Memory => "MEMORY",
            Self::Off => "OFF",
        }
    }

    /// 解析（忽略大小写；未知值返回 `None`）。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "wal" => Some(Self::Wal),
            "delete" => Some(Self::Delete),
            "truncate" => Some(Self::Truncate),
            "persist" => Some(Self::Persist),
            "memory" => Some(Self::Memory),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }
}

/// SQLite 连接参数（默认值对齐 C# DH.NCode：`Journal Mode=WAL` + `Synchronous=Off`）。
///
/// 连接串可覆盖（键名与 C# 一致、大小写不敏感）：
/// `Synchronous=Off|Normal|Full|Extra`、`Cache Size=-8192`（KiB，负数为单位）、
/// `Journal Mode=WAL|Delete|...`、`BusyTimeout=5000`（毫秒）、`Readonly=true`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteOptions {
    /// 日志模式（`None` = 不设置；只读连接使用，避免在只读文件上写 WAL）
    pub journal_mode: Option<SqliteJournalMode>,
    /// 同步模式
    pub synchronous: SqliteSynchronous,
    /// 页缓存（KiB，负数；`None` = 用 SQLite 默认）。C# 默认 512 MiB/连接，本库不默认跟随
    pub cache_size_kib: Option<i64>,
    /// 忙等待毫秒（0 = 不设置）
    pub busy_timeout_ms: u64,
    /// 只读连接（不设置 journal_mode）
    pub read_only: bool,
    /// **只读池**连接（`PRAGMA query_only=ON`）：连接本身可写，但 SQLite 层直接拒绝任何写入。
    ///
    /// 与 [`Self::read_only`] 的区别：`read_only` 是“这个库文件只能读”（连 WAL 都不设），
    /// `query_only` 是“这个连接只允许 SELECT”—— 用于 `SharedStore` 的只读池，
    /// 同一个 WAL 库可以有 1 个写连接 + N 个 `query_only` 读连接。
    pub query_only: bool,
}

impl Default for SqliteOptions {
    fn default() -> Self {
        Self {
            journal_mode: Some(SqliteJournalMode::Wal),
            synchronous: DEFAULT_SYNCHRONOUS,
            cache_size_kib: None,
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            read_only: false,            query_only: false,        }
    }
}

impl SqliteOptions {
    /// 从连接串解析（键名与 C# 一致；未知/非法值回退默认）。
    pub fn from_connection_string(conn: &crate::dal::ConnectionString) -> Self {
        let mut options = Self::default();
        if let Some(mode) = conn.get("journal mode").and_then(SqliteJournalMode::parse) {
            options.journal_mode = Some(mode);
        }
        if let Some(sync) = conn.get("synchronous").and_then(SqliteSynchronous::parse) {
            options.synchronous = sync;
        }
        if let Some(size) = conn
            .get("cache size")
            .and_then(|value| value.trim().parse::<i64>().ok())
        {
            options.cache_size_kib = Some(size);
        }
        if let Some(ms) = conn
            .get("busytimeout")
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            options.busy_timeout_ms = ms;
        }
        // 只读连接：跳过 journal_mode（只读文件不能写 WAL/-shm，与 C# 的 Readonly 分支一致）
        options.read_only = conn
            .get("readonly")
            .is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"));
        if options.read_only {
            options.journal_mode = None;
        }
        options
    }
}

/// SQLite 会话。
pub struct SqliteSession {
    /// rusqlite 连接
    conn: rusqlite::Connection,
    /// 列名缓存（SQL → 列名）：同构查询只构建一次列名列表，省掉每轮 N 次 `String` 分配
    column_cache: HashMap<String, Arc<Vec<String>>>,
}

impl SqliteSession {
    /// 打开数据库文件（支持 `:memory:` 内存库），使用默认参数
    /// （WAL + `Synchronous=Off` + busy_timeout 5s，对齐 C# 版）。
    pub fn open(path: &str) -> Result<Self> {
        Self::open_with_options(path, &SqliteOptions::default())
    }

    /// 按指定参数打开数据库文件（连接串 `Synchronous` / `Cache Size` / `Journal Mode` / `BusyTimeout`）。
    pub fn open_with_options(path: &str, options: &SqliteOptions) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;

        // 预编译语句缓存：此前每次 `execute`/`query` 都重新编译 SQL（SQLite 编译 20 列表的
        // INSERT 开销与执行同级）；开启缓存后同构 SQL 只编译一次。
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);

        // 忙等待：多进程/多连接共用同一个 DG.db 时减少 "database is locked"
        if options.busy_timeout_ms > 0 {
            let _ = conn.busy_timeout(std::time::Duration::from_millis(options.busy_timeout_ms));
        }
        // 日志模式（内存库/只读库不支持时失败忽略）
        if let Some(mode) = options.journal_mode {
            let sql = format!("PRAGMA journal_mode={};", mode.pragma());
            let _ = conn.query_row(&sql, [], |_| Ok(()));
        }
        // 同步模式（默认 Off，对齐 C#；连接串可改 Normal/Full/Extra）
        let _ = conn.execute_batch(&format!(
            "PRAGMA synchronous={};",
            options.synchronous.pragma()
        ));
        // 页缓存（负数 = KiB；未配置则用 SQLite 默认）
        if let Some(size) = options.cache_size_kib {
            let _ = conn.execute_batch(&format!("PRAGMA cache_size={size};"));
        }
        // 只读池连接：SQLite 层直接拒绝写入（写 SQL 属调用方 bug，尽快报错而不是偷偷写进库）
        if options.query_only {
            let _ = conn.execute_batch("PRAGMA query_only=ON;");
        }

        Ok(Self {
            conn,
            column_cache: HashMap::new(),
        })
    }

    /// 包装现有的 rusqlite 连接（例如测试中的内存连接）。
    pub fn from_connection(conn: rusqlite::Connection) -> Self {
        Self {
            conn,
            column_cache: HashMap::new(),
        }
    }

    /// 底层连接引用（高级用法，如批量 Prepare）。
    pub fn connection(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

/// 绑定参数：能借用就借用（`Text`/`Blob` 零拷贝），需渲染成文本的才分配。
///
/// 说明：时间统一按 XCode 的文本格式（7 位小数秒）存入 TEXT 列，
/// 与 C# 写入的数据保持一致的字符串形态，便于 SQLite 侧比较与排序。
///
/// 大批量写入时每行每列复制一次字符串是纯开销 —— 这里的 `Borrowed` 分支把它省掉。
enum SqlParam<'a> {
    /// 借用自 `DbValue`（Text/Blob/数值）
    Borrowed(ValueRef<'a>),
    /// 需临时渲染（Decimal/DateTime）
    Owned(SqlValue),
}

impl ToSql for SqlParam<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        match self {
            // `ValueRef` 自身可直接作为绑定值（rusqlite 的 `ToSqlOutput::Borrowed`）
            SqlParam::Borrowed(value) => Ok(ToSqlOutput::Borrowed(*value)),
            SqlParam::Owned(value) => value.to_sql(),
        }
    }
}

/// `DbValue` → 绑定参数（Text/Blob 借用，不复制）。
fn to_param(value: &DbValue) -> SqlParam<'_> {
    match value {
        DbValue::Null => SqlParam::Borrowed(ValueRef::Null),
        DbValue::Bool(v) => SqlParam::Borrowed(ValueRef::Integer(i64::from(*v))),
        DbValue::Int(v) => SqlParam::Borrowed(ValueRef::Integer(*v)),
        DbValue::Float(v) => SqlParam::Borrowed(ValueRef::Real(*v)),
        DbValue::Text(v) => SqlParam::Borrowed(ValueRef::Text(v.as_bytes())),
        DbValue::Blob(v) => SqlParam::Borrowed(ValueRef::Blob(v)),
        DbValue::Decimal(v) => SqlParam::Owned(SqlValue::Text(v.to_string())),
        DbValue::DateTime(v) => SqlParam::Owned(SqlValue::Text(format_datetime(v))),
    }
}

/// rusqlite 值 → `DbValue`。
fn from_sql_ref(value: ValueRef<'_>) -> DbValue {
    match value {
        ValueRef::Null => DbValue::Null,
        ValueRef::Integer(v) => DbValue::Int(v),
        ValueRef::Real(v) => DbValue::Float(v),
        ValueRef::Text(bytes) => DbValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => DbValue::Blob(bytes.to_vec()),
    }
}

impl SqlSession for SqliteSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::Sqlite
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let values: Vec<SqlParam> = params.iter().map(to_param).collect();
        // 语句缓存：同构 SQL 免重复编译（缓存未命中时与 `execute` 等价）
        let mut stmt = self.conn.prepare_cached(sql)?;
        let affected = stmt.execute(rusqlite::params_from_iter(values))?;
        Ok(affected as u64)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let values: Vec<SqlParam> = params.iter().map(to_param).collect();
        let Self {
            conn,
            column_cache,
        } = self;
        let mut stmt = conn.prepare_cached(sql)?;

        // 列名按 SQL 缓存：分页/条件只有参数不同的同构查询，不再每轮重建列名列表
        // （宽表每轮省 N 次 `String` 分配）；动态 SQL 超出容量时整表清空，避免无界增长。
        let columns = match column_cache.get(sql) {
            Some(cached) => cached.clone(),
            None => {
                let names = Arc::new(
                    stmt.column_names()
                        .into_iter()
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                );
                if column_cache.len() >= COLUMN_CACHE_CAPACITY {
                    column_cache.clear();
                }
                column_cache.insert(sql.to_string(), names.clone());
                names
            }
        };

        let mut result = RowSet::with_columns(columns);
        let column_count = result.columns.len();

        let mut rows = stmt.query(rusqlite::params_from_iter(values))?;
        while let Some(row) = rows.next()? {
            let mut line = Vec::with_capacity(column_count);
            for i in 0..column_count {
                line.push(from_sql_ref(row.get_ref(i)?));
            }
            result.push(line);
        }

        Ok(result)
    }

    fn begin(&mut self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    fn last_identity(&mut self) -> Result<i64> {
        let value: i64 = self
            .conn
            .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))?;
        Ok(value)
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1 COLLATE NOCASE",
            [table],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        // PRAGMA 不支持参数化，需对表名进行引用转义
        let sql = format!("PRAGMA table_info({})", DatabaseKind::Sqlite.quote(table));
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| Error::Db(format!("读取表结构失败：{e}")))?;

        let mut rows = stmt.query([])?;
        let mut names = Vec::new();
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            names.push(name);
        }
        Ok(names)
    }
}

/// 便捷方法：执行查询并返回首行（无结果时为 None）。
pub fn query_one(session: &mut dyn SqlSession, sql: &str, params: &[DbValue]) -> Result<Option<DbRow>> {
    let set = session.query(sql, params)?;
    Ok(set.rows.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SqlSession;

    fn memory() -> SqliteSession {
        SqliteSession::open(":memory:").unwrap()
    }

    #[test]
    fn default_options_match_csharp() {
        let options = SqliteOptions::default();
        assert_eq!(options.synchronous, SqliteSynchronous::Off, "默认对齐 C#：Synchronous=Off");
        assert_eq!(options.journal_mode, Some(SqliteJournalMode::Wal));
        assert_eq!(options.cache_size_kib, None);
        assert_eq!(options.busy_timeout_ms, 5000);
        assert!(!options.read_only);
    }

    #[test]
    fn options_from_connection_string() {
        let conn = crate::dal::ConnectionString::parse(
            "Data Source=x.db;Provider=SQLite;Synchronous=Normal;Cache Size=-8192;Journal Mode=Delete;BusyTimeout=1500",
        );
        let options = SqliteOptions::from_connection_string(&conn);
        assert_eq!(options.synchronous, SqliteSynchronous::Normal);
        assert_eq!(options.journal_mode, Some(SqliteJournalMode::Delete));
        assert_eq!(options.cache_size_kib, Some(-8192));
        assert_eq!(options.busy_timeout_ms, 1500);
    }

    #[test]
    fn invalid_options_fall_back_to_default() {
        let conn = crate::dal::ConnectionString::parse(
            "Data Source=x.db;Provider=SQLite;Synchronous=Whatever;Cache Size=abc;Journal Mode=?",
        );
        let options = SqliteOptions::from_connection_string(&conn);
        assert_eq!(options.synchronous, SqliteSynchronous::Off);
        assert_eq!(options.cache_size_kib, None);
        assert_eq!(options.journal_mode, Some(SqliteJournalMode::Wal));
    }

    #[test]
    fn readonly_connection_skips_journal_mode() {
        let conn = crate::dal::ConnectionString::parse(
            "Data Source=x.db;Provider=SQLite;Readonly=true;Journal Mode=Delete",
        );
        let options = SqliteOptions::from_connection_string(&conn);
        assert!(options.read_only);
        assert_eq!(options.journal_mode, None, "只读连接不设置 journal_mode");
    }

    #[test]
    fn pragmas_are_applied_to_connection() {
        let dir = std::env::temp_dir().join(format!(
            "pek-rcode-sqlite-options-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("options.db");
        let options = SqliteOptions {
            synchronous: SqliteSynchronous::Normal,
            cache_size_kib: Some(-8192),
            ..Default::default()
        };
        let session = SqliteSession::open_with_options(path.to_str().unwrap(), &options).unwrap();
        let conn = session.connection();
        let sync: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
        assert_eq!(sync, 1, "NORMAL = 1");
        let cache: i64 = conn
            .query_row("PRAGMA cache_size", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cache, -8192);
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        drop(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ddl_dml_roundtrip() {
        let mut s = memory();
        s.execute(
            "CREATE TABLE t (id integer PRIMARY KEY AUTOINCREMENT, name nvarchar(50) COLLATE NOCASE, amount decimal, ok bit, at datetime)",
            &[],
        )
        .unwrap();

        let affected = s
            .execute(
                "INSERT INTO t (name, amount, ok, at) VALUES (?1, ?2, ?3, ?4)",
                &[
                    DbValue::from("Demo"),
                    DbValue::from("12.3400".parse::<rust_decimal::Decimal>().unwrap()),
                    DbValue::from(true),
                    DbValue::from(chrono::NaiveDate::from_ymd_opt(2026, 9, 26).unwrap().and_hms_opt(18, 1, 2).unwrap()),
                ],
            )
            .unwrap();
        assert_eq!(affected, 1);

        let id = s.last_identity().unwrap();
        assert_eq!(id, 1);

        let set = s.query("SELECT * FROM t WHERE name = ?1", &[DbValue::from("demo")]).unwrap();
        assert_eq!(set.len(), 1, "COLLATE NOCASE 应支持忽略大小写查询");

        let row = set.first().unwrap();
        assert_eq!(row.get_by_name("ID").unwrap().as_i64(), Some(1));
        assert_eq!(row.get_by_name("name").unwrap().as_str(), Some("Demo"));
        assert_eq!(row.get_by_name("ok").unwrap().as_bool(), Some(true));
        let at = row.get_by_name("at").unwrap().as_datetime().unwrap();
        assert_eq!(at.to_string(), "2026-09-26 18:01:02");
    }

    #[test]
    fn transactions_rollback() {
        let mut s = memory();
        s.execute("CREATE TABLE t (id int)", &[]).unwrap();

        s.begin().unwrap();
        s.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
        s.rollback().unwrap();
        let set = s.query("SELECT COUNT(*) FROM t", &[]).unwrap();
        assert_eq!(set.first().unwrap().get(0).unwrap().as_i64(), Some(0));

        s.begin().unwrap();
        s.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
        s.commit().unwrap();
        let set = s.query("SELECT COUNT(*) FROM t", &[]).unwrap();
        assert_eq!(set.first().unwrap().get(0).unwrap().as_i64(), Some(1));
    }

    #[test]
    fn introspection() {
        let mut s = memory();
        s.execute("CREATE TABLE DH_Order (Id int, Code nvarchar(50))", &[]).unwrap();

        assert!(s.table_exists("dh_order").unwrap(), "表名匹配应忽略大小写");
        assert!(!s.table_exists("NotExists").unwrap());

        let cols = s.table_columns("DH_Order").unwrap();
        assert_eq!(cols, vec!["Id", "Code"]);
    }
}
