//! 批量写入端到端测试（真实 SQLite）：多行插入 / 分表分组批量插入 / 主键 IN 批量删除 /
//! 性能基准（`#[ignore]`，手动运行）。
//!
//! 对照 C# `EntityExtension.Insert(list) / Delete(list)`（DH.NCode）：
//! - 普通批量插入：多行 `VALUES` + 按批分块（默认 5000，对齐 `DAL.GetBatchSize()`）；
//! - 分表批量插入：提前计算分片 → 按（连接, 物理表）分组 → 分组批量插入；
//! - 批量删除：单一主键按 `IN` 分批（默认 1000）；分表场景自动分组路由。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use pek_rcode::shards::{self, TimeShardPolicy};
use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Query, Result, Where};

// ================= 测试实体（与 codegen 生成代码同构的手写实现） =================

/// 时间分表实体（自增主键）。
#[derive(Debug, Clone, PartialEq)]
struct TradeLog {
    id: i32,
    create_time: NaiveDateTime,
    note: String,
}

impl TradeLog {
    const TABLE_NAME: &'static str = "TradeLog";

    fn new(create_time: NaiveDateTime, note: &str) -> Self {
        Self {
            id: 0,
            create_time,
            note: note.to_string(),
        }
    }
}

impl Entity for TradeLog {
    fn table() -> &'static str {
        Self::TABLE_NAME
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

    fn from_row(row: &DbRow) -> Result<Self> {
        Ok(Self {
            id: row
                .get_by_name("Id")
                .and_then(DbValue::as_i32)
                .unwrap_or_default(),
            create_time: row
                .get_by_name("CreateTime")
                .and_then(DbValue::as_datetime)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc()),
            note: row
                .get_by_name("Note")
                .map(DbValue::to_text)
                .unwrap_or_default(),
        })
    }

    fn set_identity(&mut self, value: i64) -> Result<()> {
        self.id = value as i32;
        Ok(())
    }

    fn set_field(&mut self, column: &str, value: DbValue) -> Result<bool> {
        if column.eq_ignore_ascii_case("CreateTime") {
            self.create_time = value
                .as_datetime()
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc());
            return Ok(true);
        }
        Ok(false)
    }
}

/// 雪花 Id 分表实体（Int64 非自增主键）。
#[derive(Debug, Clone, PartialEq)]
struct EventLog {
    id: i64,
    create_time: NaiveDateTime,
    message: String,
}

impl Entity for EventLog {
    fn table() -> &'static str {
        "EventLog"
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "CreateTime", "Message"]
    }

    fn primary_keys() -> &'static [&'static str] {
        &["Id"]
    }

    fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
        vec![
            ("Id", self.id.into()),
            ("CreateTime", self.create_time.into()),
            ("Message", self.message.clone().into()),
        ]
    }

    fn from_row(row: &DbRow) -> Result<Self> {
        Ok(Self {
            id: row
                .get_by_name("Id")
                .and_then(DbValue::as_i64)
                .unwrap_or_default(),
            create_time: row
                .get_by_name("CreateTime")
                .and_then(DbValue::as_datetime)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc()),
            message: row
                .get_by_name("Message")
                .map(DbValue::to_text)
                .unwrap_or_default(),
        })
    }

    /// 生成代码为分表实体实现 `set_field`（回写自动生成的雪花主键）。
    fn set_field(&mut self, column: &str, value: DbValue) -> Result<bool> {
        if column.eq_ignore_ascii_case("Id") {
            self.id = value.as_i64().unwrap_or_default();
            return Ok(true);
        }
        if column.eq_ignore_ascii_case("CreateTime") {
            self.create_time = value
                .as_datetime()
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc());
            return Ok(true);
        }
        Ok(false)
    }
}

// ================= 测试基础设施 =================

const MODEL: &str = r#"<EntityModel><Option><ConnName>DH</ConnName></Option><Tables>
  <Table Name="TradeLog" TableName="TradeLog" Description="交易日志">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      <Column Name="CreateTime" DataType="DateTime" />
      <Column Name="Note" DataType="String" Length="100" />
    </Columns>
  </Table>
  <Table Name="EventLog" TableName="EventLog" Description="事件日志（雪花主键）">
    <Columns>
      <Column Name="Id" DataType="Int64" PrimaryKey="True" />
      <Column Name="CreateTime" DataType="DateTime" />
      <Column Name="Message" DataType="String" Length="200" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

fn temp_db(name: &str) -> (PathBuf, PathBuf) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rcode-batch-{}-{stamp}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (dir.clone(), dir.join("batch.db"))
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

// ================= 测试 =================

/// 普通批量插入：多行 VALUES、自增列自动排除、按批分块、空列表。
#[test]
fn plain_insert_batch_writes_all_rows() {
    let (dir, db) = temp_db("plain");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();

    // 5 行一次写入（默认批大小 5000 → 单条语句）
    let list: Vec<TradeLog> = (0..5)
        .map(|i| TradeLog::new(day(2026, 9, 1), &format!("n{i}")))
        .collect();
    assert_eq!(
        TradeLog::insert_batch(&dal, session.as_mut(), &list, None).unwrap(),
        5
    );
    assert_eq!(TradeLog::count(&dal, session.as_mut(), None).unwrap(), 5);

    // 自增列被整批排除并由数据库生成
    let found = TradeLog::query(
        &dal,
        session.as_mut(),
        &Query::new().filter(Where::new().eq("Note", "n0")),
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].id > 0, "自增主键应由数据库生成");

    // 显式批大小 2 → 分 3 块写入
    let list2: Vec<TradeLog> = (0..5)
        .map(|i| TradeLog::new(day(2026, 9, 2), &format!("m{i}")))
        .collect();
    assert_eq!(
        TradeLog::insert_batch(&dal, session.as_mut(), &list2, Some(2)).unwrap(),
        5
    );
    assert_eq!(TradeLog::count(&dal, session.as_mut(), None).unwrap(), 10);

    // 空列表：0 行，不报错
    assert_eq!(
        TradeLog::insert_batch(&dal, session.as_mut(), &[], None).unwrap(),
        0
    );

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 分表批量插入：跨天 + 跨库（注册连接 + 自动 SQLite 回退）自动分组；
/// 跨库查询回读后按分表批量删除。
#[test]
fn sharded_insert_batch_groups_by_conn_and_table() {
    let (dir, db) = temp_db("sharded");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();

    // 2027 → 注册独立库；其余年份 → 自动 SQLite 回退（C# DAL.Init 行为）
    let db27 = dir.join("dh27.db");
    let dal27 = Arc::new(
        Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db27.display()),
            EntityModel::parse(MODEL).unwrap(),
        )
        .unwrap(),
    );
    dal27.sync_schema().unwrap();
    shards::register_connection("DH_2027", dal27.clone()).unwrap();
    shards::set_auto_db_dir(dir.join("auto"));

    let policy = TimeShardPolicy::new("CreateTime")
        .with_conn_policy("{0}_{1:yyyy}")
        .with_table_policy("{0}_{1:yyyyMMdd}");
    let mut session = dal.open_session().unwrap();

    // 6 行，跨 2 个年份 3 张分表（含 2027 年两组）
    let mut list = vec![
        TradeLog::new(day(2026, 9, 1), "a1"),
        TradeLog::new(day(2026, 9, 1), "a2"),
        TradeLog::new(day(2026, 9, 2), "b1"),
        TradeLog::new(day(2027, 9, 1), "c1"),
        TradeLog::new(day(2027, 9, 1), "c2"),
        TradeLog::new(day(2028, 1, 1), "d1"),
    ];
    assert_eq!(
        TradeLog::insert_batch_sharded(&dal, session.as_mut(), &policy, &mut list, None).unwrap(),
        6
    );

    // 各库落表：2026/2028 自动库、2027 注册库；基础库为空
    let auto26 = open_dal(&dir.join("auto").join("DH_2026.db"));
    let auto28 = open_dal(&dir.join("auto").join("DH_2028.db"));
    assert!(
        auto26
            .open_session()
            .unwrap()
            .table_exists("TradeLog_20260901")
            .unwrap()
    );
    assert!(
        auto28
            .open_session()
            .unwrap()
            .table_exists("TradeLog_20280101")
            .unwrap()
    );
    assert!(
        dal27
            .open_session()
            .unwrap()
            .table_exists("TradeLog_20270901")
            .unwrap()
    );
    assert!(!session.table_exists("TradeLog_20260901").unwrap());
    assert_eq!(TradeLog::count(&dal, session.as_mut(), None).unwrap(), 0);

    // 跨库计数：6 行
    let table = dal.table("TradeLog").unwrap();
    let range = Where::new()
        .ge("CreateTime", day(2026, 1, 1))
        .lt("CreateTime", day(2029, 1, 1));
    assert_eq!(
        table
            .count_sharded(session.as_mut(), &policy, Some(&range))
            .unwrap(),
        6
    );

    // 跨库查询回读（跨表分页合并），取 2027 两行做分表批量删除
    let rows = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new()
            .filter(range.clone())
            .order_by("CreateTime", false),
    )
    .unwrap();
    assert_eq!(rows.len(), 6);
    let del: Vec<TradeLog> = rows
        .into_iter()
        .filter(|r| r.create_time.year() == 2027)
        .collect();
    assert_eq!(del.len(), 2);
    assert_eq!(
        TradeLog::delete_batch_sharded(&dal, session.as_mut(), &policy, &del, None).unwrap(),
        2
    );
    assert_eq!(
        table
            .count_sharded(session.as_mut(), &policy, Some(&range))
            .unwrap(),
        4
    );

    // 清理：注销注册连接 + 回收连接池（Windows 文件占用）
    for name in ["DH_2026", "DH_2027", "DH_2028"] {
        if let Some(d) = shards::registered_connection(name) {
            d.clear_pool();
        }
        shards::unregister_connection(name);
    }
    auto26.clear_pool();
    auto28.clear_pool();
    dal27.clear_pool();
    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 雪花主键分表批量插入：逐行生成并回写 Id（批内顺序稳定），按月路由。
#[test]
fn snowflake_batch_generates_ids() {
    let (dir, db) = temp_db("snow");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();

    let policy = TimeShardPolicy::new("Id")
        .with_table_policy("{0}_{1:yyyyMM}")
        .with_snow(pek_rcode::snowflake::shared());

    let now = chrono::Local::now().naive_local();
    let month_start = now.date().with_day(1).unwrap().and_hms_opt(0, 0, 0).unwrap();

    let mut list = vec![
        EventLog {
            id: 0,
            create_time: now,
            message: "e1".into(),
        },
        EventLog {
            id: 0,
            create_time: now,
            message: "e2".into(),
        },
        EventLog {
            id: 0,
            create_time: now,
            message: "e3".into(),
        },
    ];
    assert_eq!(
        EventLog::insert_batch_sharded(&dal, session.as_mut(), &policy, &mut list, None).unwrap(),
        3
    );

    // 雪花 Id 已生成并回写、批内唯一
    assert!(list.iter().all(|e| e.id > 0));
    let mut ids: Vec<i64> = list.iter().map(|e| e.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "雪花 Id 应唯一");

    // 落表为当前月份分表，并可批量计数【本月】
    let expected = format!("EventLog_{}", now.format("%Y%m"));
    assert!(
        session.table_exists(&expected).unwrap(),
        "数据应路由到 {expected}"
    );
    let table = dal.table("EventLog").unwrap();
    let start_id = pek_rcode::snowflake::shared().id_at(month_start);
    assert_eq!(
        table
            .count_sharded(
                session.as_mut(),
                &policy,
                Some(&Where::new().ge("Id", start_id))
            )
            .unwrap(),
        3
    );

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 普通批量删除：单一主键按 IN 分批。
#[test]
fn delete_batch_uses_primary_key_in() {
    let (dir, db) = temp_db("del");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();

    let list: Vec<TradeLog> = (0..5)
        .map(|i| TradeLog::new(day(2026, 9, 1), &format!("d{i}")))
        .collect();
    TradeLog::insert_batch(&dal, session.as_mut(), &list, None).unwrap();

    // 取回带主键的行（批插不回写自增主键）
    let rows = TradeLog::query(&dal, session.as_mut(), &Query::new()).unwrap();
    assert_eq!(rows.len(), 5);

    // 3 行批量删除（显式批大小 2 → 2+1 两条 IN 语句），剩 2 行
    assert_eq!(
        TradeLog::delete_batch(&dal, session.as_mut(), &rows[..3], Some(2)).unwrap(),
        3
    );
    assert_eq!(TradeLog::count(&dal, session.as_mut(), None).unwrap(), 2);

    // 再删剩余 2 行
    assert_eq!(
        TradeLog::delete_batch(&dal, session.as_mut(), &rows[3..], None).unwrap(),
        2
    );
    assert_eq!(TradeLog::count(&dal, session.as_mut(), None).unwrap(), 0);

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 性能基准（手动运行）：
/// `cargo test --test batch_e2e -- --ignored --nocapture`
#[test]
#[ignore = "性能基准（手动运行）：cargo test --test batch_e2e -- --ignored --nocapture"]
fn bench_single_vs_batch() {
    use std::time::Instant;

    const N: usize = 2_000;
    const M: usize = 3_000;

    // —— 普通表：单行 vs 批量 ——
    let (dir1, db1) = temp_db("bench-single");
    let dal1 = open_dal(&db1);
    dal1.sync_schema().unwrap();
    let mut s1 = dal1.open_session().unwrap();
    let t = Instant::now();
    for i in 0..N {
        let mut row = TradeLog::new(day(2026, 9, 1), &format!("s{i}"));
        row.insert(&dal1, s1.as_mut()).unwrap();
    }
    let single_insert = t.elapsed();

    let (dir2, db2) = temp_db("bench-batch");
    let dal2 = open_dal(&db2);
    dal2.sync_schema().unwrap();
    let mut s2 = dal2.open_session().unwrap();
    let rows2: Vec<TradeLog> = (0..N)
        .map(|i| TradeLog::new(day(2026, 9, 1), &format!("b{i}")))
        .collect();
    let t = Instant::now();
    TradeLog::insert_batch(&dal2, s2.as_mut(), &rows2, None).unwrap();
    let batch_insert = t.elapsed();

    // —— 分表表：单行 vs 批量（30 天 × 100 行）——
    let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
    let (dir3, db3) = temp_db("bench-shard-single");
    let dal3 = open_dal(&db3);
    dal3.sync_schema().unwrap();
    let mut s3 = dal3.open_session().unwrap();
    let t = Instant::now();
    for i in 0..M {
        let mut row = TradeLog::new(
            day(2026, 1, 1) + chrono::TimeDelta::days((i % 30) as i64),
            &format!("x{i}"),
        );
        row.insert_sharded(&dal3, s3.as_mut(), &policy).unwrap();
    }
    let single_shard = t.elapsed();

    let (dir4, db4) = temp_db("bench-shard-batch");
    let dal4 = open_dal(&db4);
    dal4.sync_schema().unwrap();
    let mut s4 = dal4.open_session().unwrap();
    let mut rows4: Vec<TradeLog> = (0..M)
        .map(|i| {
            TradeLog::new(
                day(2026, 1, 1) + chrono::TimeDelta::days((i % 30) as i64),
                &format!("y{i}"),
            )
        })
        .collect();
    let t = Instant::now();
    TradeLog::insert_batch_sharded(&dal4, s4.as_mut(), &policy, &mut rows4, None).unwrap();
    let batch_shard = t.elapsed();

    // —— 删除：单行 vs 主键 IN 批量 ——
    let all = TradeLog::query(&dal2, s2.as_mut(), &Query::new()).unwrap();
    let (half_a, half_b) = all.split_at(all.len() / 2);
    let t = Instant::now();
    for row in half_a {
        row.delete(&dal2, s2.as_mut()).unwrap();
    }
    let single_delete = t.elapsed();
    let t = Instant::now();
    TradeLog::delete_batch(&dal2, s2.as_mut(), half_b, None).unwrap();
    let batch_delete = t.elapsed();

    // —— 分库分表：单行 vs 批量（2 库，各 15 张分表）——
    let db_policy = TimeShardPolicy::new("CreateTime")
        .with_conn_policy("{0}_{1:yyyy}")
        .with_table_policy("{0}_{1:yyyyMMdd}");
    let (dir5, db5) = temp_db("bench-cross");
    let dal5 = open_dal(&db5);
    dal5.sync_schema().unwrap();
    let mut s5 = dal5.open_session().unwrap();
    let db27 = dir5.join("dh27.db");
    let dal5_27 = Arc::new(
        Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db27.display()),
            EntityModel::parse(MODEL).unwrap(),
        )
        .unwrap(),
    );
    dal5_27.sync_schema().unwrap();
    shards::register_connection("DH_2027", dal5_27.clone()).unwrap();
    shards::set_auto_db_dir(dir5.join("auto"));

    const C: usize = 1_000;
    let t = Instant::now();
    for i in 0..C {
        let year = 2026 + (i % 2);
        let d = 1 + ((i / 2) % 15) as u32; // 上月 1~15 号
        let mut row = TradeLog::new(day(year as i32, 6, d), &format!("c{i}"));
        row.insert_sharded(&dal5, s5.as_mut(), &db_policy).unwrap();
    }
    let single_cross = t.elapsed();

    let mut cross_rows: Vec<TradeLog> = (0..C)
        .map(|i| {
            let year = 2026 + (i % 2);
            let d = 16 + ((i / 2) % 15) as u32; // 另一半月分表（避免与上段重合）
            TradeLog::new(day(year as i32, 6, d), &format!("e{i}"))
        })
        .collect();
    let t = Instant::now();
    TradeLog::insert_batch_sharded(&dal5, s5.as_mut(), &db_policy, &mut cross_rows, None).unwrap();
    let batch_cross = t.elapsed();

    let x = |a: std::time::Duration, b: std::time::Duration| a.as_secs_f64() / b.as_secs_f64();
    println!("\n=== 批量基准（SQLite，单进程）===");
    println!(
        "普通插入 {N} 行：单行 {:?} → 批量 {:?}（{:.1}x）",
        single_insert,
        batch_insert,
        x(single_insert, batch_insert)
    );
    println!(
        "分表插入 {M} 行/30 表：单行 {:?} → 批量 {:?}（{:.1}x）",
        single_shard,
        batch_shard,
        x(single_shard, batch_shard)
    );
    println!(
        "分库分表插入 {C} 行/2 库：单行 {:?} → 批量 {:?}（{:.1}x）",
        single_cross,
        batch_cross,
        x(single_cross, batch_cross)
    );
    println!(
        "主键删除 {N} 行：单行 {:?} → 批量 {:?}（{:.1}x）",
        single_delete,
        batch_delete,
        x(single_delete, batch_delete)
    );

    // 清理注册连接（Windows 文件占用）
    for name in ["DH_2026", "DH_2027"] {
        if let Some(d) = shards::registered_connection(name) {
            d.clear_pool();
        }
        shards::unregister_connection(name);
    }
    dal5_27.clear_pool();
    dal5.clear_pool();

    for (dir, dal) in [
        (&dir1, &dal1),
        (&dir2, &dal2),
        (&dir3, &dal3),
        (&dir4, &dal4),
        (&dir5, &dal5),
    ] {
        dal.clear_pool();
        let _ = std::fs::remove_dir_all(dir);
    }
}
