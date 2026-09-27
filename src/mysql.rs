//! MySQL 驱动：对应 DH.NCode 的 `MySql.cs`（基于 `mysql` crate，纯 Rust 协议实现）。
//!
//! 连接串与 XCode 完全兼容：
//!
//! ```text
//! Server=localhost;Port=3306;Database=mes;Uid=root;Pwd=***;provider=mysql;SslMode=None
//! ```
//!
//! 实现要点：
//! - 值转换按 MySQL 协议类型处理：`BIT(n)` 大端字节 → 整数；二进制列 → Blob；其余字节尝试 UTF-8 文本
//! - 时间使用原生 `DATE/DATETIME` 绑定与解析（含微秒），非法日期（如 `0000-00-00`）退化为文本不丢数据
//! - 表结构探测走 `information_schema`（与 XCode 反向工程一致）
//! - 自增回写：`SELECT LAST_INSERT_ID()`
//!
//! TLS 说明（缺省 tls-native 后端；`--no-default-features --features tls-rustls` 可换 rustls 后端）：
//! - `SslMode=None/Disabled`：明文连接
//! - `SslMode=Preferred`（缺省）：先尝试 TLS，服务器不支持时回退明文（对齐 MySqlConnector）
//! - `SslMode=Required`：强制 TLS，只加密不校验证书
//! - `SslMode=VerifyCA`：校验证书链、不校验主机名；`SslMode=VerifyFull`：全量校验
//! - 根证书可用 `SslCa`/`CertificateFile` 指定（PEM/DER）
//! - 客户端证书（`SslCert`/`SslKey`，PEM）需 rustls 后端：
//!   `--no-default-features --features tls-rustls`；缺省 native-tls 后端仅支持 PKCS#12，会返回明确错误

use std::time::Duration;

use chrono::{Datelike, Timelike};
use mysql::consts::{ColumnFlags, ColumnType};
use mysql::prelude::Queryable;
use mysql::{Column, Conn, Opts, OptsBuilder, Value};

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// MySQL 会话。
pub struct MysqlSession {
    /// mysql crate 连接（同步、内部带缓冲）
    conn: Conn,
}

impl MysqlSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        let settings = parse_settings(conn_str)?;

        let result = match settings.ssl_mode {
            None => connect_once(&settings, false),
            // Preferred：先尝试 TLS；服务器不支持 SSL 时回退明文（对齐 MySqlConnector 语义）
            Some(MysqlSslMode::Preferred) => match connect_once(&settings, true) {
                Err(mysql::Error::DriverError(mysql::DriverError::TlsNotSupported)) => {
                    connect_once(&settings, false)
                }
                other => other,
            },
            Some(_) => connect_once(&settings, true),
        };

        let conn = result.map_err(|e| {
            Error::Db(format!(
                "连接 MySQL 失败（{}:{}）：{e}",
                settings.host, settings.port
            ))
        })?;

        Ok(Self { conn })
    }
}

/// 单次建连（按是否启用 TLS 组装选项）。
fn connect_once(settings: &MysqlSettings, tls: bool) -> std::result::Result<Conn, mysql::Error> {
    Conn::new(build_opts(settings, tls))
}

/// MySQL 的 TLS 模式（对齐 MySqlConnector 的 SslMode 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MysqlSslMode {
    /// 能 TLS 就 TLS，服务器不支持时回退明文（缺省）
    Preferred,
    /// 强制 TLS，只加密不校验证书
    Required,
    /// 校验证书链、不校验主机名
    VerifyCa,
    /// 校验证书链与主机名
    VerifyFull,
}

/// 从连接串解析出的 MySQL 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct MysqlSettings {
    /// 主机
    host: String,
    /// 端口（默认 3306）
    port: u16,
    /// 用户名（默认 root）
    user: String,
    /// 密码
    password: String,
    /// 数据库名（可空）
    database: Option<String>,
    /// 字符集（默认 utf8mb4，保证中文与 emoji 兼容）
    charset: String,
    /// 连接超时
    connect_timeout: Option<Duration>,
    /// TLS 模式（None 表示明文）
    ssl_mode: Option<MysqlSslMode>,
    /// 根证书路径（SslCa/CertificateFile，PEM/DER）
    ssl_root_cert: Option<String>,
    /// 客户端证书链路径（PEM；仅 tls-rustls 后端，`SslCert`）
    #[cfg(feature = "tls-rustls")]
    ssl_client_cert: Option<String>,
    /// 客户端私钥路径（PEM；仅 tls-rustls 后端，`SslKey`）
    #[cfg(feature = "tls-rustls")]
    ssl_client_key: Option<String>,
}

/// 解析连接串为设置结构（与 XCode 的键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<MysqlSettings> {
    // SslMode：与 MySqlConnector 语义对齐；缺省 Preferred（先试 TLS，服务器不支持回退明文）
    let ssl_mode = match cs.get("sslmode").map(|v| v.trim().to_ascii_lowercase()) {
        None => Some(MysqlSslMode::Preferred),
        Some(mode) => match mode.as_str() {
            "none" | "disabled" | "disable" | "off" => None,
            "preferred" | "prefer" => Some(MysqlSslMode::Preferred),
            "required" | "require" => Some(MysqlSslMode::Required),
            "verifyca" | "verify-ca" => Some(MysqlSslMode::VerifyCa),
            "verifyfull" | "verify-full" => Some(MysqlSslMode::VerifyFull),
            other => return Err(Error::Model(format!("无效的 SslMode \"{other}\""))),
        },
    };
    // 客户端证书：rustls 后端支持 PEM（SslCert/SslKey）；native-tls 仅 PKCS#12
    #[cfg(feature = "tls-rustls")]
    let (ssl_client_cert, ssl_client_key) = (
        cs.get("sslcert").map(str::to_string),
        cs.get("sslkey").map(str::to_string),
    );
    #[cfg(not(feature = "tls-rustls"))]
    if cs.get("sslcert").is_some() || cs.get("sslkey").is_some() {
        return Err(Error::Unsupported(
            "MySQL 客户端证书（SslCert/SslKey）需 rustls 后端：请用 --no-default-features --features tls-rustls 构建"
                .into(),
        ));
    }
    let ssl_root_cert = cs
        .get("sslca")
        .or(cs.get("certificatefile"))
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
        None => 3306,
    };

    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("user id"))
        .or(cs.get("username"))
        .unwrap_or("root")
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

    let charset = cs
        .get("charset")
        .or(cs.get("characterset"))
        .unwrap_or("utf8mb4")
        .to_string();
    // 字符集名称将拼入初始化命令，需严格校验（防注入）
    if charset.is_empty()
        || !charset
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(Error::Model(format!("无效的字符集 \"{charset}\"")));
    }

    let connect_timeout = match cs.get("timeout").or(cs.get("connect timeout")) {
        Some(v) => Some(Duration::from_secs(
            v.parse::<u64>()
                .map_err(|e| Error::Model(format!("无效的 Timeout \"{v}\"：{e}")))?,
        )),
        None => Some(Duration::from_secs(15)),
    };

    Ok(MysqlSettings {
        host,
        port,
        user,
        password,
        database,
        charset,
        connect_timeout,
        ssl_mode,
        ssl_root_cert,
        #[cfg(feature = "tls-rustls")]
        ssl_client_cert,
        #[cfg(feature = "tls-rustls")]
        ssl_client_key,
    })
}

/// 设置 → mysql crate 选项。
fn build_opts(settings: &MysqlSettings, tls: bool) -> Opts {
    let mut builder = OptsBuilder::new()
        .ip_or_hostname(Some(settings.host.clone()))
        .tcp_port(settings.port)
        .user(Some(settings.user.clone()))
        .pass(Some(settings.password.clone()));

    // v28 无 charset 选项：通过会话初始化命令设置（字符集已严格校验）
    builder = builder.init(vec![format!("SET NAMES {}", settings.charset)]);

    if let Some(db) = &settings.database {
        builder = builder.db_name(Some(db.clone()));
    }
    if let Some(timeout) = settings.connect_timeout {
        builder = builder.tcp_connect_timeout(Some(timeout));
    }
    if tls {
        #[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
        {
            builder = builder.ssl_opts(Some(build_ssl_opts(settings)));
        }
        #[cfg(not(any(feature = "tls-native", feature = "tls-rustls")))]
        {
            let _ = settings;
        }
    }

    Opts::from(builder)
}

/// 按 SslMode 组装 TLS 选项（校验开关对齐 MySqlConnector 语义）。
#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
fn build_ssl_opts(settings: &MysqlSettings) -> mysql::SslOpts {
    let mut ssl = mysql::SslOpts::default();
    match settings.ssl_mode {
        // Preferred/Required：只加密，不校验证书与主机名
        Some(MysqlSslMode::Preferred) | Some(MysqlSslMode::Required) => {
            ssl = ssl
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true);
        }
        // VerifyCA：校验证书链、不校验主机名
        Some(MysqlSslMode::VerifyCa) => {
            ssl = ssl.with_danger_skip_domain_validation(true);
        }
        // VerifyFull：证书链与主机名全量校验
        Some(MysqlSslMode::VerifyFull) | None => {}
    }
    if let Some(path) = settings.ssl_root_cert.as_deref() {
        ssl = ssl.with_root_cert_path(Some(std::path::PathBuf::from(path)));
    }
    // 客户端证书（PEM；仅 rustls 后端可用）
    #[cfg(feature = "tls-rustls")]
    if let (Some(cert), Some(key)) = (
        settings.ssl_client_cert.as_deref(),
        settings.ssl_client_key.as_deref(),
    ) {
        ssl = ssl.with_client_identity(Some(mysql::ClientIdentity::new(
            std::path::PathBuf::from(cert),
            std::path::PathBuf::from(key),
        )));
    }
    ssl
}

/// `DbValue` → MySQL 参数值。
fn dbvalue_to_mysql(value: &DbValue) -> Value {
    match value {
        DbValue::Null => Value::NULL,
        DbValue::Bool(v) => Value::Int(i64::from(*v)),
        DbValue::Int(v) => Value::Int(*v),
        DbValue::Float(v) => Value::Double(*v),
        // DECIMAL 走文本传输（服务端解析，保持精度）
        DbValue::Decimal(v) => Value::Bytes(v.to_string().into_bytes()),
        DbValue::Text(v) => Value::Bytes(v.as_bytes().to_vec()),
        DbValue::Blob(v) => Value::Bytes(v.clone()),
        DbValue::DateTime(v) => Value::Date(
            v.year() as u16,
            v.month() as u8,
            v.day() as u8,
            v.hour() as u8,
            v.minute() as u8,
            v.second() as u8,
            v.and_utc().timestamp_subsec_micros(),
        ),
    }
}

/// 列级元信息（读取转换用）。
struct ValueMeta {
    /// 是否 BIT 类型（按大端字节解析为整数）
    is_bit: bool,
    /// 是否二进制列（按 Blob 处理而非文本）
    is_binary: bool,
}

impl ValueMeta {
    /// 从驱动列描述构造。
    fn of(column: &Column) -> Self {
        Self {
            is_bit: column.column_type() == ColumnType::MYSQL_TYPE_BIT,
            is_binary: column.flags().contains(ColumnFlags::BINARY_FLAG),
        }
    }
}

/// MySQL 值 → `DbValue`。
fn mysql_to_dbvalue(value: Value, meta: &ValueMeta) -> DbValue {
    match value {
        Value::NULL => DbValue::Null,
        Value::Int(v) => DbValue::Int(v),
        Value::UInt(v) => DbValue::Int(v as i64),
        Value::Float(v) => DbValue::Float(f64::from(v)),
        Value::Double(v) => DbValue::Float(v),
        Value::Bytes(bytes) => {
            if meta.is_bit {
                // BIT(n) 按大端字节序列返回
                let mut acc: u64 = 0;
                for b in &bytes {
                    acc = (acc << 8) | u64::from(*b);
                }
                DbValue::Int(acc as i64)
            } else if meta.is_binary {
                DbValue::Blob(bytes)
            } else {
                match String::from_utf8(bytes) {
                    Ok(text) => DbValue::Text(text),
                    Err(e) => DbValue::Blob(e.into_bytes()),
                }
            }
        }
        Value::Date(y, mo, d, h, mi, s, us) => {
            match chrono::NaiveDate::from_ymd_opt(y as i32, mo as u32, d as u32)
                .and_then(|date| date.and_hms_micro_opt(h as u32, mi as u32, s as u32, us))
            {
                Some(dt) => DbValue::DateTime(dt),
                // 非法日期（如 0000-00-00）退化为文本，保证不丢数据
                None => DbValue::Text(format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")),
            }
        }
        Value::Time(neg, days, h, mi, s, us) => {
            // TIME 在本 ORM 数据类型中未单独建模：以文本保留原值
            let hours = u64::from(days) * 24 + u64::from(h);
            let sign = if neg { "-" } else { "" };
            DbValue::Text(format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}"))
        }
    }
}

/// 驱动错误 → 统一错误。
fn map_err(e: mysql::Error) -> Error {
    Error::Db(format!("MySQL 错误：{e}"))
}

impl SqlSession for MysqlSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::MySql
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let values: Vec<Value> = params.iter().map(dbvalue_to_mysql).collect();
        self.conn.exec_drop(sql, values).map_err(map_err)?;
        Ok(self.conn.affected_rows())
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let values: Vec<Value> = params.iter().map(dbvalue_to_mysql).collect();
        let result = self.conn.exec_iter(sql, values).map_err(map_err)?;

        // 先取列信息（迭代会消耗结果集）
        let columns: Vec<String> = result
            .columns()
            .as_ref()
            .iter()
            .map(|c| c.name_str().to_string())
            .collect();
        let metas: Vec<ValueMeta> = result
            .columns()
            .as_ref()
            .iter()
            .map(ValueMeta::of)
            .collect();

        let mut rows = RowSet::new(columns);
        for row in result {
            let row = row.map_err(map_err)?;
            let values = row.unwrap();
            let mut line = Vec::with_capacity(values.len());
            for (index, value) in values.into_iter().enumerate() {
                let meta = metas.get(index);
                line.push(match meta {
                    Some(meta) => mysql_to_dbvalue(value, meta),
                    // 理论上不会发生（值数量与列数量一致）
                    None => mysql_to_dbvalue(value, &ValueMeta { is_bit: false, is_binary: false }),
                });
            }
            rows.push(line);
        }

        Ok(rows)
    }

    fn begin(&mut self) -> Result<()> {
        self.conn.query_drop("START TRANSACTION").map_err(map_err)?;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.conn.query_drop("COMMIT").map_err(map_err)?;
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.conn.query_drop("ROLLBACK").map_err(map_err)?;
        Ok(())
    }

    fn last_identity(&mut self) -> Result<i64> {
        let value: Option<i64> = self
            .conn
            .query_first("SELECT LAST_INSERT_ID()")
            .map_err(map_err)?;
        Ok(value.unwrap_or(0))
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let count: Option<i64> = self
            .conn
            .exec_first(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND LOWER(table_name) = LOWER(?)",
                (table,),
            )
            .map_err(map_err)?;
        Ok(count.unwrap_or(0) > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let names: Vec<String> = self
            .conn
            .exec(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_schema = DATABASE() AND LOWER(table_name) = LOWER(?) \
                 ORDER BY ordinal_position",
                (table,),
            )
            .map_err(map_err)?;
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_xcode_connection_string() {
        let cs = ConnectionString::parse(
            "Server=10.0.0.8;Port=3307;Database=mes;Uid=sa;Pwd=p@ss;provider=mysql;SslMode=None",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "10.0.0.8");
        assert_eq!(s.port, 3307);
        assert_eq!(s.user, "sa");
        assert_eq!(s.password, "p@ss");
        assert_eq!(s.database.as_deref(), Some("mes"));
        assert_eq!(s.charset, "utf8mb4");
        assert!(s.connect_timeout.is_some());
    }

    #[test]
    fn settings_defaults() {
        let cs = ConnectionString::parse("provider=mysql");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "127.0.0.1");
        assert_eq!(s.port, 3306);
        assert_eq!(s.user, "root");
        assert_eq!(s.database, None);
    }

    #[test]
    fn ssl_mode_parsing() {
        // 显式 None/Disabled → 明文
        let cs = ConnectionString::parse("Server=x;provider=mysql;SslMode=None");
        assert_eq!(parse_settings(&cs).unwrap().ssl_mode, None);
        // 缺省 → Preferred（对齐 MySqlConnector）
        let cs = ConnectionString::parse("Server=x;provider=mysql");
        assert_eq!(
            parse_settings(&cs).unwrap().ssl_mode,
            Some(MysqlSslMode::Preferred)
        );
        // 各校验级别
        let cs = ConnectionString::parse("Server=x;provider=mysql;SslMode=Required");
        assert_eq!(
            parse_settings(&cs).unwrap().ssl_mode,
            Some(MysqlSslMode::Required)
        );
        let cs = ConnectionString::parse("Server=x;provider=mysql;SslMode=VerifyCA");
        assert_eq!(
            parse_settings(&cs).unwrap().ssl_mode,
            Some(MysqlSslMode::VerifyCa)
        );
        // 无效值报错
        let cs = ConnectionString::parse("Server=x;provider=mysql;SslMode=Bogus");
        assert!(parse_settings(&cs).is_err());
    }

    /// 客户端证书按后端特性区分：rustls 接受 PEM，native-tls 明确报错。
    #[test]
    fn ssl_client_cert_by_backend() {
        let cs = ConnectionString::parse("Server=x;provider=mysql;SslCert=c.pem;SslKey=k.pem");
        #[cfg(feature = "tls-rustls")]
        {
            let settings = parse_settings(&cs).unwrap();
            assert_eq!(settings.ssl_client_cert.as_deref(), Some("c.pem"));
            assert_eq!(settings.ssl_client_key.as_deref(), Some("k.pem"));
            // PEM 客户端证书进入 TLS 选项
            assert!(build_ssl_opts(&settings).client_identity().is_some());
        }
        #[cfg(not(feature = "tls-rustls"))]
        {
            let err = parse_settings(&cs).unwrap_err().to_string();
            assert!(err.contains("客户端证书"), "{err}");
            assert!(err.contains("tls-rustls"), "{err}");
        }
    }

    #[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
    #[test]
    fn ssl_opts_flags_by_mode() {
        let settings = |conn: &str| parse_settings(&ConnectionString::parse(conn)).unwrap();

        // Required：只加密，不校验证书与主机名
        let opts = build_ssl_opts(&settings("Server=x;provider=mysql;SslMode=Required"));
        assert!(opts.accept_invalid_certs());
        assert!(opts.skip_domain_validation());

        // VerifyCA：校验证书链、不校验主机名
        let opts = build_ssl_opts(&settings("Server=x;provider=mysql;SslMode=VerifyCA"));
        assert!(!opts.accept_invalid_certs());
        assert!(opts.skip_domain_validation());

        // VerifyFull：全量校验；根证书写入
        let opts = build_ssl_opts(&settings("Server=x;provider=mysql;SslMode=VerifyFull;SslCa=ca.pem"));
        assert!(!opts.accept_invalid_certs());
        assert!(!opts.skip_domain_validation());
        assert_eq!(opts.root_cert_path(), Some(std::path::Path::new("ca.pem")));
    }

    #[test]
    fn value_binding_conversions() {
        assert!(matches!(dbvalue_to_mysql(&DbValue::Null), Value::NULL));
        assert!(matches!(dbvalue_to_mysql(&DbValue::Bool(true)), Value::Int(1)));
        assert!(matches!(dbvalue_to_mysql(&DbValue::Int(5)), Value::Int(5)));
        assert!(matches!(dbvalue_to_mysql(&DbValue::Float(1.5)), Value::Double(_)));

        let d = "12.3400".parse::<rust_decimal::Decimal>().unwrap();
        match dbvalue_to_mysql(&DbValue::Decimal(d)) {
            Value::Bytes(b) => assert_eq!(String::from_utf8(b).unwrap(), "12.3400"),
            other => panic!("Decimal 应以文本传输：{other:?}"),
        }

        let dt = chrono::NaiveDate::from_ymd_opt(2026, 9, 26)
            .unwrap()
            .and_hms_micro_opt(18, 1, 2, 123_456)
            .unwrap();
        match dbvalue_to_mysql(&DbValue::DateTime(dt)) {
            Value::Date(2026, 9, 26, 18, 1, 2, 123456) => {}
            other => panic!("DateTime 应以原生日期传输：{other:?}"),
        }
    }

    #[test]
    fn value_reading_conversions() {
        let text_meta = ValueMeta { is_bit: false, is_binary: false };
        let blob_meta = ValueMeta { is_bit: false, is_binary: true };
        let bit_meta = ValueMeta { is_bit: true, is_binary: true };

        assert!(mysql_to_dbvalue(Value::NULL, &text_meta).is_null());
        assert_eq!(mysql_to_dbvalue(Value::Int(-3), &text_meta).as_i64(), Some(-3));
        assert_eq!(mysql_to_dbvalue(Value::UInt(7), &text_meta).as_i64(), Some(7));
        assert_eq!(
            mysql_to_dbvalue(Value::Bytes(b"hello".to_vec()), &text_meta).as_str(),
            Some("hello")
        );
        // 二进制列 → Blob（即便内容是合法 UTF-8 也不当文本）
        assert!(matches!(
            mysql_to_dbvalue(Value::Bytes(b"hello".to_vec()), &blob_meta),
            DbValue::Blob(_)
        ));
        // BIT(1)：单字节 0x01 → 1
        assert_eq!(mysql_to_dbvalue(Value::Bytes(vec![1]), &bit_meta).as_i64(), Some(1));
        // BIT(9)：0x01 0x00 → 256
        assert_eq!(
            mysql_to_dbvalue(Value::Bytes(vec![1, 0]), &bit_meta).as_i64(),
            Some(256)
        );

        // 非法日期 → 文本兜底
        let bad = mysql_to_dbvalue(Value::Date(0, 0, 0, 0, 0, 0, 0), &text_meta);
        assert_eq!(bad.as_str(), Some("0000-00-00 00:00:00.000000"));

        // 合法日期 → DateTime（含微秒）
        let good = mysql_to_dbvalue(Value::Date(2026, 9, 26, 18, 1, 2, 123456), &text_meta);
        let dt = good.as_datetime().unwrap();
        assert_eq!(dt.to_string(), "2026-09-26 18:01:02.123456");
    }
}
