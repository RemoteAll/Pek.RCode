//! 数据库方言：把逻辑模型/查询翻译成各数据库的具体 SQL。
//!
//! 对应 DH.NCode 中每个数据库的 `DbBase` 子类（SqlServer.cs / MySql.cs / SQLite.cs ...）：
//! - 标识符引用、参数占位符
//! - 字段类型映射（CLR 类型 → 数据库类型）
//! - 建表 / 加列 DDL
//! - 分页语法（`LIMIT/OFFSET` 与 `OFFSET..FETCH`）
//! - 自增主键与自增 ID 读取语句
//!
//! 当前阶段：SQLite 方言用于真实执行（驱动已实现）；其余数据库方言可用于
//! 生成脚本与后续驱动接入（MySQL 优先）。

use crate::error::{Error, Result};
use crate::model::{ColumnMeta, TableMeta};
use crate::types::DataType;

/// 数据库类型（对应 C# 的 `DatabaseType`，先覆盖常用五种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatabaseKind {
    /// SQLite（文件数据库）
    Sqlite,
    /// MySQL / MariaDB
    MySql,
    /// Microsoft SQL Server
    SqlServer,
    /// PostgreSQL
    PostgreSql,
    /// Oracle
    Oracle,
}

impl DatabaseKind {
    /// 全部已建模的数据库类型。
    pub const ALL: [DatabaseKind; 5] = [
        DatabaseKind::Sqlite,
        DatabaseKind::MySql,
        DatabaseKind::SqlServer,
        DatabaseKind::PostgreSql,
        DatabaseKind::Oracle,
    ];

    /// 显示名称。
    pub fn name(&self) -> &'static str {
        match self {
            DatabaseKind::Sqlite => "SQLite",
            DatabaseKind::MySql => "MySql",
            DatabaseKind::SqlServer => "SqlServer",
            DatabaseKind::PostgreSql => "PostgreSQL",
            DatabaseKind::Oracle => "Oracle",
        }
    }

    /// 是否为文件型数据库（SQLite）。
    pub fn is_file_based(&self) -> bool {
        matches!(self, DatabaseKind::Sqlite)
    }

    /// 从连接串中的 `provider` 名称解析（兼容常见别名）。
    ///
    /// 与 DH.NCode 支持的国产/衍生库对应关系：
    /// - HighGo（瀚高）/KingBase（金仓）/VastBase（海量）与 PostgreSQL 同协议，驱动直接复用
    /// - DH.NCode 的其它数据库（Access/ClickHouse/DaMeng/DB2/DuckDB/Firebird/Hana/InfluxDB/IRIS/MongoDB/SqlCe/TDengine）
    ///   暂未接入驱动，会返回可操作的错误提示
    pub fn from_provider(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "sqlite" | "sqlite3" | "system.data.sqlite" | "microsoft.data.sqlite" => {
                Ok(DatabaseKind::Sqlite)
            }
            "mysql" | "mariadb" | "system.data.mysql" => Ok(DatabaseKind::MySql),
            "sqlserver" | "mssql" | "system.data.sqlclient" | "microsoft.data.sqlclient" => {
                Ok(DatabaseKind::SqlServer)
            }
            "postgresql" | "postgres" | "pgsql" | "npgsql" => Ok(DatabaseKind::PostgreSql),
            // 国产 PostgreSQL 系（瀚高/金仓/海量）：同协议，复用 PostgreSQL 驱动
            "highgo" | "kingbase" | "kingbasees" | "vastbase" => Ok(DatabaseKind::PostgreSql),
            "oracle" | "system.data.oracleclient" | "oracle.manageddataaccess" => {
                Ok(DatabaseKind::Oracle)
            }
            other @ ("access" | "clickhouse" | "dameng" | "dm" | "db2" | "duckdb" | "firebird"
            | "hana" | "influxdb" | "iris" | "mongodb" | "sqlce" | "tdengine") => {
                Err(Error::Unsupported(format!(
                    "provider={other} 已在 DH.NCode 支持范围内，但 Pek.RCode 驱动尚在路线图（见 README，欢迎贡献适配）"
                )))
            }
            other => Err(Error::Unsupported(format!(
                "未知的数据库类型 provider={other}（支持：sqlite/mysql/sqlserver/postgresql(含 highgo/kingbase/vastbase)/oracle）"
            ))),
        }
    }

    /// 引用标识符（表名/列名）。
    pub fn quote(&self, ident: &str) -> String {
        // 内部引号统一双写转义，避免标识符注入
        match self {
            DatabaseKind::MySql => format!("`{}`", ident.replace('`', "``")),
            DatabaseKind::SqlServer => format!("[{}]", ident.replace(']', "]]")),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    /// 参数占位符（`index` 从 0 开始）。
    ///
    /// Oracle 使用 1 基的 `:1/:2/...`（OCI 位置绑定，与 rust oracle 驱动一致）。
    pub fn placeholder(&self, index: usize) -> String {
        match self {
            DatabaseKind::Sqlite | DatabaseKind::MySql => "?".to_string(),
            DatabaseKind::SqlServer => format!("@p{index}"),
            DatabaseKind::PostgreSql => format!("${}", index + 1),
            DatabaseKind::Oracle => format!(":{}", index + 1),
        }
    }

    /// 自增列的列内联修饰（`None` 表示该方言不用内联写法，由类型改写/序列/特殊规则处理）。
    fn identity_clause(&self, _col: &ColumnMeta) -> Option<String> {
        match self {
            DatabaseKind::Sqlite => None, // INTEGER PRIMARY KEY AUTOINCREMENT，创建时特殊处理
            DatabaseKind::MySql => Some(" AUTO_INCREMENT".into()),
            DatabaseKind::SqlServer => Some(" IDENTITY(1,1)".into()),
            // PostgreSQL 的 serial/serial8 是伪类型，需要替换列类型（见 identity_type）
            DatabaseKind::PostgreSql => None,
            // Oracle 自增由独立序列承担（XCode 约定 SEQ_{表名}），列本身无内联属性
            DatabaseKind::Oracle => None,
        }
    }

    /// 自增列的列类型改写（如 PostgreSQL 的 `serial`/`serial8` 是伪类型，需整体替换原类型）。
    fn identity_type(&self, col: &ColumnMeta) -> Option<String> {
        match self {
            // 与 DH.NCode 对齐：PostgreSQL 自增使用 serial（Int32）/serial8（Int64）
            DatabaseKind::PostgreSql if col.identity && col.data_type == DataType::Int64 => {
                Some("serial8".into())
            }
            DatabaseKind::PostgreSql if col.identity => Some("serial".into()),
            _ => None,
        }
    }

    /// 查询“最近一次自增 ID”的语句（对应 XCode 的自增回写）。
    ///
    /// 说明：驱动实现已改为插入后直接回读（PostgreSQL 系 `RETURNING`、SQL Server `SCOPE_IDENTITY()`、
    /// Oracle 按表名推导序列 CURRVAL）；本方法保留用于脚本导出与诊断输出。
    pub fn last_identity_sql(&self) -> &'static str {
        match self {
            DatabaseKind::Sqlite => "SELECT last_insert_rowid()",
            DatabaseKind::MySql => "SELECT LAST_INSERT_ID()",
            DatabaseKind::SqlServer => "SELECT SCOPE_IDENTITY()",
            DatabaseKind::PostgreSql => "SELECT lastval()",
            DatabaseKind::Oracle => "SELECT \"SEQ_<表名>\".CURRVAL FROM DUAL",
        }
    }

    /// 字段类型映射（CLR 类型 → 数据库字段类型）。
    pub fn field_type(&self, col: &ColumnMeta) -> String {
        let t = col.data_type;
        match self {
            DatabaseKind::Sqlite => match t {
                DataType::Boolean => "bit".into(),
                DataType::Byte => "tinyint".into(),
                DataType::Int16 => "smallint".into(),
                DataType::Int32 => {
                    if col.identity {
                        "integer".into()
                    } else {
                        "int".into()
                    }
                }
                DataType::Int64 => "integer".into(),
                DataType::Single => "single".into(),
                DataType::Double => "real".into(),
                DataType::Decimal => "decimal".into(),
                DataType::String => {
                    if col.length > 0 {
                        format!("nvarchar({})", col.length)
                    } else {
                        "text".into()
                    }
                }
                DataType::DateTime => "datetime".into(),
                DataType::Binary => "binary".into(),
            },
            DatabaseKind::MySql => match t {
                // 与 DH.NCode 对齐：MySQL 布尔用 TINYINT（读取时 0/1 → bool）
                DataType::Boolean => "TINYINT".into(),
                DataType::Byte => "tinyint".into(),
                DataType::Int16 => "smallint".into(),
                DataType::Int32 => "int".into(),
                DataType::Int64 => "bigint".into(),
                DataType::Single => "float".into(),
                DataType::Double => "double".into(),
                DataType::Decimal => {
                    let (p, s) = mysql_decimal_spec(col);
                    format!("decimal({p},{s})")
                }
                DataType::String => {
                    if col.length > 0 && col.length <= 16383 {
                        format!("varchar({})", col.length)
                    } else {
                        "longtext".into()
                    }
                }
                DataType::DateTime => "datetime".into(),
                DataType::Binary => "longblob".into(),
            },
            DatabaseKind::SqlServer => match t {
                DataType::Boolean => "bit".into(),
                DataType::Byte => "tinyint".into(),
                DataType::Int16 => "smallint".into(),
                DataType::Int32 => "int".into(),
                DataType::Int64 => "bigint".into(),
                DataType::Single => "real".into(),
                DataType::Double => "float".into(),
                DataType::Decimal => format!("decimal({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length <= 0 || col.length > 4000 {
                        "nvarchar(max)".into()
                    } else {
                        format!("nvarchar({})", col.length)
                    }
                }
                DataType::DateTime => "datetime".into(),
                DataType::Binary => "varbinary(max)".into(),
            },
            DatabaseKind::PostgreSql => match t {
                DataType::Boolean => "boolean".into(),
                DataType::Byte | DataType::Int16 => "smallint".into(),
                DataType::Int32 => "integer".into(),
                DataType::Int64 => "bigint".into(),
                DataType::Single => "real".into(),
                DataType::Double => "double precision".into(),
                DataType::Decimal => format!("numeric({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length > 0 {
                        format!("varchar({})", col.length)
                    } else {
                        "text".into()
                    }
                }
                DataType::DateTime => "timestamp".into(),
                DataType::Binary => "bytea".into(),
            },
            DatabaseKind::Oracle => match t {
                DataType::Boolean => "number(1)".into(),
                DataType::Byte => "number(3)".into(),
                DataType::Int16 => "number(5)".into(),
                DataType::Int32 => "number(10)".into(),
                DataType::Int64 => "number(19)".into(),
                DataType::Single => "binary_float".into(),
                DataType::Double => "binary_double".into(),
                DataType::Decimal => format!("number({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length <= 0 || col.length > 2000 {
                        "clob".into()
                    } else {
                        format!("varchar2({})", col.length)
                    }
                }
                DataType::DateTime => "timestamp".into(),
                DataType::Binary => "blob".into(),
            },
        }
    }

    /// 生成建表语句（表 + 索引）。
    ///
    /// 说明：
    /// - 主键：SQLite 自增列内联为主键；其余数据库使用表级 `PRIMARY KEY (...)`
    /// - `Nullable` 缺省 false → 追加 `NOT NULL`（与 XCode 一致）
    /// - SQLite 字符串列追加 `COLLATE NOCASE`（与 XCode 保持一致，保证大小写不敏感检索）
    pub fn create_table_sql(&self, table: &TableMeta) -> Vec<String> {
        let tname = table.effective_table_name();
        let mut sql = format!("CREATE TABLE {} (\n", self.quote(tname));

        let mut lines: Vec<String> = Vec::with_capacity(table.columns.len() + 1);
        let mut inline_pk = false;

        for col in &table.columns {
            let cname = table.effective_column_name(col);
            // 自增列在部分方言中需要改写类型（PostgreSQL 的 serial/serial8）
            let type_name = self
                .identity_type(col)
                .unwrap_or_else(|| self.field_type(col));
            let mut line = format!("  {} {}", self.quote(cname), type_name);

            if col.identity {
                match self {
                    DatabaseKind::Sqlite => {
                        // SQLite 自增必须是 INTEGER PRIMARY KEY AUTOINCREMENT
                        line.push_str(" PRIMARY KEY AUTOINCREMENT");
                        inline_pk = true;
                    }
                    _ => {
                        if let Some(extra) = self.identity_clause(col) {
                            line.push_str(&extra);
                        }
                    }
                }
            }

            // 默认值：Oracle 要求 DEFAULT 出现在 NOT NULL 之前
            let default_part = column_default_sql(self, col).map(|d| format!(" DEFAULT {d}"));
            let not_null = !col.nullable && !(col.identity && inline_pk);
            if self == &DatabaseKind::Oracle {
                if let Some(part) = &default_part {
                    line.push_str(part);
                }
                if not_null {
                    line.push_str(" NOT NULL");
                }
            } else {
                if not_null {
                    line.push_str(" NOT NULL");
                }
                if let Some(part) = &default_part {
                    line.push_str(part);
                }
            }

            if self == &DatabaseKind::Sqlite && col.data_type == DataType::String {
                line.push_str(" COLLATE NOCASE");
            }

            // MySQL 特有：把字段说明写成列注释（与 DH.NCode 的 COMMENT 行为一致）
            if self == &DatabaseKind::MySql && !col.description.is_empty() {
                line.push_str(&format!(" COMMENT '{}'", escape_mysql_text(&col.description)));
            }

            lines.push(line);
        }

        // 表级主键（SQLite 自增列除外）
        if !inline_pk {
            let pk: Vec<String> = table
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| self.quote(table.effective_column_name(c)))
                .collect();
            if !pk.is_empty() {
                lines.push(format!("  PRIMARY KEY ({})", pk.join(", ")));
            }
        }

        sql.push_str(&lines.join(",\n"));
        sql.push_str("\n)");

        let mut statements = vec![sql];

        // 索引
        for idx in &table.indexes {
            if idx.columns.is_empty() {
                continue;
            }
            let cols: Vec<String> = idx
                .columns
                .iter()
                .map(|c| match table.column(c) {
                    Some(col) => self.quote(table.effective_column_name(col)),
                    None => self.quote(c),
                })
                .collect();
            let idx_name = idx.name.clone().unwrap_or_else(|| {
                format!("ix_{}_{}", tname, idx.columns.join("_"))
            });
            statements.push(format!(
                "CREATE {}INDEX {} ON {} ({})",
                if idx.unique { "UNIQUE " } else { "" },
                self.quote(&idx_name),
                self.quote(tname),
                cols.join(", ")
            ));
        }

        // Oracle：自增列依赖独立序列（XCode 约定 SEQ_{表名}），随建表一并创建
        if self == &DatabaseKind::Oracle && table.identity().is_some() {
            statements.push(format!(
                "CREATE SEQUENCE {} START WITH 1 INCREMENT BY 1 CACHE 20",
                self.quote(&oracle_identity_sequence(tname))
            ));
        }

        statements
    }

    /// 生成“新增列”的 ALTER 语句。
    ///
    /// 安全策略：目标列为 NOT NULL 且没有默认值时，降级为可空列，
    /// 避免历史数据无法通过约束检查（与 XCode 迁移时的做法一致）。
    pub fn add_column_sql(&self, table: &TableMeta, col: &ColumnMeta) -> String {
        let tname = self.quote(table.effective_table_name());
        let cname = self.quote(table.effective_column_name(col));
        // 自增列在部分方言中需要改写类型（PostgreSQL 的 serial/serial8）
        let type_name = self
            .identity_type(col)
            .unwrap_or_else(|| self.field_type(col));
        // SQL Server / Oracle 的 ADD 子句不带 COLUMN 关键字
        let mut sql = match self {
            DatabaseKind::SqlServer | DatabaseKind::Oracle => {
                format!("ALTER TABLE {tname} ADD {cname} {type_name}")
            }
            _ => format!("ALTER TABLE {tname} ADD COLUMN {cname} {type_name}"),
        };

        let default_sql = column_default_sql(self, col);
        if col.identity
            && let Some(extra) = self.identity_clause(col)
        {
            sql.push_str(&extra);
        }

        // 无默认值时不追加 NOT NULL：存量数据无法满足约束
        let not_null = !col.nullable && default_sql.is_some();
        if self == &DatabaseKind::Oracle {
            if let Some(d) = &default_sql {
                sql.push_str(&format!(" DEFAULT {d}"));
            }
            if not_null {
                sql.push_str(" NOT NULL");
            }
        } else {
            if not_null {
                sql.push_str(" NOT NULL");
            }
            if let Some(d) = &default_sql {
                sql.push_str(&format!(" DEFAULT {d}"));
            }
        }
        if self == &DatabaseKind::MySql && !col.description.is_empty() {
            sql.push_str(&format!(" COMMENT '{}'", escape_mysql_text(&col.description)));
        }
        sql
    }

    /// 拼装分页：`sql` 为不含排序与分页的查询；`order_sql` 为完整的 `ORDER BY ...`（可空）。
    ///
    /// - SQLite / MySQL / PostgreSQL 系：`LIMIT size OFFSET offset`
    /// - SQL Server：`OFFSET n ROWS FETCH NEXT m ROWS ONLY`（无排序时补 `ORDER BY (SELECT NULL)`）
    /// - Oracle：ROWNUM 双层包装（兼容 11g，与 DH.NCode 的分页思路一致）
    pub fn apply_paging(&self, sql: &str, order_sql: &str, offset: usize, size: usize) -> String {
        match self {
            DatabaseKind::Sqlite | DatabaseKind::MySql | DatabaseKind::PostgreSql => {
                let mut s = sql.to_string();
                if !order_sql.is_empty() {
                    s.push(' ');
                    s.push_str(order_sql);
                }
                s.push_str(&format!(" LIMIT {size} OFFSET {offset}"));
                s
            }
            DatabaseKind::SqlServer => {
                let mut s = sql.to_string();
                if !order_sql.is_empty() {
                    s.push(' ');
                    s.push_str(order_sql);
                } else {
                    // SQL Server 分页要求 ORDER BY，缺失时给出语法合法的兜底
                    s.push_str(" ORDER BY (SELECT NULL)");
                }
                s.push_str(&format!(" OFFSET {offset} ROWS FETCH NEXT {size} ROWS ONLY"));
                s
            }
            DatabaseKind::Oracle => {
                let mut inner = sql.to_string();
                if !order_sql.is_empty() {
                    inner.push(' ');
                    inner.push_str(order_sql);
                }
                let upper = offset + size;
                format!(
                    "SELECT * FROM (SELECT T0.*, ROWNUM AS rowNumber FROM ({inner}) T0) \
                     WHERE rowNumber > {offset} AND rowNumber <= {upper}"
                )
            }
        }
    }
}

/// Oracle 自增序列名（与 XCode 约定一致：`SEQ_{表名}`）。
pub fn oracle_identity_sequence(table_name: &str) -> String {
    format!("SEQ_{table_name}")
}

/// 列的默认值渲染（含 Oracle 隐式默认值，与 DH.NCode 行为一致）。
///
/// Oracle 为 NOT NULL 的 DateTime 列补 `To_Date('0001-01-01','yyyy-mm-dd')`，
/// 对应 DH.NCode `Oracle.GetDefault` 的处理。
fn column_default_sql(kind: &DatabaseKind, col: &ColumnMeta) -> Option<String> {
    if let Some(default) = &col.default_value
        && !default.is_empty()
    {
        return Some(render_default(kind, default, col));
    }
    if *kind == DatabaseKind::Oracle
        && col.data_type == DataType::DateTime
        && !col.nullable
        && !col.identity
    {
        return Some("To_Date('0001-01-01','yyyy-mm-dd')".into());
    }
    None
}

/// MySQL DECIMAL 精度规则（对齐 DH.NCode：Length 有值时覆盖 Precision，上限 255）。
fn mysql_decimal_spec(col: &ColumnMeta) -> (i32, i32) {
    let scale = col.scale.max(0);
    let mut precision = if col.length > 0 {
        col.length.min(255)
    } else if col.precision > 0 {
        col.precision
    } else {
        10
    };
    if precision <= scale {
        precision = scale + 1;
    }
    (precision, scale)
}

/// MySQL 字符串字面量转义（COMMENT 等场景：先转义反斜杠，再双写单引号）。
fn escape_mysql_text(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\'', "''")
}

/// 渲染默认值：数字/布尔直接内联；其余作为字符串字面量（已含引号的按原样处理）。
fn render_default(kind: &DatabaseKind, default: &str, col: &ColumnMeta) -> String {
    let trimmed = default.trim();
    if col.data_type.is_numeric() && trimmed.parse::<f64>().is_ok() {
        return trimmed.to_string();
    }
    if col.data_type == DataType::Boolean {
        return match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" => {
                if *kind == DatabaseKind::PostgreSql {
                    "TRUE".into()
                } else {
                    "1".into()
                }
            }
            "false" | "0" => {
                if *kind == DatabaseKind::PostgreSql {
                    "FALSE".into()
                } else {
                    "0".into()
                }
            }
            _ => "0".into(),
        };
    }
    // 字符串型默认值：已带引号时不重复包裹
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        return trimmed.to_string();
    }
    format!("'{}'", trimmed.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityModel, TableMeta};

    fn sample_table() -> TableMeta {
        let xml = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="50" />
            <Column Name="Title" DataType="String" />
            <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" />
            <Column Name="Ok" DataType="Boolean" />
            <Column Name="CreateTime" DataType="DateTime" />
          </Columns>
        </Table></Tables></EntityModel>"#;
        EntityModel::parse(xml).unwrap().tables.remove(0)
    }

    #[test]
    fn provider_parsing() {
        assert_eq!(DatabaseKind::from_provider("SQLite").unwrap(), DatabaseKind::Sqlite);
        assert_eq!(DatabaseKind::from_provider("mysql").unwrap(), DatabaseKind::MySql);
        assert_eq!(DatabaseKind::from_provider("SqlServer").unwrap(), DatabaseKind::SqlServer);
        assert_eq!(DatabaseKind::from_provider("postgresql").unwrap(), DatabaseKind::PostgreSql);
        assert!(DatabaseKind::from_provider("dameng").is_err());
    }

    #[test]
    fn quoting_and_placeholders() {
        assert_eq!(DatabaseKind::Sqlite.quote("DH_Order"), "\"DH_Order\"");
        assert_eq!(DatabaseKind::MySql.quote("DH_Order"), "`DH_Order`");
        assert_eq!(DatabaseKind::SqlServer.quote("DH_Order"), "[DH_Order]");
        // 标识符中的引号需要转义，避免注入
        assert_eq!(DatabaseKind::Sqlite.quote("a\"b"), "\"a\"\"b\"");

        assert_eq!(DatabaseKind::Sqlite.placeholder(0), "?");
        assert_eq!(DatabaseKind::SqlServer.placeholder(2), "@p2");
        assert_eq!(DatabaseKind::PostgreSql.placeholder(0), "$1");
        // Oracle 使用 1 基位置绑定（OCI）
        assert_eq!(DatabaseKind::Oracle.placeholder(0), ":1");
        assert_eq!(DatabaseKind::Oracle.placeholder(1), ":2");
    }

    #[test]
    fn field_type_mapping() {
        let table = sample_table();
        let id = table.column("Id").unwrap();
        let code = table.column("Code").unwrap();
        let title = table.column("Title").unwrap();
        let amount = table.column("Amount").unwrap();

        assert_eq!(DatabaseKind::Sqlite.field_type(id), "integer");
        assert_eq!(DatabaseKind::MySql.field_type(id), "int");
        assert_eq!(DatabaseKind::SqlServer.field_type(id), "int");

        assert_eq!(DatabaseKind::Sqlite.field_type(code), "nvarchar(50)");
        assert_eq!(DatabaseKind::Sqlite.field_type(title), "text");
        assert_eq!(DatabaseKind::MySql.field_type(code), "varchar(50)");
        assert_eq!(DatabaseKind::MySql.field_type(title), "longtext");
        assert_eq!(DatabaseKind::SqlServer.field_type(title), "nvarchar(max)");
        assert_eq!(DatabaseKind::PostgreSql.field_type(title), "text");

        assert_eq!(DatabaseKind::SqlServer.field_type(amount), "decimal(18,4)");
        assert_eq!(DatabaseKind::PostgreSql.field_type(amount), "numeric(18,4)");

        // 与 DH.NCode 对齐：MySQL 布尔为 TINYINT，SQLite 为 bit
        let ok = table.column("Ok").unwrap();
        assert_eq!(DatabaseKind::MySql.field_type(ok), "TINYINT");
        assert_eq!(DatabaseKind::Sqlite.field_type(ok), "bit");
    }

    #[test]
    fn mysql_ddl_comment_and_decimal_rules() {
        let xml = r#"<EntityModel><Tables><Table Name="T" TableName="T">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Amount" DataType="Decimal" Length="12" Scale="4" Description="金额合计" />
            <Column Name="Ok" DataType="Boolean" Description="是否有效" />
          </Columns>
        </Table></Tables></EntityModel>"#;
        let table = EntityModel::parse(xml).unwrap().tables.remove(0);

        let sql = DatabaseKind::MySql.create_table_sql(&table).remove(0);
        assert!(sql.contains("`Amount` decimal(12,4)"), "Length 应覆盖 Precision：{sql}");
        assert!(sql.contains("COMMENT '金额合计'"), "{sql}");
        assert!(sql.contains("`Ok` TINYINT"), "{sql}");

        // 其它数据库不生成 MySQL 风格的列注释
        let sqlite = DatabaseKind::Sqlite.create_table_sql(&table).remove(0);
        assert!(!sqlite.contains("COMMENT"), "{sqlite}");
    }

    #[test]
    fn create_table_sqlite_matches_xcode_conventions() {
        let table = sample_table();
        let stmts = DatabaseKind::Sqlite.create_table_sql(&table);
        let sql = &stmts[0];

        assert!(sql.contains("\"Id\" integer PRIMARY KEY AUTOINCREMENT"), "{sql}");
        assert!(sql.contains("\"Code\" nvarchar(50) NOT NULL COLLATE NOCASE"), "{sql}");
        assert!(sql.contains("\"Title\" text NOT NULL COLLATE NOCASE"), "{sql}");
        assert!(sql.contains("\"Amount\" decimal NOT NULL"), "{sql}");
        assert!(sql.contains("\"Ok\" bit NOT NULL"), "{sql}");
        assert!(sql.contains("\"CreateTime\" datetime NOT NULL"), "{sql}");
        // 自增列已内联主键，不再生成表级主键
        assert!(!sql.contains("PRIMARY KEY ("), "{sql}");
    }

    #[test]
    fn create_table_sqlserver_has_table_level_pk_and_identity() {
        let table = sample_table();
        let stmts = DatabaseKind::SqlServer.create_table_sql(&table);
        let sql = &stmts[0];
        assert!(sql.contains("[Id] int IDENTITY(1,1) NOT NULL"), "{sql}");
        assert!(sql.contains("PRIMARY KEY ([Id])"), "{sql}");
    }

    #[test]
    fn index_sql_generation() {
        let mut table = sample_table();
        table.indexes.push(crate::model::IndexMeta {
            name: None,
            columns: vec!["Code".into()],
            unique: true,
        });
        let stmts = DatabaseKind::Sqlite.create_table_sql(&table);
        assert_eq!(stmts.len(), 2);
        assert_eq!(
            stmts[1],
            "CREATE UNIQUE INDEX \"ix_DH_Order_Code\" ON \"DH_Order\" (\"Code\")"
        );
    }

    #[test]
    fn add_column_with_not_null_without_default_degrades_to_nullable() {
        let table = sample_table();
        let mut col = ColumnMeta {
            name: "Extra".into(),
            column_name: None,
            data_type: DataType::String,
            raw_type: None,
            length: 10,
            precision: 0,
            scale: 0,
            identity: false,
            primary_key: false,
            master: false,
            nullable: false,
            default_value: None,
            description: String::new(),
            enum_type: None,
            data_scale: None,
            map: None,
            show_in: None,
        };

        let sql = DatabaseKind::Sqlite.add_column_sql(&table, &col);
        assert!(!sql.contains("NOT NULL"), "{sql}");

        col.default_value = Some("0".into());
        let sql = DatabaseKind::Sqlite.add_column_sql(&table, &col);
        assert!(sql.contains("NOT NULL DEFAULT '0'"), "{sql}");
    }

    #[test]
    fn paging_syntax() {
        let base = "SELECT * FROM \"T\"";
        assert_eq!(
            DatabaseKind::Sqlite.apply_paging(base, "ORDER BY \"Id\"", 20, 10),
            "SELECT * FROM \"T\" ORDER BY \"Id\" LIMIT 10 OFFSET 20"
        );
        assert_eq!(
            DatabaseKind::SqlServer.apply_paging(base, "ORDER BY [Id]", 20, 10),
            "SELECT * FROM \"T\" ORDER BY [Id] OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        // SQL Server 缺少排序时给出兜底排序，保证语法合法
        assert!(DatabaseKind::SqlServer
            .apply_paging(base, "", 0, 10)
            .contains("ORDER BY (SELECT NULL)"));
        // Oracle 使用 ROWNUM 双层包装（兼容 11g）
        assert_eq!(
            DatabaseKind::Oracle.apply_paging(base, "ORDER BY \"Id\"", 20, 10),
            "SELECT * FROM (SELECT T0.*, ROWNUM AS rowNumber FROM (SELECT * FROM \"T\" ORDER BY \"Id\") T0) \
             WHERE rowNumber > 20 AND rowNumber <= 30"
        );
    }

    #[test]
    fn postgres_serial_identity_and_oracle_sequence() {
        let table = sample_table();
        let sql = DatabaseKind::PostgreSql.create_table_sql(&table).remove(0);
        // 与 DH.NCode 对齐：PostgreSQL 自增使用 serial/serial8（伪类型，替换原类型）
        assert!(sql.contains("\"Id\" serial NOT NULL"), "{sql}");

        let mut t64 = table.clone();
        t64.columns[0].data_type = DataType::Int64;
        let sql64 = DatabaseKind::PostgreSql.create_table_sql(&t64).remove(0);
        assert!(sql64.contains("\"Id\" serial8 NOT NULL"), "{sql64}");

        // Oracle：自增列无内联属性，SQL 跟随建表脚本一起导出
        let stmts = DatabaseKind::Oracle.create_table_sql(&table);
        assert!(stmts[0].contains("\"Id\" number(10) NOT NULL"), "{}", stmts[0]);
        assert!(!stmts[0].contains("IDENTITY"), "{}", stmts[0]);
        // NOT NULL 的 DateTime 列补固定默认值（与 DH.NCode 一致），且 DEFAULT 在 NOT NULL 之前
        assert!(
            stmts[0].contains("DEFAULT To_Date('0001-01-01','yyyy-mm-dd') NOT NULL"),
            "{}",
            stmts[0]
        );
        assert_eq!(
            stmts.last().unwrap(),
            "CREATE SEQUENCE \"SEQ_DH_Order\" START WITH 1 INCREMENT BY 1 CACHE 20"
        );
    }

    #[test]
    fn add_column_keyword_differs_by_dialect() {
        let table = sample_table();
        let mut col = table.column("Code").unwrap().clone();
        col.default_value = Some("x".into());

        let sqlite = DatabaseKind::Sqlite.add_column_sql(&table, &col);
        assert!(sqlite.starts_with("ALTER TABLE \"DH_Order\" ADD COLUMN \"Code\""), "{sqlite}");

        let mssql = DatabaseKind::SqlServer.add_column_sql(&table, &col);
        assert!(mssql.starts_with("ALTER TABLE [DH_Order] ADD [Code]"), "{mssql}");
        assert!(!mssql.contains("ADD COLUMN"), "{mssql}");

        let oracle = DatabaseKind::Oracle.add_column_sql(&table, &col);
        assert!(oracle.starts_with("ALTER TABLE \"DH_Order\" ADD \"Code\""), "{oracle}");
        assert!(oracle.contains("DEFAULT 'x' NOT NULL"), "{oracle}");
    }

    #[test]
    fn default_value_rendering() {
        let table = sample_table();
        let mut col = table.column("Ok").unwrap().clone();
        col.default_value = Some("True".into());
        assert!(DatabaseKind::Sqlite.create_table_sql(&{
            let mut t = table.clone();
            t.columns = vec![col.clone()];
            t
        })[0]
        .contains("DEFAULT 1"));

        let mut s = col.clone();
        s.data_type = DataType::String;
        s.length = 10;
        s.default_value = Some("待处理".into());
        let mut t2 = table.clone();
        t2.columns = vec![s];
        assert!(DatabaseKind::Sqlite.create_table_sql(&t2)[0].contains("DEFAULT '待处理'"));
    }
}
