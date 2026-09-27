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

use crate::{
    dal::Dal,
    error::{Error, Result},
    query::{Query, Where},
    session::{DbRow, SqlSession},
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

    /// 插入一行（自动跳过自增列并回写主键），返回自增主键（无自增时 0）。
    fn insert(&mut self, dal: &Dal, session: &mut dyn SqlSession) -> Result<i64> {
        let table = dal.table(Self::table())?;
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

    /// 按主键更新（主键值与自增列不参与 SET），返回受影响行数。
    fn update(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<u64> {
        let table = dal.table(Self::table())?;
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
                Self::find(dal, session, &pk)?.is_none()
            }
        };

        if is_new {
            self.insert(dal, session)?;
            Ok(1)
        } else {
            self.update(dal, session)
        }
    }

    /// 按主键删除本对象对应的行，返回受影响行数。
    fn delete(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<u64> {
        let table = dal.table(Self::table())?;
        let pk = pk_values_of(&self.to_fields(), Self::primary_keys())?;
        table.delete_by_pk(session, &pk)
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
