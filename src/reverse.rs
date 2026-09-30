//! 反向工程：数据库结构 → 实体模型（对应 DH.NCode 的 `DAL.GetTables` / 各库 `OnGetTables`）。
//!
//! 与 [`crate::codegen`]（模型 → Rust 实体）配合形成双向链路：
//!
//! ```text
//! 数据库 ──read_model()──▶ EntityModel ──to_xml()──▶ Model.xml ──codegen──▶ Rust 实体
//! ```
//!
//! 目录读取（表/列/索引）统一由 [`crate::catalog`] 承担：
//! - 已支持：SQLite、MySQL、PostgreSQL（含 HighGo/KingBase/VastBase）、SQL Server、Oracle、DuckDB
//! - 索引与唯一约束一并反向（主键索引不写入 `Indexes`）
//! - 其余驱动返回明确的 `Unsupported` 提示（按批补齐中）
//!
//! 已知限制：
//! - SQLite 的 DECIMAL 不带精度参数（与 DH.NCode 的建表行为一致），反向后的
//!   `Precision`/`Scale` 为 0，需要时可在模型里补充

use crate::catalog;
use crate::dal::Dal;
use crate::error::{Error, Result};
use crate::model::{ColumnMeta, EntityModel, IndexMeta, ModelOptions, TableMeta};
use crate::session::SqlSession;

impl Dal {
    /// 反向工程：读取数据库结构，生成实体模型（可 [`EntityModel::to_xml`] 输出 `Model.xml`）。
    ///
    /// 与 DH.NCode 的 `DAL.GetTables()` 对应：读取全部用户表及其列定义
    /// （名称、类型、长度、主键、自增、可空、默认值）与索引（名称/列/唯一）。
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

    /// 读取数据库中的全部用户表定义（含非主键索引）。
    pub fn read_tables(&self, session: &mut dyn SqlSession) -> Result<Vec<TableMeta>> {
        self.ensure_reverse_supported()?;
        let infos = catalog::read_tables(session, self.kind(), None)?;
        Ok(infos.into_iter().map(to_table_meta).collect())
    }

    /// 按表名读取表定义（供 `rcodegen --table` 等按需反向使用）。
    /// <param name="session">会话</param>
    /// <param name="names">表名（忽略大小写）</param>
    /// <returns>命中的表定义</returns>
    pub fn read_tables_of(
        &self,
        session: &mut dyn SqlSession,
        names: &[String],
    ) -> Result<Vec<TableMeta>> {
        self.ensure_reverse_supported()?;
        let infos = catalog::read_tables(session, self.kind(), Some(names))?;
        Ok(infos.into_iter().map(to_table_meta).collect())
    }

    /// 校验当前数据库是否支持反向工程。
    fn ensure_reverse_supported(&self) -> Result<()> {
        if catalog::supports_reverse(self.kind()) {
            Ok(())
        } else {
            Err(Error::Unsupported(format!(
                "反向工程不支持 {}：文档数据库无固定表结构；其余驱动均已支持",
                self.kind().name()
            )))
        }
    }

    /// 仅列出数据库中的用户表名（对应 `xcode` 的 `--list` 反向用法）。
    pub fn read_table_names(&self) -> Result<Vec<String>> {
        let mut session = self.open_session()?;
        let tables = self.read_tables(session.as_mut())?;
        Ok(tables.into_iter().map(|t| t.name).collect())
    }
}

/// 目录信息 → 模型表定义。
fn to_table_meta(info: catalog::TableInfo) -> TableMeta {
    TableMeta {
        name: info.name,
        table_name: String::new(),
        description: info.description,
        conn_name: None,
        migration: None,
        columns: info
            .columns
            .into_iter()
            .map(|col| ColumnMeta {
                name: col.name,
                column_name: None,
                data_type: col.data_type,
                raw_type: (!col.raw_type.is_empty()).then_some(col.raw_type),
                length: col.length,
                precision: col.precision,
                scale: col.scale,
                identity: col.identity,
                primary_key: col.primary_key,
                master: false,
                nullable: col.nullable,
                default_value: col.default_value,
                description: col.description,
                enum_type: None,
                data_scale: None,
                map: None,
                show_in: None,
                model: None,
            })
            .collect(),
        indexes: info
            .indexes
            .into_iter()
            .map(|idx| IndexMeta {
                name: Some(idx.name),
                columns: idx.columns,
                unique: idx.unique,
            })
            .collect(),
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::map_sqlite_type;
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
      <Indexes><Index Columns="Title" Unique="True" /></Indexes>
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

        // 索引反向（含唯一标记与列序）
        assert_eq!(table.indexes.len(), 1);
        assert!(table.indexes[0].unique);
        assert_eq!(table.indexes[0].columns, vec!["Title"]);

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
    fn unsupported_for_uncovered_db() {
        // 连接串只是构造 Dal（不真正连接数据库即可验证分支）
        let dal = Dal::open("Server=127.0.0.1;Port=27017;Database=x;provider=mongodb").unwrap();
        let err = dal.read_model().unwrap_err();
        assert!(err.to_string().contains("反向工程不支持"), "{err}");
    }
}
