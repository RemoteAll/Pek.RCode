//! 条件更新（CAS / 乐观锁）端到端测试。
//!
//! 覆盖 `TableRef::update_if` 与 `Entity::update_if_unchanged`：
//! 多应用共享同一张表时，用「读取时的旧值」作为 `WHERE` 条件，
//! 避免把旧值覆盖到别的系统刚写入的数据上（见 `docs/使用规范.md` §4.5）。

use std::path::PathBuf;

use pek_rcode::dirty::DirtyState;
use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Error, Result};

/// 手写实体（带脏追踪，等价于 `rcodegen` 生成的实体）。
#[derive(Debug, Clone, Default)]
struct Account {
    id: i32,
    name: String,
    balance: i32,
    version: i32,
    dirty: DirtyState,
}

impl Account {
    fn set_name(&mut self, value: &str) {
        if self.name == value {
            return;
        }
        self.name = value.to_string();
        self.dirty.mark_static("Name");
    }

    fn set_balance(&mut self, value: i32) {
        if self.balance == value {
            return;
        }
        self.balance = value;
        self.dirty.mark_static("Balance");
    }

    fn set_version(&mut self, value: i32) {
        if self.version == value {
            return;
        }
        self.version = value;
        self.dirty.mark_static("Version");
    }
}

impl Entity for Account {
    fn table() -> &'static str {
        "Account"
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "Name", "Balance", "Version"]
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
            ("Balance", self.balance.into()),
            ("Version", self.version.into()),
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
            balance: row
                .get_by_name("Balance")
                .and_then(DbValue::as_i32)
                .unwrap_or_default(),
            version: row
                .get_by_name("Version")
                .and_then(DbValue::as_i32)
                .unwrap_or_default(),
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
}

/// `MaxCacheCount="0"`：避免 `find` 走单对象缓存，干扰"读到旧值"的测试语义。
const MODEL: &str = r#"<EntityModel><Tables><Table Name="Account" TableName="DH_Account" MaxCacheCount="0">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="Name" DataType="String" Length="50" />
    <Column Name="Balance" DataType="Int32" />
    <Column Name="Version" DataType="Int32" />
  </Columns>
</Table></Tables></EntityModel>"#;

fn temp_dir(name: &str) -> PathBuf {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir =
        std::env::temp_dir().join(format!("rcode-cas-{}-{}-{name}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 返回 `(目录, 主 Dal, 连接串)`；连接串用于开第二个 Dal 模拟"另一个应用"。
fn setup(name: &str) -> (PathBuf, Dal, String) {
    let dir = temp_dir(name);
    let db = dir.join("test.db");
    let conn = format!("Data Source={};Provider=SQLite", db.display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();
    (dir, dal, conn)
}

/// 另一个连接（另一个应用）直接改库：不经过本 Dal，**不会**触发本进程缓存失效。
fn other_app(conn: &str) -> Dal {
    Dal::open_with_model(conn, EntityModel::parse(MODEL).unwrap()).unwrap()
}

fn find_row(dal: &Dal, session: &mut dyn pek_rcode::SqlSession, id: i64) -> DbRow {
    dal.table("Account")
        .unwrap()
        .find_by_pk(session, &[DbValue::Int(id)])
        .unwrap()
        .expect("行应存在")
}

#[test]
fn insert_and_read_back() {
    let (dir, dal, _conn) = setup("basic");
    let mut session = dal.open_session().unwrap();
    let mut account = Account {
        name: "A".into(),
        balance: 100,
        version: 1,
        ..Default::default()
    };
    let id = account.insert(&dal, session.as_mut()).unwrap();
    assert!(id > 0);

    let row = find_row(&dal, session.as_mut(), id);
    assert_eq!(row.get_by_name("Name").and_then(DbValue::as_str), Some("A"));
    assert_eq!(
        row.get_by_name("Balance").and_then(DbValue::as_i32),
        Some(100)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 期望值匹配 → 成功，未参与 `SET` 的列保持原值。
#[test]
fn table_update_if_succeeds_when_expected_matches() {
    let (dir, dal, _conn) = setup("ok");
    let mut session = dal.open_session().unwrap();
    let table = dal.table("Account").unwrap();

    let mut account = Account {
        name: "A".into(),
        balance: 100,
        version: 1,
        ..Default::default()
    };
    let id = account.insert(&dal, session.as_mut()).unwrap();

    let affected = table
        .update_if(
            session.as_mut(),
            &[("Name", "B".into())],
            &[DbValue::Int(id)],
            &[("Version", 1.into())],
        )
        .unwrap();
    assert_eq!(affected, 1, "期望值未变时应更新成功");

    let row = find_row(&dal, session.as_mut(), id);
    assert_eq!(row.get_by_name("Name").and_then(DbValue::as_str), Some("B"));
    assert_eq!(
        row.get_by_name("Balance").and_then(DbValue::as_i32),
        Some(100),
        "未参与 SET 的列不应被改动"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 期望值已被另一个应用改掉 → 0 行、不覆盖别人的数据。
#[test]
fn table_update_if_conflict_keeps_other_write() {
    let (dir, dal, conn) = setup("conflict");
    let mut session = dal.open_session().unwrap();
    let table = dal.table("Account").unwrap();

    let mut account = Account {
        name: "A".into(),
        balance: 100,
        version: 1,
        ..Default::default()
    };
    let id = account.insert(&dal, session.as_mut()).unwrap();

    // 另一个应用把 Balance 改成 200
    let other = other_app(&conn);
    let mut other_session = other.open_session().unwrap();
    other
        .table("Account")
        .unwrap()
        .update_by_pk(
            other_session.as_mut(),
            &[("Balance", 200.into())],
            &[DbValue::Int(id)],
        )
        .unwrap();

    // 本进程拿着旧值（Balance=100）做 CAS → 冲突
    let affected = table
        .update_if(
            session.as_mut(),
            &[("Name", "B".into())],
            &[DbValue::Int(id)],
            &[("Balance", 100.into())],
        )
        .unwrap();
    assert_eq!(affected, 0, "期望值已变应返回 0");

    let row = find_row(&dal, session.as_mut(), id);
    assert_eq!(
        row.get_by_name("Name").and_then(DbValue::as_str),
        Some("A"),
        "冲突时不应写入"
    );
    assert_eq!(
        row.get_by_name("Balance").and_then(DbValue::as_i32),
        Some(200),
        "另一个应用的写入必须保留"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 实体级：冲突返回 `Error::Conflict` 且**保留脏数据**，重读合并后可再次提交。
#[test]
fn entity_update_if_unchanged_keeps_dirty_on_conflict() {
    let (dir, dal, conn) = setup("entity-conflict");
    let mut session = dal.open_session().unwrap();

    let mut account = Account {
        name: "A".into(),
        balance: 100,
        version: 1,
        ..Default::default()
    };
    let id = account.insert(&dal, session.as_mut()).unwrap();

    let mut loaded = Account::find(&dal, session.as_mut(), &[DbValue::Int(id)])
        .unwrap()
        .expect("应能查到");
    loaded.set_name("B");

    // 另一个应用抢先把 Balance 改掉
    let other = other_app(&conn);
    let mut other_session = other.open_session().unwrap();
    other
        .table("Account")
        .unwrap()
        .update_by_pk(
            other_session.as_mut(),
            &[("Balance", 200.into())],
            &[DbValue::Int(id)],
        )
        .unwrap();

    let err = loaded
        .update_if_unchanged(&dal, session.as_mut(), &[("Balance", 100.into())])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "应返回并发冲突：{err}");
    assert!(
        loaded.is_dirty("Name"),
        "冲突后脏数据应保留，便于重读合并后再次提交"
    );

    // 库内仍是旧值（没有写入）
    let row = find_row(&dal, session.as_mut(), id);
    assert_eq!(row.get_by_name("Name").and_then(DbValue::as_str), Some("A"));

    // 用正确的旧值重试 → 成功，且只写 Name
    let affected = loaded
        .update_if_unchanged(&dal, session.as_mut(), &[("Balance", 200.into())])
        .unwrap();
    assert_eq!(affected, 1);
    assert!(!loaded.is_dirty("Name"), "成功后脏数据应清空");

    let row = find_row(&dal, session.as_mut(), id);
    assert_eq!(row.get_by_name("Name").and_then(DbValue::as_str), Some("B"));
    assert_eq!(
        row.get_by_name("Balance").and_then(DbValue::as_i32),
        Some(200),
        "未改动的列保持另一个应用写入的值"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 无脏数据 → 返回 0、不发 SQL（也不报冲突）。
#[test]
fn entity_update_if_unchanged_without_dirty_is_noop() {
    let (dir, dal, _conn) = setup("noop");
    let mut session = dal.open_session().unwrap();

    let mut account = Account {
        name: "A".into(),
        balance: 100,
        version: 1,
        ..Default::default()
    };
    let id = account.insert(&dal, session.as_mut()).unwrap();

    let clean = Account::find(&dal, session.as_mut(), &[DbValue::Int(id)])
        .unwrap()
        .expect("应能查到");
    assert!(!clean.has_dirty());
    let affected = clean
        .update_if_unchanged(&dal, session.as_mut(), &[("Balance", 100.into())])
        .unwrap();
    assert_eq!(affected, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 冲突（0 行）后本进程缓存必须失效：否则重读会一直命中旧值、永远冲突。
#[test]
fn conflict_invalidates_entity_cache() {
    let (dir, dal, conn) = setup("cache");
    let mut session = dal.open_session().unwrap();
    let table = dal.table("Account").unwrap();

    let mut account = Account {
        name: "A".into(),
        balance: 100,
        version: 1,
        ..Default::default()
    };
    let id = account.insert(&dal, session.as_mut()).unwrap();

    // 显式加载整表缓存
    let cache = dal.entity_cache("Account").unwrap();
    let rows = cache.entities(&dal, session.as_mut()).unwrap();
    assert!(
        rows.iter()
            .any(|row| { row.get_by_name("Balance").and_then(DbValue::as_i32) == Some(100) })
    );

    // 另一个应用改库：本进程缓存不会自动失效
    let other = other_app(&conn);
    let mut other_session = other.open_session().unwrap();
    other
        .table("Account")
        .unwrap()
        .update_by_pk(
            other_session.as_mut(),
            &[("Balance", 200.into())],
            &[DbValue::Int(id)],
        )
        .unwrap();

    // 用旧值做 CAS → 冲突
    let affected = table
        .update_if(
            session.as_mut(),
            &[("Name", "B".into())],
            &[DbValue::Int(id)],
            &[("Balance", 100.into())],
        )
        .unwrap();
    assert_eq!(affected, 0);

    // 冲突后缓存已失效 → 重新加载看到另一个应用写入的值
    let rows = cache.entities(&dal, session.as_mut()).unwrap();
    assert_eq!(
        rows.iter()
            .find(|row| row.get_by_name("Id").and_then(DbValue::as_i32) == Some(id as i32))
            .and_then(|row| row.get_by_name("Balance").and_then(DbValue::as_i32)),
        Some(200),
        "冲突后应使缓存失效，重读拿到最新值"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 期望列拼错 → 明确报错（不生成非法 SQL）。
#[test]
fn update_if_rejects_unknown_expected_column() {
    let (dir, dal, _conn) = setup("bad-column");
    let mut session = dal.open_session().unwrap();
    let table = dal.table("Account").unwrap();

    let mut account = Account {
        name: "A".into(),
        balance: 100,
        version: 1,
        ..Default::default()
    };
    let id = account.insert(&dal, session.as_mut()).unwrap();

    let err = table
        .update_if(
            session.as_mut(),
            &[("Name", "B".into())],
            &[DbValue::Int(id)],
            &[("Nope", 1.into())],
        )
        .unwrap_err();
    assert!(matches!(err, Error::Model(_)), "应报列不存在：{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 未使用的方法保留给后续场景（避免 dead_code 警告）。
#[test]
fn setters_mark_dirty_only_on_change() {
    let mut account = Account::default();
    account.set_balance(10);
    assert!(account.is_dirty("Balance"));
    account.clear_dirty();
    account.set_balance(10);
    assert!(!account.is_dirty("Balance"), "值未变不应标脏");

    account.set_version(2);
    assert!(account.is_dirty("Version"));
}
