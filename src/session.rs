//! SQL 会话抽象：对应 DH.NCode 的 `IDbSession`。
//!
//! 上层 ORM（建表迁移、增删改查）只依赖本 trait，具体数据库由各驱动实现：
//! - 已实现：SQLite（[`crate::sqlite::SqliteSession`]）、MySQL（[`crate::mysql::MysqlSession`]）、
//!   SQL Server（[`crate::mssql::MssqlSession`]）、PostgreSQL 系（[`crate::postgres::PostgresSession`]，
//!   含 HighGo/KingBase/VastBase）、Oracle（[`crate::oracle::OracleSession`]）

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

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
    /// 列名索引（同一结果集的所有行共享一份；**惰性构建**——只数行不取值的场景零成本）
    pub(crate) index: Arc<OnceLock<Arc<HashMap<String, usize>>>>,
}

impl RowSet {
    /// 创建空结果集。
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            columns: Arc::new(columns),
            rows: Vec::new(),
            index: Arc::new(OnceLock::new()),
        }
    }

    /// 由**共享**列名创建空结果集（驱动层复用列名缓存时使用：同构查询不再重建列名列表）。
    pub(crate) fn with_columns(columns: Arc<Vec<String>>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            index: Arc::new(OnceLock::new()),
        }
    }

    /// 追加一行。
    pub fn push(&mut self, values: Vec<DbValue>) {
        // 宽表：列名索引与本结果集共享，**首次按名取值时才构建**；小表直接线性比较无需索引
        self.rows.push(DbRow::with_shared_index(
            self.columns.clone(),
            values,
            self.index.clone(),
        ));
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
        if self.columns.len() <= SMALL_COLUMN_COUNT {
            return self
                .columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name));
        }
        let index = self
            .index
            .get_or_init(|| Arc::new(build_column_index(&self.columns)));
        lookup_column(index, name)
    }
}

impl<'a> IntoIterator for &'a RowSet {
    type Item = &'a DbRow;
    type IntoIter = std::slice::Iter<'a, DbRow>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

/// 小列表阀值：列数不超过该值时按名取值走线性比较（避免为少列构建 HashMap）。
pub(crate) const SMALL_COLUMN_COUNT: usize = 8;

/// 单行数据。
#[derive(Debug)]
pub struct DbRow {
    /// 列名（与结果集共享）
    columns: Arc<Vec<String>>,
    /// 值（与列一一对应）
    values: Vec<DbValue>,
    /// 列名 → 下标（**与结果集共享**：仅 8 字节指针，首次按名取值时才构建索引）
    index: Arc<OnceLock<Arc<HashMap<String, usize>>>>,
}

/// 构建列名索引（**原文 + ASCII 小写**双键；重复列名保留首个）。
///
/// 双键的收益：调用方绝大多数直接传模型里的原文列名（生成代码即如此），
/// 此时命中原文键 → **零分配**；只有确实用小写/混合大小写时才退化为一次小写化。
fn build_column_index(columns: &[String]) -> HashMap<String, usize> {
    let mut map = HashMap::with_capacity(columns.len() * 2);
    for (position, name) in columns.iter().enumerate() {
        map.entry(name.clone()).or_insert(position);
        let lower = name.to_ascii_lowercase();
        if lower != *name {
            map.entry(lower).or_insert(position);
        }
    }
    map
}

/// 按列名查索引：先原文精确匹配（不分配），未命中再小写化查找。
fn lookup_column(index: &HashMap<String, usize>, name: &str) -> Option<usize> {
    if let Some(position) = index.get(name) {
        return Some(*position);
    }
    // 原文未命中：大小写不同的情况（`HashMap<String, _>` 可直接用 `&str` 查，无需先分配）
    if name.bytes().all(|b| !b.is_ascii_uppercase()) {
        return None;
    }
    index.get(&name.to_ascii_lowercase()).copied()
}

impl Clone for DbRow {
    fn clone(&self) -> Self {
        // 索引是共享的（`Arc`），克隆只加一次引用计数
        Self {
            columns: self.columns.clone(),
            values: self.values.clone(),
            index: self.index.clone(),
        }
    }
}

impl DbRow {
    /// 创建一行（自带独立列名索引）。
    pub fn new(columns: Arc<Vec<String>>, values: Vec<DbValue>) -> Self {
        Self {
            columns,
            values,
            index: Arc::new(OnceLock::new()),
        }
    }

    /// 创建一行并复用共享列名索引（[`RowSet`] 使用）。
    pub(crate) fn with_shared_index(
        columns: Arc<Vec<String>>,
        values: Vec<DbValue>,
        index: Arc<OnceLock<Arc<HashMap<String, usize>>>>,
    ) -> Self {
        Self {
            columns,
            values,
            index,
        }
    }

    /// 列名索引（惰性构建）。
    fn name_index(&self) -> &HashMap<String, usize> {
        self.index
            .get_or_init(|| Arc::new(build_column_index(&self.columns)))
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

    /// 按列名取值（忽略大小写；宽表列名索引缓存，小表直接线性比较）。
    pub fn get_by_name(&self, name: &str) -> Option<&DbValue> {
        self.values.get(self.position_of(name)?)
    }

    /// 按列名取出**所有权**（取出后该位置变为 `Null`）。
    ///
    /// 实体映射用：已经把数据行拿到手（`Arc` 独占）时，移动文本/二进制值比再复制一份便宜得多
    /// （每列省一次分配 + 复制）。列名规则与 [`DbRow::get_by_name`] 完全一致。
    pub fn take_by_name(&mut self, name: &str) -> Option<DbValue> {
        let position = self.position_of(name)?;
        Some(std::mem::replace(
            self.values.get_mut(position)?,
            DbValue::Null,
        ))
    }

    /// 列名 → 下标（小表线性比较，宽表走共享列名索引）。
    fn position_of(&self, name: &str) -> Option<usize> {
        if self.columns.len() <= SMALL_COLUMN_COUNT {
            return self.columns.iter().position(|c| c.eq_ignore_ascii_case(name));
        }
        lookup_column(self.name_index(), name)
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
///
/// 约束 `Send`：会话本身是独占使用的（`&mut self`），但连接池需要把空闲会话
/// 保存在池中供任意线程领取，因此要求会话类型可在线程间移动（不要求 `Sync`）。
pub trait SqlSession: Send {
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

    /// 最近一次自增主键值（带表名）。
    ///
    /// Oracle 需要由表名推导序列（`SEQ_{表名}` 的 CURRVAL），其余数据库忽略 `table` 参数。
    fn last_identity_of(&mut self, table: &str) -> Result<i64> {
        let _ = table;
        self.last_identity()
    }

    /// 插入并返回自增主键（组合操作，对应 C# `DbSession.InsertAndGetIdentity`）。
    ///
    /// 默认实现为 `execute + last_identity_of(table)`；`network` 等需要把
    /// "插入 + 取主键"整体转发给远端的驱动可覆写本方法。
    /// <param name="sql">INSERT 语句</param>
    /// <param name="params">参数</param>
    /// <param name="table">表名（供 Oracle 序列等场景；`None` 表示用会话级自增函数）</param>
    /// <returns>自增主键值</returns>
    fn insert_and_get_identity(
        &mut self,
        sql: &str,
        params: &[DbValue],
        table: Option<&str>,
    ) -> Result<i64> {
        self.execute(sql, params)?;
        match table {
            Some(table) => self.last_identity_of(table),
            None => self.last_identity(),
        }
    }

    /// 表是否存在。
    fn table_exists(&mut self, table: &str) -> Result<bool>;

    /// 现有表的列名列表。
    fn table_columns(&mut self, table: &str) -> Result<Vec<String>>;

    /// 表结构目录（驱动内建通道，供反向工程/结构比对）。
    ///
    /// 默认返回 `Ok(None)`：由 [`crate::catalog`] 的通用 SQL 路径处理；
    /// 仅无 SQL 型目录的驱动（如 ODBC 桥的 Access）实现本方法。
    fn catalog_tables(&mut self) -> Result<Option<Vec<crate::catalog::TableInfo>>> {
        Ok(None)
    }
}
