//! SQL Server 驱动：对应 DH.NCode 的 `SqlServer.cs`（基于 `tiberius`，纯 Rust TDS 协议实现）。
//!
//! 连接串与 XCode 完全兼容：
//!
//! ```text
//! Server=localhost;Port=1433;Database=mes;Uid=sa;Pwd=***;provider=sqlserver
//! ```
//!
//! 实现要点：
//! - 自增回写：插入后 `SELECT SCOPE_IDENTITY()`（与 DH.NCode 的 `SET NOCOUNT ON; ...; Select SCOPE_IDENTITY()` 一致）
//! - 参数按 Rust 类型直接绑定（bit/bigint/float/numeric/datetime2）；NULL 以文本 NULL 声明，
//!   由 SQL Server 隐式转换规则处理（NULL 可转换到任意列类型）
//! - tds73/tds80 默认开启，时间使用 `datetime2`（微秒精度）
//! - 表结构探测走 `INFORMATION_SCHEMA`
//!
//! 端点与加密：
//! - `Encrypt` 缺省按现代 SqlClient 行为取 `Required`（配合 `TrustServerCertificate` 缺省 true，
//!   可直连标准安装的自签证书实例）；`Encrypt=false` → `NotSupported`（明文登录包）
//! - `TrustServerCertificate=false` 时不跳过证书校验（要求实例使用受信任 CA 证书）
//!
//! 线程模型：`tiberius` 是异步实现，本驱动内部维护一个专用 tokio 运行时，
//! 以 `block_on` 方式提供同步接口。**不要在 tokio 异步上下文中调用**（会直接报错提示）。
//! 自签名证书场景请保持 `TrustServerCertificate=true`（默认）。

use std::borrow::Cow;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use tiberius::{
    AuthMethod, Client, ColumnData, Config as MssqlConfig, EncryptionLevel, ToSql,
};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::rt::runtime;
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// SQL Server 会话。
pub struct MssqlSession {
    /// tiberius 连接（基于 Compat 包装的 tokio 流）
    client: Client<Compat<TcpStream>>,
}

impl MssqlSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        let settings = parse_settings(conn_str)?;
        let runtime = runtime()?;

        let result = runtime.block_on(async {
            let mut config = MssqlConfig::new();
            config.host(&settings.host);
            config.port(settings.port);
            if let Some(db) = &settings.database {
                config.database(db.as_str());
            }
            config.application_name("Pek.RCode");
            config.encryption(settings.encryption);
            config.authentication(AuthMethod::sql_server(
                &settings.user,
                &settings.password,
            ));
            if settings.trust_server_certificate {
                config.trust_cert();
            }

            let addr = config.get_addr();
            let stream = match settings.connect_timeout {
                Some(timeout) => tokio::time::timeout(timeout, TcpStream::connect(&addr))
                    .await
                    .map_err(|_| Error::Db(format!("连接 SQL Server 超时（{addr}）")))?
                    .map_err(|e| Error::Db(format!("连接 SQL Server 失败（{addr}）：{e}")))?,
                None => TcpStream::connect(&addr)
                    .await
                    .map_err(|e| Error::Db(format!("连接 SQL Server 失败（{addr}）：{e}")))?,
            };
            stream
                .set_nodelay(true)
                .map_err(|e| Error::Db(format!("设置 TCP_NODELAY 失败：{e}")))?;

            Client::connect(config, stream.compat_write())
                .await
                .map_err(map_err)
        });

        Ok(Self { client: result? })
    }
}

/// 从连接串解析出的 SQL Server 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct MssqlSettings {
    /// 主机
    host: String,
    /// 端口（默认 1433）
    port: u16,
    /// 用户名（默认 sa）
    user: String,
    /// 密码
    password: String,
    /// 数据库名（可空）
    database: Option<String>,
    /// 连接超时
    connect_timeout: Option<Duration>,
    /// 是否跳过证书校验（`TrustServerCertificate`，默认 true，便于直连自签证书实例）
    trust_server_certificate: bool,
    /// 加密级别
    encryption: EncryptionLevel,
}

/// 解析连接串为设置结构（与 XCode 的键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<MssqlSettings> {
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
        None => 1433,
    };

    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("user id"))
        .or(cs.get("username"))
        .unwrap_or("sa")
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

    let trust_server_certificate = match cs
        .get("trustservercertificate")
        .or(cs.get("trust server certificate"))
    {
        Some(v) => parse_bool(v)?,
        None => true,
    };

    let encryption = match cs.get("encrypt") {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "required" | "mandatory" => EncryptionLevel::Required,
            "strict" => EncryptionLevel::Strict,
            "false" | "no" | "0" | "notsupported" | "disabled" => EncryptionLevel::NotSupported,
            other => return Err(Error::Model(format!("无效的 Encrypt \"{other}\""))),
        },
        // 缺省与新版 SqlClient 一致：加密连接 + 跳过自签证书校验（trust 缺省 true）
        None => EncryptionLevel::Required,
    };

    let connect_timeout = match cs.get("timeout").or(cs.get("connect timeout")) {
        Some(v) => Some(Duration::from_secs(
            v.parse::<u64>()
                .map_err(|e| Error::Model(format!("无效的 Timeout \"{v}\"：{e}")))?,
        )),
        None => Some(Duration::from_secs(15)),
    };

    Ok(MssqlSettings {
        host,
        port,
        user,
        password,
        database,
        connect_timeout,
        trust_server_certificate,
        encryption,
    })
}

/// 解析布尔配置值。
fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        other => Err(Error::Model(format!("无效的布尔值 \"{other}\""))),
    }
}

/// SQL Server 参数：按 Rust 类型直接绑定（tiberius 的 RPC 参数声明类型）。
#[derive(Debug)]
struct MssqlParam(DbValue);

impl ToSql for MssqlParam {
    fn to_sql(&self) -> ColumnData<'_> {
        match &self.0 {
            // NULL 以文本 NULL 声明，SQL Server 会按目标列类型隐式转换
            DbValue::Null => ColumnData::String(None),
            DbValue::Bool(v) => v.to_sql(),
            DbValue::Int(v) => v.to_sql(),
            DbValue::Float(v) => v.to_sql(),
            DbValue::Decimal(v) => v.to_sql(),
            DbValue::Text(v) => ColumnData::String(Some(Cow::Borrowed(v.as_str()))),
            DbValue::Blob(v) => ColumnData::Binary(Some(Cow::Borrowed(v.as_slice()))),
            DbValue::DateTime(v) => v.to_sql(),
        }
    }
}

/// SQL Server 单元格 → `DbValue`。
fn mssql_to_dbvalue(
    row: &tiberius::Row,
    index: usize,
    data: &ColumnData<'_>,
) -> Result<DbValue> {
    Ok(match data {
        // 时间类：通过 FromSql 读取为 chrono 类型（覆盖 datetime/smalldatetime/datetime2）
        ColumnData::DateTime(Some(_))
        | ColumnData::SmallDateTime(Some(_))
        | ColumnData::DateTime2(Some(_)) => match row.try_get::<NaiveDateTime, usize>(index) {
            Ok(Some(v)) => DbValue::DateTime(v),
            Ok(None) => DbValue::Null,
            Err(e) => return Err(Error::Db(format!("读取 datetime 列失败：{e}"))),
        },
        ColumnData::DateTimeOffset(Some(_)) => {
            match row.try_get::<DateTime<FixedOffset>, usize>(index) {
                Ok(Some(v)) => DbValue::DateTime(v.naive_local()),
                Ok(None) => DbValue::Null,
                // 退化为 UTC 时间
                Err(_) => match row.try_get::<NaiveDateTime, usize>(index) {
                    Ok(Some(v)) => DbValue::DateTime(v),
                    Ok(None) => DbValue::Null,
                    Err(e) => return Err(Error::Db(format!("读取 datetimeoffset 列失败：{e}"))),
                },
            }
        }
        ColumnData::Date(Some(_)) => match row.try_get::<NaiveDate, usize>(index) {
            Ok(Some(v)) => DbValue::DateTime(v.and_time(NaiveTime::MIN)),
            Ok(None) => DbValue::Null,
            Err(e) => return Err(Error::Db(format!("读取 date 列失败：{e}"))),
        },
        ColumnData::Time(Some(_)) => match row.try_get::<NaiveTime, usize>(index) {
            Ok(Some(v)) => DbValue::Text(v.format("%H:%M:%S%.7f").to_string()),
            Ok(None) => DbValue::Null,
            Err(e) => return Err(Error::Db(format!("读取 time 列失败：{e}"))),
        },
        // 标量类型：直接转换
        ColumnData::U8(Some(v)) => DbValue::Int(i64::from(*v)),
        ColumnData::I16(Some(v)) => DbValue::Int(i64::from(*v)),
        ColumnData::I32(Some(v)) => DbValue::Int(i64::from(*v)),
        ColumnData::I64(Some(v)) => DbValue::Int(*v),
        ColumnData::F32(Some(v)) => DbValue::Float(f64::from(*v)),
        ColumnData::F64(Some(v)) => DbValue::Float(*v),
        ColumnData::Bit(Some(v)) => DbValue::Bool(*v),
        ColumnData::String(Some(v)) => DbValue::Text(v.to_string()),
        ColumnData::Guid(Some(v)) => DbValue::Text(v.to_string()),
        ColumnData::Binary(Some(v)) => DbValue::Blob(v.to_vec()),
        ColumnData::Numeric(Some(v)) => match v.to_string().parse::<rust_decimal::Decimal>() {
            Ok(d) => DbValue::Decimal(d),
            // 超出 Decimal 范围（>38 位/特殊值）时保底为文本，不丢数据
            Err(_) => DbValue::Text(v.to_string()),
        },
        ColumnData::Xml(Some(v)) => DbValue::Text(v.to_string()),
        // 全部 NULL 分支
        ColumnData::U8(None)
        | ColumnData::I16(None)
        | ColumnData::I32(None)
        | ColumnData::I64(None)
        | ColumnData::F32(None)
        | ColumnData::F64(None)
        | ColumnData::Bit(None)
        | ColumnData::String(None)
        | ColumnData::Guid(None)
        | ColumnData::Binary(None)
        | ColumnData::Numeric(None)
        | ColumnData::Xml(None)
        | ColumnData::DateTime(None)
        | ColumnData::SmallDateTime(None)
        | ColumnData::Date(None)
        | ColumnData::Time(None)
        | ColumnData::DateTime2(None)
        | ColumnData::DateTimeOffset(None) => DbValue::Null,
    })
}

/// 驱动错误 → 统一错误。
fn map_err(e: tiberius::error::Error) -> Error {
    Error::Db(format!("SQL Server 错误：{e}"))
}

impl SqlSession for MssqlSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::SqlServer
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let bound: Vec<MssqlParam> = params.iter().cloned().map(MssqlParam).collect();
        let refs: Vec<&dyn ToSql> = bound.iter().map(|p| p as &dyn ToSql).collect();
        let client = &mut self.client;

        runtime()?.block_on(async move {
            let result = client.execute(sql, &refs).await.map_err(map_err)?;
            Ok(result.rows_affected().first().copied().unwrap_or(0))
        })
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let bound: Vec<MssqlParam> = params.iter().cloned().map(MssqlParam).collect();
        let refs: Vec<&dyn ToSql> = bound.iter().map(|p| p as &dyn ToSql).collect();
        let client = &mut self.client;

        runtime()?.block_on(async move {
            let mut stream = client.query(sql, &refs).await.map_err(map_err)?;
            // 先取列名：空结果集也能返回列信息（与其它驱动行为一致）
            let columns: Vec<String> = match stream.columns().await.map_err(map_err)? {
                Some(cols) => cols.iter().map(|c| c.name().to_string()).collect(),
                None => Vec::new(),
            };
            let rows = stream.into_first_result().await.map_err(map_err)?;

            let mut set = RowSet::new(columns);
            for row in rows {
                let mut values = Vec::new();
                for (index, (_, data)) in row.cells().enumerate() {
                    values.push(mssql_to_dbvalue(&row, index, data)?);
                }
                set.push(values);
            }
            Ok(set)
        })
    }

    fn begin(&mut self) -> Result<()> {
        let client = &mut self.client;
        runtime()?.block_on(async move {
            // 事务作用于会话，与批处理无关；simple_query 不返回结果集
            client
                .simple_query("BEGIN TRANSACTION")
                .await
                .map_err(map_err)?;
            Ok(())
        })
    }

    fn commit(&mut self) -> Result<()> {
        let client = &mut self.client;
        runtime()?.block_on(async move {
            client
                .simple_query("COMMIT TRANSACTION")
                .await
                .map_err(map_err)?;
            Ok(())
        })
    }

    fn rollback(&mut self) -> Result<()> {
        let client = &mut self.client;
        runtime()?.block_on(async move {
            client
                .simple_query("ROLLBACK TRANSACTION")
                .await
                .map_err(map_err)?;
            Ok(())
        })
    }

    fn last_identity(&mut self) -> Result<i64> {
        // 与 DH.NCode 一致：取当前作用域最近一次自增（同一会话内）
        let set = self.query("SELECT CAST(SCOPE_IDENTITY() AS BIGINT) AS id", &[])?;
        Ok(set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0))
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let set = self.query(
            "SELECT COUNT(*) AS n FROM INFORMATION_SCHEMA.TABLES WHERE LOWER(TABLE_NAME) = LOWER(@p0)",
            &[DbValue::Text(table.to_string())],
        )?;
        Ok(set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0)
            > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let set = self.query(
            "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS \
             WHERE LOWER(TABLE_NAME) = LOWER(@p0) ORDER BY ORDINAL_POSITION",
            &[DbValue::Text(table.to_string())],
        )?;
        Ok(set
            .rows
            .iter()
            .filter_map(|row| row.get(0).map(DbValue::to_text))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_xcode_connection_string() {
        let cs = ConnectionString::parse(
            "Server=10.0.0.5;Port=14330;Database=mes;Uid=sa;Pwd=p@ss;provider=sqlserver",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "10.0.0.5");
        assert_eq!(s.port, 14330);
        assert_eq!(s.user, "sa");
        assert_eq!(s.password, "p@ss");
        assert_eq!(s.database.as_deref(), Some("mes"));
        // 缺省：加密 + 跳过自签证书校验
        assert_eq!(s.encryption, EncryptionLevel::Required);
        assert!(s.trust_server_certificate);
    }

    #[test]
    fn settings_defaults_and_encrypt_switch() {
        let cs = ConnectionString::parse("provider=sqlserver");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "127.0.0.1");
        assert_eq!(s.port, 1433);
        assert_eq!(s.user, "sa");

        let cs = ConnectionString::parse("Encrypt=false;provider=sqlserver");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.encryption, EncryptionLevel::NotSupported);
        assert!(s.trust_server_certificate);

        let cs = ConnectionString::parse("TrustServerCertificate=false;provider=sqlserver");
        let s = parse_settings(&cs).unwrap();
        assert!(!s.trust_server_certificate);
        assert_eq!(s.encryption, EncryptionLevel::Required);

        // 无效值报错
        assert!(parse_settings(&ConnectionString::parse("Encrypt=maybe;provider=sqlserver")).is_err());
    }

    #[test]
    fn param_conversion() {
        assert_eq!(
            MssqlParam(DbValue::Bool(true)).to_sql(),
            ColumnData::Bit(Some(true))
        );
        assert_eq!(
            MssqlParam(DbValue::Int(5)).to_sql(),
            ColumnData::I64(Some(5))
        );
        assert_eq!(
            MssqlParam(DbValue::Float(1.5)).to_sql(),
            ColumnData::F64(Some(1.5))
        );
        match MssqlParam(DbValue::Text("abc".into())).to_sql() {
            ColumnData::String(Some(v)) => assert_eq!(v, "abc"),
            other => panic!("文本应绑定为字符串：{other:?}"),
        }
        match MssqlParam(DbValue::Blob(vec![1, 2])).to_sql() {
            ColumnData::Binary(Some(v)) => assert_eq!(&v[..], &[1, 2]),
            other => panic!("二进制应绑定为 varbinary：{other:?}"),
        }
        let d = "12.3400".parse::<rust_decimal::Decimal>().unwrap();
        match MssqlParam(DbValue::Decimal(d)).to_sql() {
            ColumnData::Numeric(Some(_)) => {}
            other => panic!("Decimal 应绑定为 numeric：{other:?}"),
        }
        // NULL 以文本 NULL 声明
        assert_eq!(
            MssqlParam(DbValue::Null).to_sql(),
            ColumnData::String(None)
        );
    }
}
