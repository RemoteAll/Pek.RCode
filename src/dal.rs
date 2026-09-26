//! 数据访问层：对应 DH.NCode 的 `DAL`（连接管理）与实体表操作。
//!
//! 职责：
//! - 解析 XCode 风格的连接串（`Data Source=..;Provider=SQLite;ShowSql=false`）
//! - 按模型同步数据库结构（建表 / 补列，对应 XCode 的反向工程与迁移）
//! - 提供实体表的增删改查（与 `SqlSession` 组合使用，会话可复用可独立）

use std::{collections::BTreeMap, fmt, sync::Arc};

use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::model::{EntityModel, TableMeta};
use crate::query::{Query, Where};
use crate::session::{DbRow, RowSet, SqlSession};
use crate::sqlbuild;
use crate::sqlite::SqliteSession;
use crate::value::DbValue;

/// 连接串：大小写无关的键值对（`key=value` 以 `;` 分隔）。
#[derive(Debug, Clone)]
pub struct ConnectionString {
    /// 原始连接串
    raw: String,
    /// 小写键 → 值
    items: BTreeMap<String, String>,
}

impl ConnectionString {
    /// 解析连接串。
    pub fn parse(raw: &str) -> Self {
        let mut items = BTreeMap::new();
        for part in raw.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((key, value)) = part.split_once('=')
                && !key.trim().is_empty()
            {
                items.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        Self {
            raw: raw.to_string(),
            items,
        }
    }

    /// 原始连接串。
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// 取值（忽略键大小写）。
    pub fn get(&self, key: &str) -> Option<&str> {
        match self.items.get(&key.to_ascii_lowercase()) {
            Some(v) if !v.is_empty() => Some(v),
            _ => None,
        }
    }

    /// `provider=` 值。
    pub fn provider(&self) -> Option<&str> {
        self.get("provider")
    }

    /// 数据源（文件路径/数据库名），兼容多种写法。
    pub fn data_source(&self) -> Option<&str> {
        ["data source", "datasource", "filename", "file", "database"]
            .iter()
            .find_map(|k| self.get(k))
    }

    /// 是否开启 SQL 输出（`ShowSql=true`，与 XCode 行为一致）。
    pub fn show_sql(&self) -> bool {
        self.get("showsql")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    }

    /// 探测数据库类型：
    /// 1. 有 `provider` 时按其解析
    /// 2. 否则根据数据源后缀推断 SQLite（`.db`/`.sqlite`/`:memory:`）
    pub fn kind(&self) -> Result<DatabaseKind> {
        if let Some(provider) = self.provider() {
            return DatabaseKind::from_provider(provider);
        }

        if let Some(source) = self.data_source() {
            let lower = source.to_ascii_lowercase();
            if lower == ":memory:"
                || lower.ends_with(".db")
                || lower.ends_with(".sqlite")
                || lower.ends_with(".sqlite3")
                || lower.ends_with(".db3")
            {
                return Ok(DatabaseKind::Sqlite);
            }
        }

        Err(Error::Unsupported(
            "无法识别数据库类型，请在连接串中指定 provider=sqlite/mysql/sqlserver/postgresql/oracle".into(),
        ))
    }
}

/// 数据访问层入口。
pub struct Dal {
    /// 连接串
    conn_str: ConnectionString,
    /// 数据库类型
    kind: DatabaseKind,
    /// 数据模型（可选；结构迁移与表操作需要）
    model: Option<Arc<EntityModel>>,
    /// 是否输出执行的 SQL
    show_sql: bool,
}

impl Dal {
    /// 仅按连接串创建（不加载模型，不做任何连接）。
    pub fn open(conn_str: &str) -> Result<Self> {
        let conn_str = ConnectionString::parse(conn_str);
        let kind = conn_str.kind()?;
        let show_sql = conn_str.show_sql();
        Ok(Self {
            conn_str,
            kind,
            model: None,
            show_sql,
        })
    }

    /// 创建并绑定数据模型（后续可执行建表迁移与表操作）。
    pub fn open_with_model(conn_str: &str, model: EntityModel) -> Result<Self> {
        let mut dal = Self::open(conn_str)?;
        dal.model = Some(Arc::new(model));
        Ok(dal)
    }

    /// 数据库类型。
    pub fn kind(&self) -> DatabaseKind {
        self.kind
    }

    /// 连接串。
    pub fn connection_string(&self) -> &ConnectionString {
        &self.conn_str
    }

    /// 数据模型。
    pub fn model(&self) -> Option<&Arc<EntityModel>> {
        self.model.as_ref()
    }

    /// 替换数据模型。
    pub fn set_model(&mut self, model: EntityModel) {
        self.model = Some(Arc::new(model));
    }

    /// 设置 SQL 输出开关（覆盖连接串中的 `ShowSql`）。
    pub fn set_show_sql(&mut self, value: bool) {
        self.show_sql = value;
    }

    /// 输出一条 SQL（开启 ShowSql 时）。
    pub fn log_sql(&self, sql: &str) {
        if self.show_sql {
            println!("[SQL] {sql}");
        }
    }

    /// 打开数据库会话。
    ///
    /// 当前已实现 SQLite 驱动；其余数据库在连接时会返回“暂不支持”，
    /// 但其方言 SQL 仍可由 [`crate::dialect`] 与 [`crate::sqlbuild`] 生成（用于脚本导出）。
    pub fn open_session(&self) -> Result<Box<dyn SqlSession>> {
        match self.kind {
            DatabaseKind::Sqlite => {
                let path = self.conn_str.data_source().ok_or_else(|| {
                    Error::Model("SQLite 连接串缺少 Data Source（数据库文件路径）".into())
                })?;
                Ok(Box::new(SqliteSession::open(path)?))
            }
            other => Err(Error::Unsupported(format!(
                "{} 驱动尚未接入（已完成方言与 SQL 生成），当前版本请使用 SQLite",
                other.name()
            ))),
        }
    }

    /// 获取表操作句柄。
    pub fn table(&self, name: &str) -> Result<TableRef<'_>> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，请使用 open_with_model 或 set_model".into()))?;
        let table = model
            .table(name)
            .ok_or_else(|| Error::Model(format!("模型中不存在表/实体 {name}")))?;
        Ok(TableRef { dal: self, table })
    }

    /// 按模型同步数据库结构（建表 / 补列），返回本次变更清单。
    ///
    /// 安全策略：只做“增量补齐”，不修改、不删除已有对象（与 XCode 迁移一致）。
    pub fn sync_schema(&self) -> Result<SchemaReport> {
        let model = self
            .model
            .as_ref()
            .ok_or_else(|| Error::Model("尚未加载数据模型，无法同步结构".into()))?;

        let mut session = self.open_session()?;
        let mut report = SchemaReport::default();

        for table in &model.tables {
            let table_name = table.effective_table_name();

            if !session.table_exists(table_name)? {
                for stmt in self.kind.create_table_sql(table) {
                    self.log_sql(&stmt);
                    session.execute(&stmt, &[])?;
                }
                report.created_tables.push(table_name.to_string());
                continue;
            }

            // 已存在的表：补齐缺失的列
            let existing = session.table_columns(table_name)?;
            for col in &table.columns {
                let col_name = table.effective_column_name(col);
                if !existing.iter().any(|c| c.eq_ignore_ascii_case(col_name)) {
                    let sql = self.kind.add_column_sql(table, col);
                    self.log_sql(&sql);
                    session.execute(&sql, &[])?;
                    report
                        .added_columns
                        .push((table_name.to_string(), col_name.to_string()));
                }
            }
        }

        Ok(report)
    }
}

/// 结构同步结果。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SchemaReport {
    /// 新建的表
    pub created_tables: Vec<String>,
    /// 补充的列（表名, 列名）
    pub added_columns: Vec<(String, String)>,
}

impl SchemaReport {
    /// 是否没有任何变更。
    pub fn is_empty(&self) -> bool {
        self.created_tables.is_empty() && self.added_columns.is_empty()
    }
}

impl fmt::Display for SchemaReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("数据库结构已是最新");
        }
        write!(
            f,
            "新建表 {} 张，补充列 {} 个",
            self.created_tables.len(),
            self.added_columns.len()
        )?;
        if !self.created_tables.is_empty() {
            write!(f, "；新建：{}", self.created_tables.join(", "))?;
        }
        if !self.added_columns.is_empty() {
            let list: Vec<String> = self
                .added_columns
                .iter()
                .map(|(t, c)| format!("{t}.{c}"))
                .collect();
            write!(f, "；补列：{}", list.join(", "))?;
        }
        Ok(())
    }
}

/// 表操作句柄（绑定模型中的某张表）。
pub struct TableRef<'a> {
    /// 所属数据访问层
    dal: &'a Dal,
    /// 表定义
    table: &'a TableMeta,
}

impl<'a> TableRef<'a> {
    /// 表定义。
    pub fn meta(&self) -> &TableMeta {
        self.table
    }

    /// 插入一行，返回自增主键（无自增列时返回 0）。
    pub fn insert(&self, session: &mut dyn SqlSession, fields: &[(&str, DbValue)]) -> Result<i64> {
        let (sql, params) = sqlbuild::insert_sql(self.dal.kind, self.table, fields)?;
        self.dal.log_sql(&sql);
        session.execute(&sql, &params)?;

        if self.table.identity().is_some() {
            Ok(session.last_identity()?)
        } else {
            Ok(0)
        }
    }

    /// 按主键查找（主键值按 `TableMeta::primary_keys()` 顺序传入）。
    pub fn find_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<Option<DbRow>> {
        let filter = self.pk_filter(pk)?;
        let query = Query::new().filter(filter).take(1);
        let (sql, params) = sqlbuild::select_sql(self.dal.kind, self.table, &query);
        self.dal.log_sql(&sql);
        let set = session.query(&sql, &params)?;
        Ok(set.rows.into_iter().next())
    }

    /// 按主键更新，返回受影响行数。
    pub fn update_by_pk(
        &self,
        session: &mut dyn SqlSession,
        sets: &[(&str, DbValue)],
        pk: &[DbValue],
    ) -> Result<u64> {
        let filter = self.pk_filter(pk)?;
        let (sql, params) = sqlbuild::update_sql(self.dal.kind, self.table, sets, &filter)?;
        self.dal.log_sql(&sql);
        session.execute(&sql, &params)
    }

    /// 按主键删除，返回受影响行数。
    pub fn delete_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<u64> {
        let filter = self.pk_filter(pk)?;
        let (sql, params) = sqlbuild::delete_sql(self.dal.kind, self.table, &filter);
        self.dal.log_sql(&sql);
        session.execute(&sql, &params)
    }

    /// 主键是否存在。
    pub fn exists_by_pk(&self, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<bool> {
        Ok(self.find_by_pk(session, pk)?.is_some())
    }

    /// 统计行数。
    pub fn count(&self, session: &mut dyn SqlSession, filter: Option<&Where>) -> Result<i64> {
        let (sql, params) = sqlbuild::count_sql(self.dal.kind, self.table, filter);
        self.dal.log_sql(&sql);
        let set = session.query(&sql, &params)?;
        Ok(set
            .first()
            .and_then(|r| r.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0))
    }

    /// 查询。
    pub fn query(&self, session: &mut dyn SqlSession, query: &Query) -> Result<RowSet> {
        let (sql, params) = sqlbuild::select_sql(self.dal.kind, self.table, query);
        self.dal.log_sql(&sql);
        session.query(&sql, &params)
    }

    /// 组装主键过滤条件。
    fn pk_filter(&self, pk: &[DbValue]) -> Result<Where> {
        let keys = self.table.primary_keys();
        if keys.is_empty() {
            return Err(Error::Model(format!(
                "表 {} 没有主键，无法按主键操作",
                self.table.name
            )));
        }
        if keys.len() != pk.len() {
            return Err(Error::Model(format!(
                "表 {} 主键需要 {} 个值，实际传入 {} 个",
                self.table.name,
                keys.len(),
                pk.len()
            )));
        }

        let mut filter = Where::new();
        for (key, value) in keys.iter().zip(pk.iter()) {
            filter = filter.eq(key.name.clone(), value.clone());
        }
        Ok(filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order" Description="订单">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
        <Column Name="Status" DataType="Int32" />
        <Column Name="CreateTime" DataType="DateTime" />
      </Columns>
      <Indexes><Index Columns="Code" Unique="True" /></Indexes>
    </Table></Tables></EntityModel>"#;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let stamp = chrono::Local::now().format("%H%M%S%.6f").to_string().replace('.', "");
        let dir = std::env::temp_dir().join(format!("rcode-{}-{}-{name}", std::process::id(), stamp));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn connection_string_parsing() {
        let cs = ConnectionString::parse("Data Source=..\\..\\Data\\DG.db;ShowSql=false;Provider=SQLite");
        assert_eq!(cs.kind().unwrap(), DatabaseKind::Sqlite);
        assert_eq!(cs.data_source(), Some("..\\..\\Data\\DG.db"));
        assert!(!cs.show_sql());
        assert_eq!(cs.get("PROVIDER"), Some("SQLite"));

        let cs = ConnectionString::parse("Server=localhost;Port=3307;Database=mes;Uid=root;Pwd=123456;provider=mysql");
        assert_eq!(cs.kind().unwrap(), DatabaseKind::MySql);

        // 无 provider 但后缀为 .db → SQLite
        let cs = ConnectionString::parse("Data Source=demo.sqlite");
        assert_eq!(cs.kind().unwrap(), DatabaseKind::Sqlite);

        // 无法识别时给出明确错误
        assert!(ConnectionString::parse("Server=x").kind().is_err());
    }

    #[test]
    fn sync_schema_creates_and_extends() {
        let dir = temp_dir("sync");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert_eq!(report.created_tables, vec!["DH_Order"]);
        assert!(report.added_columns.is_empty());

        // 再次同步应无变更
        assert!(dal.sync_schema().unwrap().is_empty());

        // 模型新增一列 → 补列
        let mut model = EntityModel::parse(MODEL).unwrap();
        model.tables[0].columns.push(crate::model::ColumnMeta {
            name: "Remark".into(),
            column_name: None,
            data_type: crate::types::DataType::String,
            raw_type: None,
            length: 100,
            precision: 0,
            scale: 0,
            identity: false,
            primary_key: false,
            master: false,
            nullable: true,
            default_value: None,
            description: String::new(),
            enum_type: None,
            data_scale: None,
            map: None,
            show_in: None,
        });
        let dal2 = Dal::open_with_model(&conn, model).unwrap();
        let report = dal2.sync_schema().unwrap();
        assert_eq!(report.added_columns, vec![("DH_Order".to_string(), "Remark".to_string())]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn table_crud_roundtrip() {
        let dir = temp_dir("crud");
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        let table = dal.table("Order").unwrap();
        let mut session = dal.open_session().unwrap();

        // 插入
        let id = table
            .insert(
                session.as_mut(),
                &[
                    ("Code", "HLT-001".into()),
                    ("Status", 1.into()),
                    ("CreateTime", chrono::NaiveDate::from_ymd_opt(2026, 9, 26).unwrap().and_hms_opt(18, 0, 0).unwrap().into()),
                ],
            )
            .unwrap();
        assert!(id > 0, "应返回自增主键");

        // 查询
        let row = table.find_by_pk(session.as_mut(), &[id.into()]).unwrap().unwrap();
        assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("HLT-001"));
        assert!(table.exists_by_pk(session.as_mut(), &[id.into()]).unwrap());

        // 更新
        let affected = table
            .update_by_pk(session.as_mut(), &[("Status", 9.into())], &[id.into()])
            .unwrap();
        assert_eq!(affected, 1);
        let row = table.find_by_pk(session.as_mut(), &[id.into()]).unwrap().unwrap();
        assert_eq!(row.get_by_name("Status").unwrap().as_i64(), Some(9));

        // 统计与条件查询
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 1);
        let filter = Where::new().eq("Status", 9);
        assert_eq!(table.count(session.as_mut(), Some(&filter)).unwrap(), 1);

        // 删除
        assert_eq!(table.delete_by_pk(session.as_mut(), &[id.into()]).unwrap(), 1);
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
