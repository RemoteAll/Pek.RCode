//! 分表端到端测试（真实 SQLite）：策略路由、自动建表、跨表查询/计数/删除、自动分表遍历、雪花主键。
//!
//! 对照 C# `ShardTests`（DH.NCode）的核心场景，验证两边可共用同一批数据库表：
//! - 时间分表：`TradeLog` → `TradeLog_20260901`
//! - 雪花 Id 分表：`EventLog` → `EventLog_202609`（插入时自动生成雪花 Id）
//! - 跨表分页 / 计数 / 条件删除 / AutoShard / 缺失分表跳过 / 迁移档位 Off 不建表

use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use pek_rcode::shards::{self, TimeShardPolicy};
use pek_rcode::snowflake;
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
}

/// 雪花 Id 分表实体（Int64 非自增主键）。
#[derive(Debug, Clone, PartialEq)]
struct EventLog {
    id: i64,
    create_time: NaiveDateTime,
    message: String,
}

impl EventLog {
    const TABLE_NAME: &'static str = "EventLog";

    fn new(create_time: NaiveDateTime, message: &str) -> Self {
        Self {
            id: 0,
            create_time,
            message: message.to_string(),
        }
    }
}

impl Entity for EventLog {
    fn table() -> &'static str {
        Self::TABLE_NAME
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

    /// 分表实体生成代码会实现 `set_field`（用于回写自动生成的雪花主键）。
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

const MODEL: &str = r#"<EntityModel><Tables>
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
    let dir = std::env::temp_dir().join(format!("rcode-shards-{}-{stamp}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (dir.clone(), dir.join("shards.db"))
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

/// 时间策略（按日分表）。
fn day_policy() -> TimeShardPolicy {
    TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}")
}

/// 雪花策略（按月分表，字段为 Id）。
fn snow_policy() -> TimeShardPolicy {
    TimeShardPolicy::new("Id")
        .with_table_policy("{0}_{1:yyyyMM}")
        .with_snow(snowflake::shared())
}

// ================= 测试 =================

/// 时间分表：插入自动建分表、路由正确、按主键查找、更新/删除、跨表计数/条件删除。
#[test]
fn time_sharded_crud_routes_to_physical_tables() {
    let (dir, db) = temp_db("crud");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();

    let policy = day_policy();
    let mut session = dal.open_session().unwrap();

    // 跨 3 天插入 5 行（首日 2 行）
    let rows = [
        TradeLog::new(day(2026, 9, 1), "a1"),
        TradeLog::new(day(2026, 9, 1), "a2"),
        TradeLog::new(day(2026, 9, 2), "b1"),
        TradeLog::new(day(2026, 9, 3), "c1"),
        TradeLog::new(day(2026, 9, 3), "c2"),
    ];
    let mut ids = Vec::new();
    for mut row in rows {
        let id = row.insert_sharded(&dal, session.as_mut(), &policy).unwrap();
        assert!(id > 0, "自增主键应回写");
        assert_eq!(row.id as i64, id);
        ids.push(row.id);
    }

    // 分表已自动创建，且基础表保持为空（数据全部落在分表）
    for table in ["TradeLog_20260901", "TradeLog_20260902", "TradeLog_20260903"] {
        assert!(session.table_exists(table).unwrap(), "{table} 应已自动创建");
    }
    assert!(
        !session.table_exists("TradeLog_20260904").unwrap(),
        "未写入的日期不应建表"
    );
    assert_eq!(TradeLog::count(&dal, session.as_mut(), None).unwrap(), 0);

    // 按主键 + 分表值查找（对应 C# FindByKey）
    let found = TradeLog::find_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &DbValue::DateTime(day(2026, 9, 2)),
        &[ids[2].into()],
    )
    .unwrap()
    .expect("应能查到 20260902 分表中的记录");
    assert_eq!(found.note, "b1");

    // 不存在的分表 → None 且不建表
    let missing = TradeLog::find_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &DbValue::DateTime(day(2026, 9, 9)),
        &[9999.into()],
    )
    .unwrap();
    assert!(missing.is_none());
    assert!(!session.table_exists("TradeLog_20260909").unwrap());

    // 跨表汇总计数（3 张表求和）
    let filter = Where::new()
        .ge("CreateTime", day(2026, 9, 1))
        .lt("CreateTime", day(2026, 9, 4));
    assert_eq!(
        TradeLog::count_sharded(&dal, session.as_mut(), &policy, Some(&filter)).unwrap(),
        5
    );

    // 更新：写回原分表
    let mut to_update = found.clone();
    to_update.note = "b1-updated".into();
    assert_eq!(
        to_update.update_sharded(&dal, session.as_mut(), &policy).unwrap(),
        1
    );
    let again = TradeLog::find_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &DbValue::DateTime(day(2026, 9, 2)),
        &[ids[2].into()],
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.note, "b1-updated");

    // 单行删除
    assert_eq!(again.delete_sharded(&dal, session.as_mut(), &policy).unwrap(), 1);

    // 条件删除（跨表）：删掉 09-03 两行
    let filter = Where::new()
        .ge("CreateTime", day(2026, 9, 3))
        .lt("CreateTime", day(2026, 9, 4));
    assert_eq!(
        TradeLog::delete_where_sharded(&dal, session.as_mut(), &policy, &filter).unwrap(),
        2
    );
    let all_filter = Where::new()
        .ge("CreateTime", day(2026, 9, 1))
        .lt("CreateTime", day(2026, 9, 4));
    assert_eq!(
        TradeLog::count_sharded(&dal, session.as_mut(), &policy, Some(&all_filter)).unwrap(),
        2
    );

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 跨表查询：合并、排序、逐表跳过、分页续页、缺失分表跳过、AutoShard 遍历只走已存在表。
#[test]
fn sharded_query_paging_and_auto_shard() {
    let (dir, db) = temp_db("query");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();

    let policy = day_policy();
    let mut session = dal.open_session().unwrap();

    // 3 天插入 5 行；09-02 无数据（查询时应被跳过而不是报错）
    for (time, note) in [
        (day(2026, 9, 1), "a1"),
        (day(2026, 9, 1), "a2"),
        (day(2026, 9, 3), "c1"),
        (day(2026, 9, 3), "c2"),
        (day(2026, 9, 3), "c3"),
    ] {
        TradeLog::new(time, note)
            .insert_sharded(&dal, session.as_mut(), &policy)
            .unwrap();
    }

    let filter = Where::new()
        .ge("CreateTime", day(2026, 9, 1))
        .lt("CreateTime", day(2026, 9, 5));

    // 全量跨表查询（升序）
    let all = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new()
            .filter(filter.clone())
            .order_by("CreateTime", false)
            .order_by("Id", false),
    )
    .unwrap();
    assert_eq!(all.len(), 5);
    let notes: Vec<&str> = all.iter().map(|e| e.note.as_str()).collect();
    assert_eq!(notes, ["a1", "a2", "c1", "c2", "c3"]);

    // 倒序（分表顺序按分表字段倒排）
    let desc = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new()
            .filter(filter.clone())
            .order_by("CreateTime", true)
            .order_by("Id", false),
    )
    .unwrap();
    let notes: Vec<&str> = desc.iter().map(|e| e.note.as_str()).collect();
    assert_eq!(notes, ["c1", "c2", "c3", "a1", "a2"]);

    // 跨表分页：page_size=2，第 2 页从第 3 行开始（跨过 09-01 与 09-02 空表）
    let page2 = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new()
            .filter(filter.clone())
            .order_by("CreateTime", false)
            .order_by("Id", false)
            .page(2, 2),
    )
    .unwrap();
    let notes: Vec<&str> = page2.iter().map(|e| e.note.as_str()).collect();
    assert_eq!(notes, ["c1", "c2"]);

    let page3 = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new()
            .filter(filter.clone())
            .order_by("CreateTime", false)
            .order_by("Id", false)
            .page(3, 2),
    )
    .unwrap();
    let notes: Vec<&str> = page3.iter().map(|e| e.note.as_str()).collect();
    assert_eq!(notes, ["c3"]);

    // offset + take（原始跳过/限量）
    let offset_rows = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new()
            .filter(filter.clone())
            .order_by("CreateTime", false)
            .order_by("Id", false)
            .offset(3)
            .take(2),
    )
    .unwrap();
    let notes: Vec<&str> = offset_rows.iter().map(|e| e.note.as_str()).collect();
    assert_eq!(notes, ["c2", "c3"]);

    // 单表命中（等值条件）：只查一张分表
    let single = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new().filter(Where::new().eq("CreateTime", day(2026, 9, 1))),
    )
    .unwrap();
    assert_eq!(single.len(), 2);

    // AutoShard：只遍历已存在的分表（09-01、09-03；09-02/09-04 不存在被跳过）
    let table = dal.table("TradeLog").unwrap();
    let counts = table
        .auto_shard(&policy, day(2026, 9, 1), day(2026, 9, 5), |t, s| {
            eprintln!("分表遍历：{}", t.physical_name());
            t.count(s, None)
        })
        .unwrap();
    assert_eq!(counts, vec![2, 3]);

    // 无分表字段条件的查询回退单表（基础表为空 → 0 行，不报错）
    let none = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new().filter(Where::new().eq("Note", "a1")),
    )
    .unwrap();
    assert!(none.is_empty());

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 雪花 Id 分表：插入时自动生成 Id 并回写实体（对应 C# AutoFillSnowIdPrimaryKey），物理表按月。
#[test]
fn snow_id_sharded_insert_generates_id() {
    let (dir, db) = temp_db("snow");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();

    let policy = snow_policy();
    let mut session = dal.open_session().unwrap();

    let mut row = EventLog::new(day(2026, 9, 15), "boot");
    let id = row.insert_sharded(&dal, session.as_mut(), &policy).unwrap();
    assert_eq!(row.id, id, "雪花 Id 应回写实体");
    let (parsed, _, _) = snowflake::shared().parse(id);
    assert!(parsed.year() >= 2026, "雪花 Id 应解析出生成时间（{parsed}）");

    // 雪花 Id 按“生成时刻”路由（Id 是分表字段）：落到解析出的月份分表
    let month_table = format!("EventLog_{:04}{:02}", parsed.year(), parsed.month());
    assert!(session.table_exists(&month_table).unwrap());
    let set = session
        .query(&format!("SELECT COUNT(*) FROM \"{month_table}\""), &[])
        .unwrap();
    assert_eq!(
        set.first().and_then(|r| r.get(0)).and_then(DbValue::as_i64),
        Some(1)
    );

    // 按 Id 作为分表值查找（对应 C# FindByID 的 CreateShard(雪花Id) 路径）
    let found = EventLog::find_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &DbValue::Int(id),
        &[id.into()],
    )
    .unwrap()
    .expect("雪花分表应可查到");
    assert_eq!(found.message, "boot");

    // 与 C# 相同的边界：Id 列上的条件可直接构建区间查询（id_at 只含时间部分）
    let snow = snowflake::shared();
    let snow = snow.as_ref();
    let filter = Where::new()
        .ge("Id", snow.id_at(day(2020, 1, 1)))
        .lt("Id", snow.id_at(day(2030, 1, 1)));
    assert_eq!(
        EventLog::count_sharded(&dal, session.as_mut(), &policy, Some(&filter)).unwrap(),
        1
    );

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 建表语义：迁移档位 Off / 只读档不建分表（对齐 C# SetTables 的档位处理）。
#[test]
fn ensure_shard_table_respects_migration() {
    let (dir, db) = temp_db("migration");

    // Off：不建表
    let dal = Dal::open_with_model(
        &format!("Data Source={};Provider=SQLite;Migration=Off", db.display()),
        EntityModel::parse(MODEL).unwrap(),
    )
    .unwrap();
    let mut session = dal.open_session().unwrap();
    assert!(!dal.ensure_shard_table("TradeLog", "TradeLog_20260901").unwrap());
    assert!(!session.table_exists("TradeLog_20260901").unwrap());
    drop(session);

    // ReadOnly：只检查不执行 → 同样不建表
    let dal = Dal::open_with_model(
        &format!("Data Source={};Provider=SQLite;Migration=ReadOnly", db.display()),
        EntityModel::parse(MODEL).unwrap(),
    )
    .unwrap();
    assert!(!dal.ensure_shard_table("TradeLog", "TradeLog_20260902").unwrap());

    // On（缺省）：建表 + 幂等
    let dal = open_dal(&db);
    assert!(dal.ensure_shard_table("TradeLog", "TradeLog_20260903").unwrap());
    assert!(!dal.ensure_shard_table("TradeLog", "TradeLog_20260903").unwrap());
    let mut session = dal.open_session().unwrap();
    assert!(session.table_exists("TradeLog_20260903").unwrap());

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 策略 API 冒烟：`query_sharded` 与底层函数在无分表条件/空结果时的降级行为。
#[test]
fn sharded_query_falls_back_to_base_table() {
    let (dir, db) = temp_db("fallback");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();

    let policy = day_policy();
    let mut session = dal.open_session().unwrap();

    // 基础表（sync_schema 创建）写入一行：无分表条件查询应命中基础表
    let base = dal.table("TradeLog").unwrap();
    base.insert(session.as_mut(), &[("CreateTime", day(2026, 9, 1).into()), ("Note", "base".into())])
        .unwrap();

    let rows = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &Query::new().filter(Where::new().eq("Note", "base")),
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].note, "base");

    // 无策略（未配置模板）→ 不分表，直查基础表
    let plain = TimeShardPolicy::new("CreateTime");
    let rows = TradeLog::query_sharded(
        &dal,
        session.as_mut(),
        &plain,
        &Query::new().filter(Where::new().eq("Note", "base")),
    )
    .unwrap();
    assert_eq!(rows.len(), 1);

    // shards 模块的函数式入口
    let table = dal.table("TradeLog").unwrap();
    let set = table
        .query_sharded(session.as_mut(), &policy, &Query::new())
        .unwrap();
    assert_eq!(set.len(), 1);

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 兼容性断言：表名/连接名生成与 C# `String.Format` 一致（含连接策略）。
#[test]
fn table_names_match_dotnet_conventions() {
    let policy = TimeShardPolicy::new("CreateTime")
        .with_conn_policy("{0}_{1:yyyy}")
        .with_table_policy("{0}_{1:yyyyMM}");
    let base = shards::ShardBase::new("ExpressLogs").with_conn(Some("DH"));
    let model = policy.resolve_time(base, day(2026, 9, 27)).unwrap();
    assert_eq!(model.conn_name.as_deref(), Some("DH_2026"));
    assert_eq!(model.table_name.as_deref(), Some("ExpressLogs_202609"));
}

/// 删除分表（对应 C# `DropWith`）：只删已存在的分表，基础表不受影响。
#[test]
fn drop_shards_removes_only_existing() {
    let (dir, db) = temp_db("drop");
    let dal = open_dal(&db);
    dal.sync_schema().unwrap();

    let policy = day_policy();
    let mut session = dal.open_session().unwrap();
    for (time, note) in [
        (day(2026, 9, 1), "a"),
        (day(2026, 9, 2), "b"),
        (day(2026, 9, 3), "c"),
    ] {
        TradeLog::new(time, note)
            .insert_sharded(&dal, session.as_mut(), &policy)
            .unwrap();
    }

    let table = dal.table("TradeLog").unwrap();
    // [09-01, 09-03) → 删除 09-01 / 09-02 两张分表
    let dropped = table
        .drop_shards(&policy, day(2026, 9, 1), day(2026, 9, 3))
        .unwrap();
    assert_eq!(dropped, 2);
    assert!(!session.table_exists("TradeLog_20260901").unwrap());
    assert!(!session.table_exists("TradeLog_20260902").unwrap());
    assert!(session.table_exists("TradeLog_20260903").unwrap());
    assert!(session.table_exists("TradeLog").unwrap(), "基础表不受影响");

    // 再删同一区间：已不存在 → 0
    assert_eq!(
        table
            .drop_shards(&policy, day(2026, 9, 1), day(2026, 9, 3))
            .unwrap(),
        0
    );

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}
