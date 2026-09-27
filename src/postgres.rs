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
//! TLS 说明（native-tls 后端）：
//! - `SslMode=Disable/Allow`：明文连接
//! - `SslMode=Prefer`（缺省）：先尝试 TLS，服务器不支持时回退明文（对齐 Npgsql）
//! - `SslMode=Require`：强制 TLS，只加密不校验证书
//! - `SslMode=VerifyCA`：校验证书链、不校验主机名；`SslMode=VerifyFull`：全量校验
//! - 根证书可用 `Root Certificate`/`SslCa` 指定（PEM/DER）
//! - 客户端证书（`SSL Certificate`/`SSL Key`，PEM）暂不支持（native-tls 仅支持 PKCS#12），会返回明确错误

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

        let client = if settings.ssl_mode == PgSslMode::Disable {
            config.connect(NoTls)
        } else {
            let connector = build_tls_connector(&settings)?;
            config.connect(postgres_native_tls::MakeTlsConnector::new(connector))
        }
        .map_err(|e| {
            Error::Db(format!(
                "连接 PostgreSQL 失败（{}:{}）：{e}",
                settings.host, settings.port
            ))
        })?;

        Ok(Self { client })
    }
}

/// TLS 模式（对齐 Npgsql 的 SslMode 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PgSslMode {
    /// 明文
    Disable,
    /// 能 TLS 就 TLS，服务器不支持时回退明文（缺省）
    Prefer,
    /// 强制 TLS，只加密不校验证书
    Require,
    /// 校验证书链、不校验主机名
    VerifyCa,
    /// 校验证书链与主机名
    VerifyFull,
}

impl PgSslMode {
    /// → postgres crate 的协商模式（校验细节由 TLS 连接器决定）。
    fn to_driver(self) -> SslMode {
        match self {
            PgSslMode::Disable => SslMode::Disable,
            PgSslMode::Prefer => SslMode::Prefer,
            PgSslMode::Require | PgSslMode::VerifyCa | PgSslMode::VerifyFull => SslMode::Require,
        }
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
    /// TLS 模式（缺省 Prefer）
    ssl_mode: PgSslMode,
    /// 根证书路径（Root Certificate/SslCa，PEM/DER）
    ssl_root_cert: Option<String>,
}

/// 解析连接串为设置结构（与 XCode 的键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<PostgresSettings> {
    // SslMode：与 Npgsql 语义对齐；缺省 Prefer（先试 TLS，服务器不支持回退明文）
    let ssl_mode = match cs.get("sslmode").map(|v| v.trim().to_ascii_lowercase()) {
        None => PgSslMode::Prefer,
        Some(mode) => match mode.as_str() {
            // Allow：优先明文，服务器要求时才 TLS；当前以明文实现（保持原语义）
            "disable" | "none" | "allow" => PgSslMode::Disable,
            "prefer" => PgSslMode::Prefer,
            "require" => PgSslMode::Require,
            "verifyca" | "verify-ca" => PgSslMode::VerifyCa,
            "verifyfull" | "verify-full" => PgSslMode::VerifyFull,
            other => return Err(Error::Model(format!("无效的 SslMode \"{other}\""))),
        },
    };
    // 客户端证书（PEM）当前后端不支持（native-tls 仅 PKCS#12），明确报错而非静默忽略
    if cs.get("ssl certificate").is_some()
        || cs.get("sslcert").is_some()
        || cs.get("ssl key").is_some()
        || cs.get("sslkey").is_some()
    {
        return Err(Error::Unsupported(
            "PostgreSQL 客户端证书（SSL Certificate/SSL Key）暂不支持：native-tls 仅支持 PKCS#12 客户端标识"
                .into(),
        ));
    }
    let ssl_root_cert = cs
        .get("root certificate")
        .or(cs.get("sslca"))
        .map(str::to_string);

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
        ssl_root_cert,
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
    config.ssl_mode(settings.ssl_mode.to_driver());
    config
}

/// 构建 native-tls 连接器（校验开关按 SslMode 映射）。
fn build_tls_connector(settings: &PostgresSettings) -> Result<native_tls::TlsConnector> {
    let mut builder = native_tls::TlsConnector::builder();
    match settings.ssl_mode {
        // Prefer/Require：只加密，不校验证书与主机名
        PgSslMode::Prefer | PgSslMode::Require => {
            builder.danger_accept_invalid_certs(true);
            builder.danger_accept_invalid_hostnames(true);
        }
        // VerifyCA：校验证书链、不校验主机名
        PgSslMode::VerifyCa => {
            builder.danger_accept_invalid_hostnames(true);
        }
        // VerifyFull：证书链与主机名全量校验
        PgSslMode::VerifyFull => {}
        PgSslMode::Disable => unreachable!("Disable 不构建 TLS 连接器"),
    }
    if let Some(path) = &settings.ssl_root_cert {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::Model(format!("读取根证书失败（{path}）：{e}")))?;
        let cert = native_tls::Certificate::from_pem(&bytes)
            .or_else(|_| native_tls::Certificate::from_der(&bytes))
            .map_err(|e| Error::Model(format!("解析根证书失败（{path}）：{e}")))?;
        builder.add_root_certificate(cert);
    }
    builder
        .build()
        .map_err(|e| Error::Db(format!("构建 TLS 连接器失败：{e}")))
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
        // 缺省 → Prefer（对齐 Npgsql，先试 TLS、服务器不支持时回退）
        assert_eq!(s.ssl_mode, PgSslMode::Prefer);
    }

    #[test]
    fn ssl_mode_parsing() {
        let cs = ConnectionString::parse("Server=x;provider=postgresql;SslMode=Disable");
        assert_eq!(parse_settings(&cs).unwrap().ssl_mode, PgSslMode::Disable);
        let cs = ConnectionString::parse("Server=x;provider=postgresql;SslMode=Require");
        assert_eq!(parse_settings(&cs).unwrap().ssl_mode, PgSslMode::Require);
        let cs = ConnectionString::parse("Server=x;provider=postgresql;SslMode=VerifyCA");
        assert_eq!(parse_settings(&cs).unwrap().ssl_mode, PgSslMode::VerifyCa);
        let cs = ConnectionString::parse(
            "Server=x;provider=postgresql;SslMode=VerifyFull;Root Certificate=ca.pem",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.ssl_mode, PgSslMode::VerifyFull);
        assert_eq!(s.ssl_root_cert.as_deref(), Some("ca.pem"));
        // 无效值报错
        let cs = ConnectionString::parse("Server=x;provider=postgresql;SslMode=Bogus");
        assert!(parse_settings(&cs).is_err());
        // 客户端证书（PEM）明确不支持
        let cs = ConnectionString::parse(
            "Server=x;provider=postgresql;SSL Certificate=c.pem;SSL Key=k.pem",
        );
        let err = parse_settings(&cs).unwrap_err().to_string();
        assert!(err.contains("客户端证书"), "{err}");
    }

    #[test]
    fn tls_connector_rejects_missing_root_cert() {
        let cs = ConnectionString::parse(
            "Server=x;provider=postgresql;SslMode=VerifyFull;Root Certificate=no-such-ca.pem",
        );
        let s = parse_settings(&cs).unwrap();
        let err = build_tls_connector(&s).unwrap_err().to_string();
        assert!(err.contains("根证书"), "{err}");
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
