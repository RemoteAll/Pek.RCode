//! DuckDB 驱动：对应 DH.NCode 的 `DuckDb.cs`（基于 `duckdb` crate，内嵌 DuckDB 引擎）。
//!
//! 连接串（与 XCode 风格一致）：
//!
//! ```text
//! Data Source=mes.duckdb;provider=duckdb
//! Data Source=:memory:;provider=duckdb
//! ```
//!
//! 实现要点：
//! - 内嵌单机数据库（无网络服务）：`Data Source` 为文件路径，`:memory:`/`memory` 为内存库
//! - 自增回写：建表使用 `INTEGER DEFAULT nextval('"SEQ_x"')` + `CREATE SEQUENCE`（由 [`crate::dialect`] 生成），
//!   插入走 `INSERT ... RETURNING`（由 DAL 组装），因此 [`SqlSession::last_identity`] 仅作兜底
//! - 值绑定使用真参数（`?` 占位符 + 强类型绑定）；`DECIMAL` 以文本绑定（DuckDB 隐式转换），
//!   读取按 [`ValueRef`] 变体分支转换
//! - 日期时间使用原生 `TIMESTAMP`；表结构探测走 `information_schema`
//! - 事务：DuckDB 原生支持 `BEGIN`/`COMMIT`/`ROLLBACK`
//!
//! 构建要求：`cargo build --features duckdb`。内嵌引擎为 C++ 代码，首次编译需要 CMake 工具链
//! （Windows 下可设置 `CMAKE` 环境变量指向 cmake.exe），编译耗时较长属正常现象。

use std::path::PathBuf;

use chrono::{DateTime, NaiveDate};

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

use duckdb::Connection;
use duckdb::ToSql;
use duckdb::params_from_iter;
use duckdb::types::{Decimal as DuckDecimal, TimeUnit, ValueRef};

/// DuckDB 会话。
pub struct DuckDbSession {
    /// duckdb crate 连接（内嵌引擎，同步 API）
    conn: Connection,
}

impl DuckDbSession {
    /// 根据 XCode 风格连接串打开连接（文件库或内存库）。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        let settings = parse_settings(conn_str)?;
        let conn = match &settings.target {
            DuckDbTarget::Memory => Connection::open_in_memory(),
            DuckDbTarget::File(path) => Connection::open(path),
        }
        .map_err(|e| {
            Error::Db(format!(
                "打开 DuckDB 失败（{}）：{e}",
                settings.describe()
            ))
        })?;

        Ok(Self { conn })
    }
}

/// 从连接串解析出的设置。
#[derive(Debug, Clone, PartialEq)]
struct DuckDbSettings {
    /// 数据库目标（文件 / 内存）
    target: DuckDbTarget,
}

impl DuckDbSettings {
    /// 诊断用描述。
    fn describe(&self) -> String {
        match &self.target {
            DuckDbTarget::Memory => ":memory:".to_string(),
            DuckDbTarget::File(path) => path.display().to_string(),
        }
    }
}

/// 数据库目标。
#[derive(Debug, Clone, PartialEq)]
enum DuckDbTarget {
    /// 内存库（`:memory:`）
    Memory,
    /// 文件库
    File(PathBuf),
}

/// 解析连接串为设置结构（与 XCode 的键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<DuckDbSettings> {
    let raw = cs
        .get("data source")
        .or(cs.get("datasource"))
        .or(cs.get("database"))
        .or(cs.get("filename"))
        .or(cs.get("file"))
        .or(cs.get("path"))
        .ok_or_else(|| {
            Error::Model(
                "DuckDB 连接串缺少 Data Source（文件路径或 :memory:，如 Data Source=mes.duckdb）"
                    .to_string(),
            )
        })?
        .trim();

    let target = if raw.is_empty() || raw.eq_ignore_ascii_case("memory") || raw == ":memory:" {
        DuckDbTarget::Memory
    } else {
        DuckDbTarget::File(PathBuf::from(raw))
    };

    Ok(DuckDbSettings { target })
}

/// 统一错误映射。
fn map_err(e: duckdb::Error) -> Error {
    Error::Db(format!("DuckDB 执行失败：{e}"))
}

/// 绑定参数：`DbValue` → `Box<dyn ToSql>`。
///
/// 说明：`DECIMAL` 以文本绑定（DuckDB 会按目标列隐式转换），避免额外宽度/精度推断。
fn bind_params(params: &[DbValue]) -> Vec<Box<dyn ToSql>> {
    params
        .iter()
        .map(|value| -> Box<dyn ToSql> {
            match value {
                DbValue::Null => Box::new(Option::<i32>::None),
                DbValue::Bool(v) => Box::new(*v),
                DbValue::Int(v) => Box::new(*v),
                DbValue::Float(v) => Box::new(*v),
                DbValue::Decimal(v) => Box::new(v.to_string()),
                DbValue::Text(v) => Box::new(v.clone()),
                DbValue::Blob(v) => Box::new(v.clone()),
                DbValue::DateTime(v) => Box::new(*v),
            }
        })
        .collect()
}

/// 单元格 → `DbValue`。
fn duckdb_cell_to_dbvalue(cell: ValueRef<'_>) -> DbValue {
    match cell {
        ValueRef::Null => DbValue::Null,
        ValueRef::Boolean(v) => DbValue::Bool(v),
        ValueRef::TinyInt(v) => DbValue::Int(i64::from(v)),
        ValueRef::SmallInt(v) => DbValue::Int(i64::from(v)),
        ValueRef::Int(v) => DbValue::Int(i64::from(v)),
        ValueRef::BigInt(v) => DbValue::Int(v),
        ValueRef::HugeInt(v) => i64::try_from(v)
            .map(DbValue::Int)
            .unwrap_or_else(|_| DbValue::Text(v.to_string())),
        ValueRef::UTinyInt(v) => DbValue::Int(i64::from(v)),
        ValueRef::USmallInt(v) => DbValue::Int(i64::from(v)),
        ValueRef::UInt(v) => DbValue::Int(i64::from(v)),
        ValueRef::UBigInt(v) => i64::try_from(v)
            .map(DbValue::Int)
            .unwrap_or_else(|_| DbValue::Text(v.to_string())),
        ValueRef::UHugeInt(v) => i64::try_from(v)
            .map(DbValue::Int)
            .unwrap_or_else(|_| DbValue::Text(v.to_string())),
        ValueRef::Float(v) => DbValue::Float(f64::from(v)),
        ValueRef::Double(v) => DbValue::Float(v),
        ValueRef::Decimal(v) => decimal_to_dbvalue(v),
        ValueRef::Timestamp(unit, raw) => timestamp_to_dbvalue(unit, raw),
        ValueRef::Date32(days) => date32_to_dbvalue(days),
        ValueRef::Time64(unit, raw) => time64_to_dbvalue(unit, raw),
        ValueRef::Text(bytes) => DbValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) | ValueRef::Geometry(bytes) => DbValue::Blob(bytes.to_vec()),
        // 复杂类型（LIST/STRUCT/MAP 等）暂以调试文本兜底
        other => DbValue::Text(format!("{other:?}")),
    }
}

/// DECIMAL → `DbValue`（优先还原为 rust_decimal，失败时保留文本）。
fn decimal_to_dbvalue(value: DuckDecimal) -> DbValue {
    match rust_decimal::Decimal::try_from(value) {
        Ok(decimal) => DbValue::Decimal(decimal),
        Err(_) => DbValue::Text(value.to_string()),
    }
}

/// TIMESTAMP（自纪元起按单位计数）→ `DbValue`。
fn timestamp_to_dbvalue(unit: TimeUnit, raw: i64) -> DbValue {
    let nanos = match unit {
        TimeUnit::Second => raw.saturating_mul(1_000_000_000),
        TimeUnit::Millisecond => raw.saturating_mul(1_000_000),
        TimeUnit::Microsecond => raw.saturating_mul(1_000),
        TimeUnit::Nanosecond => raw,
    };
    let seconds = nanos.div_euclid(1_000_000_000);
    let subsec = nanos.rem_euclid(1_000_000_000) as u32;
    DateTime::from_timestamp(seconds, subsec)
        .map(|value| DbValue::DateTime(value.naive_utc()))
        .unwrap_or(DbValue::Null)
}

/// DATE（自纪元起的天数）→ `DbValue`（零点时间）。
fn date32_to_dbvalue(days: i32) -> DbValue {
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|epoch| epoch.checked_add_signed(chrono::TimeDelta::days(i64::from(days))))
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map_or(DbValue::Null, DbValue::DateTime)
}

/// TIME（自午夜起按单位计数）→ `DbValue`（`HH:MM:SS[.ffffff]` 文本）。
fn time64_to_dbvalue(unit: TimeUnit, raw: i64) -> DbValue {
    let nanos = match unit {
        TimeUnit::Second => raw.saturating_mul(1_000_000_000),
        TimeUnit::Millisecond => raw.saturating_mul(1_000_000),
        TimeUnit::Microsecond => raw.saturating_mul(1_000),
        TimeUnit::Nanosecond => raw,
    };
    let total_seconds = nanos.div_euclid(1_000_000_000);
    let micros = nanos.rem_euclid(1_000_000_000) / 1_000;
    let hour = total_seconds / 3_600;
    let minute = total_seconds % 3_600 / 60;
    let second = total_seconds % 60;
    if micros == 0 {
        DbValue::Text(format!("{hour:02}:{minute:02}:{second:02}"))
    } else {
        DbValue::Text(format!("{hour:02}:{minute:02}:{second:02}.{micros:06}"))
    }
}

impl SqlSession for DuckDbSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::DuckDb
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let bound = bind_params(params);
        let affected = self
            .conn
            .execute(sql, params_from_iter(bound))
            .map_err(map_err)?;
        Ok(affected as u64)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let bound = bind_params(params);
        let mut stmt = self.conn.prepare(sql).map_err(map_err)?;
        // 说明：duckdb crate 的 `schema()` / `raw_query()` 要求语句**已执行**（未执行会内部 panic），
        // 因此不能先用高层 `query()`（其返回会长期持有语句借用）再取列名，
        // 这里走「绑定 → 执行 → 取列名 → 取行」的底层三段式
        for (index, param) in bound.iter().enumerate() {
            stmt.raw_bind_parameter(index + 1, param).map_err(map_err)?;
        }
        stmt.raw_execute().map_err(map_err)?;

        let names: Vec<String> = stmt
            .schema()
            .fields()
            .iter()
            .map(|field| field.name().clone())
            .collect();
        let width = names.len();
        let mut set = RowSet::new(names);

        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next().map_err(map_err)? {
            let mut values = Vec::with_capacity(width);
            for index in 0..width {
                values.push(duckdb_cell_to_dbvalue(
                    row.get_ref(index).map_err(map_err)?,
                ));
            }
            set.push(values);
        }
        Ok(set)
    }

    fn begin(&mut self) -> Result<()> {
        self.conn.execute_batch("BEGIN").map_err(map_err)
    }

    fn commit(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT").map_err(map_err)
    }

    fn rollback(&mut self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK").map_err(map_err)
    }

    fn last_identity(&mut self) -> Result<i64> {
        // DuckDB 的自增依赖序列 + `INSERT ... RETURNING`（由 DAL 组装并回写主键），
        // 无表名上下文时无法构造 CURRVAL 表达式，这里返回 0 作为兜底
        Ok(0)
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE lower(table_name) = lower(?)",
            )
            .map_err(map_err)?;
        let count: i64 = stmt.query_row([table], |row| row.get(0)).map_err(map_err)?;
        Ok(count > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT column_name FROM information_schema.columns \
                 WHERE lower(table_name) = lower(?) ORDER BY ordinal_position",
            )
            .map_err(map_err)?;
        let mut rows = stmt.query([table]).map_err(map_err)?;
        let mut columns = Vec::new();
        while let Some(row) = rows.next().map_err(map_err)? {
            let name: String = row.get(0).map_err(map_err)?;
            columns.push(name);
        }
        Ok(columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存库会话。
    fn memory_session() -> DuckDbSession {
        let cs = ConnectionString::parse("Data Source=:memory:;provider=duckdb");
        DuckDbSession::open(&cs).unwrap()
    }

    #[test]
    fn settings_parsing() {
        let cs = ConnectionString::parse("Data Source=:memory:;provider=duckdb");
        assert_eq!(parse_settings(&cs).unwrap().target, DuckDbTarget::Memory);

        let cs = ConnectionString::parse("Data Source=memory;provider=duckdb");
        assert_eq!(parse_settings(&cs).unwrap().target, DuckDbTarget::Memory);

        let cs = ConnectionString::parse("Data Source=mes.duckdb;provider=duckdb");
        assert_eq!(
            parse_settings(&cs).unwrap().target,
            DuckDbTarget::File(PathBuf::from("mes.duckdb"))
        );

        assert!(parse_settings(&ConnectionString::parse("provider=duckdb")).is_err());
    }

    #[test]
    fn cell_conversion() {
        assert_eq!(duckdb_cell_to_dbvalue(ValueRef::Null), DbValue::Null);
        assert_eq!(
            duckdb_cell_to_dbvalue(ValueRef::Boolean(true)),
            DbValue::Bool(true)
        );
        assert_eq!(duckdb_cell_to_dbvalue(ValueRef::Int(7)), DbValue::Int(7));
        assert_eq!(
            duckdb_cell_to_dbvalue(ValueRef::UBigInt(42)),
            DbValue::Int(42)
        );
        assert_eq!(
            duckdb_cell_to_dbvalue(ValueRef::Float(1.5)),
            DbValue::Float(1.5)
        );
        assert_eq!(
            duckdb_cell_to_dbvalue(ValueRef::Text(b"A-001")),
            DbValue::Text("A-001".into())
        );
        assert_eq!(
            duckdb_cell_to_dbvalue(ValueRef::Blob(&[1, 2, 3])),
            DbValue::Blob(vec![1, 2, 3])
        );
        // 纪元首日
        assert_eq!(
            duckdb_cell_to_dbvalue(ValueRef::Date32(0)),
            DbValue::DateTime(
                NaiveDate::from_ymd_opt(1970, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
            )
        );
        // 3723 秒 = 01:02:03
        assert_eq!(
            duckdb_cell_to_dbvalue(ValueRef::Time64(TimeUnit::Microsecond, 3_723_000_000)),
            DbValue::Text("01:02:03".into())
        );
    }

    #[test]
    fn roundtrip_in_memory() {
        let mut session = memory_session();
        session
            .execute("CREATE SEQUENCE \"SEQ_DH_Item\"", &[])
            .unwrap();
        session
            .execute(
                "CREATE TABLE \"DH_Item\" (\
                 \"Id\" INTEGER NOT NULL DEFAULT nextval('\"SEQ_DH_Item\"'), \
                 \"Code\" VARCHAR(50), \
                 \"Amount\" DECIMAL(18,4), \
                 \"Payload\" BLOB, \
                 \"CreateTime\" TIMESTAMP, \
                 PRIMARY KEY (\"Id\"))",
                &[],
            )
            .unwrap();

        let now = NaiveDate::from_ymd_opt(2026, 9, 27)
            .unwrap()
            .and_hms_micro_opt(10, 30, 0, 123_000)
            .unwrap();
        let affected = session
            .execute(
                "INSERT INTO \"DH_Item\" (\"Code\", \"Amount\", \"Payload\", \"CreateTime\") \
                 VALUES (?, ?, ?, ?)",
                &[
                    DbValue::Text("A-001".into()),
                    DbValue::Decimal("12.50".parse().unwrap()),
                    DbValue::Blob(vec![0x10, 0x20]),
                    DbValue::DateTime(now),
                ],
            )
            .unwrap();
        assert_eq!(affected, 1);

        let set = session
            .query(
                "SELECT * FROM \"DH_Item\" WHERE \"Code\" = ?",
                &[DbValue::Text("A-001".into())],
            )
            .unwrap();
        assert_eq!(set.len(), 1);
        let row = set.first().unwrap();
        assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("A-001"));
        // DECIMAL 按月数值比较（scale 可能被补齐为 4）
        assert_eq!(
            row.get_by_name("Amount").unwrap().as_decimal(),
            Some("12.5".parse().unwrap())
        );
        assert_eq!(
            row.get_by_name("Payload").unwrap(),
            &DbValue::Blob(vec![0x10, 0x20])
        );
        assert_eq!(row.get_by_name("CreateTime").unwrap(), &DbValue::DateTime(now));

        // 结构探测
        assert!(session.table_exists("DH_Item").unwrap());
        assert!(!session.table_exists("DH_NotFound").unwrap());
        let columns = session.table_columns("DH_Item").unwrap();
        assert_eq!(columns, vec!["Id", "Code", "Amount", "Payload", "CreateTime"]);

        // 更新 / 删除
        let affected = session
            .execute(
                "UPDATE \"DH_Item\" SET \"Code\" = ? WHERE \"Code\" = ?",
                &[
                    DbValue::Text("B-001".into()),
                    DbValue::Text("A-001".into()),
                ],
            )
            .unwrap();
        assert_eq!(affected, 1);
        assert_eq!(
            session
                .execute("DELETE FROM \"DH_Item\" WHERE \"Code\" = ?", &[DbValue::Text("B-001".into())])
                .unwrap(),
            1
        );

        // 事务回滚
        session.begin().unwrap();
        session
            .execute(
                "INSERT INTO \"DH_Item\" (\"Code\") VALUES (?)",
                &[DbValue::Text("C-001".into())],
            )
            .unwrap();
        session.rollback().unwrap();
        let set = session
            .query("SELECT * FROM \"DH_Item\"", &[])
            .unwrap();
        assert!(set.is_empty());
    }
}
