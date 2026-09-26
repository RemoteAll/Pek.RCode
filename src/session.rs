//! SQL 会话抽象：对应 DH.NCode 的 `IDbSession`。
//!
//! 上层 ORM（建表迁移、增删改查）只依赖本 trait，具体数据库由各驱动实现：
//! - 已实现：SQLite（[`crate::sqlite::SqliteSession`]）
//! - 规划中：MySQL / SQL Server / PostgreSQL（方言已就绪，接入驱动即可）

use std::sync::Arc;

use crate::dialect::DatabaseKind;
use crate::error::Result;
use crate::value::DbValue;

/// 查询结果集：列名 + 数据行。
#[derive(Debug, Clone, Default)]
pub struct RowSet {
    /// 列名（按查询顺序）
    pub columns: Arc<Vec<String>>,
    /// 数据行
    pub rows: Vec<DbRow>,
}

impl RowSet {
    /// 创建空结果集。
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            columns: Arc::new(columns),
            rows: Vec::new(),
        }
    }

    /// 追加一行。
    pub fn push(&mut self, values: Vec<DbValue>) {
        self.rows.push(DbRow {
            columns: self.columns.clone(),
            values,
        });
    }

    /// 行数。
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 首行（常用于取单条记录）。
    pub fn first(&self) -> Option<&DbRow> {
        self.rows.first()
    }

    /// 按列名取列序号（忽略大小写）。
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
    }
}

impl<'a> IntoIterator for &'a RowSet {
    type Item = &'a DbRow;
    type IntoIter = std::slice::Iter<'a, DbRow>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

/// 单行数据。
#[derive(Debug, Clone)]
pub struct DbRow {
    /// 列名（与结果集共享）
    columns: Arc<Vec<String>>,
    /// 值（与列一一对应）
    values: Vec<DbValue>,
}

impl DbRow {
    /// 创建一行。
    pub fn new(columns: Arc<Vec<String>>, values: Vec<DbValue>) -> Self {
        Self { columns, values }
    }

    /// 全部值。
    pub fn values(&self) -> &[DbValue] {
        &self.values
    }

    /// 消耗并取出全部值。
    pub fn into_values(self) -> Vec<DbValue> {
        self.values
    }

    /// 按位置取值。
    pub fn get(&self, index: usize) -> Option<&DbValue> {
        self.values.get(index)
    }

    /// 按列名取值（忽略大小写）。
    pub fn get_by_name(&self, name: &str) -> Option<&DbValue> {
        let index = self
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))?;
        self.values.get(index)
    }

    /// 组装为 `(列名, 值)` 列表，便于转换为业务结构。
    pub fn entries(&self) -> Vec<(&str, &DbValue)> {
        self.columns
            .iter()
            .map(String::as_str)
            .zip(self.values.iter())
            .collect()
    }
}

/// 数据库会话：执行 SQL、管理事务。
pub trait SqlSession {
    /// 数据库类型。
    fn kind(&self) -> DatabaseKind;

    /// 执行语句（INSERT/UPDATE/DELETE/DDL），返回受影响行数。
    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64>;

    /// 执行查询。
    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet>;

    /// 开启事务。
    fn begin(&mut self) -> Result<()>;

    /// 提交事务。
    fn commit(&mut self) -> Result<()>;

    /// 回滚事务。
    fn rollback(&mut self) -> Result<()>;

    /// 最近一次自增主键值（对应 XCode 插入后的 Identity 回写）。
    fn last_identity(&mut self) -> Result<i64>;

    /// 表是否存在。
    fn table_exists(&mut self, table: &str) -> Result<bool>;

    /// 现有表的列名列表。
    fn table_columns(&mut self, table: &str) -> Result<Vec<String>>;
}
