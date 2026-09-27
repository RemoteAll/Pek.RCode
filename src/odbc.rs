//! ODBC 通用桥：一套实现覆盖 **DB2 / 达梦（DM8）/ InterSystems IRIS / Access（ACE）** 等
//! 具备 ODBC 驱动的数据库（`provider=db2/dameng/iris/access`）。
//!
//! 连接串与 XCode 兼容：
//!
//! ```text
//! -- 按 provider 自动生成 DSN-less 连接串（示例：达梦）
//! Server=dm.local;Port=5236;Database=SYSDBA;Uid=SYSDBA;Pwd=***;provider=dameng
//!
//! -- 自带完整 ODBC 描述（含 DRIVER= 或 DSN= 时优先透传）
//! DRIVER={DM8 ODBC DRIVER};SERVER=dm.local;PORT=5236;UID=SYSDBA;PWD=***;provider=dameng
//! ```
//!
//! 实现要点：
//! - 各行按 DH.NCode 的方言约定（DB2 使用 Oracle 兼容模式、达梦 `IDENTITY`、IRIS `IDENTITY`、
//!   Access `COUNTER`），见 [`crate::dialect`]
//! - 参数采用**字面量内联**（标准 SQL 转义），避免依赖各 ODBC 驱动的参数绑定细节
//! - 取数使用 `TextRowSet`（全列文本）再按列类型转换；二进制列按文本字节读取（有 NUL 截断风险，见已知限制）
//! - 事务：`begin` 关闭自动提交，`commit/rollback` 结束事务并恢复自动提交
//!
//! 已知限制：
//! - Access 不支持列清单探测（`sync_schema` 仅支持建表，不支持补列）
//! - 写入受影响行数依赖驱动支持，无法获取时返回 0

use odbc_api::buffers::TextRowSet;
use odbc_api::{
    ColumnDescription, Connection, ConnectionOptions, Cursor, DataType, Environment,
    ResultSetMetadata,
};
use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::http::{LiteralStyle, inline_params};
use crate::session::{RowSet, SqlSession};
use crate::value::{DbValue, parse_datetime};

/// ODBC 会话（复用同一实现承载多个 provider）。
pub struct OdbcSession {
    /// 具体数据库类型（决定元数据查询等差异）
    kind: DatabaseKind,
    /// ODBC 连接（生命周期绑定在泄漏的 Environment 上，进程内一次性释放）
    conn: Connection<'static>,
}

impl OdbcSession {
    /// 打开 ODBC 连接。
    pub fn open(kind: DatabaseKind, conn_str: &ConnectionString) -> Result<Self> {
        let connection_string = build_connection_string(kind, conn_str)?;

        let environment = Environment::new()
            .map_err(|e| Error::Db(format!("初始化 ODBC 环境失败：{e}")))?;
        // 泄漏环境以取得 'static 生命周期（每个会话约几百字节，随进程结束释放）
        let environment: &'static Environment = Box::leak(Box::new(environment));
        let conn = environment
            .connect_with_connection_string(&connection_string, ConnectionOptions::default())
            .map_err(|e| {
                Error::Db(format!(
                    "连接 ODBC 数据库失败（{}）：{e}\n连接串：{connection_string}",
                    kind.name()
                ))
            })?;

        Ok(Self { kind, conn })
    }
}

/// 构建 ODBC 连接串：自带描述时透传（剔除 XCode 专有键），否则按 provider 模板生成。
fn build_connection_string(kind: DatabaseKind, cs: &ConnectionString) -> Result<String> {
    let lower = cs.raw().to_ascii_lowercase();
    if lower.contains("driver=") || lower.contains("dsn=") {
        // 剔除 ODBC 不认识的 XCode 键
        let kept: Vec<String> = cs
            .raw()
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter(|part| {
                let key = part
                    .split_once('=')
                    .map(|(k, _)| k.trim().to_ascii_lowercase())
                    .unwrap_or_default();
                !matches!(key.as_str(), "provider" | "showsql")
            })
            .map(str::to_string)
            .collect();
        return Ok(format!("{};", kept.join(";")));
    }

    let host = cs.get("server").or(cs.get("host")).unwrap_or("127.0.0.1");
    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("user id"))
        .unwrap_or("");
    let password = cs.get("pwd").or(cs.get("password")).unwrap_or("");

    Ok(match kind {
        DatabaseKind::Db2 => {
            let port = cs.get("port").unwrap_or("50000");
            let db = cs
                .get("database")
                .or(cs.get("initial catalog"))
                .ok_or_else(|| Error::Model("DB2 连接串缺少 Database".into()))?;
            format!(
                "DRIVER={{IBM DB2 ODBC DRIVER}};HOSTNAME={host};PORT={port};DATABASE={db};UID={user};PWD={password};PROTOCOL=TCPIP;"
            )
        }
        DatabaseKind::DaMeng => {
            let port = cs.get("port").unwrap_or("5236");
            format!("DRIVER={{DM8 ODBC DRIVER}};SERVER={host};PORT={port};UID={user};PWD={password};")
        }
        DatabaseKind::Iris => {
            let port = cs.get("port").unwrap_or("1972");
            let namespace = cs
                .get("database")
                .or(cs.get("namespace"))
                .unwrap_or("USER");
            format!(
                "DRIVER={{InterSystems IRIS ODBC35}};SERVER={host}:{port};NAMESPACE={namespace};UID={user};PWD={password};"
            )
        }
        DatabaseKind::Access => {
            let path = cs
                .get("data source")
                .or(cs.get("database"))
                .or(cs.get("filename"))
                .ok_or_else(|| {
                    Error::Model("Access 连接串缺少 Data Source（.mdb/.accdb 文件路径）".into())
                })?;
            format!("DRIVER={{Microsoft Access Driver (*.mdb, *.accdb)}};DBQ={path};")
        }
        other => {
            return Err(Error::Unsupported(format!(
                "ODBC 桥不支持 {}（支持：db2/dameng/iris/access）",
                other.name()
            )));
        }
    })
}

/// 文本单元 → `DbValue`（按列类型）。
fn odbc_cell_to_dbvalue(cell: Option<&[u8]>, kind: &DataType) -> DbValue {
    let Some(bytes) = cell else {
        return DbValue::Null;
    };
    let text = String::from_utf8_lossy(bytes);

    match kind {
        DataType::TinyInt | DataType::SmallInt | DataType::Integer | DataType::BigInt => {
            match text.trim().parse::<i64>() {
                Ok(v) => DbValue::Int(v),
                Err(_) => DbValue::Text(text.into_owned()),
            }
        }
        DataType::Float { .. } | DataType::Real | DataType::Double => {
            match text.trim().parse::<f64>() {
                Ok(v) => DbValue::Float(v),
                Err(_) => DbValue::Text(text.into_owned()),
            }
        }
        DataType::Numeric { .. } | DataType::Decimal { .. } => {
            match text.trim().parse::<rust_decimal::Decimal>() {
                Ok(v) => DbValue::Decimal(v),
                Err(_) => DbValue::Text(text.into_owned()),
            }
        }
        DataType::Bit => DbValue::Bool(matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y"
        )),
        DataType::Date | DataType::Timestamp { .. } => {
            if let Some(dt) = parse_datetime(text.trim()) {
                return DbValue::DateTime(dt);
            }
            // 仅日期部分
            if let Some(date) = chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
                && let Some(dt) = date.and_hms_opt(0, 0, 0)
            {
                return DbValue::DateTime(dt);
            }
            DbValue::Text(text.into_owned())
        }
        DataType::Time { .. } => DbValue::Text(text.into_owned()),
        DataType::Binary { .. } | DataType::Varbinary { .. } | DataType::LongVarbinary { .. } => {
            DbValue::Blob(bytes.to_vec())
        }
        _ => DbValue::Text(text.into_owned()),
    }
}

/// 驱动错误 → 统一错误。
fn map_err(e: odbc_api::Error) -> Error {
    Error::Db(format!("ODBC 错误：{e}"))
}

impl SqlSession for OdbcSession {
    fn kind(&self) -> DatabaseKind {
        self.kind
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let sql = inline_params(sql, params, LiteralStyle::Standard)?;
        // ODBC 高层 API 不直接提供受影响行数
        let _ = self.conn.execute(&sql, (), None).map_err(map_err)?;
        Ok(0)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let sql = inline_params(sql, params, LiteralStyle::Standard)?;
        let mut cursor = self
            .conn
            .execute(&sql, (), None)
            .map_err(map_err)?
            .ok_or_else(|| Error::Db("ODBC 语句未返回结果集".into()))?;

        // 列名与类型
        let num_cols = cursor.num_result_cols().map_err(map_err)?;
        let mut names = Vec::with_capacity(num_cols as usize);
        let mut kinds = Vec::with_capacity(num_cols as usize);
        for index in 1..=num_cols as u16 {
            let mut description = ColumnDescription::default();
            cursor
                .describe_col(index, &mut description)
                .map_err(map_err)?;
            names.push(
                description
                    .name_to_string()
                    .map_err(|e| Error::Db(format!("ODBC 列名解码失败：{e}")))?,
            );
            kinds.push(description.data_type);
        }

        // 全列按文本取回（各驱动对类型转换的支持最好）
        let row_set = TextRowSet::for_cursor(256, &mut cursor, Some(8192)).map_err(map_err)?;
        let mut block = cursor.bind_buffer(row_set).map_err(map_err)?;

        let mut set = RowSet::new(names);
        while let Some(batch) = block.fetch().map_err(map_err)? {
            for row in 0..batch.num_rows() {
                let mut values = Vec::with_capacity(kinds.len());
                for (index, kind) in kinds.iter().enumerate() {
                    values.push(odbc_cell_to_dbvalue(batch.at(index, row), kind));
                }
                set.push(values);
            }
        }
        Ok(set)
    }

    fn begin(&mut self) -> Result<()> {
        self.conn.set_autocommit(false).map_err(map_err)
    }

    fn commit(&mut self) -> Result<()> {
        self.conn.commit().map_err(map_err)?;
        self.conn.set_autocommit(true).map_err(map_err)
    }

    fn rollback(&mut self) -> Result<()> {
        self.conn.rollback().map_err(map_err)?;
        self.conn.set_autocommit(true).map_err(map_err)
    }

    fn last_identity(&mut self) -> Result<i64> {
        let sql = match self.kind {
            DatabaseKind::Iris => "SELECT LAST_IDENTITY()",
            // DB2/SQL Server 系与 Access/达梦：@@IDENTITY / IDENTITY_VAL_LOCAL()
            DatabaseKind::Db2 => "SELECT IDENTITY_VAL_LOCAL() FROM SYSIBM.SYSDUMMY1",
            _ => "SELECT @@IDENTITY",
        };
        let set = self.query(sql, &[])?;
        Ok(set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0))
    }

    fn last_identity_of(&mut self, table: &str) -> Result<i64> {
        // DB2（Oracle 兼容模式）：序列 SEQ_表名（不引号，与 DH.NCode 一致）
        if self.kind == DatabaseKind::Db2 {
            let sequence = crate::dialect::oracle_identity_sequence(table);
            let set = self.query(&format!("SELECT {sequence}.CURRVAL FROM dual"), &[])?;
            return Ok(set
                .first()
                .and_then(|row| row.get(0))
                .and_then(DbValue::as_i64)
                .unwrap_or(0));
        }
        self.last_identity()
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let upper = table.to_uppercase();
        let sql = match self.kind {
            DatabaseKind::Db2 => format!(
                "SELECT COUNT(*) FROM SYSCAT.TABLES WHERE TABNAME = '{upper}'"
            ),
            DatabaseKind::DaMeng => format!(
                "SELECT COUNT(*) FROM USER_TABLES WHERE TABLE_NAME = '{upper}'"
            ),
            DatabaseKind::Iris => format!(
                "SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_NAME IN ('{table}', '{upper}')"
            ),
            DatabaseKind::Access => {
                // 走 ODBC 目录接口（MSysObjects 在部分驱动下不可见）
                return Ok(read_access_catalog(&self.conn)?
                    .iter()
                    .any(|t| t.name.eq_ignore_ascii_case(table)));
            }
            _ => return Err(Error::Unsupported("ODBC 表探测不支持该数据库".into())),
        };
        let set = self.query(&sql, &[])?;
        Ok(set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0)
            > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let upper = table.to_uppercase();
        let sql = match self.kind {
            DatabaseKind::Db2 => format!(
                "SELECT COLNAME FROM SYSCAT.COLUMNS WHERE TABNAME = '{upper}' ORDER BY COLNO"
            ),
            DatabaseKind::DaMeng => format!(
                "SELECT COLUMN_NAME FROM USER_TAB_COLUMNS WHERE TABLE_NAME = '{upper}' ORDER BY COLUMN_ID"
            ),
            DatabaseKind::Iris => format!(
                "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME IN ('{table}', '{upper}') ORDER BY ORDINAL_POSITION"
            ),
            DatabaseKind::Access => {
                // 走 ODBC 目录接口
                let tables = read_access_catalog(&self.conn)?;
                return Ok(tables
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(table))
                    .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default());
            }
            _ => return Err(Error::Unsupported("ODBC 列探测不支持该数据库".into())),
        };
        let set = self.query(&sql, &[])?;
        Ok(set
            .rows
            .iter()
            .filter_map(|row| row.get(0).map(DbValue::to_text))
            .collect())
    }

    fn catalog_tables(&mut self) -> Result<Option<Vec<crate::catalog::TableInfo>>> {
        if self.kind != DatabaseKind::Access {
            return Ok(None);
        }
        Ok(Some(read_access_catalog(&self.conn)?))
    }
}

/// Access：通过 ODBC 目录接口读取表/列/主键（Access 无 SQL 型目录；索引接口暂缺）。
fn read_access_catalog(conn: &Connection<'static>) -> Result<Vec<crate::catalog::TableInfo>> {
    use crate::catalog::{ColumnInfo, TableInfo};

    /// ODBC 错误 → 统一错误。
    fn odbc_err(e: odbc_api::Error) -> Error {
        Error::Db(format!("Access ODBC 目录读取失败：{e}"))
    }

    let mut tables: Vec<TableInfo> = Vec::new();
    for row in conn.tables("", "", "", "TABLE").map_err(odbc_err)? {
        let row = row.map_err(odbc_err)?;
        // as_str 的 UTF-8 错误按空串容忍（目录文本，无法解密时跳过）
        let name = row
            .table
            .as_str()
            .unwrap_or_default()
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        tables.push(TableInfo {
            name,
            description: row
                .remarks
                .as_str()
                .unwrap_or_default()
                .unwrap_or_default()
                .to_string(),
            columns: Vec::new(),
            indexes: Vec::new(),
        });
    }

    for table in &mut tables {
        let mut columns: Vec<ColumnInfo> = Vec::new();
        for row in conn.columns("", "", &table.name, "").map_err(odbc_err)? {
            let row = row.map_err(odbc_err)?;
            let name = row
                .column_name
                .as_str()
                .unwrap_or_default()
                .unwrap_or_default()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let type_name = row
                .type_name
                .as_str()
                .unwrap_or_default()
                .unwrap_or_default()
                .to_string();
            let size = row.column_size.as_opt().copied().unwrap_or(0);
            let (data_type, length, precision, scale, identity) = map_access_type(&type_name, size);
            columns.push(ColumnInfo {
                name,
                raw_type: type_name,
                data_type,
                length,
                precision,
                scale,
                identity,
                primary_key: false,
                nullable: row.nullable != 0,
                default_value: row
                    .column_default
                    .as_str()
                    .unwrap_or_default()
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
                description: row
                    .remarks
                    .as_str()
                    .unwrap_or_default()
                    .unwrap_or_default()
                    .to_string(),
            });
        }

        // 主键列（ODBC SQLPrimaryKeys）
        for pk in conn
            .primary_keys(None, None, &table.name)
            .map_err(odbc_err)?
        {
            let pk = pk.map_err(odbc_err)?;
            let column_name = pk
                .column
                .as_str()
                .unwrap_or_default()
                .unwrap_or_default()
                .to_string();
            if let Some(col) = columns
                .iter_mut()
                .find(|c| c.name.eq_ignore_ascii_case(&column_name))
            {
                col.primary_key = true;
                col.nullable = false;
            }
        }

        table.columns = columns;
    }

    Ok(tables)
}

/// Access ODBC 类型名 → 模型类型（返回 类型/长度/精度/小数位/是否自增）。
fn map_access_type(type_name: &str, size: i32) -> (crate::types::DataType, i32, i32, i32, bool) {
    use crate::types::DataType as ModelType;
    let upper = type_name.trim().to_ascii_uppercase();
    match upper.as_str() {
        "COUNTER" => (ModelType::Int32, 0, 0, 0, true),
        "BYTE" => (ModelType::Byte, 0, 0, 0, false),
        "SMALLINT" | "SHORT" => (ModelType::Int16, 0, 0, 0, false),
        "INTEGER" | "LONG" => (ModelType::Int32, 0, 0, 0, false),
        "BIGINT" => (ModelType::Int64, 0, 0, 0, false),
        "REAL" | "SINGLE" => (ModelType::Single, 0, 0, 0, false),
        "DOUBLE" | "FLOAT" => (ModelType::Double, 0, 0, 0, false),
        "DECIMAL" | "NUMERIC" | "CURRENCY" | "MONEY" => {
            (ModelType::Decimal, 0, size.max(0), 0, false)
        }
        "VARCHAR" | "CHAR" | "TEXT" => (ModelType::String, size.max(0), 0, 0, false),
        "LONGCHAR" | "MEMO" | "LONGTEXT" | "NOTE" => (ModelType::String, 0, 0, 0, false),
        "DATETIME" | "TIMESTAMP" | "DATE" => (ModelType::DateTime, 0, 0, 0, false),
        "BIT" | "LOGICAL" | "YESNO" => (ModelType::Boolean, 0, 0, 0, false),
        "BINARY" | "VARBINARY" | "LONGBINARY" | "OLE" => (ModelType::Binary, 0, 0, 0, false),
        "GUID" | "UNIQUEIDENTIFIER" => (ModelType::String, 0, 0, 0, false),
        _ => (ModelType::String, 0, 0, 0, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_string_templates() {
        let cs = ConnectionString::parse("Server=dm.local;Port=5236;Uid=SYSDBA;Pwd=***;provider=dameng");
        let s = build_connection_string(DatabaseKind::DaMeng, &cs).unwrap();
        assert!(s.contains("DRIVER={DM8 ODBC DRIVER}"), "{s}");
        assert!(s.contains("SERVER=dm.local;PORT=5236"), "{s}");

        let cs = ConnectionString::parse("Data Source=mes.mdb;provider=access");
        let s = build_connection_string(DatabaseKind::Access, &cs).unwrap();
        assert!(s.contains("DBQ=mes.mdb"), "{s}");

        // 自带 DRIVER 时透传，并剔除 XCode 专有键
        let cs = ConnectionString::parse(
            "DRIVER={IBM DB2 ODBC DRIVER};HOSTNAME=h;DATABASE=db;UID=u;PWD=p;provider=db2;ShowSql=true",
        );
        let s = build_connection_string(DatabaseKind::Db2, &cs).unwrap();
        assert!(!s.contains("provider="), "{s}");
        assert!(!s.contains("ShowSql"), "{s}");
        assert!(s.contains("HOSTNAME=h"), "{s}");

        // DB2 缺 Database 报错
        let cs = ConnectionString::parse("Server=h;provider=db2");
        assert!(build_connection_string(DatabaseKind::Db2, &cs).is_err());
    }

    #[test]
    fn cell_conversion() {
        assert!(odbc_cell_to_dbvalue(None, &DataType::Integer).is_null());
        assert_eq!(
            odbc_cell_to_dbvalue(Some(b"42"), &DataType::Integer),
            DbValue::Int(42)
        );
        assert_eq!(
            odbc_cell_to_dbvalue(Some(b"12.3400"), &DataType::Decimal { precision: 18, scale: 4 }),
            DbValue::Decimal("12.3400".parse().unwrap())
        );
        assert_eq!(
            odbc_cell_to_dbvalue(Some(b"1"), &DataType::Bit),
            DbValue::Bool(true)
        );
        assert!(odbc_cell_to_dbvalue(
            Some(b"2026-09-27 10:30:00"),
            &DataType::Timestamp { precision: 6 }
        )
        .as_datetime()
        .is_some());
        assert!(odbc_cell_to_dbvalue(Some(b"2026-09-27"), &DataType::Date)
            .as_datetime()
            .is_some());
        assert_eq!(
            odbc_cell_to_dbvalue(Some(b"\x01\x02"), &DataType::Binary { length: None }),
            DbValue::Blob(vec![1, 2])
        );
        assert_eq!(
            odbc_cell_to_dbvalue(Some(b"hello"), &DataType::Varchar { length: None }),
            DbValue::Text("hello".into())
        );
    }
}
