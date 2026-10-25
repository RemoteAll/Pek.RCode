//! 原子累加（对应 DH.NCode `AdditionalFields`）与统计行助手测试。
//!
//! 场景：IoT 设备上报计数、流量累计——必须「库内累加」而不是「读出→改→写回」，
//! 否则高并发下会丢更新。

use std::path::PathBuf;

use pek_rcode::dirty::DirtyState;
use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Query, Result, Where};

#[derive(Debug, Clone, Default)]
struct DeviceStat {
    id: i32,
    name: String,
    count: i64,
    flow: i64,
    dirty: DirtyState,
}

impl DeviceStat {
    const TABLE_NAME: &'static str = "DH_DeviceStat";

    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Default::default()
        }
    }
}

impl Entity for DeviceStat {
    fn table() -> &'static str {
        Self::TABLE_NAME
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "Name", "Count", "Flow"]
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
            ("Count", self.count.into()),
            ("Flow", self.flow.into()),
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
            count: row
                .get_by_name("Count")
                .and_then(DbValue::as_i64)
                .unwrap_or_default(),
            flow: row
                .get_by_name("Flow")
                .and_then(DbValue::as_i64)
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

    fn set_field(&mut self, column: &str, value: DbValue) -> Result<bool> {
        if column.eq_ignore_ascii_case("Count") {
            self.count = value.as_i64().unwrap_or_default();
        } else if column.eq_ignore_ascii_case("Flow") {
            self.flow = value.as_i64().unwrap_or_default();
        } else if column.eq_ignore_ascii_case("Name") {
            self.name = value.to_text();
        } else if column.eq_ignore_ascii_case("Id") {
            self.id = value.as_i32().unwrap_or_default();
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}

const MODEL: &str = r#"<EntityModel><Tables><Table Name="DeviceStat" TableName="DH_DeviceStat" Description="设备统计">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
    <Column Name="Name" DataType="String" Length="50" Description="名称" />
    <Column Name="Count" DataType="Int64" Description="上报次数" />
    <Column Name="Flow" DataType="Int64" Description="流量" />
  </Columns>
</Table></Tables></EntityModel>"#;

fn temp_dir(name: &str) -> PathBuf {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!("rcode-inc-{}-{}-{name}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(name: &str) -> (PathBuf, Dal) {
    let dir = temp_dir(name);
    let conn = format!("Data Source={};Provider=SQLite", dir.join("t.db").display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();
    (dir, dal)
}

/// 表级原子累加：多次累加正确、主键不存在返回 0。
#[test]
fn table_increment_is_atomic_and_accumulates() {
    let (dir, dal) = setup("table");
    let table = dal.table("DeviceStat").unwrap();
    let mut session = dal.open_session().unwrap();

    assert_eq!(
        table
            .insert(
                session.as_mut(),
                &[
                    ("Name", DbValue::Text("dev-1".into())),
                    ("Count", DbValue::Int(0)),
                    ("Flow", DbValue::Int(0)),
                ],
            )
            .unwrap(),
        1
    );

    // 连续累加 3 次（每次 +2 次数、+100 流量）
    for _ in 0..3 {
        assert_eq!(
            table
                .increment_by_pk(
                    session.as_mut(),
                    &[DbValue::Int(1)],
                    &[
                        ("Count", DbValue::Int(2)),
                        ("Flow", DbValue::Int(100)),
                    ],
                )
                .unwrap(),
            1
        );
    }
    let row = table
        .query(
            session.as_mut(),
            &Query::new().filter(Where::new().eq("Id", 1)).take(1),
        )
        .unwrap();
    let row = row.first().unwrap();
    assert_eq!(row.get_by_name("Count").and_then(DbValue::as_i64), Some(6));
    assert_eq!(row.get_by_name("Flow").and_then(DbValue::as_i64), Some(300));

    // 主键不存在 → 0 行
    assert_eq!(
        table
            .increment_by_pk(
                session.as_mut(),
                &[DbValue::Int(999)],
                &[("Count", DbValue::Int(1))],
            )
            .unwrap(),
        0
    );

    // 空累加列 → 参数错误
    assert!(table.increment(session.as_mut(), &[], &Where::new()).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 累加 + 赋值混合：一条语句同时写计数与快照列；主键不存在返回 0（调用方据此转插入）。
#[test]
fn table_increment_with_assigns_snapshot_in_one_statement() {
    let (dir, dal) = setup("increment-with");
    let table = dal.table("DeviceStat").unwrap();
    let mut session = dal.open_session().unwrap();

    table
        .insert(
            session.as_mut(),
            &[
                ("Name", DbValue::Text("dev-1".into())),
                ("Count", DbValue::Int(0)),
                ("Flow", DbValue::Int(5)),
            ],
        )
        .unwrap();

    let filter = Where::new().eq("Id", 1);
    assert_eq!(
        table
            .increment_with(
                session.as_mut(),
                &[("Count", DbValue::Int(1))],
                &[
                    ("Name", DbValue::Text("dev-2".into())),
                    ("Flow", DbValue::Int(9)),
                ],
                &filter,
            )
            .unwrap(),
        1
    );
    let row = table
        .query(session.as_mut(), &Query::new().filter(filter.clone()).take(1))
        .unwrap();
    let row = row.first().unwrap();
    assert_eq!(row.get_by_name("Count").and_then(DbValue::as_i64), Some(1));
    assert_eq!(row.get_by_name("Flow").and_then(DbValue::as_i64), Some(9));
    assert_eq!(
        row.get_by_name("Name").and_then(|v| v.as_str()),
        Some("dev-2")
    );

    // 主键不存在 → 0 行（累加必然改变取值，0 行即"行不存在"）
    assert_eq!(
        table
            .increment_with(
                session.as_mut(),
                &[("Count", DbValue::Int(1))],
                &[("Name", DbValue::Text("ghost".into()))],
                &Where::new().eq("Id", 999),
            )
            .unwrap(),
        0
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 实体级累加：一条语句累加并同步回对象。
#[test]
fn entity_increment_updates_object_and_database() {
    let (dir, dal) = setup("entity");
    let mut session = dal.open_session().unwrap();

    let mut stat = DeviceStat::new("dev-2");
    let id = stat.insert(&dal, session.as_mut()).unwrap();
    assert!(id > 0);

    // 读出来再累加（有脏追踪的实体）
    let mut loaded = DeviceStat::find(&dal, session.as_mut(), &[id.into()])
        .unwrap()
        .expect("应能查到");
    assert_eq!(loaded.count, 0);

    assert_eq!(
        loaded
            .increment(&dal, session.as_mut(), &[("Count", DbValue::Int(7))])
            .unwrap(),
        1
    );
    assert_eq!(loaded.count, 7, "对象内计数应同步为库内新值");

    // 再累加一次，对象与库都继续增长
    assert_eq!(
        loaded
            .increment(&dal, session.as_mut(), &[("Count", DbValue::Int(3))])
            .unwrap(),
        1
    );
    assert_eq!(loaded.count, 10);

    let reloaded = DeviceStat::find(&dal, session.as_mut(), &[id.into()])
        .unwrap()
        .expect("应能查到");
    assert_eq!(reloaded.count, 10, "库内计数应为 10");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 条件累加（批量给多行加同一个增量）：一条语句改多行。
#[test]
fn increment_where_updates_multiple_rows() {
    let (dir, dal) = setup("where");
    let table = dal.table("DeviceStat").unwrap();
    let mut session = dal.open_session().unwrap();

    for name in ["a", "b", "c"] {
        table
            .insert(
                session.as_mut(),
                &[
                    ("Name", DbValue::Text(name.into())),
                    ("Count", DbValue::Int(0)),
                    ("Flow", DbValue::Int(0)),
                ],
            )
            .unwrap();
    }

    let affected = table
        .increment(
            session.as_mut(),
            &[("Flow", DbValue::Int(5))],
            &Where::new().like("Name", "%"),
        )
        .unwrap();
    assert_eq!(affected, 3, "一条语句应累加全部 3 行");

    for row in table
        .query(
            session.as_mut(),
            &Query::new().filter(Where::new().like("Name", "%")).order_by("Id", false),
        )
        .unwrap()
        .into_iter()
    {
        assert_eq!(row.get_by_name("Flow").and_then(DbValue::as_i64), Some(5));
    }

    let _ = std::fs::remove_dir_all(&dir);
}
