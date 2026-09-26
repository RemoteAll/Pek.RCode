//! SQLite 驱动：对应 DH.NCode 的 `SQLite.cs`（基于 rusqlite，内嵌 SQLite 无需外部依赖）。
//!
//! 打开连接时套用与 XCode 一致的优化参数（WAL 日志、忙等待超时），
//! 保证与 C# 版共用同一个 `DG.db` 文件时的行为一致。

use rusqlite::types::{Value as SqlValue, ValueRef};

use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::session::{DbRow, RowSet, SqlSession};
use crate::value::{DbValue, format_datetime};

/// SQLite 会话。
pub struct SqliteSession {
    /// rusqlite 连接
    conn: rusqlite::Connection,
}

impl SqliteSession {
    /// 打开数据库文件（支持 `:memory:` 内存库）。
    pub fn open(path: &str) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;

        // 忙等待：多进程/多连接共用同一个 DG.db 时减少 "database is locked"
        let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
        // WAL 日志：读写并发更好（内存库不支持，失败忽略）
        let _ = conn.query_row("PRAGMA journal_mode=WAL;", [], |_| Ok(()));
        // NORMAL 同步：性能与安全的平衡点（XCode 生产环境使用 Off，此处更保守）
        let _ = conn.query_row("PRAGMA synchronous=NORMAL;", [], |_| Ok(()));

        Ok(Self { conn })
    }

    /// 包装现有的 rusqlite 连接（例如测试中的内存连接）。
    pub fn from_connection(conn: rusqlite::Connection) -> Self {
        Self { conn }
    }

    /// 底层连接引用（高级用法，如批量 Prepare）。
    pub fn connection(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

/// `DbValue` → rusqlite 参数值。
///
/// 说明：时间统一按 XCode 的文本格式（7 位小数秒）存入 TEXT 列，
/// 与 C# 写入的数据保持一致的字符串形态，便于 SQLite 侧比较与排序。
fn to_sql_value(value: &DbValue) -> SqlValue {
    match value {
        DbValue::Null => SqlValue::Null,
        DbValue::Bool(v) => SqlValue::Integer(i64::from(*v)),
        DbValue::Int(v) => SqlValue::Integer(*v),
        DbValue::Float(v) => SqlValue::Real(*v),
        DbValue::Decimal(v) => SqlValue::Text(v.to_string()),
        DbValue::Text(v) => SqlValue::Text(v.clone()),
        DbValue::Blob(v) => SqlValue::Blob(v.clone()),
        DbValue::DateTime(v) => SqlValue::Text(format_datetime(v)),
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
        let values: Vec<SqlValue> = params.iter().map(to_sql_value).collect();
        let affected = self
            .conn
            .execute(sql, rusqlite::params_from_iter(values))?;
        Ok(affected as u64)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let values: Vec<SqlValue> = params.iter().map(to_sql_value).collect();
        let mut stmt = self.conn.prepare(sql)?;

        // 先取出列名（借用在查询前结束）
        let columns: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect();

        let mut result = RowSet::new(columns);
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
