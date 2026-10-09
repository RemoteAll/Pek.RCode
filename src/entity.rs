//! 对象实体：让实体结构体自带增删改查行为，对应 DH.NCode 的 `Entity` 基类。
//!
//! C# 侧写法：
//!
//! ```csharp
//! var user = new User { Name = "test" };
//! user.Insert();          // 或 Save()
//! var u = User.FindByID(1);
//! u.Name = "test2";
//! u.Update();
//! ```
//!
//! Rust 侧由 [`crate::codegen`] 为每个实体生成 [`Entity`] 实现，用法保持一致：
//!
//! ```no_run
//! use pek_rcode::{Dal, DbRow, DbValue, Entity, Result, Where};
//!
//! # struct User { id: i32, name: String }
//! # impl Entity for User {
//! #     fn table() -> &'static str { "DH_User" }
//! #     fn columns() -> &'static [&'static str] { &["Id", "Name"] }
//! #     fn primary_keys() -> &'static [&'static str] { &["Id"] }
//! #     fn identity_column() -> Option<&'static str> { Some("Id") }
//! #     fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
//! #         vec![("Id", self.id.into()), ("Name", self.name.clone().into())]
//! #     }
//! #     fn from_row(row: &DbRow) -> Result<Self> {
//! #         Ok(Self {
//! #             id: row.get_by_name("Id").and_then(DbValue::as_i32).unwrap_or_default(),
//! #             name: row.get_by_name("Name").map(DbValue::to_text).unwrap_or_default(),
//! #         })
//! #     }
//! #     fn set_identity(&mut self, value: i64) -> Result<()> { self.id = value as i32; Ok(()) }
//! # }
//! # fn main() -> Result<()> {
//! let dal = Dal::open_with_model("Data Source=demo.db;Provider=SQLite", pek_rcode::EntityModel::parse(
//!     r#"<EntityModel><Tables><Table Name="User" TableName="DH_User">
//!        <Columns>
//!          <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
//!          <Column Name="Name" DataType="String" Length="50" />
//!        </Columns></Table></Tables></EntityModel>"#)?)?;
//! dal.sync_schema()?;
//! let mut session = dal.open_session()?;
//!
//! // 新增：插入后自增主键自动回写到对象
//! let mut user = User { id: 0, name: "test".into() };
//! user.insert(&dal, session.as_mut())?;
//!
//! // 按主键查询 / 条件查询
//! let mut found = User::find(&dal, session.as_mut(), &[user.id.into()])?.unwrap();
//! let list = User::query(&dal, session.as_mut(), &pek_rcode::Query::new().filter(Where::new().like("Name", "t%"))) ?;
//! println!("{} {}", found.name, list.len());
//!
//! // 保存（有自增按 Id=0 判定新增，否则更新）/ 删除
//! found.name = "test2".into();
//! found.save(&dal, session.as_mut())?;
//! found.delete(&dal, session.as_mut())?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{
    dal::{Dal, TableRef},
    entity_queue::EntityQueue,
    error::{Error, Result},
    query::{Query, Where},
    session::{DbRow, SqlSession},
    shards::TimeShardPolicy,
    sqlbuild::BatchWriteMode,
    value::DbValue,
};

/// 实体对象：由 [`crate::codegen`] 生成实现，也可手工实现。
///
/// 必须实现的部分只有“映射”相关的 5 个方法（表名/列/主键/取值/装载），
/// 增删改查由默认方法提供（对应 XCode 的 `Entity` 基类行为）。
pub trait Entity: Sized {
    /// 数据库表名（通常返回生成的 `TABLE_NAME` 常量）。
    fn table() -> &'static str;

    /// 全部数据库列名（按模型顺序）。
    fn columns() -> &'static [&'static str];

    /// 主键列名（按模型顺序；复合主键返回多个）。
    fn primary_keys() -> &'static [&'static str];

    /// 自增列名（无自增列返回 None）。
    fn identity_column() -> Option<&'static str> {
        None
    }

    /// 输出实体的全部 `(列名, 值)`（用于插入/更新/删除的主键提取）。
    fn to_fields(&self) -> Vec<(&'static str, DbValue)>;

    /// 从查询行装载实体。
    fn from_row(row: &DbRow) -> Result<Self>;

    /// 由查询行集合装载实体集合（对应 C# `DataRowEntityAccessor.LoadData`）。
    /// <param name="rows">数据行</param>
    /// <returns>实体集合</returns>
    fn from_rows(rows: &[DbRow]) -> Result<Vec<Self>> {
        rows.iter().map(Self::from_row).collect()
    }

    /// 由结果集装载实体集合（行集 → 实体集合，无数据时返回空集合）。
    /// <param name="set">结果集</param>
    /// <returns>实体集合</returns>
    fn load(set: &crate::session::RowSet) -> Result<Vec<Self>> {
        Self::from_rows(&set.rows)
    }

    /// 插入后回写自增主键（由生成代码实现；无自增实体无需覆盖）。
    fn set_identity(&mut self, value: i64) -> Result<()> {
        let _ = value;
        Ok(())
    }

    /// 回写指定列的值，返回是否识别并写入该列（由生成代码实现；手写实现可忽略）。
    ///
    /// 用于分表场景回写生成的雪花主键（[`Entity::insert_sharded`]）；
    /// 默认实现返回 `Ok(false)`（不支持）。
    fn set_field(&mut self, column: &str, value: DbValue) -> Result<bool> {
        let _ = (column, value);
        Ok(false)
    }

    /// 插入一行（自动跳过自增列并回写主键），返回自增主键（无自增时 0）。
    fn insert(&mut self, dal: &Dal, session: &mut dyn SqlSession) -> Result<i64> {
        let table = dal.table(Self::table())?;
        self.insert_with(&table, session)
    }

    /// 插入一行到指定表句柄（分表场景由 [`Entity::insert_sharded`] 传入分表句柄）。
    fn insert_with(&mut self, table: &TableRef<'_>, session: &mut dyn SqlSession) -> Result<i64> {
        let identity = table.meta().identity().map(|c| c.name.clone());

        let fields: Vec<(&'static str, DbValue)> = self
            .to_fields()
            .into_iter()
            .filter(|pair| !identity.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(pair.0)))
            .collect();

        let id = table.insert(session, &fields)?;
        if id != 0 {
            self.set_identity(id)?;
        }
        Ok(id)
    }

    /// 批量插入（对应 C# `EntityExtension.Insert(list)`）：整批多行写入，返回受影响行数。
    ///
    /// - 每行先执行拦截器补全（如 `TimeInterceptor` 补全 `CreateTime`），与单行插入一致；
    /// - **首行自增列为 0 或缺失 → 整批排除自增列**（由数据库生成，对齐 C# `BuildInsertColumns`；
    ///   需要显式指定自增值时请按含/不含自增值拆批调用）；
    /// - **不回写自增主键**（与 C# `BatchInsert` 一致）；无自增的雪花主键请先填值，
    ///   分表场景请用 [`Entity::insert_batch_sharded`]（自动生成雪花并路由分片）；
    /// - `batch_size` 默认 5000（对齐 C# `DAL.GetBatchSize()`）；支持多行 `VALUES` 的数据库
    ///   整批写入，其余数据库回退逐行（语义一致）。
    fn insert_batch(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&Self> = list.iter().collect();
        Self::insert_batch_refs(dal, session, &refs, batch_size)
    }

    /// 批量插入的核心实现（引用列表；供 [`Entity::save_batch`] 复用）。
    fn insert_batch_refs(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[&Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        Self::write_batch_refs(dal, session, list, BatchWriteMode::Insert, batch_size)
    }

    /// 通用批量写入的核心实现（引用列表；供各批量方法复用）。
    fn write_batch_refs(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[&Self],
        mode: BatchWriteMode,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let rows: Vec<Vec<(&str, DbValue)>> = list
            .iter()
            .map(|entity| {
                entity
                    .to_fields()
                    .into_iter()
                    .map(|(name, value)| {
                        let name: &str = name;
                        (name, value)
                    })
                    .collect()
            })
            .collect();
        let table = dal.table(Self::table())?;
        table.write_batch(session, &rows, mode, batch_size)
    }

    /// 批量忽略重复插入（对应 C# `EntityExtension.BatchInsertIgnore`）：重复键静默跳过。
    ///
    /// 仅 SQLite/DuckDB（`Insert Or Ignore`）、MySQL（`Insert Ignore`）、PostgreSQL（`On Conflict Do Nothing`）
    /// 支持多行实现；其余方言报 [`Error::Unsupported`]（无安全回退，与 C# 一致）。
    fn insert_ignore_batch(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&Self> = list.iter().collect();
        Self::write_batch_refs(dal, session, &refs, BatchWriteMode::InsertIgnore, batch_size)
    }

    /// 批量替换插入（对应 C# `EntityExtension.BatchReplace`）：按主键/唯一键替换整行。
    ///
    /// 仅 SQLite/DuckDB（`Insert Or Replace`）、MySQL（`Replace Into`）支持；其余方言报错（与 C# 一致）。
    fn replace_batch(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&Self> = list.iter().collect();
        Self::write_batch_refs(dal, session, &refs, BatchWriteMode::Replace, batch_size)
    }

    /// 批量插入或更新（对应 C# `EntityExtension.Save(list)` 的 Upsert 语义）：按主键冲突更新非主键列。
    ///
    /// - SQLite/DuckDB/PostgreSQL 生成 `On Conflict(pk) Do Update`、MySQL 生成 `On Duplicate Key Update`，
    ///   多行一条语句；其余方言回退逐行（查主键后更新/插入，同队列 Upsert）；
    /// - 存在自增列时**不允许部分指定主键、部分不指定**（对齐 C# `Save(list)` 的 `NotSupportedException`）；
    /// - 无自增的雪花主键请先填值，分表场景用 [`Entity::upsert_batch_sharded`]（自动生成并路由）。
    fn upsert_batch(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&Self> = list.iter().collect();
        Self::ensure_identity_uniform(&refs)?;
        Self::write_batch_refs(dal, session, &refs, BatchWriteMode::Upsert, batch_size)
    }

    /// 批量按主键更新（对应 C# `EntityExtension.BatchUpdate`）：逐条 `Update`（无批更新能力的方言行为一致）。
    fn update_batch(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[Self],
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let table = dal.table(Self::table())?;
        let mut total = 0u64;
        for entity in list {
            total += entity.update_with(&table, session)?;
        }
        Ok(total)
    }

    /// 批量更新到各自分表（逐条按分表值路由更新；返回受影响行数合计）。
    fn update_batch_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        list: &[Self],
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let mut total = 0u64;
        for entity in list {
            total += entity.update_sharded(dal, session, policy)?;
        }
        Ok(total)
    }

    /// 自增列一致性检查（对齐 C# `Save(list)`：存在自增列时不允许"部分指定主键、部分不指定"）。
    fn ensure_identity_uniform(list: &[&Self]) -> Result<()> {
        let Some(id) = Self::identity_column() else {
            return Ok(());
        };
        let mut empty = false;
        let mut full = false;
        for entity in list {
            let value = entity
                .to_fields()
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(id))
                .map(|(_, value)| value)
                .unwrap_or(DbValue::Null);
            if is_empty_pk_value(&value) {
                empty = true;
            } else {
                full = true;
            }
            if empty && full {
                return Err(Error::Unsupported(format!(
                    "实体 {} 存在自增列时不能同时包含新增与更新数据（对齐 C# Save(list)）",
                    Self::table()
                )));
            }
        }
        Ok(())
    }

    /// 按主键更新（主键值与自增列不参与 SET），返回受影响行数。
    fn update(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<u64> {
        let table = dal.table(Self::table())?;
        self.update_with(&table, session)
    }

    /// 按主键更新指定表句柄（分表场景使用）。
    fn update_with(&self, table: &TableRef<'_>, session: &mut dyn SqlSession) -> Result<u64> {
        let pks = Self::primary_keys();
        if pks.is_empty() {
            return Err(Error::Model(format!("实体 {} 没有主键，无法按主键更新", Self::table())));
        }

        let identity = Self::identity_column();
        let fields = self.to_fields();
        let sets: Vec<(&'static str, DbValue)> = fields
            .iter()
            .filter(|pair| {
                !pks.iter().any(|pk| pk.eq_ignore_ascii_case(pair.0))
                    && !identity.is_some_and(|id| id.eq_ignore_ascii_case(pair.0))
            })
            .map(|pair| (pair.0, pair.1.clone()))
            .collect();

        if sets.is_empty() {
            return Err(Error::Model(format!(
                "实体 {} 除主键/自增列外没有可更新字段",
                Self::table()
            )));
        }

        let pk_values = pk_values_of(&fields, pks)?;
        table.update_by_pk(session, &sets, &pk_values)
    }

    /// 保存（对应 XCode 的 `Save`）：
    /// - 有自增列：主键值为 0/NULL 视为新增，否则更新
    /// - 无自增列：按主键行是否存在决定新增或更新
    fn save(&mut self, dal: &Dal, session: &mut dyn SqlSession) -> Result<u64> {
        if is_new_entity(dal, session, self)? {
            self.insert(dal, session)?;
            Ok(1)
        } else {
            self.update(dal, session)
        }
    }

    /// 批量保存（对应 C# `EntityExtension.Save(list)`）：逐实体按 [`Entity::save`] 的规则拆分，
    /// **新增合并为多行批量插入**，已存在逐条更新（与 C# 无批更新能力时的回退一致）。
    ///
    /// - 与 C# 的差异：C# 对"主键非空且非来自数据库"的实体走 Upsert（行不存在则插入）；
    ///   本库按单实体 `Save` 规则处理（自增列非零 → 直接更新），需要"确保存在"请用 Upsert 能力或在业务侧判断；
    /// - **不回写自增主键**（批量插入限制，与 C# `BatchInsert` 一致）；雪花主键请先填值
    ///   （分表场景用 [`Entity::save_batch_sharded`] 自动生成）；
    /// - 返回受影响行数。
    fn save_batch(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let mut news: Vec<usize> = Vec::new();
        let mut total = 0u64;
        for (index, entity) in list.iter().enumerate() {
            if is_new_entity(dal, session, entity)? {
                news.push(index);
            } else {
                total += entity.update(dal, session)?;
            }
        }
        if !news.is_empty() {
            let refs: Vec<&Self> = news.iter().map(|&index| &list[index]).collect();
            total += Self::insert_batch_refs(dal, session, &refs, batch_size)?;
        }
        Ok(total)
    }

    /// 保存入队（对应 C# `Entity.SaveAsync(msDelay)` 的入队语义；落库由 [`EntityQueue::flush`] 批量执行）：
    ///
    /// - 有自增列且值为 0（或缺失）、或无主键值 → 按**插入**入队
    ///   （flush 时连续 Insert 段合并为多行批量插入，适合日志表等只插入场景）；
    /// - 其余 → 按 **Upsert** 入队（flush 时按主键存在性更新/插入，与单实体 [`Entity::save`] 的最终行状态一致）；
    /// - 达到队列批大小自动 flush，或由调用方显式 [`EntityQueue::flush`]；
    /// - 与 C# 的差异：无内置定时器与 `msDelay` 延迟集合（由调用方用 `dhrust::threading::Timer`
    ///   或异步任务驱动 flush）、无同实体去重、不感知分表（分表批量落库请用 [`Entity::save_batch_sharded`]）；
    /// - 雪花主键请先填值（本方法无分表策略上下文，无法自动生成）。
    fn enqueue_save(
        &mut self,
        queue: &mut EntityQueue,
        dal: &Dal,
        session: &mut dyn SqlSession,
    ) -> Result<()> {
        let fields = self.to_fields();
        let is_new = match Self::identity_column() {
            Some(id) => {
                let value = fields
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(id))
                    .map(|(_, value)| value);
                !matches!(value.and_then(|v| v.as_i64()), Some(v) if v != 0)
            }
            None => {
                let pk = pk_values_of(&fields, Self::primary_keys())?;
                pk.iter().all(is_empty_pk_value)
            }
        };
        if is_new {
            queue.insert(dal, session, &fields)
        } else {
            let pk = pk_values_of(&fields, Self::primary_keys())?;
            queue.upsert(dal, session, &pk, &fields)
        }
    }

    /// 按主键删除本对象对应的行，返回受影响行数。
    fn delete(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<u64> {
        let table = dal.table(Self::table())?;
        self.delete_with(&table, session)
    }

    /// 按主键删除指定表句柄中的行（分表场景使用）。
    fn delete_with(&self, table: &TableRef<'_>, session: &mut dyn SqlSession) -> Result<u64> {
        let pk = pk_values_of(&self.to_fields(), Self::primary_keys())?;
        table.delete_by_pk(session, &pk)
    }

    /// 批量按主键删除（对应 C# `EntityExtension.Delete(list)` 的批量分支）：
    /// 实体为**单一主键**且数量大于 1 时按主键 `IN` 分批删除（默认 1000/批，对齐 C#），
    /// 否则逐条删除（复合主键/单条）。返回受影响行数。
    fn delete_batch(
        dal: &Dal,
        session: &mut dyn SqlSession,
        list: &[Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let pks = Self::primary_keys();
        if pks.len() == 1 && list.len() > 1 {
            let pk = pks[0];
            let mut values = Vec::with_capacity(list.len());
            for entity in list {
                let fields = entity.to_fields();
                if let Some((_, value)) = fields
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(pk))
                    && !matches!(value, DbValue::Null)
                {
                    values.push(value.clone());
                }
            }
            let table = dal.table(Self::table())?;
            table.delete_by_pk_values(session, pk, &values, batch_size)
        } else {
            let mut affected = 0u64;
            for entity in list {
                affected += entity.delete(dal, session)?;
            }
            Ok(affected)
        }
    }

    /// 按主键查询单条记录（对应 XCode 的 `FindByID` 等）。
    fn find(dal: &Dal, session: &mut dyn SqlSession, pk: &[DbValue]) -> Result<Option<Self>> {
        let table = dal.table(Self::table())?;
        table
            .find_by_pk(session, pk)?
            .map(|row| Self::from_row(&row))
            .transpose()
    }

    /// 条件查询（对应 XCode 的 `FindAll` + 分页参数）。
    fn query(dal: &Dal, session: &mut dyn SqlSession, query: &Query) -> Result<Vec<Self>> {
        let table = dal.table(Self::table())?;
        let rows = table.query(session, query)?;
        rows.rows
            .into_iter()
            .map(|row| Self::from_row(&row))
            .collect()
    }

    /// 全表（或按默认顺序）查询。
    fn all(dal: &Dal, session: &mut dyn SqlSession) -> Result<Vec<Self>> {
        Self::query(dal, session, &Query::new())
    }

    /// 统计行数。
    fn count(dal: &Dal, session: &mut dyn SqlSession, filter: Option<&Where>) -> Result<i64> {
        let table = dal.table(Self::table())?;
        table.count(session, filter)
    }

    // ================= 分表（对应 C# `Meta.CreateShard` 包装的增删改查） =================
    //
    // 分表路由规则与 C# 完全一致：由实体上 `policy.field` 指定的分表字段值（时间列 / 雪花 Id）
    // 定位物理表（如 `Log2_20260927`）；写操作在分表不存在时自动建表（对齐 C# `EntitySession.CheckTable`），
    // 读操作对不存在的分表直接返回空。

    /// 插入一行到对应分表（按 `policy.field` 字段值路由；字段为空时按策略自动生成雪花 Id）。
    ///
    /// - 时间字段：字段为空时**先执行拦截器**（如 `TimeInterceptor` 自动填 `CreateTime`，
    ///   与 C# 的 `Valid → CreateShard` 顺序一致），再按补全值定位分表；
    /// - 雪花 Id 字段（`DbValue::Int` 且 <= 0）：使用策略中的 [`Snowflake`](crate::snowflake::Snowflake)
    ///   生成新 Id 并通过 [`Entity::set_field`] 回写实体（对齐 C# `AutoFillSnowIdPrimaryKey`），
    ///   未实现 `set_field` 时返回错误；
    /// - 分表不存在时自动建表（迁移档位为 `Off`/只读时不建，插入将直接报错）。
    fn insert_sharded(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
    ) -> Result<i64> {
        // 1) 拦截器先行：拿到"实际入库"的字段值（含 CreateTime 自动补全）
        let handle = dal.table(Self::table())?;
        let fields = self.to_fields();
        let own_value = fields
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
            .map(|(_, value)| value.clone())
            .ok_or_else(|| {
                Error::Model(format!(
                    "实体 {} 不存在分表字段 {}",
                    Self::table(),
                    policy.field
                ))
            })?;
        let prepared = crate::interceptor::prepare(
            handle.meta(),
            crate::interceptor::DataMethod::Insert,
            &fields,
        );
        let mut value = prepared
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| own_value.clone());
        // 拦截器补全了分表字段（如 CreateTime）→ 回写实体，保证分表解析与入库值一致
        if value != own_value {
            let _ = self.set_field(&policy.field, value.clone());
        }

        // 2) 雪花主键为空 → 生成并回写（此时必须支持 set_field，否则分表无法确定）
        let mut generated = None;
        if let DbValue::Int(id) = value
            && id <= 0
        {
            let snow = policy.snow.as_ref().ok_or_else(|| {
                Error::Model(format!(
                    "实体 {} 的雪花主键为空，且分表策略未配置 Snowflake",
                    Self::table()
                ))
            })?;
            let new_id = snow.now_id()?;
            if !self.set_field(&policy.field, DbValue::Int(new_id))? {
                return Err(Error::Model(format!(
                    "实体 {} 未实现 set_field，无法回写生成的雪花主键",
                    Self::table()
                )));
            }
            value = DbValue::Int(new_id);
            generated = Some(new_id);
        }

        match resolve_shard_target::<Self>(dal, policy, &value)? {
            Some(target) => {
                ensure_shard_target(&target, dal, Self::table())?;
                let id = run_on_shard_target(&target, dal, Self::table(), session, |t, s| {
                    self.insert_with(t, s)
                })?;
                // 雪花主键（无自增列）时返回生成值，便于调用方直接使用
                Ok(generated.unwrap_or(id))
            }
            None => {
                let table = dal.table(Self::table())?;
                let id = self.insert_with(&table, session)?;
                Ok(generated.unwrap_or(id))
            }
        }
    }

    /// 批量插入到各自分表（对应 C# `EntityExtension.Insert(list)` 的分表分支：
    /// **提前计算分表、按库表分组后分组批量插入**）。
    ///
    /// - 逐行先执行拦截器补全（如 `CreateTime` 自动填充）并回写实体，再按 `policy.field` 计算分片
    ///   （分片字段缺失时报错，与单行 [`Entity::insert_sharded`] 一致）；
    /// - 雪花主键（Int64 ≤ 0）逐行生成并回写（对齐 C# `AutoFillSnowIdPrimaryKey`，批内顺序稳定）；
    /// - 按 **(连接, 物理表)** 分组：同连接复用同一会话；跨库连接经连接注册表解析
    ///   （未注册按 C# 规则自动落 SQLite 库）；分表不存在时自动建表（对齐 `CheckTable`）；
    /// - **不回写自增主键**（与 C# `BatchInsert` 一致）；返回受影响行数。
    fn insert_batch_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        list: &mut [Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        let mut refs: Vec<&mut Self> = list.iter_mut().collect();
        Self::write_batch_sharded_refs(
            dal,
            session,
            policy,
            &mut refs,
            BatchWriteMode::Insert,
            batch_size,
        )
    }

    /// 批量插入或更新到各自分表（Upsert 模式；分表路由、拦截器、雪花生成同 [`Entity::insert_batch_sharded`]）。
    ///
    /// 存在自增列时不允许部分指定主键、部分不指定（对齐 C# `Save(list)`）。
    fn upsert_batch_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        list: &mut [Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        let check: Vec<&Self> = list.iter().collect();
        Self::ensure_identity_uniform(&check)?;
        let mut refs: Vec<&mut Self> = list.iter_mut().collect();
        Self::write_batch_sharded_refs(
            dal,
            session,
            policy,
            &mut refs,
            BatchWriteMode::Upsert,
            batch_size,
        )
    }

    /// 分表批量写入的核心实现（可变引用列表；供 [`Entity::insert_batch_sharded`] /
    /// [`Entity::upsert_batch_sharded`] / [`Entity::save_batch_sharded`] 复用）。
    fn write_batch_sharded_refs(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        list: &mut [&mut Self],
        mode: BatchWriteMode,
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let handle = dal.table(Self::table())?;
        let base = handle.shard_base();
        let base_table = handle.meta().effective_table_name().to_string();

        let mut groups: Vec<BatchGroup<Vec<(String, DbValue)>>> = Vec::new();
        let mut group_index: HashMap<String, usize> = HashMap::new();

        // 阶段一：拦截器补全 → 回写 → 雪花生成 → 分片解析，按（连接, 物理表）分组
        for entity in list.iter_mut() {
            let entity: &mut Self = entity;
            let fields = entity.to_fields();
            let own_value = fields
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| {
                    Error::Model(format!(
                        "实体 {} 不存在分表字段 {}",
                        Self::table(),
                        policy.field
                    ))
                })?;
            let mut row = crate::interceptor::prepare(
                handle.meta(),
                crate::interceptor::DataMethod::Insert,
                &fields,
            );
            let mut value = row
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| own_value.clone());
            if value != own_value {
                let _ = entity.set_field(&policy.field, value.clone());
            }
            // 雪花主键为空 → 生成并回写（同步进待插入行，保证分片与入库一致）
            if let DbValue::Int(id) = value
                && id <= 0
            {
                let snow = policy.snow.as_ref().ok_or_else(|| {
                    Error::Model(format!(
                        "实体 {} 的雪花主键为空，且分表策略未配置 Snowflake",
                        Self::table()
                    ))
                })?;
                let new_id = snow.now_id()?;
                if !entity.set_field(&policy.field, DbValue::Int(new_id))? {
                    return Err(Error::Model(format!(
                        "实体 {} 未实现 set_field，无法回写生成的雪花主键",
                        Self::table()
                    )));
                }
                value = DbValue::Int(new_id);
                if let Some((_, cell)) = row
                    .iter_mut()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
                {
                    *cell = value.clone();
                }
            }

            // 分片解析（策略未配置分表模板 → 基础表单组）
            let (target, conn, physical, ensure) = match policy.shard_of_value(base, &value)? {
                Some(model) => {
                    let physical = model
                        .table_name
                        .clone()
                        .unwrap_or_else(|| base_table.clone());
                    let target = crate::shards::resolve_shard_dal(&model, base, dal)?;
                    let conn = match &target {
                        Some(_) => model.conn_name.as_deref().unwrap_or("").to_lowercase(),
                        None => String::new(),
                    };
                    (target, conn, physical, true)
                }
                None => (None, String::new(), base_table.clone(), false),
            };

            let key = format!("{conn}\u{1}{physical}");
            let index = match group_index.get(&key) {
                Some(&index) => index,
                None => {
                    let index = groups.len();
                    groups.push(BatchGroup {
                        dal: target.clone(),
                        conn: conn.clone(),
                        physical: physical.clone(),
                        ensure,
                        payload: Vec::new(),
                    });
                    group_index.insert(key, index);
                    index
                }
            };
            groups[index].ensure |= ensure;
            groups[index].payload.push(row);
        }

        // 阶段二：按连接复用会话，逐组批量写入
        let mut affected = 0u64;
        let mut owned: HashMap<String, Box<dyn SqlSession>> = HashMap::new();
        for group in &groups {
            let (group_dal, group_session): (&Dal, &mut dyn SqlSession) = match &group.dal {
                None => (dal, &mut *session),
                Some(d) => {
                    if !owned.contains_key(&group.conn) {
                        owned.insert(group.conn.clone(), d.open_session()?);
                    }
                    (d.as_ref(), &mut **owned.get_mut(&group.conn).unwrap())
                }
            };
            if group.ensure {
                group_dal.ensure_shard_table(Self::table(), &group.physical)?;
            }
            let table = group_dal.table_as(Self::table(), &group.physical)?;
            affected += table.write_batch_prepared(group_session, &group.payload, mode, batch_size)?;
        }
        Ok(affected)
    }

    /// 更新本对象所在分表的行（按 `policy.field` 字段值路由；分表不存在时自动建表，对齐 C#）。
    fn update_sharded(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
    ) -> Result<u64> {
        let value = shard_value_of(self, policy)?;
        match resolve_shard_target::<Self>(dal, policy, &value)? {
            Some(target) => {
                ensure_shard_target(&target, dal, Self::table())?;
                run_on_shard_target(&target, dal, Self::table(), session, |t, s| {
                    self.update_with(t, s)
                })
            }
            None => {
                let table = dal.table(Self::table())?;
                self.update_with(&table, session)
            }
        }
    }

    /// 删除本对象所在分表的行（按 `policy.field` 字段值路由；分表不存在时自动建表，对齐 C#）。
    fn delete_sharded(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
    ) -> Result<u64> {
        let value = shard_value_of(self, policy)?;
        match resolve_shard_target::<Self>(dal, policy, &value)? {
            Some(target) => {
                ensure_shard_target(&target, dal, Self::table())?;
                run_on_shard_target(&target, dal, Self::table(), session, |t, s| {
                    self.delete_with(t, s)
                })
            }
            None => {
                let table = dal.table(Self::table())?;
                self.delete_with(&table, session)
            }
        }
    }

    /// 批量删除各自分表的行（分表场景的批量删除）。
    ///
    /// - 按 **(连接, 物理表)** 自动分组，组内按主键 `IN` 分批删除（默认 1000/批；
    ///   复合主键回退逐条删除）；跨库连接自动路由（注册表 / 自动 SQLite 回退）；
    /// - 与 C# 的差异：C# 批量删除不区分分表（用会话当前表），本库自动路由到正确分表（更安全）；
    /// - 缺主键值的行跳过（与 C# 一致）；不存在的分表跳过（与单行删除一致）；返回受影响行数。
    fn delete_batch_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        list: &[Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let pks = Self::primary_keys();
        if pks.len() != 1 {
            let mut affected = 0u64;
            for entity in list {
                affected += entity.delete_sharded(dal, session, policy)?;
            }
            return Ok(affected);
        }
        let pk = pks[0];
        let handle = dal.table(Self::table())?;
        let base = handle.shard_base();
        let base_table = handle.meta().effective_table_name().to_string();

        let mut groups: Vec<BatchGroup<DbValue>> = Vec::new();
        let mut group_index: HashMap<String, usize> = HashMap::new();

        for entity in list {
            let fields = entity.to_fields();
            // 主键值缺失/为空 → 跳过（对齐 C# `if (val == null) continue`）
            let Some((_, pk_value)) = fields.iter().find(|(name, _)| name.eq_ignore_ascii_case(pk))
            else {
                continue;
            };
            if matches!(pk_value, DbValue::Null) {
                continue;
            }
            let shard_value = fields
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| {
                    Error::Model(format!(
                        "实体 {} 不存在分表字段 {}",
                        Self::table(),
                        policy.field
                    ))
                })?;
            let (target, conn, physical) = match policy.shard_of_value(base, &shard_value)? {
                Some(model) => {
                    let physical = model
                        .table_name
                        .clone()
                        .unwrap_or_else(|| base_table.clone());
                    let target = crate::shards::resolve_shard_dal(&model, base, dal)?;
                    let conn = match &target {
                        Some(_) => model.conn_name.as_deref().unwrap_or("").to_lowercase(),
                        None => String::new(),
                    };
                    (target, conn, physical)
                }
                None => (None, String::new(), base_table.clone()),
            };

            let key = format!("{conn}\u{1}{physical}");
            let index = match group_index.get(&key) {
                Some(&index) => index,
                None => {
                    let index = groups.len();
                    groups.push(BatchGroup {
                        dal: target.clone(),
                        conn: conn.clone(),
                        physical: physical.clone(),
                        ensure: false,
                        payload: Vec::new(),
                    });
                    group_index.insert(key, index);
                    index
                }
            };
            groups[index].payload.push(pk_value.clone());
        }

        let mut affected = 0u64;
        let mut owned: HashMap<String, Box<dyn SqlSession>> = HashMap::new();
        for group in &groups {
            let (group_dal, group_session): (&Dal, &mut dyn SqlSession) = match &group.dal {
                None => (dal, &mut *session),
                Some(d) => {
                    if !owned.contains_key(&group.conn) {
                        owned.insert(group.conn.clone(), d.open_session()?);
                    }
                    (d.as_ref(), &mut **owned.get_mut(&group.conn).unwrap())
                }
            };
            if !group_session.table_exists(&group.physical)? {
                continue;
            }
            let table = group_dal.table_as(Self::table(), &group.physical)?;
            affected += table.delete_by_pk_values(group_session, pk, &group.payload, batch_size)?;
        }
        Ok(affected)
    }

    /// 保存到分表（对应 C# `Save()` 的分表分支）：
    /// 有自增列按 0 值判定新增/更新；无自增列按主键是否存在判定（在解析出的分表上判断）。
    ///
    /// 与 [`Entity::insert_sharded`] 一样，会**先执行拦截器**（如 `TimeInterceptor` 自动填
    /// `CreateTime`，对齐 C# 的 `Valid → CreateShard` 顺序）再判定与路由。
    fn save_sharded(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
    ) -> Result<u64> {
        // 拦截器先行：补全分表字段（空白 CreateTime 等）并回写实体
        {
            let handle = dal.table(Self::table())?;
            let fields = self.to_fields();
            if let Some((name, _)) = fields
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
            {
                let own = fields
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.clone());
                let prepared = crate::interceptor::prepare(
                    handle.meta(),
                    crate::interceptor::DataMethod::Insert,
                    &fields,
                );
                if let Some((_, value)) = prepared
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    && Some(value) != own.as_ref()
                {
                    let _ = self.set_field(&policy.field, value.clone());
                }
            }
        }

        let is_new = match Self::identity_column() {
            Some(id) => {
                let value = self
                    .to_fields()
                    .into_iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(id))
                    .map(|(_, value)| value);
                !matches!(value.and_then(|v| v.as_i64()), Some(v) if v != 0)
            }
            None => {
                let fields = self.to_fields();
                let pk = pk_values_of(&fields, Self::primary_keys())?;
                let value = shard_value_of(self, policy)?;
                match resolve_shard_target::<Self>(dal, policy, &value)? {
                    Some(target) => run_on_shard_target(
                        &target,
                        dal,
                        Self::table(),
                        session,
                        |t, s| {
                            if !s.table_exists(t.physical_name())? {
                                return Ok(true);
                            }
                            Ok(t.find_by_pk(s, &pk)?.is_none())
                        },
                    )?,
                    None => Self::find(dal, session, &pk)?.is_none(),
                }
            }
        };

        if is_new {
            self.insert_sharded(dal, session, policy)?;
            Ok(1)
        } else {
            self.update_sharded(dal, session, policy)
        }
    }

    /// 批量保存到各自分表（分表场景的批量 `Save`）：逐实体按 `save` 规则拆分，
    /// **新增合并为分组批量插入**（自动生成雪花、自动建分表、跨库自动路由），
    /// 已存在逐条更新到对应分表。返回受影响行数。
    ///
    /// - 判定规则与单实体一致：自增列 0/缺失 → 新增；无自增 → 主键全空视为新增，
    ///   否则在解析出的分表上按主键查询存在性（分表不存在视为新增）；
    /// - **不回写自增主键**（批量插入限制）；雪花主键自动生成并回写（同 [`Entity::insert_batch_sharded`]）。
    fn save_batch_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        list: &mut [Self],
        batch_size: Option<usize>,
    ) -> Result<u64> {
        if list.is_empty() {
            return Ok(0);
        }
        let mut news: Vec<usize> = Vec::new();
        let mut total = 0u64;
        for (index, entity) in list.iter().enumerate() {
            let is_new = match Self::identity_column() {
                Some(id) => {
                    let value = entity
                        .to_fields()
                        .into_iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(id))
                        .map(|(_, value)| value);
                    !matches!(value.and_then(|v| v.as_i64()), Some(v) if v != 0)
                }
                None => {
                    let fields = entity.to_fields();
                    let pk = pk_values_of(&fields, Self::primary_keys())?;
                    if pk.iter().all(is_empty_pk_value) {
                        true
                    } else {
                        let value = shard_value_of(entity, policy)?;
                        match resolve_shard_target::<Self>(dal, policy, &value)? {
                            Some(target) => run_on_shard_target(
                                &target,
                                dal,
                                Self::table(),
                                session,
                                |t, s| {
                                    if !s.table_exists(t.physical_name())? {
                                        return Ok(true);
                                    }
                                    Ok(t.find_by_pk(s, &pk)?.is_none())
                                },
                            )?,
                            None => Self::find(dal, session, &pk)?.is_none(),
                        }
                    }
                }
            };
            if is_new {
                news.push(index);
            } else {
                total += entity.update_sharded(dal, session, policy)?;
            }
        }
        if !news.is_empty() {
            let news_set: HashSet<usize> = news.iter().copied().collect();
            let mut refs: Vec<&mut Self> = list
                .iter_mut()
                .enumerate()
                .filter(|(index, _)| news_set.contains(index))
                .map(|(_, entity)| entity)
                .collect();
            total += Self::write_batch_sharded_refs(
                dal,
                session,
                policy,
                &mut refs,
                BatchWriteMode::Insert,
                batch_size,
            )?;
        }
        Ok(total)
    }

    /// 按分表值 + 主键查询单条记录（对应 C# `FindByKey` 的分表分支）：
    /// `value` 为分表字段值（时间或雪花 Id，如主键即分表字段可直接传主键值）；
    /// **不存在的分表直接返回 `None`，不自动建表**。
    fn find_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        value: &DbValue,
        pk: &[DbValue],
    ) -> Result<Option<Self>> {
        match resolve_shard_target::<Self>(dal, policy, value)? {
            Some(target) => run_on_shard_target(&target, dal, Self::table(), session, |t, s| {
                if !s.table_exists(t.physical_name())? {
                    return Ok(None);
                }
                t.find_by_pk(s, pk)?
                    .map(|row| Self::from_row(&row))
                    .transpose()
            }),
            None => Self::find(dal, session, pk),
        }
    }

    /// 跨分表条件查询（对应 C# `FindAll(where, ...)` 的分表分支，自动跨表分页）。
    ///
    /// 条件可推导分表区间时逐表查询并合并；否则按单表查询。用法见 [`crate::shards`]。
    fn query_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        query: &Query,
    ) -> Result<Vec<Self>> {
        let table = dal.table(Self::table())?;
        let set = table.query_sharded(session, policy, query)?;
        Self::load(&set)
    }

    /// 跨分表计数（对应 C# `FindCount(where)` 的分表分支：逐表计数求和）。
    fn count_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        filter: Option<&Where>,
    ) -> Result<i64> {
        let table = dal.table(Self::table())?;
        table.count_sharded(session, policy, filter)
    }

    /// 跨分表条件删除（对应 C# 静态 `Delete(Expression)` 的分表分支）。
    fn delete_where_sharded(
        dal: &Dal,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        filter: &Where,
    ) -> Result<u64> {
        let table = dal.table(Self::table())?;
        table.delete_sharded(session, policy, filter)
    }
}

/// 提取实体中分表字段的值（缺失时报错）。
fn shard_value_of<E: Entity>(entity: &E, policy: &TimeShardPolicy) -> Result<DbValue> {
    entity
        .to_fields()
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&policy.field))
        .map(|(_, value)| value)
        .ok_or_else(|| {
            Error::Model(format!(
                "实体 {} 不存在分表字段 {}",
                E::table(),
                policy.field
            ))
        })
}

/// 分表目标（对应 C# `Meta.CreateShard` 的解析结果）：物理表名 + 目标数据访问层。
struct ShardTarget {
    /// 目标连接（`None` = 当前基础连接）
    dal: Option<Arc<Dal>>,
    /// 物理表名
    physical: String,
}

/// 分片批量分组（按 **目标连接 + 物理表** 聚合；对应 C# 按会话对象分组）。
struct BatchGroup<T> {
    /// 目标连接（`None` = 基础连接）
    dal: Option<Arc<Dal>>,
    /// 连接缓存键（基础连接为空串；否则为小写连接名）
    conn: String,
    /// 物理表名
    physical: String,
    /// 是否需要确保分表存在（写操作；删除不建表）
    ensure: bool,
    /// 组内载荷（插入=待写字段行；删除=主键值）
    payload: Vec<T>,
}

/// 解析分表目标（对应 C# `Meta.CreateShard`）：
///
/// - 策略未配置分表模板 → `Ok(None)`（调用方回退基础表）；
/// - 分库（`ConnPolicy`）：目标连接由 [`crate::shards::resolve_shard_dal`] 解析
///   （注册连接 / 未注册时按 C# 规则自动落 SQLite 库），执行时自动切换会话；
/// - 写操作对分表自动建表（对齐 C# `EntitySession.CheckTable`），见 [`ensure_shard_target`]。
fn resolve_shard_target<E: Entity>(
    dal: &Dal,
    policy: &TimeShardPolicy,
    value: &DbValue,
) -> Result<Option<ShardTarget>> {
    let handle = dal.table(E::table())?;
    let base = handle.shard_base();
    let Some(model) = policy.shard_of_value(base, value)? else {
        return Ok(None);
    };
    let target = crate::shards::resolve_shard_dal(&model, base, dal)?;
    let physical = model
        .table_name
        .unwrap_or_else(|| handle.meta().effective_table_name().to_string());
    Ok(Some(ShardTarget {
        dal: target,
        physical,
    }))
}

/// 确保分表在目标连接上存在（`Off` / 只读档不建，对齐 C# `EntitySession.CheckTable`）。
fn ensure_shard_target(target: &ShardTarget, base_dal: &Dal, table: &str) -> Result<()> {
    match &target.dal {
        None => {
            base_dal.ensure_shard_table(table, &target.physical)?;
        }
        Some(d) => {
            d.ensure_shard_table(table, &target.physical)?;
        }
    }
    Ok(())
}

/// 在分表目标上执行单行操作：基础连接沿用调用方会话，跨库连接自动打开独立会话
/// （来自目标连接的会话池，随本次调用归还）。
fn run_on_shard_target<T, F>(
    target: &ShardTarget,
    base_dal: &Dal,
    table: &str,
    session: &mut dyn SqlSession,
    func: F,
) -> Result<T>
where
    F: FnOnce(&TableRef<'_>, &mut dyn SqlSession) -> Result<T>,
{
    match &target.dal {
        None => {
            let handle = base_dal.table_as(table, &target.physical)?;
            func(&handle, session)
        }
        Some(d) => {
            let handle = d.table_as(table, &target.physical)?;
            let mut own = d.open_session()?;
            func(&handle, own.as_mut())
        }
    }
}

/// 新增判定（与单实体 `save` 一致）：自增列 0/缺失 → 新增；无自增列 → 按主键查库判断存在性。
fn is_new_entity<E: Entity>(dal: &Dal, session: &mut dyn SqlSession, entity: &E) -> Result<bool> {
    match E::identity_column() {
        Some(id) => {
            let value = entity
                .to_fields()
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(id))
                .map(|(_, value)| value);
            Ok(!matches!(value.and_then(|v| v.as_i64()), Some(v) if v != 0))
        }
        None => {
            let pk = pk_values_of(&entity.to_fields(), E::primary_keys())?;
            Ok(E::find(dal, session, &pk)?.is_none())
        }
    }
}

/// 主键值是否为空（NULL / 数值 0 / 空串）——"主键全空视为新增"判定用（对齐 C# `IsNullKey` 语义）。
fn is_empty_pk_value(value: &DbValue) -> bool {
    match value {
        DbValue::Null => true,
        DbValue::Text(text) => text.is_empty(),
        other => other.as_i64() == Some(0),
    }
}

/// 从字段列表按主键顺序提取主键值。
fn pk_values_of(fields: &[(&'static str, DbValue)], pks: &[&'static str]) -> Result<Vec<DbValue>> {
    if pks.is_empty() {
        return Err(Error::Model("实体没有主键，无法按主键操作".into()));
    }
    pks.iter()
        .copied()
        .map(|pk| {
            fields
                .iter()
                .find(|pair| pair.0.eq_ignore_ascii_case(pk))
                .map(|pair| pair.1.clone())
                .ok_or_else(|| Error::Model(format!("实体字段缺少主键 {pk}")))
        })
        .collect()
}

/// 审计字段便捷访问（对应 C# 实体基类的审计属性）。
///
/// 写入侧由拦截器负责（[`crate::interceptor`] 的 Time/User/Trace 三件套），
/// 本 trait 提供读取侧的统一定位与类型转换（列名忽略大小写）。
pub trait AuditExt: Entity {
    /// 读取任意列的当前值。
    /// <param name="column">列名（忽略大小写）</param>
    /// <returns>列值；列不存在时为 None</returns>
    fn field_value(&self, column: &str) -> Option<DbValue> {
        self.to_fields()
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(column))
            .map(|(_, value)| value)
    }

    /// 创建时间（`CreateTime`）。
    fn create_time(&self) -> Option<chrono::NaiveDateTime> {
        self.field_value("CreateTime").and_then(|v| v.as_datetime())
    }

    /// 更新时间（`UpdateTime`）。
    fn update_time(&self) -> Option<chrono::NaiveDateTime> {
        self.field_value("UpdateTime").and_then(|v| v.as_datetime())
    }

    /// 创建人（`CreateUser`）。
    fn create_user(&self) -> Option<String> {
        self.field_value("CreateUser").map(|v| v.to_text())
    }

    /// 创建人编号（`CreateUserID`）。
    fn create_user_id(&self) -> Option<i32> {
        self.field_value("CreateUserID").and_then(|v| v.as_i32())
    }

    /// 更新人（`UpdateUser`）。
    fn update_user(&self) -> Option<String> {
        self.field_value("UpdateUser").map(|v| v.to_text())
    }

    /// 更新人编号（`UpdateUserID`）。
    fn update_user_id(&self) -> Option<i32> {
        self.field_value("UpdateUserID").and_then(|v| v.as_i32())
    }

    /// 链路标识（`TraceId`）。
    fn trace_id(&self) -> Option<String> {
        self.field_value("TraceId").map(|v| v.to_text())
    }
}

impl<T: Entity> AuditExt for T {}
