//! 分表 + 拦截器协作测试（独立测试进程，可安全启用全局拦截器）。
//!
//! 验证 C# 的行为顺序：`Valid → 拦截器（TimeInterceptor 填 CreateTime）→ CreateShard`。
//! Rust 侧 [`Entity::insert_sharded`] 同样先执行拦截器再按补全值路由分表。

use chrono::{Datelike, NaiveDateTime};
use pek_rcode::shards::TimeShardPolicy;
use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Result};

/// 访问日志（时间分表，自增主键）。
#[derive(Debug, Clone, PartialEq)]
struct VisitLog {
    id: i32,
    create_time: NaiveDateTime,
    page: String,
}

impl Entity for VisitLog {
    fn table() -> &'static str {
        "VisitLog"
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "CreateTime", "Page"]
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
            ("Page", self.page.clone().into()),
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
            page: row
                .get_by_name("Page")
                .map(DbValue::to_text)
                .unwrap_or_default(),
        })
    }

    fn set_identity(&mut self, value: i64) -> Result<()> {
        self.id = value as i32;
        Ok(())
    }

    /// 分表实体（生成代码）会实现 set_field：拦截器补全后回写实体。
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

const MODEL: &str = r#"<EntityModel><Tables>
  <Table Name="VisitLog" TableName="VisitLog">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      <Column Name="CreateTime" DataType="DateTime" />
      <Column Name="Page" DataType="String" Length="100" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

#[test]
fn interceptor_fills_shard_field_before_routing() {
    // 独立进程：启用默认拦截器（TimeInterceptor 自动填 CreateTime）
    pek_rcode::interceptor::enable_defaults();

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rcode-shards-int-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("shards.db");
    let dal = Dal::open_with_model(
        &format!("Data Source={};Provider=SQLite", db.display()),
        EntityModel::parse(MODEL).unwrap(),
    )
    .unwrap();
    dal.sync_schema().unwrap();

    let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
    let mut session = dal.open_session().unwrap();

    // CreateTime 留空（默认值）→ 拦截器补全 → 分表应落在"今天"
    let mut row = VisitLog {
        id: 0,
        create_time: chrono::DateTime::UNIX_EPOCH.naive_utc(),
        page: "/home".into(),
    };
    let id = row.insert_sharded(&dal, session.as_mut(), &policy).unwrap();
    assert!(id > 0);

    let today = chrono::Local::now().naive_local();
    assert_eq!(
        (row.create_time.year(), row.create_time.ordinal()),
        (today.year(), today.ordinal()),
        "拦截器补全的 CreateTime 应回写实体"
    );
    let expected_table = format!("VisitLog_{}", today.format("%Y%m%d"));
    assert!(
        session.table_exists(&expected_table).unwrap(),
        "数据应路由到 {expected_table}"
    );

    // 按补全后的时间查找
    let found = VisitLog::find_sharded(
        &dal,
        session.as_mut(),
        &policy,
        &DbValue::DateTime(row.create_time),
        &[id.into()],
    )
    .unwrap()
    .expect("应能按补全时间查回");
    assert_eq!(found.page, "/home");

    dal.clear_pool();
    drop(session);
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
}
