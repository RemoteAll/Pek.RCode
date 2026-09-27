//! PostgreSQL 驱动：对应 DH.NCode 的 `PostgreSQL.cs`（基于 `postgres` crate，纯 Rust 协议实现）。
//!
//! 与 DH.NCode 支持的国产衍生库共用本驱动（同协议，仅 `provider` 名称不同）：
//! HighGo（瀚高）、KingBase（金仓）、VastBase（海量）。
//!
//! 连接串与 XCode 完全兼容：
//!
//! ```text
//! Server=localhost;Port=5432;Database=mes;Uid=postgres;Pwd=***;provider=postgresql
//! ```
//!
//! 实现要点：
//! - 自增回写：`INSERT ... RETURNING 列`（与 DH.NCode 的 `RETURNING *` 一致，由 DAL 组装）
//! - 值绑定统一使用**文本格式**（与 MySQL 驱动的 DECIMAL 文本策略一致），服务端按目标列类型解析，
//!   避免为每种类型手写二进制编码；读取按列类型（`pg_type`）分支转换
//! - 时间使用原生 `timestamp/timestamptz/date` 类型；7 位小数秒按 PostgreSQL 微秒精度四舍五入
//! - 表结构探测走 `information_schema`（与 XCode 反向工程一致）
//!
//! TLS 说明：当前版本固定不启用 TLS；连接串要求 `SslMode=Require/VerifyCA/VerifyFull` 时
//! 会明确报错。`SslMode=Disable/Prefer` 走明文连接（Prefer 在未启用 TLS 时等效于明文）。

use std::time::Duration;

use bytes::BytesMut;
use chrono::{DateTime, NaiveDateTime, Utc};
use postgres::config::SslMode;
use postgres::types::{Format, IsNull, ToSql, Type, to_sql_checked};
use postgres::{Client, Config as PgConfig, NoTls};
use rust_decimal::Decimal;

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::session::{RowSet, SqlSession};
use crate::value::{DbValue, format_datetime};

/// PostgreSQL 会话。
pub struct PostgresSession {
    /// postgres crate 连接（同步 API，内部维护后台 I/O 线程）
    client: Client,
}

impl PostgresSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        let settings = parse_settings(conn_str)?;
        let config = build_config(&settings);

        let client = config.connect(NoTls).map_err(|e| {
            Error::Db(format!(
                "连接 PostgreSQL 失败（{}:{}）：{e}",
                settings.host, settings.port
            ))
        })?;

        Ok(Self { client })
    }
}

/// 从连接串解析出的 PostgreSQL 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct PostgresSettings {
    /// 主机
    host: String,
    /// 端口（默认 5432）
    port: u16,
    /// 用户名（默认 postgres）
    user: String,
    /// 密码
    password: String,
    /// 数据库名（缺省时由服务端使用与用户名同名的库）
    database: Option<String>,
    /// 应用名（出现在服务端 `pg_stat_activity`，便于识别 Pek.RCode 连接）
    application_name: String,
    /// 连接超时
    connect_timeout: Option<Duration>,
    /// 显式指定的 SSL 模式（None 表示使用驱动默认：无 TLS 时等效明文）
    ssl_mode: Option<SslMode>,
}

/// 解析连接串为设置结构（与 XCode 的键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<PostgresSettings> {
    // TLS 前置校验：明确给出可操作提示
    let ssl_mode = match cs.get("sslmode").map(str::to_ascii_lowercase) {
        None => None,
        Some(mode) => match mode.as_str() {
            // 无 TLS 支持时 Prefer 等效于明文（与 Npgsql 的 Prefer 语义一致）
            "disable" | "none" | "allow" | "prefer" => Some(SslMode::Disable),
            "require" | "verify-ca" | "verifyca" | "verify-full" | "verifyfull" => {
                return Err(Error::Unsupported(
                    "当前 PostgreSQL 驱动未启用 TLS：请将连接串 SslMode 设为 Disable/Prefer，\
                     或使用 TLS 隧道；后续版本将提供 native-tls 支持"
                        .into(),
                ));
            }
            other => return Err(Error::Model(format!("无效的 SslMode \"{other}\""))),
        },
    };

    let host = cs
        .get("server")
        .or(cs.get("host"))
        .or(cs.get("data source"))
        .unwrap_or("127.0.0.1")
        .to_string();

    let port = match cs.get("port") {
        Some(v) => v
            .parse::<u16>()
            .map_err(|e| Error::Model(format!("无效的 Port \"{v}\"：{e}")))?,
        None => 5432,
    };

    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("user id"))
        .or(cs.get("username"))
        .unwrap_or("postgres")
        .to_string();

    let password = cs
        .get("pwd")
        .or(cs.get("password"))
        .or(cs.get("passwd"))
        .unwrap_or("")
        .to_string();

    let database = cs
        .get("database")
        .or(cs.get("initial catalog"))
        .or(cs.get("db"))
        .map(str::to_string);

    let application_name = cs
        .get("applicationname")
        .or(cs.get("application name"))
        .unwrap_or("Pek.RCode")
        .to_string();

    let connect_timeout = match cs.get("timeout").or(cs.get("connect timeout")) {
        Some(v) => Some(Duration::from_secs(
            v.parse::<u64>()
                .map_err(|e| Error::Model(format!("无效的 Timeout \"{v}\"：{e}")))?,
        )),
        None => Some(Duration::from_secs(15)),
    };

    Ok(PostgresSettings {
        host,
        port,
        user,
        password,
        database,
        application_name,
        connect_timeout,
        ssl_mode,
    })
}

/// 设置 → postgres crate 配置。
fn build_config(settings: &PostgresSettings) -> PgConfig {
    let mut config = PgConfig::new();
    config.host(&settings.host);
    config.port(settings.port);
    config.user(settings.user.as_str());
    if !settings.password.is_empty() {
        config.password(settings.password.as_str());
    }
    if let Some(db) = &settings.database {
        config.dbname(db.as_str());
    }
    config.application_name(settings.application_name.as_str());
    if let Some(timeout) = settings.connect_timeout {
        config.connect_timeout(timeout);
    }
    if let Some(mode) = settings.ssl_mode {
        config.ssl_mode(mode);
    }
    config
}

/// PostgreSQL 参数：统一以**文本格式**传输。
///
/// 文本格式下由服务端把参数文本解析为目标列类型（与 Npgsql 行为一致），
/// 布尔 `true/false`、DECIMAL 定长文本、`bytea` 的 `\x` 十六进制、
/// 时间 `yyyy-MM-dd HH:mm:ss.fffffff` 均被 PostgreSQL 接受。
#[derive(Debug)]
struct PgParam(DbValue);

impl ToSql for PgParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        let text = match &self.0 {
            DbValue::Null => return Ok(IsNull::Yes),
            DbValue::Bool(v) => {
                if *v {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            DbValue::Int(v) => v.to_string(),
            DbValue::Float(v) => v.to_string(),
            DbValue::Decimal(v) => v.to_string(),
            DbValue::Text(v) => v.clone(),
            DbValue::Blob(v) => format!("\\x{}", to_hex(v)),
            DbValue::DateTime(v) => format_datetime(v),
        };
        out.extend_from_slice(text.as_bytes());
        Ok(IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        // 所有类型都先按文本传输，目标类型不兼容时由服务端给出明确错误
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

/// 十六进制编码（`bytea` 文本格式）。
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// PostgreSQL 值 → `DbValue`（按列类型分支）。
fn pg_to_dbvalue(row: &postgres::Row, index: usize, ty: &Type) -> Result<DbValue> {
    use Type as T;

    macro_rules! read {
        ($t:ty, $map:expr) => {
            match row.try_get::<usize, Option<$t>>(index) {
                Ok(value) => value.map($map).unwrap_or(DbValue::Null),
                Err(e) => {
                    return Err(Error::Db(format!(
                        "读取列 {}（{}）失败：{e}",
                        row.columns()[index].name(),
                        ty.name()
                    )));
                }
            }
        };
    }

    Ok(match ty.clone() {
        T::BOOL => read!(bool, DbValue::Bool),
        T::INT2 => read!(i16, |v| DbValue::Int(i64::from(v))),
        T::INT4 => read!(i32, |v| DbValue::Int(i64::from(v))),
        T::INT8 => read!(i64, DbValue::Int),
        T::FLOAT4 => read!(f32, |v| DbValue::Float(f64::from(v))),
        T::FLOAT8 => read!(f64, DbValue::Float),
        T::NUMERIC => read!(Decimal, DbValue::Decimal),
        T::TEXT | T::VARCHAR | T::BPCHAR | T::NAME | T::UNKNOWN => read!(String, DbValue::Text),
        T::BYTEA => read!(Vec<u8>, DbValue::Blob),
        T::TIMESTAMP => read!(NaiveDateTime, DbValue::DateTime),
        T::TIMESTAMPTZ => read!(DateTime<Utc>, |v| DbValue::DateTime(v.naive_utc())),
        T::DATE => read!(chrono::NaiveDate, |v| DbValue::DateTime(v.and_time(chrono::NaiveTime::MIN))),
        T::TIME => read!(chrono::NaiveTime, |v| DbValue::Text(
            v.format("%H:%M:%S%.6f").to_string()
        )),
        other => {
            // 其它类型（uuid/json/数组等）先尝试按文本读取
            match row.try_get::<usize, Option<String>>(index) {
                Ok(value) => value.map(DbValue::Text).unwrap_or(DbValue::Null),
                Err(_) => {
                    return Err(Error::Unsupported(format!(
                        "PostgreSQL 列类型 {other} 暂未支持读取（列 {}）",
                        row.columns()[index].name()
                    )));
                }
            }
        }
    })
}

/// 驱动错误 → 统一错误（数据库端错误附上消息与 SQLSTATE，便于定位）。
fn map_err(e: postgres::Error) -> Error {
    match e.as_db_error() {
        Some(db) => Error::Db(format!(
            "PostgreSQL 错误：{}（SQLSTATE {}）",
            db.message(),
            db.code().code()
        )),
        None => Error::Db(format!("PostgreSQL 错误：{e:?}")),
    }
}

impl SqlSession for PostgresSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::PostgreSql
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let bound: Vec<PgParam> = params.iter().cloned().map(PgParam).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        self.client.execute(sql, &refs).map_err(map_err)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let bound: Vec<PgParam> = params.iter().cloned().map(PgParam).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();

        // 预编译后取列信息：空结果集也能返回列名（与其它驱动行为一致）
        let statement = self.client.prepare(sql).map_err(map_err)?;
        let columns: Vec<String> = statement
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        let types: Vec<Type> = statement.columns().iter().map(|c| c.type_().clone()).collect();

        let rows = self.client.query(&statement, &refs).map_err(map_err)?;

        let mut set = RowSet::new(columns);
        for row in rows {
            let mut values = Vec::with_capacity(types.len());
            for (index, ty) in types.iter().enumerate() {
                values.push(pg_to_dbvalue(&row, index, ty)?);
            }
            set.push(values);
        }
        Ok(set)
    }

    fn begin(&mut self) -> Result<()> {
        self.client.batch_execute("BEGIN").map_err(map_err)
    }

    fn commit(&mut self) -> Result<()> {
        self.client.batch_execute("COMMIT").map_err(map_err)
    }

    fn rollback(&mut self) -> Result<()> {
        self.client.batch_execute("ROLLBACK").map_err(map_err)
    }

    fn last_identity(&mut self) -> Result<i64> {
        // 常规插入路径由 DAL 使用 RETURNING 回读；此处用于手工 SQL 场景
        let row = self
            .client
            .query_one("SELECT lastval()", &[])
            .map_err(map_err)?;
        Ok(row.get::<usize, i64>(0))
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let row = self
            .client
            .query_one(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = current_schema() AND lower(table_name) = lower($1)",
                &[&table],
            )
            .map_err(map_err)?;
        Ok(row.get::<usize, i64>(0) > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND lower(table_name) = lower($1) \
                 ORDER BY ordinal_position",
                &[&table],
            )
            .map_err(map_err)?;
        Ok(rows.iter().map(|row| row.get::<usize, String>(0)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn settings_from_xcode_connection_string() {
        let cs = ConnectionString::parse(
            "Server=10.0.0.9;Port=5433;Database=mes;Uid=sa;Pwd=p@ss;provider=highgo",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "10.0.0.9");
        assert_eq!(s.port, 5433);
        assert_eq!(s.user, "sa");
        assert_eq!(s.password, "p@ss");
        assert_eq!(s.database.as_deref(), Some("mes"));
        assert_eq!(s.application_name, "Pek.RCode");
        assert!(s.connect_timeout.is_some());
    }

    #[test]
    fn settings_defaults() {
        let cs = ConnectionString::parse("provider=postgresql");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "127.0.0.1");
        assert_eq!(s.port, 5432);
        assert_eq!(s.user, "postgres");
        assert_eq!(s.database, None);
        assert_eq!(s.ssl_mode, None);
    }

    #[test]
    fn required_tls_reports_actionable_error() {
        let cs = ConnectionString::parse(
            "Server=x;Database=d;Uid=u;Pwd=p;provider=postgresql;SslMode=Require",
        );
        let err = parse_settings(&cs).unwrap_err().to_string();
        assert!(err.contains("SslMode"), "{err}");

        // Prefer/Disable 走明文连接（与 Npgsql 的降级行为一致）
        let cs = ConnectionString::parse("Server=x;provider=postgresql;SslMode=Prefer");
        assert!(parse_settings(&cs).is_ok());
    }

    #[test]
    fn param_text_encoding() {
        let mut buffer = BytesMut::new();
        let param = PgParam(DbValue::Int(42));
        assert!(matches!(
            param.to_sql(&Type::INT8, &mut buffer),
            Ok(IsNull::No)
        ));
        assert_eq!(&buffer[..], b"42");

        let mut buffer = BytesMut::new();
        let _ = PgParam(DbValue::Bool(true)).to_sql(&Type::BOOL, &mut buffer);
        assert_eq!(&buffer[..], b"true");

        // NULL 不写入内容
        let mut buffer = BytesMut::new();
        assert!(matches!(
            PgParam(DbValue::Null).to_sql(&Type::TEXT, &mut buffer),
            Ok(IsNull::Yes)
        ));
        assert!(buffer.is_empty());

        // 二进制 → bytea 十六进制文本
        let mut buffer = BytesMut::new();
        let _ = PgParam(DbValue::Blob(vec![0x01, 0xab, 0xff])).to_sql(&Type::BYTEA, &mut buffer);
        assert_eq!(&buffer[..], b"\\x01abff");

        // 时间 → 7 位小数秒文本（PostgreSQL 解析为微秒精度）
        let dt = NaiveDate::from_ymd_opt(2026, 9, 26)
            .unwrap()
            .and_hms_micro_opt(18, 1, 2, 123_000)
            .unwrap();
        let mut buffer = BytesMut::new();
        let _ = PgParam(DbValue::DateTime(dt)).to_sql(&Type::TIMESTAMP, &mut buffer);
        assert_eq!(
            String::from_utf8(buffer.to_vec()).unwrap(),
            "2026-09-26 18:01:02.1230000"
        );

        // DECIMAL 保持文本精度
        let mut buffer = BytesMut::new();
        let d = "12.3400".parse::<Decimal>().unwrap();
        let _ = PgParam(DbValue::Decimal(d)).to_sql(&Type::NUMERIC, &mut buffer);
        assert_eq!(&buffer[..], b"12.3400");
    }
}
