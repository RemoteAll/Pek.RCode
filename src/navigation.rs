//! 实体导航属性（对应 DH.NCode `Model/NavigationProperty`、`NavigationRegistry`、
//! `NavigationLoader`、`NavigationExtensions`）。
//!
//! C# 通过 Fluent API（`Meta.Factory.HasOne/HasMany`）把实体间关系注册到全局注册表，
//! 导航属性加载器据此惰性装载引用（一对一/多对一）与集合（一对多），并供 LINQ `Include` 使用。
//! Rust 侧对象实体是普通结构体，无法像 C# 那样用反射把导航值注入属性，因此对等能力为
//! **注册表 + 显式装载函数**：
//!
//! - [`NavigationRegistry`]：按（源实体, 导航名）登记关系（本地实例或进程级全局表）；
//! - [`Navigation::load_one`] / [`Navigation::load_many`]：给定外键/主键值装载目标实体（可指定
//!   会话以复用连接/事务，对应 C# 的 `NavigationLoader`）；
//! - [`load_one`] / [`load_many`]：按全局注册表登记的名字装载（对应 `HasOne/HasMany` 注册后的使用方式）。
//!
//! 与 C# 的差异（机制边界）：
//!
//! - C# 的 LINQ `Include` 会在查询时生成 JOIN；Rust 无 LINQ，集合装载为“两条查询 + 内存关联”
//!   （等价结果，N+1 需调用方批量优化）；
//! - 外键为 NULL（`DbValue::Null`）时 HasOne 直接返回 `None`，不发起查询（与 C# 空引用语义一致）。
//!
//! 典型用法：
//!
//! ```no_run
//! # use pek_rcode::{dal::Dal, navigation::NavigationRegistry, Entity};
//! # fn demo<U: Entity, O: Entity>(dal: &Dal) -> pek_rcode::Result<()> {
//! let mut nav = NavigationRegistry::new();
//! nav.has_one("Order", "User", "UserId", "Id"); // 订单 → 用户（订单表持有外键 UserId）
//! nav.has_many("User", "Order", "Id", "UserId"); // 用户 → 订单集合
//!
//! let mut session = dal.open_session()?;
//! let order = nav.get("Order", "User").unwrap();
//! let user: Option<U> = order.load_one(dal, session.as_mut(), &7.into())?;
//! let orders: Vec<O> = nav.get("User", "Order").unwrap().load_many(dal, session.as_mut(), &1.into())?;
//! # let _ = (user, orders);
//! # Ok(())
//! # }
//! ```

use std::sync::{Mutex, OnceLock};

use crate::dal::Dal;
use crate::entity::Entity;
use crate::error::Result;
use crate::query::{Query, Where};
use crate::session::SqlSession;
use crate::value::DbValue;

/// 导航关系类型（对应 C# `NavigationType`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavType {
    /// 一对一/多对一引用导航（源持有外键）。
    HasOne,
    /// 一对多集合导航（目标持有外键）。
    HasMany,
}

/// 导航属性元数据（对应 C# `NavigationProperty`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Navigation {
    /// 导航名称（属性名）。
    pub name: String,
    /// 关系类型。
    pub nav_type: NavType,
    /// 源实体（实体名或表名）。
    pub source: String,
    /// 目标实体（实体名或表名）。
    pub target: String,
    /// 外键字段：HasOne 为源实体列，HasMany 为目标实体列。
    pub foreign_key: String,
    /// 主键字段：HasOne 为目标实体列，HasMany 为源实体列。
    pub primary_key: String,
}

impl Navigation {
    /// 装载引用目标（HasOne）：用源实体的外键值查目标主键。
    ///
    /// 外键为 NULL 时返回 `None`（不查询）。
    /// <param name="dal">数据访问层</param>
    /// <param name="session">会话（复用连接/事务）</param>
    /// <param name="foreign_key_value">源实体的外键值</param>
    /// <returns>目标实体</returns>
    pub fn load_one<E: Entity>(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        foreign_key_value: &DbValue,
    ) -> Result<Option<E>> {
        if foreign_key_value.is_null() {
            return Ok(None);
        }
        E::find(dal, session, std::slice::from_ref(foreign_key_value))
    }

    /// 装载集合目标（HasMany）：`WHERE 目标.外键 = 源主键值`。
    /// <param name="dal">数据访问层</param>
    /// <param name="session">会话（复用连接/事务）</param>
    /// <param name="primary_key_value">源实体的主键值</param>
    /// <returns>目标实体集合</returns>
    pub fn load_many<E: Entity>(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        primary_key_value: &DbValue,
    ) -> Result<Vec<E>> {
        if primary_key_value.is_null() {
            return Ok(Vec::new());
        }
        let query = Query::new().filter(
            Where::new().eq(self.foreign_key.clone(), primary_key_value.clone()),
        );
        E::query(dal, session, &query)
    }
}

/// 导航注册表（对应 C# `NavigationRegistry`）。
#[derive(Debug, Default)]
pub struct NavigationRegistry {
    items: Vec<Navigation>,
}

impl NavigationRegistry {
    /// 新建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一对一/多对一引用导航（对应 `HasOne`）。
    ///
    /// `foreign_key` 为**源实体**持有的外键字段，`primary_key` 为目标实体主键（留空自动推导为
    /// 目标实体主键名的占位逻辑由调用方提供，这里与 C# 不同不做反射推导）。
    /// <param name="source">源实体</param>
    /// <param name="target">目标实体</param>
    /// <param name="foreign_key">源实体外键字段</param>
    /// <param name="primary_key">目标实体主键字段</param>
    /// <returns>登记的关系</returns>
    pub fn has_one(
        &mut self,
        source: impl Into<String>,
        target: impl Into<String>,
        foreign_key: impl Into<String>,
        primary_key: impl Into<String>,
    ) -> &Navigation {
        self.push(NavType::HasOne, "", source, target, foreign_key, primary_key)
    }

    /// 登记一对多集合导航（对应 `HasMany`）。
    ///
    /// `primary_key` 为**源实体**主键字段，`foreign_key` 为目标实体持有并关联回源实体的字段。
    /// <param name="source">源实体（“一”方）</param>
    /// <param name="target">目标实体（“多”方）</param>
    /// <param name="primary_key">源实体主键字段</param>
    /// <param name="foreign_key">目标实体外键字段</param>
    /// <returns>登记的关系</returns>
    pub fn has_many(
        &mut self,
        source: impl Into<String>,
        target: impl Into<String>,
        primary_key: impl Into<String>,
        foreign_key: impl Into<String>,
    ) -> &Navigation {
        self.push(NavType::HasMany, "", source, target, foreign_key, primary_key)
    }

    /// 登记并显式指定导航名（缺省 `{Target}`，与 C# 自动推导一致）。
    /// <param name="nav_type">关系类型</param>
    /// <param name="name">导航名（空则按目标实体名推导）</param>
    /// <param name="source">源实体</param>
    /// <param name="target">目标实体</param>
    /// <param name="foreign_key">外键字段</param>
    /// <param name="primary_key">主键字段</param>
    /// <returns>登记的关系</returns>
    pub fn push(
        &mut self,
        nav_type: NavType,
        name: impl Into<String>,
        source: impl Into<String>,
        target: impl Into<String>,
        foreign_key: impl Into<String>,
        primary_key: impl Into<String>,
    ) -> &Navigation {
        let source = source.into();
        let target = target.into();
        let name = name.into();
        let name = if name.is_empty() { target.clone() } else { name };

        // 同（源, 名）覆盖旧登记（与 C# 注册表的覆盖语义一致）
        self.items
            .retain(|n| !(n.source.eq_ignore_ascii_case(&source) && n.name.eq_ignore_ascii_case(&name)));
        self.items.push(Navigation {
            name,
            nav_type,
            source,
            target,
            foreign_key: foreign_key.into(),
            primary_key: primary_key.into(),
        });
        self.items.last().expect("刚推入")
    }

    /// 按（源实体, 导航名）查找。
    /// <param name="source">源实体</param>
    /// <param name="name">导航名</param>
    /// <returns>导航元数据</returns>
    pub fn get(&self, source: &str, name: &str) -> Option<&Navigation> {
        self.items.iter().find(|n| {
            n.source.eq_ignore_ascii_case(source) && n.name.eq_ignore_ascii_case(name)
        })
    }

    /// 某源实体的全部导航。
    /// <param name="source">源实体</param>
    /// <returns>导航列表</returns>
    pub fn by_source(&self, source: &str) -> Vec<&Navigation> {
        self.items
            .iter()
            .filter(|n| n.source.eq_ignore_ascii_case(source))
            .collect()
    }

    /// 全部导航。
    pub fn all(&self) -> &[Navigation] {
        &self.items
    }

    /// 登记项数。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// 进程级全局注册表（对应 C# `NavigationRegistry` 的静态注册）。
static GLOBAL: OnceLock<Mutex<NavigationRegistry>> = OnceLock::new();

/// 全局注册表（只读访问请用 [`with_registry`] / [`with_registry_mut`]）。
pub fn registry() -> &'static Mutex<NavigationRegistry> {
    GLOBAL.get_or_init(|| Mutex::new(NavigationRegistry::new()))
}

/// 只读访问全局注册表。
/// <param name="f">闭包</param>
/// <returns>闭包结果</returns>
pub fn with_registry<T>(f: impl FnOnce(&NavigationRegistry) -> T) -> T {
    let guard = registry().lock().expect("导航注册表锁被毒化");
    f(&guard)
}

/// 可变访问全局注册表（登记关系）。
/// <param name="f">闭包</param>
/// <returns>闭包结果</returns>
pub fn with_registry_mut<T>(f: impl FnOnce(&mut NavigationRegistry) -> T) -> T {
    let mut guard = registry().lock().expect("导航注册表锁被毒化");
    f(&mut guard)
}

/// 按全局注册表装载引用目标（HasOne）。
/// <param name="dal">数据访问层</param>
/// <param name="session">会话</param>
/// <param name="source">源实体名</param>
/// <param name="name">导航名</param>
/// <param name="foreign_key_value">源实体的外键值</param>
/// <returns>目标实体</returns>
pub fn load_one<E: Entity>(
    dal: &Dal,
    session: &mut dyn SqlSession,
    source: &str,
    name: &str,
    foreign_key_value: &DbValue,
) -> Result<Option<E>> {
    let nav = with_registry(|r| r.get(source, name).cloned());
    let nav = nav.ok_or_else(|| {
        crate::error::Error::Model(format!("导航关系未注册：{source}.{name}"))
    })?;
    nav.load_one(dal, session, foreign_key_value)
}

/// 按全局注册表装载集合目标（HasMany）。
/// <param name="dal">数据访问层</param>
/// <param name="session">会话</param>
/// <param name="source">源实体名</param>
/// <param name="name">导航名</param>
/// <param name="primary_key_value">源实体的主键值</param>
/// <returns>目标实体集合</returns>
pub fn load_many<E: Entity>(
    dal: &Dal,
    session: &mut dyn SqlSession,
    source: &str,
    name: &str,
    primary_key_value: &DbValue,
) -> Result<Vec<E>> {
    let nav = with_registry(|r| r.get(source, name).cloned());
    let nav = nav.ok_or_else(|| {
        crate::error::Error::Model(format!("导航关系未注册：{source}.{name}"))
    })?;
    nav.load_many(dal, session, primary_key_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityModel;
    use crate::session::DbRow;

    const MODEL: &str = r#"<EntityModel><Tables>
      <Table Name="User" TableName="DH_User">
        <Columns>
          <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
          <Column Name="Name" DataType="String" Length="50" Nullable="True" />
        </Columns>
      </Table>
      <Table Name="Order" TableName="DH_Order">
        <Columns>
          <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
          <Column Name="UserId" DataType="Int32" Nullable="True" />
          <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" Nullable="True" />
        </Columns>
      </Table>
    </Tables></EntityModel>"#;

    /// 用户实体（对应表 DH_User）。
    #[derive(Debug, Clone, PartialEq)]
    struct User {
        id: i32,
        name: Option<String>,
    }

    impl Entity for User {
        fn table() -> &'static str {
            "DH_User"
        }
        fn columns() -> &'static [&'static str] {
            &["Id", "Name"]
        }
        fn primary_keys() -> &'static [&'static str] {
            &["Id"]
        }
        fn identity_column() -> Option<&'static str> {
            Some("Id")
        }
        fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
            vec![
                ("Id", DbValue::Int(self.id as i64)),
                (
                    "Name",
                    self.name.clone().map(DbValue::Text).unwrap_or(DbValue::Null),
                ),
            ]
        }
        fn from_row(row: &DbRow) -> crate::error::Result<Self> {
            Ok(Self {
                id: row.get_by_name("Id").and_then(DbValue::as_i64).unwrap_or(0) as i32,
                name: row
                    .get_by_name("Name")
                    .filter(|v| !v.is_null())
                    .map(DbValue::to_text),
            })
        }
        fn set_identity(&mut self, value: i64) -> crate::error::Result<()> {
            self.id = value as i32;
            Ok(())
        }
    }

    /// 订单实体（对应表 DH_Order）。
    #[derive(Debug, Clone, PartialEq)]
    struct Order {
        id: i32,
        user_id: Option<i32>,
    }

    impl Entity for Order {
        fn table() -> &'static str {
            "DH_Order"
        }
        fn columns() -> &'static [&'static str] {
            &["Id", "UserId", "Amount"]
        }
        fn primary_keys() -> &'static [&'static str] {
            &["Id"]
        }
        fn identity_column() -> Option<&'static str> {
            Some("Id")
        }
        fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
            vec![
                ("Id", DbValue::Int(self.id as i64)),
                (
                    "UserId",
                    self.user_id.map(|v| DbValue::Int(v as i64)).unwrap_or(DbValue::Null),
                ),
                ("Amount", DbValue::Null),
            ]
        }
        fn from_row(row: &DbRow) -> crate::error::Result<Self> {
            Ok(Self {
                id: row.get_by_name("Id").and_then(DbValue::as_i64).unwrap_or(0) as i32,
                user_id: row
                    .get_by_name("UserId")
                    .and_then(DbValue::as_i64)
                    .map(|v| v as i32),
            })
        }
        fn set_identity(&mut self, value: i64) -> crate::error::Result<()> {
            self.id = value as i32;
            Ok(())
        }
    }

    fn temp_dal(tag: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-nav-{}-{}-{tag}",
            std::process::id(),
            stamp
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        (dal, dir)
    }

    fn seed(dal: &Dal) {
        let mut session = dal.open_session().unwrap();
        for name in ["Alice", "Bob"] {
            let mut user = User {
                id: 0,
                name: Some(name.to_string()),
            };
            user.insert(dal, session.as_mut()).unwrap();
        }
        for (user_id, _amount) in [(1, "10.5"), (1, "20.25"), (2, "3.0")] {
            let mut order = Order {
                id: 0,
                user_id: Some(user_id),
            };
            order.insert(dal, session.as_mut()).unwrap();
        }
    }

    #[test]
    fn registry_registers_and_lookup() {
        let mut nav = NavigationRegistry::new();
        nav.has_one("Order", "User", "UserId", "Id");
        nav.has_many("User", "Order", "Id", "UserId");
        assert_eq!(nav.len(), 2);

        let one = nav.get("order", "user").unwrap();
        assert_eq!(one.nav_type, NavType::HasOne);
        assert_eq!(one.foreign_key, "UserId");
        assert_eq!(one.primary_key, "Id");

        let many = nav.get("User", "Order").unwrap();
        assert_eq!(many.nav_type, NavType::HasMany);
        assert_eq!(many.foreign_key, "UserId");

        assert_eq!(nav.by_source("User").len(), 1);
        assert!(nav.get("Order", "Nope").is_none());

        // 覆盖注册
        nav.has_one("Order", "User", "UserId", "Id");
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn load_navigation_over_sqlite() {
        let (dal, dir) = temp_dal("load");
        seed(&dal);

        let mut nav = NavigationRegistry::new();
        nav.has_one("Order", "User", "UserId", "Id");
        nav.has_many("User", "Order", "Id", "UserId");

        let mut session = dal.open_session().unwrap();

        // HasOne：订单 2 的用户是 Alice（UserId=1）
        let one = nav.get("Order", "User").unwrap();
        let user: Option<User> = one
            .load_one(&dal, session.as_mut(), &DbValue::Int(1))
            .unwrap();
        let user = user.expect("应装载到用户");
        assert_eq!(user.name.as_deref(), Some("Alice"));

        // 外键为 NULL：不查询直接 None
        assert!(one.load_one::<User>(&dal, session.as_mut(), &DbValue::Null).unwrap().is_none());

        // HasMany：用户 1 有 2 张订单
        let many = nav.get("User", "Order").unwrap();
        let orders: Vec<Order> = many
            .load_many(&dal, session.as_mut(), &DbValue::Int(1))
            .unwrap();
        assert_eq!(orders.len(), 2);
        assert!(orders.iter().all(|o| o.user_id == Some(1)));

        // 主键为 NULL：空集合
        assert!(many.load_many::<Order>(&dal, session.as_mut(), &DbValue::Null).unwrap().is_empty());

        // 全局注册表路径
        with_registry_mut(|r| {
            r.has_one("Order", "User", "UserId", "Id");
            r.has_many("User", "Order", "Id", "UserId");
        });
        let alice: Option<User> =
            load_one(&dal, session.as_mut(), "Order", "User", &DbValue::Int(2)).unwrap();
        assert_eq!(alice.unwrap().name.as_deref(), Some("Bob"));
        let bob_orders: Vec<Order> =
            load_many(&dal, session.as_mut(), "User", "Order", &DbValue::Int(2)).unwrap();
        assert_eq!(bob_orders.len(), 1);
        assert!(
            load_one::<User>(&dal, session.as_mut(), "Order", "Nope", &DbValue::Int(1)).is_err()
        );

        // DataRowEntityAccessor 等价：行集 → 实体集合
        let set = session
            .query("SELECT Id, Name FROM DH_User ORDER BY Id", &[])
            .unwrap();
        let users = User::load(&set).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].name.as_deref(), Some("Alice"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
