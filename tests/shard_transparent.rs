//! 透明分表（`DataScale="timeShard:..."`）端到端测试（真实 SQLite）。
//!
//! 对照 C# 的 `Meta.ShardPolicy` 自动路由：模型里声明分表后，
//! `TableRef::insert / query / count / delete_where / update_by_pk / delete_by_pk`
//! **不再需要显式调用 `*_sharded`**，行为与 C# `Entity.Insert/FindAll/Update/Delete` 一致。
//!
//! 关键约束（与 C# 对齐）：
//! - 无条件查询走模型默认表（C# `where == null` 不走分表）；
//! - `Dal::table_as`（显式物理表）**不自动分表**，这是分表引擎内部防止递归的机制；
//! - 分表不存在时写入自动建表（对齐 `CheckTable`），更新/删除则不建表、直接返回 0。

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Query, Where};

const MODEL: &str = r#"<EntityModel><Option><ConnName>DH</ConnName></Option><Tables>
  <Table Name="TradeLog" TableName="TradeLog" Description="交易日志（按天分表）">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      <Column Name="CreateTime" DataType="DateTime" DataScale="timeShard:yyyyMMdd" />
      <Column Name="Note" DataType="String" Length="100" />
    </Columns>
  </Table>
  <Table Name="EventLog" TableName="EventLog" Description="事件日志（雪花 Id 按年分表）">
    <Columns>
      <Column Name="Id" DataType="Int64" PrimaryKey="True" DataScale="timeShard:yyyy" />
      <Column Name="Message" DataType="String" Length="200" />
    </Columns>
  </Table>
  <Table Name="PlainLog" TableName="PlainLog" Description="普通表（不分表）">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      <Column Name="Note" DataType="String" Length="100" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

fn temp_db(name: &str) -> (PathBuf, PathBuf) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("rcode-shard-auto-{}-{stamp}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (dir.clone(), dir.join("auto.db"))
}

fn open_dal(db: &Path) -> Dal {
    Dal::open_with_model(
        &format!("Data Source={};Provider=SQLite", db.display()),
        EntityModel::parse(MODEL).unwrap(),
    )
    .unwrap()
}

fn day(y: i32, m: u32, d: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
}

/// 模型解析：`DataScale="timeShard:yyyyMMdd"` → 字段 + 模板（与 C# `BuildShardPolicy` 包装规则一致）。
#[test]
fn model_parses_time_shard_spec() {
    let model = EntityModel::parse(MODEL).unwrap();
    let trade = model.table("TradeLog").unwrap();
    let spec = trade.shard_spec().expect("TradeLog 应为分表");
    assert_eq!(spec.field, "CreateTime", "分表字段取物理列名");
    assert_eq!(spec.table_policy, "{0}_{1:yyyyMMdd}");
    assert_eq!(spec.conn_policy, "", "未配置连接模板");

    let event = model.table("EventLog").unwrap();
    let spec = event.shard_spec().expect("EventLog 应为分表");
    assert_eq!(spec.field, "Id");
    assert_eq!(spec.table_policy, "{0}_{1:yyyy}");

    assert!(
        model.table("PlainLog").unwrap().shard_spec().is_none(),
        "普通表不应有分表规格"
    );
}

/// 透明写入/查询/计数/条件删除：全部按天路由到物理分表。
#[test]
fn transparent_crud_routes_by_day() {
    let (_dir, db) = temp_db("crud");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();
    let table = dal.table("TradeLog").unwrap();

    // 插入 5 行（跨 3 天）—— 调用方不感知分表
    for (time, note) in [
        (day(2026, 9, 1), "a1"),
        (day(2026, 9, 1), "a2"),
        (day(2026, 9, 2), "b1"),
        (day(2026, 9, 3), "c1"),
        (day(2026, 9, 3), "c2"),
    ] {
        table
            .insert(
                session.as_mut(),
                &[
                    ("CreateTime", DbValue::DateTime(time)),
                    ("Note", DbValue::from(note)),
                ],
            )
            .unwrap();
    }

    for name in ["TradeLog_20260901", "TradeLog_20260902", "TradeLog_20260903"] {
        assert!(session.table_exists(name).unwrap(), "{name} 应已自动创建");
    }
    assert!(
        !session.table_exists("TradeLog_20260904").unwrap(),
        "未写入的日期不应建表"
    );

    // 基础表（模型默认表）保持为空：数据全部落在分表
    let base = dal.table_as("TradeLog", "TradeLog").unwrap();
    assert_eq!(base.count(session.as_mut(), None).unwrap(), 0);

    // 跨表计数
    let filter = Where::new()
        .ge("CreateTime", day(2026, 9, 1))
        .lt("CreateTime", day(2026, 9, 4));
    assert_eq!(
        table.count(session.as_mut(), Some(&filter)).unwrap(),
        5,
        "应跨 3 张分表求和"
    );

    // 跨表查询（分页参数走跨表续页）
    let query = Query::new()
        .filter(filter.clone())
        .order_by("CreateTime", false);
    let set = table.query(session.as_mut(), &query).unwrap();
    assert_eq!(set.rows.len(), 5);

    // 只取 2 条：跨表 limit
    let query = Query::new().filter(filter.clone()).take(2);
    let set = table.query(session.as_mut(), &query).unwrap();
    assert_eq!(set.rows.len(), 2);

    // 条件删除（跨表）
    let deleted = table
        .delete_where(session.as_mut(), &filter)
        .unwrap();
    assert_eq!(deleted, 5);
    assert_eq!(table.count(session.as_mut(), Some(&filter)).unwrap(), 0);
}

/// 无条件查询走模型默认表（对齐 C# `where == null` 不走分表），不会报错也不会跨表。
#[test]
fn no_time_filter_falls_back_to_base_table() {
    let (_dir, db) = temp_db("nofilter");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();
    let table = dal.table("TradeLog").unwrap();

    table
        .insert(
            session.as_mut(),
            &[
                ("CreateTime", DbValue::DateTime(day(2026, 9, 1))),
                ("Note", DbValue::from("a1")),
            ],
        )
        .unwrap();

    assert_eq!(table.count(session.as_mut(), None).unwrap(), 0);
    assert_eq!(table.query(session.as_mut(), &Query::new()).unwrap().rows.len(), 0);
}

/// 显式物理表（`table_as`）不参与自动分表 —— 分表引擎内部的防递归机制。
#[test]
fn table_as_is_explicit_and_never_reroutes() {
    let (_dir, db) = temp_db("tableas");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();

    dal.ensure_shard_table("TradeLog", "TradeLog_raw").unwrap();
    let raw = dal.table_as("TradeLog", "TradeLog_raw").unwrap();
    raw.insert(
        session.as_mut(),
        &[
            ("CreateTime", DbValue::DateTime(day(2026, 9, 1))),
            ("Note", DbValue::from("raw")),
        ],
    )
    .unwrap();

    assert_eq!(raw.count(session.as_mut(), None).unwrap(), 1);
    assert!(
        !session.table_exists("TradeLog_20260901").unwrap(),
        "显式物理表写入不得再触发分表"
    );
}

/// 雪花 Id 主键分表：写入、按主键更新、按主键删除都自动路由。
#[test]
fn snowflake_primary_key_routes_transparently() {
    let (_dir, db) = temp_db("snow");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();
    let table = dal.table("EventLog").unwrap();

    let id = pek_rcode::snowflake::shared().id_at(day(2026, 3, 15));
    table
        .insert(
            session.as_mut(),
            &[("Id", DbValue::Int(id)), ("Message", DbValue::from("x"))],
        )
        .unwrap();
    assert!(
        session.table_exists("EventLog_2026").unwrap(),
        "雪花时间 2026 年 → EventLog_2026"
    );

    // 更新：sets 不含分表字段 → 按主键雪花解析路由
    let affected = table
        .update_by_pk(
            session.as_mut(),
            &[("Message", DbValue::from("y"))],
            &[DbValue::Int(id)],
        )
        .unwrap();
    assert_eq!(affected, 1);

    // 删除：同样按主键解析
    let deleted = table
        .delete_by_pk(session.as_mut(), &[DbValue::Int(id)])
        .unwrap();
    assert_eq!(deleted, 1);

    // 目标分表不存在的批量更新：返回 0（不建表）
    let missing = pek_rcode::snowflake::shared().id_at(day(2031, 1, 1));
    let affected = table
        .update_by_pk(
            session.as_mut(),
            &[("Message", DbValue::from("z"))],
            &[DbValue::Int(missing)],
        )
        .unwrap();
    assert_eq!(affected, 0);
    assert!(!session.table_exists("EventLog_2031").unwrap());
}

/// 批量写入按分表字段分组（对齐 C# `BatchInsert` 的分表分支）。
#[test]
fn batch_insert_groups_by_shard() {
    let (_dir, db) = temp_db("batch");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();
    let table = dal.table("TradeLog").unwrap();

    let rows: Vec<Vec<(&str, DbValue)>> = vec![
        vec![
            ("CreateTime", DbValue::DateTime(day(2026, 9, 1))),
            ("Note", DbValue::from("a1")),
        ],
        vec![
            ("CreateTime", DbValue::DateTime(day(2026, 9, 2))),
            ("Note", DbValue::from("b1")),
        ],
        vec![
            ("CreateTime", DbValue::DateTime(day(2026, 9, 1))),
            ("Note", DbValue::from("a2")),
        ],
    ];
    let affected = table.insert_batch(session.as_mut(), &rows, None).unwrap();
    assert_eq!(affected, 3);

    let d1 = dal
        .table_as("TradeLog", "TradeLog_20260901")
        .unwrap()
        .count(session.as_mut(), None)
        .unwrap();
    let d2 = dal
        .table_as("TradeLog", "TradeLog_20260902")
        .unwrap()
        .count(session.as_mut(), None)
        .unwrap();
    assert_eq!((d1, d2), (2, 1), "应分别落到两天的分表");
    assert_eq!(
        dal.table_as("TradeLog", "TradeLog")
            .unwrap()
            .count(session.as_mut(), None)
            .unwrap(),
        0,
        "基础表不应有数据"
    );
}

// ================= 实体层（`Entity` trait）透明性 =================

/// 与 `TradeLog` 表同构的最小实体（验证 `Entity::insert/find_all` 也自动分表）。
#[derive(Debug, Clone)]
struct TradeRow {
    id: i32,
    create_time: NaiveDateTime,
    note: String,
}

impl Entity for TradeRow {
    fn table() -> &'static str {
        "TradeLog"
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "CreateTime", "Note"]
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
            ("CreateTime", self.create_time.into()),
            ("Note", self.note.clone().into()),
        ]
    }

    fn from_row(row: &DbRow) -> pek_rcode::Result<Self> {
        Ok(Self {
            id: row
                .get_by_name("Id")
                .and_then(DbValue::as_i32)
                .unwrap_or_default(),
            create_time: row
                .get_by_name("CreateTime")
                .and_then(DbValue::as_datetime)
                .unwrap_or_default(),
            note: row
                .get_by_name("Note")
                .map(DbValue::to_text)
                .unwrap_or_default(),
        })
    }

    fn set_identity(&mut self, value: i64) -> pek_rcode::Result<()> {
        self.id = value as i32;
        Ok(())
    }
}

/// 实体层（`Entity::insert` / `count` / `find_all`）直接继承表级透明分表。
#[test]
fn entity_layer_inherits_transparent_sharding() {
    let (_dir, db) = temp_db("entity");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();

    let mut row = TradeRow {
        id: 0,
        create_time: day(2026, 9, 1),
        note: "e1".into(),
    };
    row.insert(&dal, session.as_mut()).unwrap();
    assert!(
        session.table_exists("TradeLog_20260901").unwrap(),
        "实体插入应落分表"
    );

    let filter = Where::new()
        .ge("CreateTime", day(2026, 9, 1))
        .lt("CreateTime", day(2026, 9, 2));
    assert_eq!(
        TradeRow::count(&dal, session.as_mut(), Some(&filter)).unwrap(),
        1
    );
    let list = TradeRow::query(&dal, session.as_mut(), &Query::new().filter(filter.clone())).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].note, "e1");
}

/// 非分表表完全不受影响（回归护栏）。
#[test]
fn plain_table_keeps_single_table_semantics() {
    let (_dir, db) = temp_db("plain");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();
    let table = dal.table("PlainLog").unwrap();

    table
        .insert(session.as_mut(), &[("Note", DbValue::from("n1"))])
        .unwrap();
    assert_eq!(table.count(session.as_mut(), None).unwrap(), 1);
    assert_eq!(table.query(session.as_mut(), &Query::new()).unwrap().rows.len(), 1);
}
