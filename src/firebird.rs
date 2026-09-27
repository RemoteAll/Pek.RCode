//! Firebird 驱动：基于 `rsfbclient`（原生客户端，运行时动态加载 `fbclient.dll`）。
//!
//! 连接串与 XCode 兼容：
//!
//! ```text
//! -- 远端模式（默认 3050 端口）
//! Server=db.local;Port=3050;Database=C:\data\mes.fdb;Uid=SYSDBA;Pwd=masterkey;provider=firebird
//!
//! -- 内嵌模式（无 Server，Data Source 指向数据库文件，需 fbclient ≥3.0）
//! Data Source=C:\data\mes.fdb;Uid=SYSDBA;Pwd=masterkey;provider=firebird
//! ```
//!
//! 实现要点：
//! - 自增：序列 `SEQ_{表名}`（建表/同步自动创建，与 DH.NCode 一致），
//!   INSERT 注入 `next value for "SEQ_x"`，回读 `SELECT GEN_ID("SEQ_x", 0) FROM RDB$DATABASE`
//! - 布尔：Firebird 无布尔类型，用 SMALLINT 承载（与 DH.NCode 对齐）
//! - 分页：`ROWS a TO b`（见方言）
//! - 事务：`execute` 默认自动提交；`begin_transaction()/commit()/rollback()` 与 XCode 会话语义一致
//!
//! 运行环境：需要 `fbclient.dll`（Firebird 客户端，任意 Firebird 安装自带）。
//! 查找顺序：连接串 `Fbclient=` → 环境变量 `FBCLIENT_LIB_DIR` → PATH → `Program Files\Firebird\**`。

use std::path::PathBuf;

use rsfbclient::prelude::*;
use rsfbclient::{FbError, Row, SqlType};

use crate::dal::ConnectionString;
use crate::dialect::{DatabaseKind, oracle_identity_sequence};
use crate::error::{Error, Result};
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// Firebird 会话。
pub struct FirebirdSession {
    /// rsfbclient 连接（动态加载模式）
    conn: rsfbclient::Connection<rsfbclient_native::NativeFbClient<rsfbclient_native::DynLoad>>,
}

/// 连接模式。
#[derive(Debug, Clone, PartialEq)]
enum FirebirdMode {
    /// 远端模式
    Remote {
        /// 主机
        host: String,
        /// 端口（默认 3050）
        port: u16,
    },
    /// 内嵌模式
    Embedded,
}

/// 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct FirebirdSettings {
    /// 连接模式
    mode: FirebirdMode,
    /// 数据库路径/别名
    database: String,
    /// 用户名（默认 SYSDBA）
    user: String,
    /// 密码（默认 masterkey）
    password: String,
    /// fbclient 库路径（可空，自动查找）
    fbclient: Option<String>,
}

impl FirebirdSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        let settings = parse_settings(conn_str)?;
        let lib_path = match &settings.fbclient {
            Some(path) => path.clone(),
            None => locate_fbclient()?,
        };

        let conn_result = match &settings.mode {
            FirebirdMode::Remote { host, port } => {
                let mut builder = rsfbclient::builder_native()
                    .with_dyn_load(lib_path.clone())
                    .with_remote();
                builder.host(host.as_str());
                builder.port(*port);
                builder.db_name(settings.database.as_str());
                builder.user(settings.user.as_str());
                builder.pass(settings.password.as_str());
                builder.connect()
            }
            FirebirdMode::Embedded => {
                // 内嵌模式（本地文件直连）忽略用户名/密码
                let mut builder = rsfbclient::builder_native()
                    .with_dyn_load(lib_path.clone())
                    .with_embedded();
                builder.db_name(settings.database.as_str());
                builder.connect()
            }
        };

        let conn = conn_result.map_err(|e| {
            Error::Db(format!(
                "连接 Firebird 失败（{}）：{e}\n\
                 提示：本驱动通过 fbclient.dll 连接（动态加载），请确认已安装 Firebird 客户端",
                settings.database
            ))
        })?;

        Ok(Self { conn })
    }
}

/// 解析连接串（与 XCode 键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<FirebirdSettings> {
    let server = cs.get("server").or(cs.get("host"));
    let data_source = cs.get("data source").or(cs.get("datasource"));
    let database = cs
        .get("database")
        .or(cs.get("initial catalog"))
        .or(cs.get("db"))
        .or(data_source)
        .ok_or_else(|| {
            Error::Model("Firebird 连接串缺少 Database（数据库文件路径或别名）".into())
        })?
        .to_string();

    let mode = match server {
        Some(host) => {
            let port = match cs.get("port") {
                Some(v) => v
                    .parse::<u16>()
                    .map_err(|e| Error::Model(format!("无效的 Port \"{v}\"：{e}")))?,
                None => 3050,
            };
            FirebirdMode::Remote {
                host: host.to_string(),
                port,
            }
        }
        // 无 Server：按内嵌模式（文件直连）
        None => FirebirdMode::Embedded,
    };

    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("username"))
        .unwrap_or("SYSDBA")
        .to_string();
    let password = cs
        .get("pwd")
        .or(cs.get("password"))
        .or(cs.get("passwd"))
        .unwrap_or("masterkey")
        .to_string();
    let fbclient = cs
        .get("fbclient")
        .or(cs.get("client library"))
        .or(cs.get("library"))
        .map(str::to_string);

    Ok(FirebirdSettings {
        mode,
        database,
        user,
        password,
        fbclient,
    })
}

/// 自动查找 `fbclient.dll`：环境变量 → PATH → `Program Files\Firebird\**`。
fn locate_fbclient() -> Result<String> {
    let file_name = if cfg!(windows) { "fbclient.dll" } else { "libfbclient.so" };

    // 1) 环境变量指定目录
    if let Ok(dir) = std::env::var("FBCLIENT_LIB_DIR") {
        let candidate = PathBuf::from(&dir).join(file_name);
        if candidate.is_file() {
            return Ok(candidate.display().to_string());
        }
    }

    // 2) PATH
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(file_name);
            if candidate.is_file() {
                return Ok(candidate.display().to_string());
            }
        }
    }

    // 3) Program Files 下的 Firebird 安装（含版本子目录）
    for base in [
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let firebird = PathBuf::from(&base).join("Firebird");
        let direct = firebird.join(file_name);
        if direct.is_file() {
            return Ok(direct.display().to_string());
        }
        if let Ok(entries) = std::fs::read_dir(&firebird) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(file_name);
                if candidate.is_file() {
                    return Ok(candidate.display().to_string());
                }
            }
        }
    }

    Err(Error::Unsupported(format!(
        "未找到 {file_name}：请安装 Firebird 客户端，或在连接串指定 Fbclient=<路径>、设置环境变量 FBCLIENT_LIB_DIR"
    )))
}

/// `DbValue` → rsfbclient 绑定值。
fn dbvalue_to_firebird(value: &DbValue) -> SqlType {
    match value {
        DbValue::Null => SqlType::Null,
        DbValue::Bool(v) => SqlType::Integer(i64::from(*v)),
        DbValue::Int(v) => SqlType::Integer(*v),
        DbValue::Float(v) => SqlType::Floating(*v),
        // DECIMAL 以文本绑定，Firebird 隐式转换为目标数值列（保持精度）
        DbValue::Decimal(v) => SqlType::Text(v.to_string()),
        DbValue::Text(v) => SqlType::Text(v.clone()),
        DbValue::Blob(v) => SqlType::Binary(v.clone()),
        DbValue::DateTime(v) => SqlType::Timestamp(*v),
    }
}

/// rsfbclient 值 → `DbValue`。
fn firebird_to_dbvalue(value: &SqlType) -> DbValue {
    match value {
        SqlType::Null => DbValue::Null,
        SqlType::Boolean(v) => DbValue::Bool(*v),
        SqlType::Integer(v) => DbValue::Int(*v),
        SqlType::Floating(v) => DbValue::Float(*v),
        SqlType::Timestamp(v) => DbValue::DateTime(*v),
        SqlType::Binary(v) => DbValue::Blob(v.clone()),
        SqlType::Text(v) => {
            // 文本形式的数值（Firebird 的 DECIMAL 可能以文本返回）尝试收敛为 Decimal
            if let Ok(d) = v.parse::<rust_decimal::Decimal>()
                && v.contains('.')
            {
                return DbValue::Decimal(d);
            }
            DbValue::Text(v.clone())
        }
    }
}

/// 驱动错误 → 统一错误。
fn map_err(e: FbError) -> Error {
    Error::Db(format!("Firebird 错误：{e}"))
}

impl SqlSession for FirebirdSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::Firebird
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let values: Vec<SqlType> = params.iter().map(dbvalue_to_firebird).collect();
        let affected = Execute::execute(&mut self.conn, sql, values).map_err(map_err)?;
        Ok(affected as u64)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let values: Vec<SqlType> = params.iter().map(dbvalue_to_firebird).collect();
        let rows: Vec<Row> = Queryable::query(&mut self.conn, sql, values).map_err(map_err)?;

        // rsfbclient 的行保留列名（`Row::cols[i].name`）
        let columns: Vec<String> = rows
            .first()
            .map(|row| row.cols.iter().map(|col| col.name.clone()).collect())
            .unwrap_or_default();
        let mut set = RowSet::new(columns);
        for row in rows {
            let values: Vec<DbValue> = row
                .cols
                .iter()
                .map(|col| firebird_to_dbvalue(&col.value))
                .collect();
            set.push(values);
        }
        Ok(set)
    }

    fn begin(&mut self) -> Result<()> {
        self.conn.begin_transaction().map_err(map_err)
    }

    fn commit(&mut self) -> Result<()> {
        self.conn.commit().map_err(map_err)
    }

    fn rollback(&mut self) -> Result<()> {
        self.conn.rollback().map_err(map_err)
    }

    fn last_identity(&mut self) -> Result<i64> {
        Err(Error::Unsupported(
            "Firebird 的自增回写需要表名推导序列（SEQ_{表名}），请通过 DAL 插入（会调用 last_identity_of）"
                .into(),
        ))
    }

    fn last_identity_of(&mut self, table: &str) -> Result<i64> {
        let sequence = oracle_identity_sequence(table);
        let sql = format!(
            "SELECT GEN_ID({}, 0) FROM RDB$DATABASE",
            DatabaseKind::Firebird.quote(&sequence)
        );
        let rows: Vec<Row> = Queryable::query(&mut self.conn, &sql, ()).map_err(map_err)?;
        Ok(rows
            .first()
            .and_then(|row| row.cols.first())
            .map(|col| match &col.value {
                SqlType::Integer(v) => *v,
                other => firebird_to_dbvalue(other).as_i64().unwrap_or(0),
            })
            .unwrap_or(0))
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let rows: Vec<Row> = Queryable::query(
            &mut self.conn,
            "SELECT COUNT(*) FROM RDB$RELATIONS WHERE RDB$SYSTEM_FLAG = 0 AND (RDB$RELATION_NAME = ? OR RDB$RELATION_NAME = ? OR RDB$RELATION_NAME = ?)",
            (
                table.to_string(),
                table.to_uppercase(),
                table.to_lowercase(),
            ),
        )
        .map_err(map_err)?;
        let count = rows
            .first()
            .and_then(|row| row.cols.first())
            .map(|col| firebird_to_dbvalue(&col.value).as_i64().unwrap_or(0))
            .unwrap_or(0);
        Ok(count > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let rows: Vec<Row> = Queryable::query(
            &mut self.conn,
            "SELECT RDB$FIELD_NAME FROM RDB$RELATION_FIELDS \
             WHERE RDB$RELATION_NAME = ? OR RDB$RELATION_NAME = ? OR RDB$RELATION_NAME = ? \
             ORDER BY RDB$FIELD_POSITION",
            (
                table.to_string(),
                table.to_uppercase(),
                table.to_lowercase(),
            ),
        )
        .map_err(map_err)?;
        let mut names = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(col) = row.cols.first() {
                names.push(firebird_to_dbvalue(&col.value).to_text().trim().to_string());
            }
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn settings_remote_and_embedded() {
        let cs = ConnectionString::parse(
            "Server=fb.local;Port=3051;Database=C:\\data\\mes.fdb;Uid=SYSDBA;Pwd=masterkey;provider=firebird",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(
            s.mode,
            FirebirdMode::Remote {
                host: "fb.local".into(),
                port: 3051
            }
        );
        assert_eq!(s.database, "C:\\data\\mes.fdb");

        let cs = ConnectionString::parse("Data Source=mes.fdb;provider=firebird");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.mode, FirebirdMode::Embedded);
        assert_eq!(s.database, "mes.fdb");
        assert_eq!(s.user, "SYSDBA");
    }

    #[test]
    fn value_mapping() {
        assert!(matches!(dbvalue_to_firebird(&DbValue::Null), SqlType::Null));
        assert!(matches!(
            dbvalue_to_firebird(&DbValue::Bool(true)),
            SqlType::Integer(1)
        ));
        assert!(matches!(
            dbvalue_to_firebird(&DbValue::Int(7)),
            SqlType::Integer(7)
        ));
        assert!(matches!(
            dbvalue_to_firebird(&DbValue::Text("a".into())),
            SqlType::Text(v) if v == "a"
        ));

        let dt = NaiveDate::from_ymd_opt(2026, 9, 27)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        assert!(matches!(
            dbvalue_to_firebird(&DbValue::DateTime(dt)),
            SqlType::Timestamp(v) if v == dt
        ));

        assert_eq!(firebird_to_dbvalue(&SqlType::Integer(3)), DbValue::Int(3));
        assert_eq!(
            firebird_to_dbvalue(&SqlType::Text("12.50".into())),
            DbValue::Decimal("12.50".parse().unwrap())
        );
        assert_eq!(
            firebird_to_dbvalue(&SqlType::Text("A-001".into())),
            DbValue::Text("A-001".into())
        );
    }
}
