//! Oracle 驱动：对应 DH.NCode 的 `Oracle.cs`（基于 `oracle` crate，OCI 经 ODPI-C 动态加载）。
//!
//! 连接串与 XCode 完全兼容：
//!
//! ```text
//! -- EZConnect 方式（推荐）
//! Server=host;Port=1521;ServiceName=xepdb1;Uid=peike;Pwd=***;provider=oracle
//!
//! -- TNS 别名 / DESCRIPTION 原样透传
//! Data Source=ORCLPDB;Uid=peike;Pwd=***;provider=oracle
//! ```
//!
//! 实现要点：
//! - 自增：与 XCode 约定一致使用独立序列 `SEQ_{表名}`（建表与同步结构时自动创建，
//!   INSERT 时写入 `"SEQ_表名".NEXTVAL`，随后读 `CURRVAL` 回写，见 DAL/sqlbuild）
//! - 占位符使用 OCI 位置绑定 `:1/:2/...`（与方言一致）
//! - DECIMAL 以文本绑定（Oracle 隐式转换为 NUMBER，保持精度）；读取 NUMBER 列时优先按文本
//!   解析为 Decimal，整数（scale<=0）回落为 Int
//! - 时间使用 `TIMESTAMP/DATE`（chrono 互转）；布尔使用 NUMBER(1) 的 1/0（与 DH.NCode 一致）
//! - 事务：Oracle 隐式开启事务，`begin()` 为空操作，`commit()/rollback()` 直接提交/回滚
//! - 表结构探测走 `USER_TABLES / USER_TAB_COLUMNS`（兼容引号与非引号两种存储大小写）
//!
//! 运行环境：需要 Oracle Instant Client（ODPI-C 在运行时动态加载 OCI 库），
//! 未安装时连接会返回带提示的错误。大文本（CLOB，>4000 字节）暂按 VARCHAR2 绑定，超长场景待后续增强。

use chrono::NaiveDateTime;
use oracle::Row;
use oracle::sql_type::{FromSql, OracleType, ToSql};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::dal::ConnectionString;
use crate::dialect::{DatabaseKind, oracle_identity_sequence};
use crate::error::{Error, Result};
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// Oracle 会话。
pub struct OracleSession {
    /// oracle crate 连接（同步 API）
    conn: oracle::Connection,
}

impl OracleSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        let settings = parse_settings(conn_str)?;
        let conn = oracle::Connection::connect(
            &settings.user,
            &settings.password,
            &settings.connect_string,
        )
        .map_err(|e| {
            Error::Db(format!(
                "连接 Oracle 失败（{}）：{e}\n\
                 提示：本驱动通过 Oracle Instant Client (OCI) 连接，请确认已安装并已加入 PATH",
                settings.connect_string
            ))
        })?;

        Ok(Self { conn })
    }
}

/// 从连接串解析出的 Oracle 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct OracleSettings {
    /// 用户名
    user: String,
    /// 密码
    password: String,
    /// 连接描述串（EZConnect `//host:port/service` 或 TNS 别名/描述）
    connect_string: String,
}

/// 解析连接串为设置结构（与 XCode 的键名兼容）。
///
/// 连接描述串组装规则：
/// 1. `Data Source` 含 `(`、`=` 或 `/` → 视为完整描述（DESCRIPTION/EZConnect）原样透传
/// 2. `Data Source` 为普通名称 → 视为 TNS 别名；若同时给了 ServiceName 则按 EZConnect 组装
/// 3. 无 `Data Source` 时按 `Server` + `Port`(默认 1521) + `ServiceName/Database` 组装
fn parse_settings(cs: &ConnectionString) -> Result<OracleSettings> {
    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("user id"))
        .or(cs.get("username"))
        .ok_or_else(|| Error::Model("Oracle 连接串缺少 Uid（用户名）".into()))?
        .to_string();

    let password = cs
        .get("pwd")
        .or(cs.get("password"))
        .or(cs.get("passwd"))
        .unwrap_or("")
        .to_string();

    let port = match cs.get("port") {
        Some(v) => v
            .parse::<u16>()
            .map_err(|e| Error::Model(format!("无效的 Port \"{v}\"：{e}")))?,
        None => 1521,
    };

    let service = cs
        .get("servicename")
        .or(cs.get("service name"))
        .or(cs.get("service"))
        .or(cs.get("database"))
        .or(cs.get("initial catalog"));

    let connect_string = match cs.get("data source").or(cs.get("datasource")) {
        // 完整描述（DESCRIPTION=...) 或 EZConnect（host:port/service）原样透传
        Some(ds) if ds.contains('(') || ds.contains('=') || ds.contains('/') => ds.to_string(),
        // TNS 别名：有 ServiceName 时按 EZConnect 组装，否则按别名交给 OCI 解析
        Some(alias) => match service {
            Some(service) => format!("//{alias}:{port}/{service}"),
            None => alias.to_string(),
        },
        None => {
            let server = cs
                .get("server")
                .or(cs.get("host"))
                .ok_or_else(|| Error::Model("Oracle 连接串缺少 Data Source 或 Server".into()))?;
            // Server 已含服务名（如 host:1521/xe）时原样使用
            if server.contains('/') {
                format!("//{server}")
            } else {
                let service = service.ok_or_else(|| {
                    Error::Model("Oracle 连接串缺少 ServiceName（或 Database）".into())
                })?;
                format!("//{server}:{port}/{service}")
            }
        }
    };

    Ok(OracleSettings {
        user,
        password,
        connect_string,
    })
}

/// `DbValue` → Oracle 绑定值。
///
/// - DECIMAL 以文本绑定，Oracle 隐式转换为目标 NUMBER 列（保持精度）
/// - 布尔绑定为 1/0（与 DH.NCode 的 `CreateParameter` 行为一致）
fn dbvalue_to_oracle(value: &DbValue) -> Box<dyn ToSql> {
    match value {
        DbValue::Null => Box::new(None::<i32>),
        DbValue::Bool(v) => Box::new(i64::from(*v)),
        DbValue::Int(v) => Box::new(*v),
        DbValue::Float(v) => Box::new(*v),
        DbValue::Decimal(v) => Box::new(v.to_string()),
        DbValue::Text(v) => Box::new(v.clone()),
        DbValue::Blob(v) => Box::new(v.clone()),
        DbValue::DateTime(v) => Box::new(*v),
    }
}

/// 读取列值（NULL → `None`）。
fn read<T: FromSql>(row: &Row, index: usize) -> Result<Option<T>> {
    row.get::<usize, Option<T>>(index)
        .map_err(|e| Error::Db(format!("Oracle 读取第 {} 列失败：{e}", index + 1)))
}

/// NUMBER 文本 → `DbValue`：scale<=0 且可容纳时回落为 Int（便于实体布尔/整型映射）。
fn parse_oracle_number(text: &str, scale: i8) -> DbValue {
    match text.parse::<Decimal>() {
        Ok(d) => {
            if scale <= 0
                && let Some(v) = d.to_i64()
            {
                DbValue::Int(v)
            } else {
                DbValue::Decimal(d)
            }
        }
        // 特殊格式（如无穷大）保底为文本，不丢数据
        Err(_) => DbValue::Text(text.to_string()),
    }
}

/// Oracle 行 → `DbValue`（按列类型分支）。
fn oracle_to_dbvalue(row: &Row, index: usize) -> Result<DbValue> {
    let info = &row.column_info()[index];

    Ok(match info.oracle_type() {
        // NUMBER 优先按文本读取保持精度；scale<=0 的整数回落为 Int
        OracleType::Number(_, scale) => match read::<String>(row, index)? {
            Some(text) => parse_oracle_number(&text, *scale),
            None => DbValue::Null,
        },
        OracleType::Float(_) | OracleType::BinaryFloat | OracleType::BinaryDouble => {
            match read::<f64>(row, index)? {
                Some(v) => DbValue::Float(v),
                None => DbValue::Null,
            }
        }
        OracleType::Int64 => match read::<i64>(row, index)? {
            Some(v) => DbValue::Int(v),
            None => DbValue::Null,
        },
        OracleType::UInt64 => match read::<u64>(row, index)? {
            Some(v) => DbValue::Int(v as i64),
            None => DbValue::Null,
        },
        OracleType::Varchar2(_)
        | OracleType::NVarchar2(_)
        | OracleType::Char(_)
        | OracleType::NChar(_)
        | OracleType::Long
        | OracleType::CLOB
        | OracleType::NCLOB
        | OracleType::Json
        | OracleType::Xml
        | OracleType::Rowid => match read::<String>(row, index)? {
            Some(text) => DbValue::Text(text),
            None => DbValue::Null,
        },
        OracleType::Date
        | OracleType::Timestamp(_)
        | OracleType::TimestampTZ(_)
        | OracleType::TimestampLTZ(_) => match read::<NaiveDateTime>(row, index)? {
            Some(v) => DbValue::DateTime(v),
            None => DbValue::Null,
        },
        OracleType::Raw(_) | OracleType::LongRaw | OracleType::BLOB => {
            match read::<Vec<u8>>(row, index)? {
                Some(v) => DbValue::Blob(v),
                None => DbValue::Null,
            }
        }
        OracleType::Boolean => match read::<bool>(row, index)? {
            Some(v) => DbValue::Bool(v),
            None => DbValue::Null,
        },
        other => {
            return Err(Error::Unsupported(format!(
                "Oracle 列类型 {other} 暂未支持读取（列 {}）",
                info.name()
            )));
        }
    })
}

/// 驱动错误 → 统一错误。
fn map_err(e: oracle::Error) -> Error {
    Error::Db(format!("Oracle 错误：{e}"))
}

impl SqlSession for OracleSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::Oracle
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let bound: Vec<Box<dyn ToSql>> = params.iter().map(dbvalue_to_oracle).collect();
        let refs: Vec<&dyn ToSql> = bound.iter().map(|p| p.as_ref()).collect();
        let statement = self.conn.execute(sql, &refs).map_err(map_err)?;
        statement.row_count().map_err(map_err)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let bound: Vec<Box<dyn ToSql>> = params.iter().map(dbvalue_to_oracle).collect();
        let refs: Vec<&dyn ToSql> = bound.iter().map(|p| p.as_ref()).collect();

        let result = self.conn.query(sql, &refs).map_err(map_err)?;
        // 先取列名：空结果集也能返回列信息（与其它驱动行为一致）
        let columns: Vec<String> = result
            .column_info()
            .iter()
            .map(|info| info.name().to_string())
            .collect();

        let mut set = RowSet::new(columns);
        for row in result {
            let row = row.map_err(map_err)?;
            let mut values = Vec::new();
            for index in 0..row.column_info().len() {
                values.push(oracle_to_dbvalue(&row, index)?);
            }
            set.push(values);
        }
        Ok(set)
    }

    fn begin(&mut self) -> Result<()> {
        // Oracle 不提供显式 BEGIN：开启事务是隐式的（默认手动提交模式）
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.conn.commit().map_err(map_err)
    }

    fn rollback(&mut self) -> Result<()> {
        self.conn.rollback().map_err(map_err)
    }

    fn last_identity(&mut self) -> Result<i64> {
        Err(Error::Unsupported(
            "Oracle 的自增回写需要表名推导序列（SEQ_{表名}），请通过 DAL 插入（会调用 last_identity_of）"
                .into(),
        ))
    }

    fn last_identity_of(&mut self, table: &str) -> Result<i64> {
        // 与 DH.NCode 一致：读取序列 CURRVAL（插入语句已写入 NEXTVAL）
        let sequence = oracle_identity_sequence(table);
        let sql = format!(
            "SELECT {}.CURRVAL FROM DUAL",
            DatabaseKind::Oracle.quote(&sequence)
        );
        let set = self.query(&sql, &[])?;
        Ok(set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0))
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        // 引号建表为原大小写、未引号折为大写：两种都探测
        let set = self.query(
            "SELECT COUNT(*) FROM USER_TABLES WHERE TABLE_NAME IN (:1, :2)",
            &[
                DbValue::Text(table.to_string()),
                DbValue::Text(table.to_uppercase()),
            ],
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
            "SELECT COLUMN_NAME FROM USER_TAB_COLUMNS WHERE TABLE_NAME IN (:1, :2) ORDER BY COLUMN_ID",
            &[
                DbValue::Text(table.to_string()),
                DbValue::Text(table.to_uppercase()),
            ],
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
    fn settings_ezconnect_from_server_and_service() {
        let cs = ConnectionString::parse(
            "Server=10.0.0.6;Port=1522;ServiceName=xepdb1;Uid=peike;Pwd=p@ss;provider=oracle",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.user, "peike");
        assert_eq!(s.password, "p@ss");
        assert_eq!(s.connect_string, "//10.0.0.6:1522/xepdb1");
    }

    #[test]
    fn settings_tns_alias_and_description_passthrough() {
        // TNS 别名原样交给 OCI
        let cs = ConnectionString::parse("Data Source=ORCLPDB;Uid=u;Pwd=p;provider=oracle");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.connect_string, "ORCLPDB");

        // TNS 别名 + ServiceName → 组装 EZConnect
        let cs = ConnectionString::parse(
            "Data Source=db1;ServiceName=xe;Uid=u;Pwd=p;provider=oracle",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.connect_string, "//db1:1521/xe");

        // 完整描述原样透传
        let cs = ConnectionString::parse(
            "Data Source=(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=xe)));Uid=u;Pwd=p;provider=oracle",
        );
        let s = parse_settings(&cs).unwrap();
        assert!(s.connect_string.starts_with("(DESCRIPTION="));
    }

    #[test]
    fn settings_requires_user_and_service() {
        let cs = ConnectionString::parse("Server=h;provider=oracle");
        assert!(parse_settings(&cs).is_err(), "缺少 Uid 应报错");

        let cs = ConnectionString::parse("Server=h;Uid=u;Pwd=p;provider=oracle");
        let err = parse_settings(&cs).unwrap_err().to_string();
        assert!(err.contains("ServiceName"), "{err}");

        // Server 已含服务名时可直接使用
        let cs = ConnectionString::parse("Server=h:1521/xe;Uid=u;Pwd=p;provider=oracle");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.connect_string, "//h:1521/xe");
    }

    #[test]
    fn number_text_conversion() {
        // scale<=0 且为整数 → Int（便于布尔/整型实体字段映射）
        assert_eq!(parse_oracle_number("5", 0), DbValue::Int(5));
        assert_eq!(parse_oracle_number("-7", -2), DbValue::Int(-7));
        // 小数 → Decimal 保持精度
        assert_eq!(
            parse_oracle_number("12.3400", 4),
            DbValue::Decimal("12.3400".parse::<Decimal>().unwrap())
        );
        // 无法解析的格式保底为文本
        assert!(matches!(parse_oracle_number("~", 0), DbValue::Text(_)));
    }
}
