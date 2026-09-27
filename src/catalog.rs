//! 数据库目录（元数据）读取：反向工程与结构比对共用底座。
//!
//! 对应 DH.NCode 各 `DbBase.OnGetTables`：读取用户表、列（类型/长度/精度/可空/默认值/
//! 主键/自增）与索引（名称/列/唯一），供：
//! - [`crate::reverse`] 反向工程生成 `Model.xml`
//! - [`crate::dal::Dal::sync_schema`] 为既存表补建缺失索引
//! - [`crate::dal::Dal::diff_schema`] 结构差异报告与 ALTER 脚本导出
//!
//! 当前覆盖：SQLite / MySQL / PostgreSQL（含 HighGo/KingBase/VastBase）/ SQL Server /
//! Oracle / DuckDB。其余驱动（Firebird/HANA/ClickHouse/TDengine/InfluxDB/ODBC 系列）按批补齐。

use std::collections::{HashMap, HashSet};

use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::session::{DbRow, SqlSession};
use crate::types::DataType;
use crate::value::DbValue;

/// 列目录信息。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    /// 列名
    pub name: String,
    /// 原始类型文本（如 `varchar(50)` / `number(10,2)`）
    pub raw_type: String,
    /// 映射后的模型类型
    pub data_type: DataType,
    /// 字符串长度（0 表示不限）
    pub length: i32,
    /// 数值精度
    pub precision: i32,
    /// 小数位
    pub scale: i32,
    /// 是否自增
    pub identity: bool,
    /// 是否主键列
    pub primary_key: bool,
    /// 是否可空
    pub nullable: bool,
    /// 默认值（原始文本）
    pub default_value: Option<String>,
    /// 列说明
    pub description: String,
}

/// 索引目录信息（主键索引不列入）。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexInfo {
    /// 索引名
    pub name: String,
    /// 索引列（按索内顺序）
    pub columns: Vec<String>,
    /// 是否唯一索引
    pub unique: bool,
}

/// 表目录信息。
#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    /// 表名
    pub name: String,
    /// 表说明
    pub description: String,
    /// 列
    pub columns: Vec<ColumnInfo>,
    /// 索引（不含主键索引）
    pub indexes: Vec<IndexInfo>,
}

/// 当前已支持目录读取（反向工程）的数据库。
///
/// MongoDB 无固定结构（文档库）不提供；`network`/`sqlce` 本就不支持。
pub fn supports_reverse(kind: DatabaseKind) -> bool {
    !matches!(kind, DatabaseKind::MongoDb)
}

/// 是否支持“索引目录”读取（供结构同步补索引/差异比对使用）。
pub fn supports_index_catalog(kind: DatabaseKind) -> bool {
    matches!(
        kind,
        DatabaseKind::Sqlite
            | DatabaseKind::MySql
            | DatabaseKind::PostgreSql
            | DatabaseKind::SqlServer
            | DatabaseKind::Oracle
            | DatabaseKind::DuckDb
            | DatabaseKind::Firebird
            | DatabaseKind::Hana
            | DatabaseKind::Db2
            | DatabaseKind::DaMeng
    )
}

/// 不支持时的统一错误文案。
fn unsupported(kind: DatabaseKind) -> Error {
    Error::Unsupported(format!(
        "反向工程不支持 {}：文档数据库无固定表结构；其余驱动均已支持",
        kind.name()
    ))
}

/// 读取表目录（可按表名过滤；`names` 为空或 None 表示全部）。
pub fn read_tables(
    session: &mut dyn SqlSession,
    kind: DatabaseKind,
    names: Option<&[String]>,
) -> Result<Vec<TableInfo>> {
    let mut tables = match kind {
        DatabaseKind::Sqlite => read_sqlite(session)?,
        DatabaseKind::MySql => read_mysql(session)?,
        DatabaseKind::PostgreSql => read_postgres(session)?,
        DatabaseKind::SqlServer => read_mssql(session)?,
        DatabaseKind::Oracle | DatabaseKind::DaMeng => read_oracle(session)?,
        DatabaseKind::DuckDb => read_duckdb(session)?,
        DatabaseKind::Firebird => read_firebird(session)?,
        DatabaseKind::Hana => read_hana(session)?,
        DatabaseKind::Db2 => read_db2(session)?,
        DatabaseKind::Iris => read_iris(session)?,
        DatabaseKind::Access => session
            .catalog_tables()?
            .ok_or_else(|| Error::Unsupported("Access 目录读取需要 ODBC 元数据支持".into()))?,
        DatabaseKind::ClickHouse => read_clickhouse(session)?,
        DatabaseKind::TDengine => read_tdengine(session)?,
        DatabaseKind::InfluxDb => read_influxdb(session)?,
        other => return Err(unsupported(other)),
    };
    if let Some(names) = names {
        tables.retain(|t| names.iter().any(|n| n.eq_ignore_ascii_case(&t.name)));
    }
    Ok(tables)
}

/// 读取单表索引（供同步/比对判断缺失；主键索引不列入）。
pub fn read_indexes(
    session: &mut dyn SqlSession,
    kind: DatabaseKind,
    table: &str,
) -> Result<Vec<IndexInfo>> {
    match kind {
        DatabaseKind::Sqlite => sqlite_indexes(session, table),
        DatabaseKind::MySql => mysql_indexes(session, table),
        DatabaseKind::PostgreSql => postgres_indexes(session, table),
        DatabaseKind::SqlServer => mssql_indexes(session, table),
        DatabaseKind::Oracle | DatabaseKind::DaMeng => {
            oracle_indexes(session, table).map(|(idx, _)| idx)
        }
        DatabaseKind::DuckDb => duckdb_indexes(session, table),
        DatabaseKind::Firebird => firebird_indexes(session, table),
        DatabaseKind::Hana => hana_indexes(session, table),
        DatabaseKind::Db2 => db2_indexes(session, table),
        // 无传统索引的库（列式/时序/文档/ODBC 桥的 Access）：返回空
        DatabaseKind::Iris
        | DatabaseKind::Access
        | DatabaseKind::ClickHouse
        | DatabaseKind::TDengine
        | DatabaseKind::InfluxDb => Ok(Vec::new()),
        other => Err(unsupported(other)),
    }
}

// ---------------------------------------------------------------------------
// 通用取值助手
// ---------------------------------------------------------------------------

/// 第 i 列为文本。
fn text(row: &DbRow, i: usize) -> String {
    row.get(i)
        .and_then(DbValue::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 第 i 列为整数（缺省/空值 → 0）。
fn int(row: &DbRow, i: usize) -> i64 {
    row.get(i).and_then(DbValue::as_i64).unwrap_or(0)
}

/// 第 i 列为真值（兼容布尔与 0/1 整数）。
fn truthy(row: &DbRow, i: usize) -> bool {
    match row.get(i) {
        Some(value) => value_truthy(value),
        None => false,
    }
}

/// 单值真值判定（兼容布尔与 0/1 整数）。
fn value_truthy(value: &DbValue) -> bool {
    match value {
        DbValue::Bool(v) => *v,
        other => other.as_i64().unwrap_or(0) != 0,
    }
}

/// 拆分类型与括号参数：`varchar(50)` → `("varchar", [50])`。
fn split_type_params(text: &str) -> (&str, Vec<i32>) {
    let Some(open) = text.find('(') else {
        return (text, Vec::new());
    };
    let close = text.rfind(')').unwrap_or(text.len());
    let base = text[..open].trim();
    let args = text[open + 1..close]
        .split(',')
        .filter_map(|part| part.trim().parse::<i32>().ok())
        .collect();
    (base, args)
}

/// 是否为数值类型名（Oracle 自增判定等场景）。
fn is_numeric_type(base: &str) -> bool {
    matches!(
        base,
        "number" | "numeric" | "decimal" | "int" | "integer" | "bigint" | "smallint" | "tinyint"
    )
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

/// SQLite：读取全部用户表（排除 `sqlite_%` 内部表）。
fn read_sqlite(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let set = session.query(
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        &[],
    )?;

    let mut tables = Vec::with_capacity(set.len());
    for row in &set.rows {
        let name = text(row, 0);
        if name.is_empty() {
            continue;
        }
        let ddl = text(row, 1);
        // AUTOINCREMENT 只出现在建表 DDL 中
        let auto_increment = ddl.to_ascii_uppercase().contains("AUTOINCREMENT");

        let columns = sqlite_columns(session, &name, auto_increment)?;
        let indexes = sqlite_indexes(session, &name)?;
        tables.push(TableInfo {
            name,
            description: String::new(),
            columns,
            indexes,
        });
    }
    Ok(tables)
}

/// SQLite：读取单表列定义（`pragma_table_info` 表值函数，参数可安全绑定）。
fn sqlite_columns(
    session: &mut dyn SqlSession,
    table: &str,
    auto_increment: bool,
) -> Result<Vec<ColumnInfo>> {
    let set = session.query(
        "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?)",
        &[DbValue::Text(table.to_string())],
    )?;

    let mut columns = Vec::with_capacity(set.len());
    for row in &set.rows {
        let name = text(row, 0);
        if name.is_empty() {
            continue;
        }
        let raw_type = text(row, 1);
        let not_null = int(row, 2) == 1;
        let default_value = row.get(3).and_then(DbValue::as_str).map(str::to_string);
        let pk_order = int(row, 4);
        let primary_key = pk_order > 0;

        // 自增：AUTOINCREMENT 且为整数主键首列（与 XCode 的建表约定一致）
        let identity = auto_increment && primary_key && pk_order == 1;

        let (data_type, length, precision, scale) = map_sqlite_type(&raw_type, identity);

        columns.push(ColumnInfo {
            name,
            raw_type,
            data_type,
            length,
            precision,
            scale,
            identity,
            primary_key,
            // XML 语义：Nullable 缺省 false（即 NOT NULL）。
            // 主键强制 NOT NULL（SQLite 的 rowid 主键不报 notnull，但 XCode 模型主键总是非空）
            nullable: !not_null && !primary_key,
            default_value,
            description: String::new(),
        });
    }
    Ok(columns)
}

/// SQLite：读取非主键索引（含 UNIQUE 约束自动索引）。
fn sqlite_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let set = session.query(
        "SELECT name, \"unique\", origin FROM pragma_index_list(?)",
        &[DbValue::Text(table.to_string())],
    )?;

    let mut indexes = Vec::new();
    for row in &set.rows {
        let name = text(row, 0);
        if name.is_empty() || text(row, 2).eq_ignore_ascii_case("pk") {
            continue;
        }
        let unique = int(row, 1) == 1;
        let cols = session.query(
            "SELECT name FROM pragma_index_info(?) ORDER BY seqno",
            &[DbValue::Text(name.clone())],
        )?;
        let columns: Vec<String> = cols.rows.iter().map(|r| text(r, 0)).collect();
        if columns.is_empty() {
            continue;
        }
        indexes.push(IndexInfo {
            name,
            columns,
            unique,
        });
    }
    Ok(indexes)
}

/// SQLite 列类型文本 → 模型类型（与 [`crate::dialect`] 的正向映射互逆）。
///
/// 返回 `(数据类型, 长度, 精度, 小数位)`。
pub(crate) fn map_sqlite_type(raw_type: &str, identity: bool) -> (DataType, i32, i32, i32) {
    let text = raw_type.trim().to_ascii_lowercase();
    if text.is_empty() {
        // SQLite 允许无类型列（动态类型）
        return (DataType::String, 0, 0, 0);
    }

    let (base, args) = split_type_params(&text);
    match base {
        // 自增主键建表为 integer（AUTOINCREMENT 要求）；其余 integer 对应 Int64
        "integer" => {
            if identity {
                (DataType::Int32, 0, 0, 0)
            } else {
                (DataType::Int64, 0, 0, 0)
            }
        }
        "int" => (DataType::Int32, 0, 0, 0),
        "tinyint" => (DataType::Byte, 0, 0, 0),
        "smallint" => (DataType::Int16, 0, 0, 0),
        "bigint" => (DataType::Int64, 0, 0, 0),
        "bit" | "bool" | "boolean" => (DataType::Boolean, 0, 0, 0),
        "single" | "float" => (DataType::Single, 0, 0, 0),
        "real" | "double" => (DataType::Double, 0, 0, 0),
        "decimal" | "numeric" => {
            let precision = args.first().copied().unwrap_or(0);
            let scale = args.get(1).copied().unwrap_or(0);
            (DataType::Decimal, 0, precision, scale)
        }
        "nvarchar" | "varchar" | "nchar" | "char" | "character" => {
            (DataType::String, args.first().copied().unwrap_or(0), 0, 0)
        }
        "text" | "clob" | "ntext" | "longtext" => (DataType::String, 0, 0, 0),
        "datetime" | "timestamp" | "date" => (DataType::DateTime, 0, 0, 0),
        "binary" | "varbinary" | "blob" => (DataType::Binary, 0, 0, 0),
        // 宽松兜底：含 char 视为文本、含 int 视为整数、其余按文本
        other => {
            if other.contains("char") || other.contains("text") {
                (DataType::String, args.first().copied().unwrap_or(0), 0, 0)
            } else if other.contains("int") {
                (DataType::Int32, 0, 0, 0)
            } else {
                (DataType::String, 0, 0, 0)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

/// MySQL：读取表/列/索引（information_schema，单次查询后在内存组装）。
fn read_mysql(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
        &[],
    )?;

    let columns = session.query(
        "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, \
                COLUMN_KEY, NUMERIC_PRECISION, NUMERIC_SCALE, COLUMN_COMMENT \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() \
         ORDER BY TABLE_NAME, ORDINAL_POSITION",
        &[],
    )?;

    let index_rows = session.query(
        "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME \
         FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = DATABASE() \
         ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
        &[],
    )?;

    let mut result: Vec<TableInfo> = Vec::with_capacity(tables.len());
    for row in &tables.rows {
        let name = text(row, 0);
        result.push(TableInfo {
            name,
            description: text(row, 1),
            columns: Vec::new(),
            indexes: Vec::new(),
        });
    }

    for row in &columns.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let raw_type = text(row, 2);
        let nullable = text(row, 3).eq_ignore_ascii_case("YES");
        let default_value = row.get(4).and_then(DbValue::as_str).map(str::to_string);
        let identity = text(row, 5).to_ascii_lowercase().contains("auto_increment");
        let primary_key = text(row, 6).eq_ignore_ascii_case("PRI");
        let precision = int(row, 7) as i32;
        let scale = int(row, 8) as i32;
        let (data_type, length, precision, scale) =
            map_mysql_type(&raw_type, precision, scale, nullable);
        table.columns.push(ColumnInfo {
            name: text(row, 1),
            raw_type,
            data_type,
            length,
            precision,
            scale,
            identity,
            primary_key,
            nullable: nullable || primary_key,
            default_value,
            description: text(row, 9),
        });
    }

    for row in &index_rows.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let index_name = text(row, 1);
        let unique = int(row, 2) == 0;
        let column = text(row, 4);
        if index_name.eq_ignore_ascii_case("PRIMARY") || column.is_empty() {
            continue;
        }
        match table.indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => table.indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique,
            }),
        }
    }

    Ok(result)
}

/// MySQL：读取单表非主键索引。
fn mysql_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let set = session.query(
        "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? \
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
        &[DbValue::Text(table.to_string())],
    )?;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in &set.rows {
        let index_name = text(row, 0);
        let column = text(row, 3);
        if index_name.eq_ignore_ascii_case("PRIMARY") || column.is_empty() {
            continue;
        }
        match indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: int(row, 1) == 0,
            }),
        }
    }
    Ok(indexes)
}

/// MySQL 列类型文本 → 模型类型（`tinyint(1)`/`enum('N','Y')` 视为布尔，与 XCode 约定一致）。
fn map_mysql_type(
    raw_type: &str,
    precision: i32,
    scale: i32,
    _nullable: bool,
) -> (DataType, i32, i32, i32) {
    let lower = raw_type.trim().to_ascii_lowercase();
    let unsigned_stripped = lower.replace(" unsigned", "");
    let (base, args) = split_type_params(&unsigned_stripped);

    match base {
        "tinyint" => {
            if args.first().copied().unwrap_or(0) == 1 {
                (DataType::Boolean, 0, 0, 0)
            } else {
                (DataType::Byte, 0, 0, 0)
            }
        }
        "bool" | "boolean" => (DataType::Boolean, 0, 0, 0),
        "bit" => {
            let bits = args.first().copied().unwrap_or(1);
            if bits <= 1 {
                (DataType::Boolean, 0, 0, 0)
            } else {
                (DataType::Int64, 0, 0, 0)
            }
        }
        "smallint" => (DataType::Int16, 0, 0, 0),
        "mediumint" | "int" | "integer" | "year" => (DataType::Int32, 0, 0, 0),
        "bigint" => (DataType::Int64, 0, 0, 0),
        "float" => (DataType::Single, 0, 0, 0),
        "double" | "real" => (DataType::Double, 0, 0, 0),
        "decimal" | "numeric" => (DataType::Decimal, 0, precision, scale),
        "varchar" | "char" | "nvarchar" | "nchar" => {
            (DataType::String, args.first().copied().unwrap_or(0), 0, 0)
        }
        "text" | "tinytext" | "mediumtext" | "longtext" | "json" | "set" => {
            (DataType::String, 0, 0, 0)
        }
        "enum" => {
            // XCode 约定：enum('N','Y') / enum('Y','N') 作为布尔
            if lower.contains("'n','y'") || lower.contains("'y','n'") {
                (DataType::Boolean, 0, 0, 0)
            } else {
                (DataType::String, 0, 0, 0)
            }
        }
        "datetime" | "timestamp" | "date" | "time" => (DataType::DateTime, 0, 0, 0),
        "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "geometry" => {
            (DataType::Binary, 0, 0, 0)
        }
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL（含 HighGo / KingBase / VastBase）
// ---------------------------------------------------------------------------

/// PostgreSQL：读取表/列/索引（information_schema + pg_catalog）。
fn read_postgres(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' ORDER BY table_name",
        &[],
    )?;

    let columns = session.query(
        "SELECT table_name, column_name, data_type, character_maximum_length, numeric_precision, \
                numeric_scale, is_nullable, column_default \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() ORDER BY table_name, ordinal_position",
        &[],
    )?;

    let index_rows = session.query(
        "SELECT t.relname AS table_name, i.relname AS index_name, ix.indisunique, \
                ix.indisprimary, a.attname \
         FROM pg_index ix \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_class t ON t.oid = ix.indrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ANY(ix.indkey) \
         WHERE n.nspname = current_schema() \
         ORDER BY t.relname, i.relname, array_position(ix.indkey, a.attnum)",
        &[],
    )?;

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: String::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    for row in &columns.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let raw_type = text(row, 2);
        let max_len = int(row, 3) as i32;
        let precision = int(row, 4) as i32;
        let scale = int(row, 5) as i32;
        let nullable = text(row, 6).eq_ignore_ascii_case("YES");
        let default_value = row.get(7).and_then(DbValue::as_str).map(str::to_string);
        // 自增：serial 系列在列默认值上表现为 nextval('...'); 单列主键的数值列视为自增
        let identity = default_value
            .as_deref()
            .map(|d| d.to_ascii_lowercase().starts_with("nextval("))
            .unwrap_or(false);
        let (data_type, length, precision, scale) =
            map_postgres_type(&raw_type, max_len, precision, scale);
        table.columns.push(ColumnInfo {
            name: text(row, 1),
            raw_type,
            data_type,
            length,
            precision,
            scale,
            identity,
            primary_key: false, // 先置位，主键在索引阶段按 indisprimary 标记
            nullable,
            default_value,
            description: String::new(),
        });
    }

    for row in &index_rows.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let index_name = text(row, 1);
        let unique = truthy(row, 2);
        let primary = truthy(row, 3);
        let column = text(row, 4);
        if primary {
            // 主键索引：只标记列，不写入索引列表
            if let Some(col) = table.columns.iter_mut().find(|c| c.name == column) {
                col.primary_key = true;
                col.nullable = false;
            }
            continue;
        }
        if column.is_empty() {
            continue;
        }
        match table.indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => table.indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique,
            }),
        }
    }

    Ok(result)
}

/// PostgreSQL：读取单表非主键索引。
fn postgres_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let set = session.query(
        "SELECT i.relname AS index_name, ix.indisunique, a.attname \
         FROM pg_index ix \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_class t ON t.oid = ix.indrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ANY(ix.indkey) \
         WHERE n.nspname = current_schema() AND t.relname = ? AND ix.indisprimary = false \
         ORDER BY i.relname, array_position(ix.indkey, a.attnum)",
        &[DbValue::Text(table.to_string())],
    )?;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in &set.rows {
        let index_name = text(row, 0);
        let column = text(row, 2);
        if column.is_empty() {
            continue;
        }
        match indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: truthy(row, 1),
            }),
        }
    }
    Ok(indexes)
}

/// PostgreSQL 类型文本 → 模型类型。
fn map_postgres_type(
    raw_type: &str,
    max_len: i32,
    precision: i32,
    scale: i32,
) -> (DataType, i32, i32, i32) {
    let lower = raw_type.trim().to_ascii_lowercase();
    let (base, _) = split_type_params(&lower);
    match base {
        "smallint" | "int2" => (DataType::Int16, 0, 0, 0),
        "integer" | "int" | "int4" => (DataType::Int32, 0, 0, 0),
        "bigint" | "int8" => (DataType::Int64, 0, 0, 0),
        "boolean" | "bool" => (DataType::Boolean, 0, 0, 0),
        "real" | "float4" => (DataType::Single, 0, 0, 0),
        "double precision" | "float8" | "float" => (DataType::Double, 0, 0, 0),
        "numeric" | "decimal" | "money" => (DataType::Decimal, 0, precision, scale),
        "character varying" | "varchar" | "character" | "char" | "bpchar" | "name" => {
            (DataType::String, max_len.max(0), 0, 0)
        }
        "text" | "citext" | "json" | "jsonb" | "uuid" | "xml" | "inet" | "cidr" | "macaddr"
        | "interval" | "tsvector" | "tsquery" => (DataType::String, 0, 0, 0),
        "timestamp without time zone" | "timestamp" | "timestamp with time zone" | "timestamptz"
        | "date" | "time without time zone" | "time" | "time with time zone" | "timetz" => {
            (DataType::DateTime, 0, 0, 0)
        }
        "bytea" => (DataType::Binary, 0, 0, 0),
        // 数组与自定义类型兜底为文本
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// SQL Server
// ---------------------------------------------------------------------------

/// SQL Server：读取表/列/索引（sys.* 目录视图）。
fn read_mssql(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query("SELECT name FROM sys.tables ORDER BY name", &[])?;

    let columns = session.query(
        "SELECT t.name AS table_name, c.name AS column_name, ty.name AS type_name, \
                c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, dc.definition, \
                CASE WHEN pk.column_id IS NULL THEN 0 ELSE 1 END AS is_pk \
         FROM sys.columns c \
         JOIN sys.tables t ON t.object_id = c.object_id \
         JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
         LEFT JOIN sys.default_constraints dc ON dc.parent_object_id = c.object_id \
              AND dc.parent_column_id = c.column_id \
         LEFT JOIN (SELECT ic.object_id, ic.column_id FROM sys.index_columns ic \
                    JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
                    WHERE i.is_primary_key = 1) pk \
              ON pk.object_id = c.object_id AND pk.column_id = c.column_id \
         ORDER BY t.name, c.column_id",
        &[],
    )?;

    let index_rows = session.query(
        "SELECT t.name AS table_name, i.name AS index_name, i.is_unique, col.name AS column_name \
         FROM sys.indexes i \
         JOIN sys.tables t ON t.object_id = i.object_id \
         JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
         WHERE i.is_primary_key = 0 AND i.is_hypothetical = 0 AND i.name IS NOT NULL \
         ORDER BY t.name, i.name, ic.key_ordinal",
        &[],
    )?;

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: String::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    for row in &columns.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let type_name = text(row, 2);
        // max_length：nchar/nvarchar 为字节数需 /2；-1 表示 max
        let mut max_length = int(row, 3) as i32;
        let lower_type = type_name.to_ascii_lowercase();
        if lower_type.starts_with('n') && max_length > 0 {
            max_length /= 2;
        }
        let (data_type, length) = map_mssql_type(&type_name, max_length);
        let precision = int(row, 4) as i32;
        let scale = int(row, 5) as i32;
        table.columns.push(ColumnInfo {
            name: text(row, 1),
            raw_type: if length > 0 {
                format!("{type_name}({length})")
            } else {
                type_name
            },
            data_type,
            length,
            precision,
            scale,
            identity: truthy(row, 7),
            primary_key: truthy(row, 9),
            nullable: truthy(row, 6),
            default_value: row.get(8).and_then(DbValue::as_str).map(str::to_string),
            description: String::new(),
        });
    }

    for row in &index_rows.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let index_name = text(row, 1);
        let column = text(row, 3);
        if column.is_empty() {
            continue;
        }
        match table.indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => table.indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: truthy(row, 2),
            }),
        }
    }

    Ok(result)
}

/// SQL Server：读取单表非主键索引。
fn mssql_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let set = session.query(
        "SELECT i.name AS index_name, i.is_unique, col.name AS column_name \
         FROM sys.indexes i \
         JOIN sys.tables t ON t.object_id = i.object_id \
         JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
         WHERE i.is_primary_key = 0 AND i.is_hypothetical = 0 AND i.name IS NOT NULL \
               AND t.name = @p1 \
         ORDER BY i.name, ic.key_ordinal",
        &[DbValue::Text(table.to_string())],
    )?;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in &set.rows {
        let index_name = text(row, 0);
        let column = text(row, 2);
        if column.is_empty() {
            continue;
        }
        match indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: truthy(row, 1),
            }),
        }
    }
    Ok(indexes)
}

/// SQL Server 类型文本 → 模型类型（返回 `(类型, 字符串长度)`；max_length 已按字符调整，-1→不限）。
fn map_mssql_type(type_name: &str, max_length: i32) -> (DataType, i32) {
    let lower = type_name.trim().to_ascii_lowercase();
    let len = if max_length < 0 { 0 } else { max_length };
    match lower.as_str() {
        "bit" => (DataType::Boolean, 0),
        "tinyint" => (DataType::Byte, 0),
        "smallint" => (DataType::Int16, 0),
        "int" => (DataType::Int32, 0),
        "bigint" => (DataType::Int64, 0),
        "real" => (DataType::Single, 0),
        "float" => (DataType::Double, 0),
        "decimal" | "numeric" | "money" | "smallmoney" => (DataType::Decimal, 0),
        "nvarchar" | "nchar" | "varchar" | "char" => (DataType::String, len.max(0)),
        "ntext" | "text" | "xml" | "sql_variant" | "uniqueidentifier" => (DataType::String, 0),
        "datetime" | "smalldatetime" | "datetime2" | "date" | "time" | "datetimeoffset" => {
            (DataType::DateTime, 0)
        }
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => (DataType::Binary, 0),
        _ => (DataType::String, 0),
    }
}

// ---------------------------------------------------------------------------
// Oracle
// ---------------------------------------------------------------------------

/// Oracle：读取表/列/索引（user_* 数据字典；索引返回 (非主键索引, 主键约束索引名)）。
fn read_oracle(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let (indexes, _) = oracle_indexes_all(session)?;

    // 每表的主键列（按位置）
    let pk_rows = session.query(
        "SELECT ucc.table_name, ucc.column_name, ucc.position \
         FROM user_constraints uc \
         JOIN user_cons_columns ucc ON ucc.constraint_name = uc.constraint_name \
         WHERE uc.constraint_type = 'P' ORDER BY ucc.table_name, ucc.position",
        &[],
    )?;

    let tables = session.query(
        "SELECT table_name, comments FROM user_tab_comments \
         WHERE table_type = 'TABLE' ORDER BY table_name",
        &[],
    )?;

    let columns = session.query(
        "SELECT c.table_name, c.column_name, c.data_type, c.data_length, c.char_length, \
                c.data_precision, c.data_scale, c.nullable, c.data_default, cc.comments, \
                CASE WHEN EXISTS (SELECT 1 FROM user_sequences s \
                                  WHERE UPPER(s.sequence_name) = UPPER('SEQ_' || c.table_name)) \
                     THEN 1 ELSE 0 END AS has_seq \
         FROM user_tab_columns c \
         LEFT JOIN user_col_comments cc ON cc.table_name = c.table_name \
              AND cc.column_name = c.column_name \
         ORDER BY c.table_name, c.column_id",
        &[],
    )?;

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: text(row, 1),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    // 主键映射：表 → 列集合
    let mut pk_map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for row in &pk_rows.rows {
        pk_map.entry(text(row, 0)).or_default().push(text(row, 1));
    }

    for row in &columns.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let column_name = text(row, 1);
        let raw_type = match int(row, 4) {
            0 => text(row, 2),
            char_len => format!("{}({char_len})", text(row, 2)),
        };
        let has_seq = int(row, 10) == 1;
        let is_pk = pk_map
            .get(&table_name)
            .map(|cols| cols.iter().any(|c| c == &column_name))
            .unwrap_or(false);
        let single_pk = pk_map.get(&table_name).map(|c| c.len() == 1).unwrap_or(false);
        let type_lower = text(row, 2).to_ascii_lowercase();
        let (base, _) = split_type_params(&type_lower);
        let identity = has_seq && is_pk && single_pk && is_numeric_type(base);
        let (data_type, length, precision, scale) = map_oracle_type(
            &text(row, 2),
            int(row, 3) as i32,
            int(row, 4) as i32,
            int(row, 5) as i32,
            int(row, 6) as i32,
        );
        table.columns.push(ColumnInfo {
            name: column_name,
            raw_type,
            data_type,
            length,
            precision,
            scale,
            identity,
            primary_key: is_pk,
            nullable: text(row, 7).eq_ignore_ascii_case("Y") && !is_pk,
            default_value: row.get(8).and_then(DbValue::as_str).map(str::to_string),
            description: text(row, 9),
        });
    }

    for (table_name, index) in indexes {
        if let Some(table) = result.iter_mut().find(|t| t.name == table_name) {
            table.indexes.push(index);
        }
    }

    Ok(result)
}

/// Oracle：读取单表非主键索引（附带主键约束索引名）。
fn oracle_indexes(
    session: &mut dyn SqlSession,
    table: &str,
) -> Result<(Vec<IndexInfo>, Vec<String>)> {
    let (all, pk_names) = oracle_indexes_all(session)?;
    Ok((
        all.into_iter()
            .filter(|(t, _)| t.eq_ignore_ascii_case(table))
            .map(|(_, i)| i)
            .collect(),
        pk_names,
    ))
}

/// Oracle：读取全部非主键索引与主键约束索引名。
#[allow(clippy::type_complexity)]
fn oracle_indexes_all(
    session: &mut dyn SqlSession,
) -> Result<(Vec<(String, IndexInfo)>, Vec<String>)> {
    let pk_names: Vec<String> = session
        .query(
            "SELECT index_name FROM user_constraints \
             WHERE constraint_type = 'P' AND index_name IS NOT NULL",
            &[],
        )?
        .rows
        .iter()
        .map(|r| text(r, 0).to_ascii_lowercase())
        .collect();

    let set = session.query(
        "SELECT i.table_name, i.index_name, i.uniqueness, ic.column_name \
         FROM user_indexes i \
         JOIN user_ind_columns ic ON ic.index_name = i.index_name AND ic.table_name = i.table_name \
         WHERE i.index_type NOT IN ('LOB') \
         ORDER BY i.table_name, i.index_name, ic.column_position",
        &[],
    )?;

    let mut indexes: Vec<(String, IndexInfo)> = Vec::new();
    for row in &set.rows {
        let table_name = text(row, 0);
        let index_name = text(row, 1);
        if pk_names.iter().any(|n| n.eq_ignore_ascii_case(&index_name)) {
            continue;
        }
        let column = text(row, 3);
        if column.is_empty() {
            continue;
        }
        match indexes
            .iter_mut()
            .find(|(t, i)| t == &table_name && i.name == index_name)
        {
            Some((_, existing)) => existing.columns.push(column),
            None => indexes.push((
                table_name,
                IndexInfo {
                    name: index_name,
                    columns: vec![column],
                    unique: text(row, 2).eq_ignore_ascii_case("UNIQUE"),
                },
            )),
        }
    }
    Ok((indexes, pk_names))
}

/// Oracle 类型文本 → 模型类型（`NUMBER` 按精度/小数位还原整数族，与 XCode 约定一致）。
fn map_oracle_type(
    data_type: &str,
    data_length: i32,
    char_length: i32,
    precision: i32,
    scale: i32,
) -> (DataType, i32, i32, i32) {
    let lower = data_type.trim().to_ascii_lowercase();
    let (base, args) = split_type_params(&lower);
    match base {
        "number" | "numeric" | "decimal" => {
            let (p, s) = if precision > 0 {
                (precision, scale)
            } else {
                (args.first().copied().unwrap_or(0), args.get(1).copied().unwrap_or(0))
            };
            match (p, s) {
                (1, 0) => (DataType::Boolean, 0, 0, 0),
                (3, 0) => (DataType::Byte, 0, 0, 0),
                (5, 0) => (DataType::Int16, 0, 0, 0),
                (10, 0) => (DataType::Int32, 0, 0, 0),
                (19, 0) => (DataType::Int64, 0, 0, 0),
                (p, 0) if p > 0 && p <= 18 => (DataType::Int64, 0, 0, 0),
                _ => (DataType::Decimal, 0, p.max(0), s.max(0)),
            }
        }
        "binary_float" => (DataType::Single, 0, 0, 0),
        "binary_double" | "float" => (DataType::Double, 0, 0, 0),
        "varchar2" | "nvarchar2" | "char" | "nchar" => {
            let len = if char_length > 0 { char_length } else { data_length };
            (DataType::String, len, 0, 0)
        }
        "clob" | "nclob" | "long" | "rowid" | "xmltype" => (DataType::String, 0, 0, 0),
        "date" | "timestamp" | "timestamp with time zone" | "timestamp with local time zone" => {
            (DataType::DateTime, 0, 0, 0)
        }
        "blob" | "raw" | "long raw" => (DataType::Binary, 0, 0, 0),
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// DuckDB
// ---------------------------------------------------------------------------

/// DuckDB：读取表/列/索引（`duckdb_tables` / `duckdb_columns` / `duckdb_indexes` 表值函数）。
fn read_duckdb(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT table_name FROM duckdb_tables() WHERE internal = false ORDER BY table_name",
        &[],
    )?;

    let columns = session.query(
        "SELECT table_name, column_name, data_type, is_nullable, column_default \
         FROM duckdb_columns() WHERE internal = false ORDER BY table_name, column_index",
        &[],
    )?;

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: String::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    for row in &columns.rows {
        let table_name = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        let raw_type = text(row, 2);
        let nullable = row.get(3).map(value_truthy).unwrap_or(true);
        let default_value = row.get(4).and_then(DbValue::as_str).map(str::to_string);
        let identity = default_value
            .as_deref()
            .map(|d| d.to_ascii_lowercase().contains("nextval("))
            .unwrap_or(false);
        let (data_type, length, precision, scale) = map_duckdb_type(&raw_type);
        table.columns.push(ColumnInfo {
            name: text(row, 1),
            raw_type,
            data_type,
            length,
            precision,
            scale,
            identity,
            primary_key: false,
            nullable,
            default_value,
            description: String::new(),
        });
    }

    // 索引：DuckDB 索引信息在 duckdb_indexes()（含表达式数组）
    let mut table_names: Vec<String> = result.iter().map(|t| t.name.clone()).collect();
    table_names.sort();
    table_names.dedup();
    for name in table_names {
        let indexes = duckdb_indexes(session, &name)?;
        if let Some(table) = result.iter_mut().find(|t| t.name == name) {
            table.indexes = indexes;
        }
    }

    Ok(result)
}

/// DuckDB：读取单表索引（`expressions` 数组的文本形式解析列名）。
fn duckdb_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let set = session.query(
        "SELECT index_name, is_unique, expressions FROM duckdb_indexes() \
         WHERE table_name = ? ORDER BY index_name",
        &[DbValue::Text(table.to_string())],
    )?;

    let mut indexes = Vec::with_capacity(set.len());
    for row in &set.rows {
        let name = text(row, 0);
        if name.is_empty() {
            continue;
        }
        let unique = truthy(row, 1);
        // expressions 为 VARCHAR[]，文本形如 [col1, col2]；剥离引号与括号
        let raw = text(row, 2);
        let columns: Vec<String> = raw
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|c| c.trim().trim_matches('\'').trim_matches('"').to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if columns.is_empty() {
            continue;
        }
        indexes.push(IndexInfo {
            name,
            columns,
            unique,
        });
    }
    Ok(indexes)
}

/// DuckDB 类型文本 → 模型类型。
fn map_duckdb_type(raw_type: &str) -> (DataType, i32, i32, i32) {
    let lower = raw_type.trim().to_ascii_lowercase();
    let (base, args) = split_type_params(&lower);
    match base {
        "boolean" | "bool" | "logical" => (DataType::Boolean, 0, 0, 0),
        "tinyint" | "int1" => (DataType::Byte, 0, 0, 0),
        "smallint" | "int2" => (DataType::Int16, 0, 0, 0),
        "integer" | "int" | "int4" => (DataType::Int32, 0, 0, 0),
        "bigint" | "int8" => (DataType::Int64, 0, 0, 0),
        "utinyint" => (DataType::Byte, 0, 0, 0),
        "usmallint" => (DataType::Int16, 0, 0, 0),
        "uinteger" => (DataType::Int32, 0, 0, 0),
        "ubigint" => (DataType::Int64, 0, 0, 0),
        "hugeint" => (DataType::Decimal, 0, 38, 0),
        "uhugeint" => (DataType::Decimal, 0, 39, 0),
        "real" | "float" | "float4" => (DataType::Single, 0, 0, 0),
        "double" | "float8" => (DataType::Double, 0, 0, 0),
        "decimal" | "numeric" => {
            let precision = args.first().copied().unwrap_or(18);
            let scale = args.get(1).copied().unwrap_or(3);
            (DataType::Decimal, 0, precision, scale)
        }
        "varchar" | "char" | "bpchar" | "string" => {
            (DataType::String, args.first().copied().unwrap_or(0), 0, 0)
        }
        "text" | "uuid" | "json" | "interval" | "time" => (DataType::String, 0, 0, 0),
        "timestamp" | "timestamp_s" | "timestamp_ms" | "timestamp_ns" | "date"
        | "timestamp with time zone" | "timestamptz" => (DataType::DateTime, 0, 0, 0),
        "blob" | "bytea" | "bitstring" => (DataType::Binary, 0, 0, 0),
        _ => (DataType::String, 0, 0, 0),
    }
}

/// 文本真值（TRUE/YES/Y/1）。
fn text_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_uppercase().as_str(),
        "TRUE" | "YES" | "Y" | "1"
    )
}

/// 空文本 → None。
fn opt_text(row: &DbRow, i: usize) -> Option<String> {
    let value = text(row, i);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

// ---------------------------------------------------------------------------
// Firebird
// ---------------------------------------------------------------------------

/// Firebird 类型名文本（用于 `raw_type` 展示）。
fn firebird_type_name(
    type_code: i64,
    sub_type: i64,
    char_len: i32,
    precision: i32,
    scale: i32,
) -> String {
    if scale < 0 && matches!(type_code, 7 | 8 | 16) {
        return format!("DECIMAL({},{})", precision.max(0), -scale);
    }
    match type_code {
        7 => "SMALLINT".into(),
        8 => "INTEGER".into(),
        16 => "BIGINT".into(),
        10 => "FLOAT".into(),
        27 => "DOUBLE PRECISION".into(),
        23 => "BOOLEAN".into(),
        14 => format!("CHAR({char_len})"),
        37 => format!("VARCHAR({char_len})"),
        35 => "TIMESTAMP".into(),
        12 => "DATE".into(),
        13 => "TIME".into(),
        261 => {
            if sub_type == 1 {
                "BLOB SUB_TYPE TEXT".into()
            } else {
                "BLOB".into()
            }
        }
        other => format!("TYPE_{other}"),
    }
}

/// Firebird：读取表/列/索引（RDB$ 系统表；自增按 `SEQ_{表名}` 序列判定）。
fn read_firebird(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT TRIM(RDB$RELATION_NAME) FROM RDB$RELATIONS \
         WHERE RDB$SYSTEM_FLAG = 0 AND RDB$VIEW_BLR IS NULL ORDER BY 1",
        &[],
    )?;

    let pk_rows = session.query(
        "SELECT TRIM(rc.RDB$RELATION_NAME), TRIM(s.RDB$FIELD_NAME) \
         FROM RDB$RELATION_CONSTRAINTS rc \
         JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME \
         WHERE rc.RDB$CONSTRAINT_TYPE = 'PRIMARY KEY' \
         ORDER BY rc.RDB$RELATION_NAME, s.RDB$FIELD_POSITION",
        &[],
    )?;
    let constraint_rows = session.query(
        "SELECT TRIM(RDB$INDEX_NAME) FROM RDB$RELATION_CONSTRAINTS \
         WHERE RDB$INDEX_NAME IS NOT NULL",
        &[],
    )?;
    let constraint_indexes: HashSet<String> = constraint_rows
        .rows
        .iter()
        .map(|r| text(r, 0).to_ascii_uppercase())
        .collect();
    let sequence_rows = session.query(
        "SELECT TRIM(RDB$GENERATOR_NAME) FROM RDB$GENERATORS WHERE RDB$SYSTEM_FLAG = 0",
        &[],
    )?;
    let sequence_names: HashSet<String> = sequence_rows
        .rows
        .iter()
        .map(|r| text(r, 0).to_ascii_uppercase())
        .collect();

    let mut pk_map: HashMap<String, Vec<String>> = HashMap::new();
    for row in &pk_rows.rows {
        pk_map
            .entry(text(row, 0).to_ascii_uppercase())
            .or_default()
            .push(text(row, 1));
    }

    let columns = session.query(
        "SELECT TRIM(rf.RDB$RELATION_NAME), TRIM(rf.RDB$FIELD_NAME), f.RDB$FIELD_TYPE, \
                f.RDB$FIELD_SUB_TYPE, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, \
                f.RDB$FIELD_SCALE, rf.RDB$NULL_FLAG, rf.RDB$DEFAULT_SOURCE \
         FROM RDB$RELATION_FIELDS rf \
         JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = rf.RDB$FIELD_SOURCE \
         WHERE rf.RDB$SYSTEM_FLAG = 0 \
         ORDER BY rf.RDB$RELATION_NAME, rf.RDB$FIELD_POSITION",
        &[],
    )?;

    let index_rows = session.query(
        "SELECT TRIM(i.RDB$RELATION_NAME), TRIM(i.RDB$INDEX_NAME), i.RDB$UNIQUE_FLAG, \
                TRIM(s.RDB$FIELD_NAME) \
         FROM RDB$INDICES i \
         JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = i.RDB$INDEX_NAME \
         WHERE i.RDB$SYSTEM_FLAG = 0 \
         ORDER BY i.RDB$RELATION_NAME, i.RDB$INDEX_NAME, s.RDB$FIELD_POSITION",
        &[],
    )?;

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: String::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    for row in &columns.rows {
        let tname = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == tname) else {
            continue;
        };
        let field_name = text(row, 1);
        let type_code = int(row, 2);
        let sub_type = int(row, 3);
        let char_len = int(row, 4) as i32;
        let precision = int(row, 5) as i32;
        let scale = int(row, 6) as i32;
        let nullable = row.get(7).map(|v| v.is_null()).unwrap_or(true);
        let default_value = row.get(8).and_then(DbValue::as_str).map(str::to_string);
        let is_pk = pk_map
            .get(&tname.to_ascii_uppercase())
            .map(|cols| cols.iter().any(|c| c.eq_ignore_ascii_case(&field_name)))
            .unwrap_or(false);
        let single_pk = pk_map
            .get(&tname.to_ascii_uppercase())
            .map(|cols| cols.len() == 1)
            .unwrap_or(false);
        let has_seq = sequence_names.contains(&format!("SEQ_{tname}").to_ascii_uppercase());
        let (data_type, length, precision, scale) =
            map_firebird_type(type_code, sub_type, char_len, precision, scale);
        let numeric = matches!(
            data_type,
            DataType::Int16 | DataType::Int32 | DataType::Int64
        );
        table.columns.push(ColumnInfo {
            name: field_name,
            raw_type: firebird_type_name(type_code, sub_type, char_len, precision, scale),
            data_type,
            length,
            precision,
            scale,
            identity: has_seq && is_pk && single_pk && numeric,
            primary_key: is_pk,
            nullable: nullable && !is_pk,
            default_value,
            description: String::new(),
        });
    }

    for row in &index_rows.rows {
        let tname = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == tname) else {
            continue;
        };
        let index_name = text(row, 1);
        let column = text(row, 3);
        if index_name.is_empty()
            || column.is_empty()
            || constraint_indexes.contains(&index_name.to_ascii_uppercase())
        {
            continue;
        }
        match table.indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => table.indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: int(row, 2) == 1,
            }),
        }
    }

    Ok(result)
}

/// Firebird：读取单表非约束索引。
fn firebird_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let constraint_rows = session.query(
        "SELECT TRIM(RDB$INDEX_NAME) FROM RDB$RELATION_CONSTRAINTS \
         WHERE RDB$INDEX_NAME IS NOT NULL",
        &[],
    )?;
    let constraint_indexes: HashSet<String> = constraint_rows
        .rows
        .iter()
        .map(|r| text(r, 0).to_ascii_uppercase())
        .collect();
    let set = session.query(
        "SELECT TRIM(i.RDB$INDEX_NAME), i.RDB$UNIQUE_FLAG, TRIM(s.RDB$FIELD_NAME) \
         FROM RDB$INDICES i \
         JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = i.RDB$INDEX_NAME \
         WHERE i.RDB$SYSTEM_FLAG = 0 AND TRIM(i.RDB$RELATION_NAME) = ? \
         ORDER BY i.RDB$INDEX_NAME, s.RDB$FIELD_POSITION",
        &[DbValue::Text(table.to_string())],
    )?;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in &set.rows {
        let index_name = text(row, 0);
        let column = text(row, 2);
        if index_name.is_empty()
            || column.is_empty()
            || constraint_indexes.contains(&index_name.to_ascii_uppercase())
        {
            continue;
        }
        match indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: int(row, 1) == 1,
            }),
        }
    }
    Ok(indexes)
}

/// Firebird 类型码 → 模型类型（负小数位表示 DECIMAL）。
fn map_firebird_type(
    type_code: i64,
    sub_type: i64,
    char_len: i32,
    precision: i32,
    scale: i32,
) -> (DataType, i32, i32, i32) {
    if scale < 0 && matches!(type_code, 7 | 8 | 16) {
        return (DataType::Decimal, 0, precision.max(0), -scale);
    }
    match type_code {
        7 => (DataType::Int16, 0, 0, 0),
        8 => (DataType::Int32, 0, 0, 0),
        16 => (DataType::Int64, 0, 0, 0),
        10 => (DataType::Single, 0, 0, 0),
        27 => (DataType::Double, 0, 0, 0),
        23 => (DataType::Boolean, 0, 0, 0),
        14 | 37 => (DataType::String, char_len.max(0), 0, 0),
        35 | 12 | 13 => (DataType::DateTime, 0, 0, 0),
        261 => {
            if sub_type == 1 {
                (DataType::String, 0, 0, 0)
            } else {
                (DataType::Binary, 0, 0, 0)
            }
        }
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// SAP HANA
// ---------------------------------------------------------------------------

/// HANA：读取表/列/索引（SYS.* 目录；`IS_IDENTITY` 列缺失的旧版本自动回退）。
fn read_hana(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT TABLE_NAME FROM SYS.TABLES WHERE SCHEMA_NAME = CURRENT_SCHEMA ORDER BY TABLE_NAME",
        &[],
    )?;

    let columns = match session.query(
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE_NAME, LENGTH, SCALE, IS_NULLABLE, \
                DEFAULT_VALUE, IS_IDENTITY, COMMENTS \
         FROM SYS.TABLE_COLUMNS WHERE SCHEMA_NAME = CURRENT_SCHEMA \
         ORDER BY TABLE_NAME, POSITION",
        &[],
    ) {
        Ok(set) => set,
        // 旧版本无 IS_IDENTITY 列：回退为不识别自增
        Err(_) => session.query(
            "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE_NAME, LENGTH, SCALE, IS_NULLABLE, \
                    DEFAULT_VALUE, COMMENTS \
             FROM SYS.TABLE_COLUMNS WHERE SCHEMA_NAME = CURRENT_SCHEMA \
             ORDER BY TABLE_NAME, POSITION",
            &[],
        )?,
    };
    let has_identity_col = columns
        .columns
        .iter()
        .any(|c| c.eq_ignore_ascii_case("IS_IDENTITY"));

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: String::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    for row in &columns.rows {
        let tname = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == tname) else {
            continue;
        };
        let type_name = text(row, 2);
        let (data_type, length, precision, scale) =
            map_hana_type(&type_name, int(row, 3) as i32, int(row, 4) as i32);
        table.columns.push(ColumnInfo {
            name: text(row, 1),
            raw_type: type_name,
            data_type,
            length,
            precision,
            scale,
            identity: has_identity_col && text_true(&text(row, 7)),
            primary_key: false,
            nullable: text_true(&text(row, 5)),
            default_value: opt_text(row, 6),
            description: if has_identity_col {
                opt_text(row, 8).unwrap_or_default()
            } else {
                opt_text(row, 7).unwrap_or_default()
            },
        });
    }

    for table in &mut result {
        table.indexes = hana_indexes(session, &table.name)?;
    }

    Ok(result)
}

/// HANA：读取单表非约束索引（`CONSTRAINT` 过滤在旧版本缺失时回退）。
fn hana_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let with_filter = "SELECT i.INDEX_NAME, i.IS_UNIQUE, ic.COLUMN_NAME \
         FROM SYS.INDEXES i \
         JOIN SYS.INDEX_COLUMNS ic ON ic.SCHEMA_NAME = i.SCHEMA_NAME \
              AND ic.INDEX_NAME = i.INDEX_NAME AND ic.TABLE_NAME = i.TABLE_NAME \
         WHERE i.SCHEMA_NAME = CURRENT_SCHEMA AND i.TABLE_NAME = ? AND i.CONSTRAINT IS NULL \
         ORDER BY i.INDEX_NAME, ic.POSITION";
    let without_filter = "SELECT i.INDEX_NAME, i.IS_UNIQUE, ic.COLUMN_NAME \
         FROM SYS.INDEXES i \
         JOIN SYS.INDEX_COLUMNS ic ON ic.SCHEMA_NAME = i.SCHEMA_NAME \
              AND ic.INDEX_NAME = i.INDEX_NAME AND ic.TABLE_NAME = i.TABLE_NAME \
         WHERE i.SCHEMA_NAME = CURRENT_SCHEMA AND i.TABLE_NAME = ? \
         ORDER BY i.INDEX_NAME, ic.POSITION";
    let params = [DbValue::Text(table.to_string())];
    let set = match session.query(with_filter, &params) {
        Ok(set) => set,
        Err(_) => session.query(without_filter, &params)?,
    };
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in &set.rows {
        let index_name = text(row, 0);
        let column = text(row, 2);
        if index_name.is_empty() || column.is_empty() {
            continue;
        }
        match indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: text_true(&text(row, 1)),
            }),
        }
    }
    Ok(indexes)
}

/// HANA 类型名 → 模型类型（DECIMAL 的 LENGTH 即精度）。
fn map_hana_type(type_name: &str, length: i32, scale: i32) -> (DataType, i32, i32, i32) {
    let upper = type_name.trim().to_ascii_uppercase();
    match upper.as_str() {
        "TINYINT" => (DataType::Byte, 0, 0, 0),
        "SMALLINT" => (DataType::Int16, 0, 0, 0),
        "INTEGER" => (DataType::Int32, 0, 0, 0),
        "BIGINT" => (DataType::Int64, 0, 0, 0),
        "REAL" => (DataType::Single, 0, 0, 0),
        "DOUBLE" => (DataType::Double, 0, 0, 0),
        "DECIMAL" | "SMALLDECIMAL" | "FIXED" => (DataType::Decimal, 0, length.max(0), scale.max(0)),
        "BOOLEAN" => (DataType::Boolean, 0, 0, 0),
        "VARCHAR" | "NVARCHAR" | "ALPHANUM" | "SHORTTEXT" => {
            (DataType::String, length.max(0), 0, 0)
        }
        "CLOB" | "NCLOB" | "TEXT" => (DataType::String, 0, 0, 0),
        "BINARY" | "VARBINARY" | "BLOB" => (DataType::Binary, 0, 0, 0),
        "DATE" | "TIMESTAMP" | "SECONDDATE" => (DataType::DateTime, 0, 0, 0),
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// IBM DB2
// ---------------------------------------------------------------------------

/// DB2：读取表/列/索引（SYSCAT 目录视图）。
fn read_db2(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT TRIM(TABNAME) FROM SYSCAT.TABLES \
         WHERE TYPE = 'T' AND TABSCHEMA = CURRENT SCHEMA ORDER BY TABNAME",
        &[],
    )?;
    let pk_rows = session.query(
        "SELECT TRIM(i.TABNAME), TRIM(c.COLNAME) \
         FROM SYSCAT.INDEXES i \
         JOIN SYSCAT.INDEXCOLUSE c ON c.INDSCHEMA = i.INDSCHEMA AND c.INDNAME = i.INDNAME \
         WHERE i.UNIQUERULE = 'P' AND i.TABSCHEMA = CURRENT SCHEMA \
         ORDER BY i.TABNAME, c.COLSEQ",
        &[],
    )?;
    let columns = session.query(
        "SELECT TRIM(TABNAME), TRIM(COLNAME), TRIM(TYPENAME), LENGTH, SCALE, NULLS, \"DEFAULT\", \
                IDENTITY, REMARKS \
         FROM SYSCAT.COLUMNS WHERE TABSCHEMA = CURRENT SCHEMA ORDER BY TABNAME, COLNO",
        &[],
    )?;
    let index_rows = session.query(
        "SELECT TRIM(i.TABNAME), TRIM(i.INDNAME), i.UNIQUERULE, TRIM(c.COLNAME) \
         FROM SYSCAT.INDEXES i \
         JOIN SYSCAT.INDEXCOLUSE c ON c.INDSCHEMA = i.INDSCHEMA AND c.INDNAME = i.INDNAME \
         WHERE i.TABSCHEMA = CURRENT SCHEMA AND i.UNIQUERULE IN ('U', 'D') \
         ORDER BY i.TABNAME, i.INDNAME, c.COLSEQ",
        &[],
    )?;

    let mut pk_map: HashMap<String, Vec<String>> = HashMap::new();
    for row in &pk_rows.rows {
        pk_map.entry(text(row, 0)).or_default().push(text(row, 1));
    }

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: String::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    for row in &columns.rows {
        let tname = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == tname) else {
            continue;
        };
        let column_name = text(row, 1);
        let type_name = text(row, 2);
        let (data_type, length, precision, scale) =
            map_db2_type(&type_name, int(row, 3) as i32, int(row, 4) as i32);
        let is_pk = pk_map
            .get(&tname)
            .map(|cols| cols.iter().any(|c| c.eq_ignore_ascii_case(&column_name)))
            .unwrap_or(false);
        table.columns.push(ColumnInfo {
            name: column_name,
            raw_type: type_name,
            data_type,
            length,
            precision,
            scale,
            identity: text_true(&text(row, 7)),
            primary_key: is_pk,
            nullable: text_true(&text(row, 5)) && !is_pk,
            default_value: opt_text(row, 6),
            description: text(row, 8),
        });
    }

    for row in &index_rows.rows {
        let tname = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == tname) else {
            continue;
        };
        let index_name = text(row, 1);
        let column = text(row, 3);
        if index_name.is_empty() || column.is_empty() {
            continue;
        }
        match table.indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => table.indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: text(row, 2).eq_ignore_ascii_case("U"),
            }),
        }
    }

    Ok(result)
}

/// DB2：读取单表非主键索引。
fn db2_indexes(session: &mut dyn SqlSession, table: &str) -> Result<Vec<IndexInfo>> {
    let set = session.query(
        "SELECT TRIM(i.INDNAME), i.UNIQUERULE, TRIM(c.COLNAME) \
         FROM SYSCAT.INDEXES i \
         JOIN SYSCAT.INDEXCOLUSE c ON c.INDSCHEMA = i.INDSCHEMA AND c.INDNAME = i.INDNAME \
         WHERE i.TABSCHEMA = CURRENT SCHEMA AND i.UNIQUERULE IN ('U', 'D') AND i.TABNAME = ? \
         ORDER BY i.INDNAME, c.COLSEQ",
        &[DbValue::Text(table.to_string())],
    )?;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in &set.rows {
        let index_name = text(row, 0);
        let column = text(row, 2);
        if index_name.is_empty() || column.is_empty() {
            continue;
        }
        match indexes.iter_mut().find(|i| i.name == index_name) {
            Some(existing) => existing.columns.push(column),
            None => indexes.push(IndexInfo {
                name: index_name,
                columns: vec![column],
                unique: text(row, 1).eq_ignore_ascii_case("U"),
            }),
        }
    }
    Ok(indexes)
}

/// DB2 类型名 → 模型类型。
fn map_db2_type(type_name: &str, length: i32, scale: i32) -> (DataType, i32, i32, i32) {
    let upper = type_name.trim().to_ascii_uppercase();
    match upper.as_str() {
        "SMALLINT" => (DataType::Int16, 0, 0, 0),
        "INTEGER" => (DataType::Int32, 0, 0, 0),
        "BIGINT" => (DataType::Int64, 0, 0, 0),
        "REAL" => (DataType::Single, 0, 0, 0),
        "DOUBLE" | "DOUBLE PRECISION" | "FLOAT" => (DataType::Double, 0, 0, 0),
        "DECIMAL" | "NUMERIC" | "DECFLOAT" => (DataType::Decimal, 0, length.max(0), scale.max(0)),
        "BOOLEAN" => (DataType::Boolean, 0, 0, 0),
        "CHARACTER" | "CHAR" | "VARCHAR" | "GRAPHIC" | "VARGRAPHIC" | "LONG VARCHAR" => {
            (DataType::String, length.max(0), 0, 0)
        }
        "CLOB" | "DBCLOB" | "XML" => (DataType::String, 0, 0, 0),
        "BLOB" | "BINARY" | "VARBINARY" => (DataType::Binary, 0, 0, 0),
        "DATE" | "TIME" | "TIMESTAMP" => (DataType::DateTime, 0, 0, 0),
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// InterSystems IRIS
// ---------------------------------------------------------------------------

/// IRIS：读取表/列（INFORMATION_SCHEMA；系统模式过滤）。
fn read_iris(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.TABLES \
         WHERE TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_SCHEMA, TABLE_NAME",
        &[],
    )?;
    let columns = session.query(
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, \
                NUMERIC_SCALE, IS_NULLABLE, COLUMN_DEFAULT \
         FROM INFORMATION_SCHEMA.COLUMNS ORDER BY TABLE_NAME, ORDINAL_POSITION",
        &[],
    )?;

    let mut result: Vec<TableInfo> = Vec::new();
    for row in &tables.rows {
        let schema = text(row, 0);
        let lower = schema.to_ascii_lowercase();
        if lower == "information_schema" || lower.contains("%sys") || lower.starts_with('%') {
            continue;
        }
        result.push(TableInfo {
            name: text(row, 1),
            description: String::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
        });
    }

    for row in &columns.rows {
        let tname = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == tname) else {
            continue;
        };
        let type_name = text(row, 2);
        let length = int(row, 3) as i32;
        let precision = int(row, 4) as i32;
        let scale = int(row, 5) as i32;
        let (data_type, length, precision, scale) =
            map_iris_type(&type_name, length, precision, scale);
        table.columns.push(ColumnInfo {
            name: text(row, 1),
            raw_type: type_name,
            data_type,
            length,
            precision,
            scale,
            identity: false,
            primary_key: false,
            nullable: text(row, 6).eq_ignore_ascii_case("YES"),
            default_value: opt_text(row, 7),
            description: String::new(),
        });
    }

    Ok(result)
}

/// IRIS 类型名 → 模型类型。
fn map_iris_type(type_name: &str, length: i32, precision: i32, scale: i32) -> (DataType, i32, i32, i32) {
    let upper = type_name.trim().to_ascii_uppercase();
    match upper.as_str() {
        "TINYINT" => (DataType::Byte, 0, 0, 0),
        "SMALLINT" => (DataType::Int16, 0, 0, 0),
        "INTEGER" | "INT" => (DataType::Int32, 0, 0, 0),
        "BIGINT" => (DataType::Int64, 0, 0, 0),
        "REAL" => (DataType::Single, 0, 0, 0),
        "DOUBLE" | "FLOAT" => (DataType::Double, 0, 0, 0),
        "NUMERIC" | "DECIMAL" => (DataType::Decimal, 0, precision.max(0), scale.max(0)),
        "BIT" | "BOOLEAN" => (DataType::Boolean, 0, 0, 0),
        "VARCHAR" | "CHAR" | "NVARCHAR" | "NCHAR" => (DataType::String, length.max(0), 0, 0),
        "LONGVARCHAR" | "CLOB" | "TEXT" => (DataType::String, 0, 0, 0),
        "VARBINARY" | "BINARY" | "LONGVARBINARY" | "IMAGE" => (DataType::Binary, 0, 0, 0),
        "DATE" | "TIME" | "TIMESTAMP" => (DataType::DateTime, 0, 0, 0),
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// ClickHouse
// ---------------------------------------------------------------------------

/// ClickHouse：读取表/列（system.tables / system.columns；无传统索引）。
fn read_clickhouse(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let tables = session.query(
        "SELECT name, comment FROM system.tables WHERE database = currentDatabase() ORDER BY name",
        &[],
    )?;
    let columns = session.query(
        "SELECT table, name, type, default_expression FROM system.columns \
         WHERE database = currentDatabase() ORDER BY table, position",
        &[],
    )?;

    let mut result: Vec<TableInfo> = tables
        .rows
        .iter()
        .map(|row| TableInfo {
            name: text(row, 0),
            description: text(row, 1),
            columns: Vec::new(),
            indexes: Vec::new(),
        })
        .collect();

    for row in &columns.rows {
        let tname = text(row, 0);
        let Some(table) = result.iter_mut().find(|t| t.name == tname) else {
            continue;
        };
        let raw_type = text(row, 2);
        let (data_type, length, precision, scale, nullable) = map_clickhouse_type(&raw_type);
        table.columns.push(ColumnInfo {
            name: text(row, 1),
            raw_type,
            data_type,
            length,
            precision,
            scale,
            identity: false,
            primary_key: false,
            nullable,
            default_value: opt_text(row, 3),
            description: String::new(),
        });
    }

    Ok(result)
}

/// ClickHouse 类型文本 → 模型类型（剥离 `Nullable(...)`/`LowCardinality(...)` 包装）。
fn map_clickhouse_type(raw_type: &str) -> (DataType, i32, i32, i32, bool) {
    let mut text_value = raw_type.trim().to_string();
    let mut nullable = false;
    loop {
        let lower = text_value.to_ascii_lowercase();
        if lower.starts_with("nullable(") && text_value.ends_with(')') {
            nullable = true;
            text_value = text_value[9..text_value.len() - 1].to_string();
            continue;
        }
        if lower.starts_with("lowcardinality(") && text_value.ends_with(')') {
            text_value = text_value[15..text_value.len() - 1].to_string();
            continue;
        }
        break;
    }
    let lowered = text_value.to_ascii_lowercase();
    let (base, args) = split_type_params(&lowered);
    let mapped = match base {
        "uint8" => (DataType::Byte, 0, 0, 0),
        "int8" => (DataType::Byte, 0, 0, 0),
        "uint16" | "int16" => (DataType::Int16, 0, 0, 0),
        "uint32" | "int32" => (DataType::Int32, 0, 0, 0),
        "uint64" | "int64" => (DataType::Int64, 0, 0, 0),
        "float32" => (DataType::Single, 0, 0, 0),
        "float64" => (DataType::Double, 0, 0, 0),
        "decimal" | "decimal32" | "decimal64" | "decimal128" => (
            DataType::Decimal,
            0,
            args.first().copied().unwrap_or(18),
            args.get(1).copied().unwrap_or(4),
        ),
        "string" | "fixedstring" => {
            (DataType::String, args.first().copied().unwrap_or(0), 0, 0)
        }
        "enum8" | "enum16" | "uuid" | "ipv4" | "ipv6" | "json" => (DataType::String, 0, 0, 0),
        "date" | "date32" | "datetime" | "datetime32" | "datetime64" => {
            (DataType::DateTime, 0, 0, 0)
        }
        _ => (DataType::String, 0, 0, 0),
    };
    (mapped.0, mapped.1, mapped.2, mapped.3, nullable)
}

// ---------------------------------------------------------------------------
// TDengine
// ---------------------------------------------------------------------------

/// TDengine：读取表/列（`SHOW TABLES` + 逐表 `DESCRIBE`，无传统索引）。
fn read_tdengine(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let set = session.query("SHOW TABLES", &[])?;
    let name_index = set.column_index("table_name").unwrap_or(0);

    let mut result = Vec::new();
    for row in &set.rows {
        let name = row
            .get(name_index)
            .and_then(DbValue::as_str)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let describe = session.query(
            &format!("DESCRIBE `{}`", name.replace('`', "``")),
            &[],
        )?;
        let mut columns = Vec::new();
        for d in &describe.rows {
            let column_name = text(d, 0);
            if column_name.is_empty() {
                continue;
            }
            let type_name = text(d, 1);
            let length = int(d, 2) as i32;
            let note = text(d, 3);
            let (data_type, length, precision, scale) = map_tdengine_type(&type_name, length);
            columns.push(ColumnInfo {
                name: column_name,
                raw_type: type_name,
                data_type,
                length,
                precision,
                scale,
                identity: false,
                primary_key: false,
                // 首列 TIMESTAMP 为时间主键语义，但模型不强制主键；其余列可空
                nullable: true,
                default_value: None,
                description: note,
            });
        }
        result.push(TableInfo {
            name,
            description: String::new(),
            columns,
            indexes: Vec::new(),
        });
    }
    Ok(result)
}

/// TDengine 类型名 → 模型类型。
fn map_tdengine_type(type_name: &str, length: i32) -> (DataType, i32, i32, i32) {
    let upper = type_name.trim().to_ascii_uppercase();
    let lowered = upper.to_ascii_lowercase();
    let (base, args) = split_type_params(&lowered);
    match base {
        "bool" => (DataType::Boolean, 0, 0, 0),
        "tinyint" => (DataType::Byte, 0, 0, 0),
        "smallint" => (DataType::Int16, 0, 0, 0),
        "int" => (DataType::Int32, 0, 0, 0),
        "bigint" => (DataType::Int64, 0, 0, 0),
        "float" => (DataType::Single, 0, 0, 0),
        "double" => (DataType::Double, 0, 0, 0),
        "decimal" => (
            DataType::Decimal,
            0,
            args.first().copied().unwrap_or(0),
            args.get(1).copied().unwrap_or(0),
        ),
        "binary" | "nchar" | "varchar" | "varbinary" => {
            let len = if length > 0 {
                length
            } else {
                args.first().copied().unwrap_or(0)
            };
            (DataType::String, len, 0, 0)
        }
        "json" | "geometry" => (DataType::String, 0, 0, 0),
        "timestamp" => (DataType::DateTime, 0, 0, 0),
        _ => (DataType::String, 0, 0, 0),
    }
}

// ---------------------------------------------------------------------------
// InfluxDB（1.x）
// ---------------------------------------------------------------------------

/// InfluxDB：按 measurement 读取字段/标签键（风络无索引、无自增）。
fn read_influxdb(session: &mut dyn SqlSession) -> Result<Vec<TableInfo>> {
    let set = session.query("SHOW MEASUREMENTS", &[])?;
    let name_index = set.column_index("name").unwrap_or(0);

    let mut result = Vec::new();
    for row in &set.rows {
        let name = row
            .get(name_index)
            .and_then(DbValue::as_str)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let quoted = name.replace('"', "\\\"");
        let mut columns: Vec<ColumnInfo> = Vec::new();

        let fields = session.query(&format!("SHOW FIELD KEYS FROM \"{quoted}\""), &[])?;
        for f in &fields.rows {
            let field_name = text(f, 0);
            if field_name.is_empty() {
                continue;
            }
            let field_type = text(f, 1);
            let (data_type, length, precision, scale) = map_influx_type(&field_type);
            columns.push(ColumnInfo {
                name: field_name,
                raw_type: field_type,
                data_type,
                length,
                precision,
                scale,
                identity: false,
                primary_key: false,
                nullable: true,
                default_value: None,
                description: String::new(),
            });
        }

        let tags = session.query(&format!("SHOW TAG KEYS FROM \"{quoted}\""), &[])?;
        for t in &tags.rows {
            let tag_name = text(t, 0);
            if tag_name.is_empty() || columns.iter().any(|c| c.name == tag_name) {
                continue;
            }
            columns.push(ColumnInfo {
                name: tag_name,
                raw_type: "string".into(),
                data_type: DataType::String,
                length: 0,
                precision: 0,
                scale: 0,
                identity: false,
                primary_key: false,
                nullable: true,
                default_value: None,
                description: "tag".into(),
            });
        }

        result.push(TableInfo {
            name,
            description: String::new(),
            columns,
            indexes: Vec::new(),
        });
    }
    Ok(result)
}

/// InfluxDB 字段类型 → 模型类型。
fn map_influx_type(field_type: &str) -> (DataType, i32, i32, i32) {
    match field_type.trim().to_ascii_lowercase().as_str() {
        "float" => (DataType::Double, 0, 0, 0),
        "integer" | "unsigned" => (DataType::Int64, 0, 0, 0),
        "boolean" => (DataType::Boolean, 0, 0, 0),
        _ => (DataType::String, 0, 0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_type_mapping() {
        assert_eq!(map_sqlite_type("integer", true).0, DataType::Int32);
        assert_eq!(map_sqlite_type("integer", false).0, DataType::Int64);
        assert_eq!(map_sqlite_type("int", false).0, DataType::Int32);
        assert_eq!(map_sqlite_type("bit", false).0, DataType::Boolean);
        assert_eq!(map_sqlite_type("single", false).0, DataType::Single);
        assert_eq!(map_sqlite_type("real", false).0, DataType::Double);
        assert_eq!(
            map_sqlite_type("nvarchar(50)", false),
            (DataType::String, 50, 0, 0)
        );
        assert_eq!(
            map_sqlite_type("decimal(18,4)", false),
            (DataType::Decimal, 0, 18, 4)
        );
        assert_eq!(map_sqlite_type("weird", false).0, DataType::String);
        assert_eq!(map_sqlite_type("", false).0, DataType::String);
    }

    #[test]
    fn mysql_type_mapping() {
        assert_eq!(map_mysql_type("tinyint(1)", 0, 0, false).0, DataType::Boolean);
        assert_eq!(map_mysql_type("tinyint", 0, 0, false).0, DataType::Byte);
        assert_eq!(map_mysql_type("int", 10, 0, false).0, DataType::Int32);
        assert_eq!(map_mysql_type("bigint", 19, 0, false).0, DataType::Int64);
        assert_eq!(
            map_mysql_type("decimal(18,4)", 18, 4, false),
            (DataType::Decimal, 0, 18, 4)
        );
        assert_eq!(
            map_mysql_type("varchar(50)", 0, 0, false),
            (DataType::String, 50, 0, 0)
        );
        assert_eq!(map_mysql_type("longtext", 0, 0, false).0, DataType::String);
        assert_eq!(map_mysql_type("datetime", 0, 0, false).0, DataType::DateTime);
        assert_eq!(map_mysql_type("longblob", 0, 0, false).0, DataType::Binary);
        assert_eq!(
            map_mysql_type("enum('N','Y')", 0, 0, false).0,
            DataType::Boolean
        );
        assert_eq!(
            map_mysql_type("enum('A','B','C')", 0, 0, false).0,
            DataType::String
        );
        assert_eq!(map_mysql_type("bit(1)", 0, 0, false).0, DataType::Boolean);
    }

    #[test]
    fn postgres_type_mapping() {
        assert_eq!(
            map_postgres_type("integer", 0, 0, 0).0,
            DataType::Int32
        );
        assert_eq!(
            map_postgres_type("character varying", 50, 0, 0),
            (DataType::String, 50, 0, 0)
        );
        assert_eq!(
            map_postgres_type("numeric", 0, 18, 4),
            (DataType::Decimal, 0, 18, 4)
        );
        assert_eq!(
            map_postgres_type("timestamp without time zone", 0, 0, 0).0,
            DataType::DateTime
        );
        assert_eq!(
            map_postgres_type("timestamp with time zone", 0, 0, 0).0,
            DataType::DateTime
        );
        assert_eq!(map_postgres_type("bytea", 0, 0, 0).0, DataType::Binary);
        assert_eq!(map_postgres_type("boolean", 0, 0, 0).0, DataType::Boolean);
        assert_eq!(map_postgres_type("double precision", 0, 0, 0).0, DataType::Double);
        assert_eq!(map_postgres_type("text", 0, 0, 0), (DataType::String, 0, 0, 0));
    }

    #[test]
    fn mssql_type_mapping() {
        assert_eq!(map_mssql_type("bit", 1), (DataType::Boolean, 0));
        assert_eq!(map_mssql_type("int", 4), (DataType::Int32, 0));
        assert_eq!(map_mssql_type("nvarchar", 100), (DataType::String, 100));
        assert_eq!(map_mssql_type("nvarchar", -1), (DataType::String, 0));
        assert_eq!(map_mssql_type("varchar", 50), (DataType::String, 50));
        assert_eq!(map_mssql_type("decimal", 9), (DataType::Decimal, 0));
        assert_eq!(map_mssql_type("datetime", 8), (DataType::DateTime, 0));
        assert_eq!(map_mssql_type("varbinary", -1), (DataType::Binary, 0));
    }

    #[test]
    fn oracle_type_mapping() {
        assert_eq!(map_oracle_type("NUMBER", 22, 0, 1, 0).0, DataType::Boolean);
        assert_eq!(map_oracle_type("NUMBER", 22, 0, 3, 0).0, DataType::Byte);
        assert_eq!(map_oracle_type("NUMBER", 22, 0, 5, 0).0, DataType::Int16);
        assert_eq!(map_oracle_type("NUMBER", 22, 0, 10, 0).0, DataType::Int32);
        assert_eq!(map_oracle_type("NUMBER", 22, 0, 19, 0).0, DataType::Int64);
        assert_eq!(
            map_oracle_type("NUMBER", 22, 0, 18, 4),
            (DataType::Decimal, 0, 18, 4)
        );
        assert_eq!(
            map_oracle_type("VARCHAR2", 50, 50, 0, 0),
            (DataType::String, 50, 0, 0)
        );
        assert_eq!(map_oracle_type("CLOB", 0, 0, 0, 0), (DataType::String, 0, 0, 0));
        assert_eq!(
            map_oracle_type("TIMESTAMP(6)", 11, 0, 0, 0).0,
            DataType::DateTime
        );
        assert_eq!(map_oracle_type("BLOB", 0, 0, 0, 0).0, DataType::Binary);
        assert_eq!(map_oracle_type("BINARY_DOUBLE", 8, 0, 0, 0).0, DataType::Double);
    }

    #[test]
    fn duckdb_type_mapping() {
        assert_eq!(map_duckdb_type("BOOLEAN").0, DataType::Boolean);
        assert_eq!(map_duckdb_type("INTEGER").0, DataType::Int32);
        assert_eq!(map_duckdb_type("BIGINT").0, DataType::Int64);
        assert_eq!(map_duckdb_type("VARCHAR(50)").1, 50);
        assert_eq!(map_duckdb_type("DECIMAL(18,4)"), (DataType::Decimal, 0, 18, 4));
        assert_eq!(map_duckdb_type("TIMESTAMP").0, DataType::DateTime);
        assert_eq!(map_duckdb_type("BLOB").0, DataType::Binary);
        assert_eq!(map_duckdb_type("HUGEINT").0, DataType::Decimal);
    }

    /// SQLite：目录读取 roundtrip（含非主键索引与唯一索引）。
    #[test]
    fn sqlite_catalog_roundtrip_with_indexes() {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-catalog-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("catalog.db");

        let dal = crate::dal::Dal::open(&format!(
            "Data Source={};Provider=SQLite",
            db.display()
        ))
        .unwrap();
        let mut session = dal.open_session().unwrap();
        session
            .execute(
                "CREATE TABLE \"DH_Item\" (\"Id\" INTEGER PRIMARY KEY AUTOINCREMENT, \
                 \"Code\" nvarchar(50) NOT NULL, \"Note\" text)",
                &[],
            )
            .unwrap();
        session
            .execute(
                "CREATE UNIQUE INDEX \"ix_DH_Item_Code\" ON \"DH_Item\" (\"Code\")",
                &[],
            )
            .unwrap();
        session
            .execute(
                "CREATE INDEX \"ix_DH_Item_Note\" ON \"DH_Item\" (\"Note\")",
                &[],
            )
            .unwrap();

        let tables = read_tables(session.as_mut(), DatabaseKind::Sqlite, None).unwrap();
        assert_eq!(tables.len(), 1);
        let table = &tables[0];
        assert_eq!(table.name, "DH_Item");
        assert_eq!(table.columns.len(), 3);

        let id = table.columns.iter().find(|c| c.name == "Id").unwrap();
        assert!(id.identity && id.primary_key);
        assert_eq!(id.data_type, DataType::Int32);

        let code = table.columns.iter().find(|c| c.name == "Code").unwrap();
        assert_eq!(code.data_type, DataType::String);
        assert_eq!(code.length, 50);
        assert!(!code.nullable);

        let mut index_names: Vec<&str> = table.indexes.iter().map(|i| i.name.as_str()).collect();
        index_names.sort();
        assert_eq!(index_names, vec!["ix_DH_Item_Code", "ix_DH_Item_Note"]);
        let code_idx = table
            .indexes
            .iter()
            .find(|i| i.name == "ix_DH_Item_Code")
            .unwrap();
        assert!(code_idx.unique);
        assert_eq!(code_idx.columns, vec!["Code"]);

        // 单表索引读取 API
        let single = read_indexes(session.as_mut(), DatabaseKind::Sqlite, "DH_Item").unwrap();
        assert_eq!(single.len(), 2);

        drop(session);
        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
