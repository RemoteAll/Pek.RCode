//! 对象实体层端到端测试。
//!
//! `Order` 是按 [`pek_rcode::codegen`] 生成模式手工编写的实体（保证生成代码用到的
//! trait 方法与表达式都能编译、能工作），随后走完整对象化流程：
//! new → insert（自增回写）→ find → save（更新/新增）→ query/count → delete。

use std::path::PathBuf;

use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Query, Result, Where};

/// 与生成代码同构的实体
#[derive(Debug, Clone, PartialEq)]
struct Order {
    id: i32,
    code: Option<String>,
    amount: rust_decimal::Decimal,
    ok: bool,
    create_time: chrono::NaiveDateTime,
    data: Option<Vec<u8>>,
}

impl Order {
    /// 数据库表名。
    pub const TABLE_NAME: &'static str = "DH_Order";

    /// 按列类型默认值创建新实体。
    pub fn new() -> Self {
        Self {
            id: 0,
            code: None,
            amount: Default::default(),
            ok: false,
            create_time: chrono::DateTime::UNIX_EPOCH.naive_utc(),
            data: None,
        }
    }
}

impl Default for Order {
    fn default() -> Self {
        Self::new()
    }
}

impl Entity for Order {
    fn table() -> &'static str {
        Self::TABLE_NAME
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "Code", "Amount", "Ok", "CreateTime", "Data"]
    }

    fn primary_keys() -> &'static [&'static str] {
        &["Id"]
    }

    fn identity_column() -> Option<&'static str> {
        Some("Id")
    }

    fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
        vec![
            ("Id", self.id.into()),
            ("Code", self.code.clone().into()),
            ("Amount", self.amount.into()),
            ("Ok", self.ok.into()),
            ("CreateTime", self.create_time.into()),
            ("Data", self.data.clone().into()),
        ]
    }

    fn from_row(row: &DbRow) -> Result<Self> {
        Ok(Self {
            id: row.get_by_name("Id").and_then(DbValue::as_i32).unwrap_or_default(),
            code: row.get_by_name("Code").and_then(|v| (!v.is_null()).then(|| v.to_text())),
            amount: row.get_by_name("Amount").and_then(DbValue::as_decimal).unwrap_or_default(),
            ok: row.get_by_name("Ok").and_then(DbValue::as_bool).unwrap_or_default(),
            create_time: row
                .get_by_name("CreateTime")
                .and_then(DbValue::as_datetime)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc()),
            data: row.get_by_name("Data").and_then(|v| v.as_blob().map(<[u8]>::to_vec)),
        })
    }

    fn set_identity(&mut self, value: i64) -> Result<()> {
        self.id = value as i32;
        Ok(())
    }
}

const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order" Description="订单">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
    <Column Name="Code" DataType="String" Length="50" Nullable="True" Description="单号" />
    <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" Description="金额" />
    <Column Name="Ok" DataType="Boolean" Description="是否有效" />
    <Column Name="CreateTime" DataType="DateTime" Description="创建时间" />
    <Column Name="Data" DataType="Binary" Nullable="True" Description="附件" />
  </Columns>
</Table></Tables></EntityModel>"#;

/// 临时目录（每次唯一，避免并行冲突）
fn temp_dir(name: &str) -> PathBuf {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!("rcode-ent-{}-{}-{name}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 准备一个绑定临时库的 Dal。
fn setup(name: &str) -> (PathBuf, Dal) {
    let dir = temp_dir(name);
    let db = dir.join("test.db");
    let conn = format!("Data Source={};Provider=SQLite", db.display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();

    // 生成的表应包含全部列
    let mut session = dal.open_session().unwrap();
    let columns = session.table_columns("DH_Order").unwrap();
    assert_eq!(columns, vec!["Id", "Code", "Amount", "Ok", "CreateTime", "Data"]);

    (dir, dal)
}

#[test]
fn object_crud_roundtrip() {
    let (dir, dal) = setup("crud");
    let mut session = dal.open_session().unwrap();

    // new → insert：自增主键回写对象
    let mut order = Order::new();
    order.code = Some("HLT-1".into());
    order.amount = "12.34".parse().unwrap();
    order.ok = true;
    order.create_time = chrono::NaiveDate::from_ymd_opt(2026, 9, 26)
        .unwrap()
        .and_hms_opt(8, 0, 0)
        .unwrap();
    order.data = Some(vec![1, 2, 3]);

    let id = order.insert(&dal, session.as_mut()).unwrap();
    assert_eq!(id, 1);
    assert_eq!(order.id, 1, "插入后主键应自动回写到实体");

    // find：全字段一致（含 Decimal 精度、时间、二进制、布尔）
    let found = Order::find(&dal, session.as_mut(), &[order.id.into()])
        .unwrap()
        .expect("应能按主键查到");
    assert_eq!(found, order);

    // save（Id != 0）→ 更新
    let mut edit = found.clone();
    edit.code = Some("HLT-2".into());
    assert_eq!(edit.save(&dal, session.as_mut()).unwrap(), 1);
    let reloaded = Order::find(&dal, session.as_mut(), &[1.into()]).unwrap().unwrap();
    assert_eq!(reloaded.code.as_deref(), Some("HLT-2"));

    // save（新对象 Id = 0）→ 插入
    let mut second = Order::new();
    second.code = Some("HLT-3".into());
    assert_eq!(second.save(&dal, session.as_mut()).unwrap(), 1);
    assert_eq!(second.id, 2, "save 新对象应回写主键");

    // query / all / count
    let filter = Where::new().like("Code", "HLT%");
    let list = Order::query(
        &dal,
        session.as_mut(),
        &Query::new().filter(filter.clone()).order_by("Id", false),
    )
    .unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].code.as_deref(), Some("HLT-2"));
    assert_eq!(Order::all(&dal, session.as_mut()).unwrap().len(), 2);
    assert_eq!(Order::count(&dal, session.as_mut(), Some(&filter)).unwrap(), 2);

    // delete
    assert_eq!(reloaded.delete(&dal, session.as_mut()).unwrap(), 1);
    assert!(Order::find(&dal, session.as_mut(), &[1.into()]).unwrap().is_none());
    assert_eq!(Order::count(&dal, session.as_mut(), None).unwrap(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn null_and_default_values() {
    let (dir, dal) = setup("nulls");
    let mut session = dal.open_session().unwrap();

    // 全默认值对象：可空列写入 NULL 并能读回
    let mut order = Order::new();
    order.insert(&dal, session.as_mut()).unwrap();

    let found = Order::find(&dal, session.as_mut(), &[order.id.into()]).unwrap().unwrap();
    assert_eq!(found.code, None);
    assert_eq!(found.data, None);
    assert_eq!(found.amount.to_string(), "0");
    assert!(!found.ok);
    assert_eq!(found.create_time, chrono::DateTime::UNIX_EPOCH.naive_utc());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_skips_pk_and_identity() {
    let (dir, dal) = setup("update");
    let mut session = dal.open_session().unwrap();

    let mut order = Order::new();
    order.insert(&dal, session.as_mut()).unwrap();

    // 篡改内存中的主键值：update 不应把它写回数据库（WHERE 用主键，SET 不含主键）
    order.id = 999;
    order.code = Some("X".into());
    let affected = order.save(&dal, session.as_mut()).unwrap();
    assert_eq!(affected, 0, "主键被改动后更新不到任何行（符合预期，避免误写）");

    let _ = std::fs::remove_dir_all(&dir);
}
