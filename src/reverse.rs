//! 反向工程：数据库结构 → 实体模型（对应 DH.NCode 的 `DAL.GetTables` / 各库 `OnGetTables`）。
//!
//! 与 [`crate::codegen`]（模型 → Rust 实体）配合形成双向链路：
//!
//! ```text
//! 数据库 ──read_model()──▶ EntityModel ──to_xml()──▶ Model.xml ──codegen──▶ Rust 实体
//! ```
//!
//! 当前支持范围：
//! - **SQLite**（完整）：表清单取 `sqlite_master`，列信息取 `pragma_table_info` 表值函数，
//!   自增按 DDL 中的 `AUTOINCREMENT` 识别（与 XCode 的建表约定一致）
//! - 其它数据库返回明确的 `Unsupported` 提示（可按需扩展各库的 `OnGetTables` 对应实现）
//!
//! 已知限制：
//! - SQLite 的 DECIMAL 不带精度参数（与 DH.NCode 的建表行为一致），反向后的
//!   `Precision`/`Scale` 为 0，需要时可在模型里补充
//! - 索引/唯一约束暂不反向（`Indexes` 为空）

use crate::dal::Dal;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::model::{ColumnMeta, EntityModel, ModelOptions, TableMeta};
use crate::session::SqlSession;
use crate::types::DataType;
use crate::value::DbValue;

impl Dal {
    /// 反向工程：读取数据库结构，生成实体模型（可 [`EntityModel::to_xml`] 输出 `Model.xml`）。
    ///
    /// 与 DH.NCode 的 `DAL.GetTables()` 对应：读取全部用户表及其列定义
    /// （名称、类型、长度、主键、自增、可空、默认值）。
    pub fn read_model(&self) -> Result<EntityModel> {
        self.ensure_reverse_supported()?;
        let mut session = self.open_session()?;
        Ok(EntityModel {
            version: None,
            model_version: None,
            document: None,
            options: ModelOptions::default(),
            tables: self.read_tables(session.as_mut())?,
        })
    }

    /// 读取数据库中的全部用户表定义。
    pub fn read_tables(&self, session: &mut dyn SqlSession) -> Result<Vec<TableMeta>> {
        self.ensure_reverse_supported()?;
        match self.kind() {
            DatabaseKind::Sqlite => read_tables_sqlite(session),
            _ => unreachable!("ensure_reverse_supported 已拦截非 SQLite"),
        }
    }

    /// 校验当前数据库是否支持反向工程。
    fn ensure_reverse_supported(&self) -> Result<()> {
        match self.kind() {
            DatabaseKind::Sqlite => Ok(()),
            other => Err(Error::Unsupported(format!(
                "反向工程当前已支持 SQLite（本次连接为 {}）；按需可扩展其它数据库",
                other.name()
            ))),
        }
    }

    /// 仅列出数据库中的用户表名（对应 `xcode` 的 `--list` 反向用法）。
    pub fn read_table_names(&self) -> Result<Vec<String>> {
        let mut session = self.open_session()?;
        let tables = self.read_tables(session.as_mut())?;
        Ok(tables.into_iter().map(|t| t.name).collect())
    }
}

/// SQLite：读取全部用户表（排除 `sqlite_%` 内部表）。
fn read_tables_sqlite(session: &mut dyn SqlSession) -> Result<Vec<TableMeta>> {
    let set = session.query(
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        &[],
    )?;

    let mut tables = Vec::with_capacity(set.len());
    for row in &set.rows {
        let name = row
            .get(0)
            .and_then(DbValue::as_str)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let ddl = row.get(1).and_then(DbValue::as_str).unwrap_or_default();

        // AUTOINCREMENT 只出现在建表 DDL 中
        let auto_increment = ddl.to_ascii_uppercase().contains("AUTOINCREMENT");

        let columns = read_columns_sqlite(session, &name, auto_increment)?;
        tables.push(TableMeta {
            name,
            table_name: String::new(),
            description: String::new(),
            conn_name: None,
            columns,
            indexes: Vec::new(),
        });
    }
    Ok(tables)
}

/// SQLite：读取单表列定义（`pragma_table_info` 表值函数，参数可安全绑定）。
fn read_columns_sqlite(
    session: &mut dyn SqlSession,
    table: &str,
    auto_increment: bool,
) -> Result<Vec<ColumnMeta>> {
    let set = session.query(
        "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?)",
        &[DbValue::Text(table.to_string())],
    )?;

    let mut columns = Vec::with_capacity(set.len());
    for row in &set.rows {
        let name = row
            .get(0)
            .and_then(DbValue::as_str)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let raw_type = row.get(1).and_then(DbValue::as_str).unwrap_or_default();
        let not_null = row.get(2).and_then(DbValue::as_i64).unwrap_or(0) == 1;
        let default_value = row.get(3).and_then(DbValue::as_str).map(str::to_string);
        let pk_order = row.get(4).and_then(DbValue::as_i64).unwrap_or(0);
        let primary_key = pk_order > 0;

        // 自增：AUTOINCREMENT 且为整数主键首列（与 XCode 的建表约定一致）
        let identity = auto_increment && primary_key && pk_order == 1;

        let (data_type, length, precision, scale) = map_sqlite_type(raw_type, identity);

        columns.push(ColumnMeta {
            name,
            column_name: None,
            data_type,
            raw_type: None,
            length,
            precision,
            scale,
            identity,
            primary_key,
            master: false,
            // XML 语义：Nullable 缺省 false（即 NOT NULL）。
            // 主键强制 NOT NULL（SQLite 的 rowid 主键不报 notnull，但 XCode 模型主键总是非空）
            nullable: !not_null && !primary_key,
            default_value,
            description: String::new(),
            enum_type: None,
            data_scale: None,
            map: None,
            show_in: None,
            model: None,
        });
    }
    Ok(columns)
}

/// SQLite 列类型文本 → 模型类型（与 [`crate::dialect`] 的正向映射互逆）。
///
/// 返回 `(数据类型, 长度, 精度, 小数位)`。
fn map_sqlite_type(raw_type: &str, identity: bool) -> (DataType, i32, i32, i32) {
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

/// 拆分类型与括号参数：`nvarchar(50)` → `("nvarchar", [50])`。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::types::DataType;

    /// 覆盖全部 11 种数据类型的模型（含自增主键与可空列）。
    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Reverse" TableName="DH_Reverse">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Ok" DataType="Boolean" />
        <Column Name="Tiny" DataType="Byte" />
        <Column Name="Small" DataType="Int16" />
        <Column Name="Big" DataType="Int64" />
        <Column Name="F1" DataType="Single" />
        <Column Name="F2" DataType="Double" />
        <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" />
        <Column Name="Title" DataType="String" Length="50" />
        <Column Name="Body" DataType="String" />
        <Column Name="When" DataType="DateTime" />
        <Column Name="Data" DataType="Binary" Nullable="True" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    #[test]
    fn sqlite_type_mapping() {
        assert_eq!(map_sqlite_type("integer", true).0, DataType::Int32);
        assert_eq!(map_sqlite_type("integer", false).0, DataType::Int64);
        assert_eq!(map_sqlite_type("int", false).0, DataType::Int32);
        assert_eq!(map_sqlite_type("bit", false).0, DataType::Boolean);
        assert_eq!(map_sqlite_type("single", false).0, DataType::Single);
        assert_eq!(map_sqlite_type("real", false).0, DataType::Double);
        assert_eq!(map_sqlite_type("nvarchar(50)", false), (DataType::String, 50, 0, 0));
        assert_eq!(map_sqlite_type("text", false), (DataType::String, 0, 0, 0));
        assert_eq!(map_sqlite_type("decimal(18,4)", false), (DataType::Decimal, 0, 18, 4));
        assert_eq!(map_sqlite_type("datetime", false).0, DataType::DateTime);
        assert_eq!(map_sqlite_type("binary", false).0, DataType::Binary);
        // 未知类型兜底为文本
        assert_eq!(map_sqlite_type("weird", false).0, DataType::String);
        // 无类型列（SQLite 动态类型）
        assert_eq!(map_sqlite_type("", false).0, DataType::String);
    }

    #[test]
    fn sqlite_roundtrip_full_types() {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-reverse-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("reverse.db");

        let source = EntityModel::parse(MODEL).unwrap();
        let dal = Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db.display()),
            source.clone(),
        )
        .unwrap();
        dal.sync_schema().unwrap();

        // 反向读取
        let reversed = dal.read_model().unwrap();
        assert_eq!(reversed.tables.len(), 1);
        let table = &reversed.tables[0];
        assert_eq!(table.effective_table_name(), "DH_Reverse");
        assert_eq!(table.columns.len(), source.tables[0].columns.len());

        // 逐列核对：类型 / 长度 / 主键 / 自增 / 可空
        for src in &source.tables[0].columns {
            let col = table
                .column(&src.name)
                .unwrap_or_else(|| panic!("反向结果缺少列 {}", src.name));
            assert_eq!(col.data_type, src.data_type, "列 {} 类型", src.name);
            assert_eq!(col.primary_key, src.primary_key, "列 {} 主键", src.name);
            assert_eq!(col.identity, src.identity, "列 {} 自增", src.name);
            assert_eq!(col.nullable, src.nullable, "列 {} 可空", src.name);
            if src.data_type == DataType::String {
                assert_eq!(col.length, src.length, "列 {} 长度", src.name);
            }
        }

        // 再写出 XML 并解析：结构稳定（表数与列数不变）
        let xml = reversed.to_xml();
        let again = EntityModel::parse(&xml).unwrap();
        assert_eq!(again.tables.len(), 1);
        assert_eq!(again.tables[0].columns.len(), source.tables[0].columns.len());

        // 默认值反向（手工建表带默认值的列）
        let mut session = dal.open_session().unwrap();
        session
            .execute(
                "CREATE TABLE \"DH_Def\" (\"Id\" INTEGER PRIMARY KEY AUTOINCREMENT, \
                 \"Flag\" bit NOT NULL DEFAULT 0)",
                &[],
            )
            .unwrap();
        let reversed = dal.read_model().unwrap();
        let def = reversed
            .tables
            .iter()
            .find(|t| t.name == "DH_Def")
            .expect("应反向出 DH_Def");
        let flag = def.column("Flag").unwrap();
        assert_eq!(flag.default_value.as_deref(), Some("0"));
        assert!(!flag.nullable);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_for_non_sqlite() {
        // 连接串只是构造 Dal（不真正连接其它库即可验证分支）
        let dal = Dal::open("Server=127.0.0.1;Database=x;Uid=u;Pwd=p;provider=mysql").unwrap();
        let err = dal.read_model().unwrap_err();
        assert!(
            err.to_string().contains("反向工程当前已支持 SQLite"),
            "{err}"
        );
    }
}
