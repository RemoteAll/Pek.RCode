//! 字段缓存（`FieldCache`，对应 DH.NCode `FieldCache<T>`）端到端测试。
//!
//! 场景：分类/字典下拉选项——按列分组统计（出现次数降序）、10 分钟缓存、写入即失效。

use std::path::PathBuf;

use pek_rcode::Dal;
use pek_rcode::model::EntityModel;
use pek_rcode::value::DbValue;

const MODEL: &str = r#"<EntityModel><Tables>
<Table Name="Article" TableName="DH_Article" Description="文章">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="Category" DataType="String" Length="50" Description="分类" />
    <Column Name="Title" DataType="String" Length="200" Nullable="True" />
  </Columns>
  <Indexes>
    <Index Name="IX_Category" Columns="Category" Unique="False" />
  </Indexes>
</Table>
</Tables></EntityModel>"#;

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pek-rcode-fieldcache-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(tag: &str) -> (PathBuf, Dal) {
    let dir = temp_dir(tag);
    let db = dir.join("test.db");
    let conn = format!("Data Source={};Provider=SQLite", db.display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();
    (dir, dal)
}

/// 通过表句柄插入（走正式写入路径，会触发缓存失效）。
fn add(dal: &Dal, category: &str, times: usize) {
    let table = dal.table("Article").unwrap();
    let mut s = dal.open_session().unwrap();
    for _ in 0..times {
        table
            .insert(&mut *s, &[("Category", DbValue::Text(category.into()))])
            .unwrap();
    }
}

#[test]
fn field_cache_groups_and_orders_by_count() {
    let (dir, dal) = setup("order");
    add(&dal, "A", 3);
    add(&dal, "B", 2);
    add(&dal, "C", 1);

    let cache = dal.field_cache("Article", "Category").unwrap();
    let mut s = dal.open_session().unwrap();
    assert_eq!(
        cache.names(&dal, &mut *s).unwrap(),
        vec!["A", "B", "C"],
        "按出现次数降序"
    );

    // 值 → 值映射（对应 C# `FindAllName()`）
    let map = cache.name_map(&dal, &mut *s).unwrap();
    assert_eq!(map.len(), 3);
    assert!(map.contains_key("B"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn field_cache_top_limits_groups() {
    let (dir, dal) = setup("top");
    add(&dal, "A", 3);
    add(&dal, "B", 2);
    add(&dal, "C", 1);

    let cache = dal.field_cache("Article", "Category").unwrap();
    cache.set_top(2);
    assert_eq!(cache.top(), 2);
    let mut s = dal.open_session().unwrap();
    assert_eq!(cache.names(&dal, &mut *s).unwrap(), vec!["A", "B"]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn field_cache_invalidated_on_write() {
    let (dir, dal) = setup("invalidate");
    add(&dal, "A", 1);

    let cache = dal.field_cache("Article", "Category").unwrap();
    let mut s = dal.open_session().unwrap();
    assert_eq!(cache.names(&dal, &mut *s).unwrap(), vec!["A"]);

    // 新写入"应该"让缓存失效（否则下拉里看不到新分类）
    add(&dal, "D", 5);
    assert_eq!(
        cache.names(&dal, &mut *s).unwrap(),
        vec!["D", "A"],
        "写入后缓存失效，新分类出现且按计数降序"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn field_cache_is_shared_per_table_and_column() {
    let (dir, dal) = setup("shared");
    let a1 = dal.field_cache("Article", "Category").unwrap();
    let a2 = dal.field_cache("DH_Article", "Category").unwrap();
    assert!(std::sync::Arc::ptr_eq(&a1, &a2), "同一表列共享同一缓存实例");

    // 列为空时明确报错（避免悄悄查不存在的列）
    assert!(dal.field_cache("Article", "NotExists").is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 表级 `find_cached_by`：小表按唯一键查询走整表缓存（手写结构体消费方用）。
#[test]
fn table_find_cached_by_uses_entity_cache_and_invalidates() {
    let (dir, dal) = setup("table-find");
    let table = dal.table("Article").unwrap();
    let mut session = dal.open_session().unwrap();

    table
        .insert(
            session.as_mut(),
            &[("Category", DbValue::Text("Redis".into()))],
        )
        .unwrap();

    // 首次命中（此时可能走 SQL），字符串忽略大小写
    let row = table
        .find_cached_by(
            &dal,
            session.as_mut(),
            &[("Category", DbValue::Text("redis".into()))],
        )
        .unwrap()
        .expect("应能按唯一键查到");
    assert_eq!(
        row.get_by_name("Category").map(DbValue::to_text).as_deref(),
        Some("Redis")
    );

    // 再查一次：应命中缓存（行数 < MaxCacheCount），结果一致
    let again = table
        .find_cached_by(
            &dal,
            session.as_mut(),
            &[("Category", DbValue::Text("REDIS".into()))],
        )
        .unwrap()
        .expect("缓存命中");
    assert_eq!(
        again.get_by_name("Id").and_then(DbValue::as_i64),
        row.get_by_name("Id").and_then(DbValue::as_i64)
    );

    // 未命中
    assert!(
        table
            .find_cached_by(
                &dal,
                session.as_mut(),
                &[("Category", DbValue::Text("Nope".into()))]
            )
            .unwrap()
            .is_none()
    );

    // 写入后缓存失效：新行立即可查
    table
        .insert(
            session.as_mut(),
            &[("Category", DbValue::Text("Kafka".into()))],
        )
        .unwrap();
    assert!(
        table
            .find_cached_by(
                &dal,
                session.as_mut(),
                &[("Category", DbValue::Text("Kafka".into()))]
            )
            .unwrap()
            .is_some(),
        "写入后缓存应失效，新行立即可查"
    );

    // 空键拒绝（避免退化为"取首行"的危险语义）
    assert!(table.find_cached_by(&dal, session.as_mut(), &[]).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}
