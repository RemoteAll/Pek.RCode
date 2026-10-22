//! 数据校验链（`Valid`）端到端测试。
//!
//! 对齐 DH.NCode：`Entity::validate` 是写入前钩子——`insert` / `update` / `insert_sharded`
//! 在产生任何 SQL **之前**调用，失败返回 `Error::Validation`。
//! `rcodegen` 为「非空字符串列（无默认值、非审计列）」生成必填校验。

use std::path::PathBuf;

use pek_rcode::dirty::DirtyState;
use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Error, Result};

/// 手写实体（对应生成代码的 `validate` 覆写）。
#[derive(Debug, Clone, Default)]
struct Customer {
    id: i32,
    name: String,
    mobile: Option<String>,
    dirty: DirtyState,
}

impl Customer {
    const TABLE_NAME: &'static str = "DH_Customer";

    fn set_name(&mut self, value: &str) {
        self.name = value.to_string();
        self.dirty.mark_static("Name");
    }
}

impl Entity for Customer {
    fn table() -> &'static str {
        Self::TABLE_NAME
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "Name", "Mobile"]
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
            ("Name", self.name.clone().into()),
            ("Mobile", self.mobile.clone().into()),
        ]
    }

    fn from_row(row: &DbRow) -> Result<Self> {
        Ok(Self {
            id: row
                .get_by_name("Id")
                .and_then(DbValue::as_i32)
                .unwrap_or_default(),
            name: row
                .get_by_name("Name")
                .map(DbValue::to_text)
                .unwrap_or_default(),
            mobile: row
                .get_by_name("Mobile")
                .and_then(|v| (!v.is_null()).then(|| v.to_text())),
            dirty: DirtyState::new(),
        })
    }

    fn dirty_state(&self) -> Option<&DirtyState> {
        Some(&self.dirty)
    }

    fn set_identity(&mut self, value: i64) -> Result<()> {
        self.id = value as i32;
        Ok(())
    }

    /// 对齐生成代码：必填列不能为空串。
    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::Validation("名称 不能为空".to_string()));
        }
        Ok(())
    }
}

const MODEL: &str = r#"<EntityModel><Tables><Table Name="Customer" TableName="DH_Customer" Description="客户">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
    <Column Name="Name" DataType="String" Length="50" Description="名称" />
    <Column Name="Mobile" DataType="String" Length="20" Nullable="True" Description="手机" />
  </Columns>
</Table></Tables></EntityModel>"#;

fn temp_dir(name: &str) -> PathBuf {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!("rcode-val-{}-{}-{name}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(name: &str) -> (PathBuf, Dal) {
    let dir = temp_dir(name);
    let db = dir.join("test.db");
    let conn = format!("Data Source={};Provider=SQLite", db.display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();
    (dir, dal)
}

/// 校验失败 → `Error::Validation`，且**不产生任何 SQL**（表仍是空的）。
#[test]
fn insert_rejects_empty_required_column() {
    let (dir, dal) = setup("insert-empty");
    let mut session = dal.open_session().unwrap();

    let mut customer = Customer::default();
    customer.set_name("   ");
    let err = customer.insert(&dal, session.as_mut()).unwrap_err();
    assert!(
        matches!(&err, Error::Validation(msg) if msg.contains("不能为空")),
        "应返回校验错误，实际：{err}"
    );
    assert_eq!(
        Customer::count(&dal, session.as_mut(), None).unwrap(),
        0,
        "校验失败不应插入任何数据"
    );

    // 补齐必填列 → 正常插入
    customer.set_name("张三");
    let id = customer.insert(&dal, session.as_mut()).unwrap();
    assert!(id > 0);
    assert_eq!(Customer::count(&dal, session.as_mut(), None).unwrap(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 更新路径同样校验（脏数据为空串时拒绝），且原数据保持不变。
#[test]
fn update_rejects_empty_required_column() {
    let (dir, dal) = setup("update-empty");
    let mut session = dal.open_session().unwrap();

    let mut customer = Customer::default();
    customer.set_name("李四");
    let id = customer.insert(&dal, session.as_mut()).unwrap();

    customer.set_name("");
    let err = customer.update(&dal, session.as_mut()).unwrap_err();
    assert!(matches!(err, Error::Validation(_)), "应返回校验错误");

    let reloaded = Customer::find(&dal, session.as_mut(), &[id.into()])
        .unwrap()
        .expect("原数据应仍在");
    assert_eq!(reloaded.name, "李四", "校验失败不应改动数据");

    // save 走 update 分支，同样被拦截
    let err = customer.save(&dal, session.as_mut()).unwrap_err();
    assert!(matches!(err, Error::Validation(_)), "save 也应校验");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 无脏数据时跳过校验（对齐 C# `if (!HasDirty) return true`）；
/// 手写实体无脏追踪时始终校验。
#[test]
fn validation_skipped_when_no_dirty() {
    let (dir, dal) = setup("skip-clean");
    let mut session = dal.open_session().unwrap();

    // 有脏追踪：insert 一个合法对象
    let mut customer = Customer::default();
    customer.set_name("王五");
    let id = customer.insert(&dal, session.as_mut()).unwrap();

    // 找到对象（无脏数据）→ update 不做校验，能正常执行
    let mut clean = Customer::find(&dal, session.as_mut(), &[id.into()])
        .unwrap()
        .expect("应能查到");
    assert!(!clean.has_dirty(), "刚读出的实体应无脏数据");
    assert!(!clean.should_validate() || clean.validate().is_ok());
    clean.mobile = Some("13800000000".into());
    clean.dirty.mark_static("Mobile");
    assert_eq!(clean.update(&dal, session.as_mut()).unwrap(), 1);

    // 无脏追踪的实体（dirty_state = None）→ 始终校验
    struct Stateless(Customer);
    impl Entity for Stateless {
        fn table() -> &'static str {
            Customer::TABLE_NAME
        }
        fn columns() -> &'static [&'static str] {
            Customer::columns()
        }
        fn primary_keys() -> &'static [&'static str] {
            Customer::primary_keys()
        }
        fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
            self.0.to_fields()
        }
        fn from_row(row: &DbRow) -> Result<Self> {
            Ok(Stateless(Customer::from_row(row)?))
        }
        fn validate(&self) -> Result<()> {
            self.0.validate()
        }
    }
    let stateless = Stateless(Customer::default());
    assert!(stateless.should_validate(), "无脏追踪的实体应始终校验");
    assert!(matches!(stateless.validate(), Err(Error::Validation(_))));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `save_without_valid` 跳过校验（对齐 C# `SaveWithoutValid()`），用于历史数据导入。
#[test]
fn save_without_valid_skips_validation() {
    let (dir, dal) = setup("save-no-valid");
    let mut session = dal.open_session().unwrap();

    let mut customer = Customer::default();
    customer.set_name(""); // 非法数据：正常 save 会被拦截
    assert!(matches!(
        customer.save(&dal, session.as_mut()),
        Err(Error::Validation(_))
    ));

    assert_eq!(
        customer.save_without_valid(&dal, session.as_mut()).unwrap(),
        1,
        "跳过校验后应能正常写入"
    );
    assert!(customer.id > 0);
    let reloaded = Customer::find(&dal, session.as_mut(), &[customer.id.into()])
        .unwrap()
        .expect("应能查到");
    assert_eq!(reloaded.name, "", "空串按原样入库");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 删除不做校验（对齐 C# `DataMethod.Delete` 分支：不校验，InsertOnly 另有拦截）。
#[test]
fn delete_does_not_validate() {
    let (dir, dal) = setup("delete");
    let mut session = dal.open_session().unwrap();

    let mut customer = Customer::default();
    customer.set_name("赵六");
    let id = customer.insert(&dal, session.as_mut()).unwrap();
    assert!(id > 0);

    // 即使必填列为空，删除也不校验
    let mut victim = customer.clone();
    victim.set_name("");
    assert_eq!(victim.delete(&dal, session.as_mut()).unwrap(), 1);
    assert_eq!(Customer::count(&dal, session.as_mut(), None).unwrap(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}
