//! DuckDB 端到端集成测试：**内嵌真实引擎**（无需外部服务）。
//!
//! 仅在 `--features duckdb` 构建时启用（默认构建该文件为空）：
//!
//! ```powershell
//! $env:CMAKE = "<cmake.exe 路径>"   # 首次编译 bundled 引擎需要 CMake
//! cargo test --features duckdb --test duckdb_e2e
//! ```
//!
//! 覆盖点：
//! - 内存库（`Data Source=:memory:`）建序列 / 建表 / 幂等同步
//! - 自增主键回写（`INSERT ... RETURNING`）
//! - Decimal / 二进制 / 时间列的往返
//! - 条件查询、分页、更新、删除、事务回滚
//! - 字符串主键表的增查
#![cfg(feature = "duckdb")]

use std::path::PathBuf;

use chrono::NaiveDate;
use pek_rcode::{DatabaseKind, Dal, DbValue, EntityModel, Query, Where, dal::ConnectionString};

/// 测试模型（与 dal.rs 单测的 XML 结构一致）。
const MODEL: &str = r#"<EntityModel><Tables>
  <Table Name="DuckItem" TableName="DH_DuckItem">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      <Column Name="Code" DataType="String" Length="50" />
      <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" />
      <Column Name="Payload" DataType="Binary" Nullable="True" />
      <Column Name="CreateTime" DataType="DateTime" />
    </Columns>
  </Table>
  <Table Name="DuckKey" TableName="DH_DuckKey">
    <Columns>
      <Column Name="Key" DataType="String" Length="50" PrimaryKey="True" />
      <Column Name="Value" DataType="String" Nullable="True" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

/// 临时目录（每次唯一，避免并行冲突）。
fn temp_dir() -> PathBuf {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!("rcode-duckdb-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 打开文件库的 Dal（DuckDB 内存库为“每连接独立”，端到端流程需用文件库）。
///
/// 返回 `(Dal, 临时目录)`：测试结束时删除临时目录。
fn open_dal() -> (Dal, PathBuf) {
    let dir = temp_dir();
    let db = dir.join("test.duckdb");
    let model = EntityModel::parse(MODEL).expect("测试模型应可解析");
    let subset = model.subset(&["DuckItem", "DuckKey"]);
    let dal = Dal::open_with_model(
        &format!("Data Source={};provider=duckdb", db.display()),
        subset,
    )
    .expect("DuckDB 文件库应可打开");
    (dal, dir)
}

/// 建表 + 建序列 + 幂等校验。
#[test]
fn sync_schema_creates_tables_and_is_idempotent() {
    let (dal, dir) = open_dal();
    assert_eq!(dal.kind(), DatabaseKind::DuckDb);

    let report = dal.sync_schema().expect("建表应成功");
    assert_eq!(report.created_tables.len(), 2, "{report}");

    let mut session = dal.open_session().unwrap();
    // 序列由建表脚本一并创建（DuckDB 的 identity 默认值引用它）
    let set = session
        .query(
            "SELECT COUNT(*) FROM duckdb_sequences() WHERE sequence_name = ?",
            &[DbValue::Text("SEQ_DH_DuckItem".into())],
        )
        .unwrap();
    assert_eq!(
        set.first().unwrap().get(0).unwrap().as_i64(),
        Some(1),
        "自增列应创建序列：{report}"
    );

    // 幂等：再次同步无变更（含序列探测）。
    // 注意：DuckDB 文件库同一文件不允许并存两个连接，需先释放查询连接再二次同步
    let columns = session.table_columns("DH_DuckItem").unwrap();
    assert_eq!(
        columns,
        vec!["Id", "Code", "Amount", "Payload", "CreateTime"]
    );
    assert!(session.table_exists("DH_DuckKey").unwrap());

    drop(session);
    assert!(dal.sync_schema().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 自增主键回写 + 各类型往返 + 增删改查。
#[test]
fn entity_crud_roundtrip() {
    let (dal, dir) = open_dal();
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();

    let now = NaiveDate::from_ymd_opt(2026, 9, 27)
        .unwrap()
        .and_hms_micro_opt(10, 30, 0, 123_000)
        .unwrap();

    // 插入两条
    let item = dal.table("DuckItem").unwrap();
    let id1 = item
        .insert(
            session.as_mut(),
            &[
                ("Code", "A-001".into()),
                ("Amount", "12.50".parse::<rust_decimal::Decimal>().unwrap().into()),
                ("Payload", vec![0x10, 0x20].into()),
                ("CreateTime", now.into()),
            ],
        )
        .unwrap();
    assert!(id1 > 0, "自增主键应通过 RETURNING 回写");
    let id2 = item
        .insert(
            session.as_mut(),
            &[
                ("Code", "B-001".into()),
                ("Amount", "7".parse::<rust_decimal::Decimal>().unwrap().into()),
                ("Payload", DbValue::Null),
                ("CreateTime", now.into()),
            ],
        )
        .unwrap();
    assert_eq!(id2, id1 + 1, "序列应连续递增");

    // 主键查找
    let row = item
        .find_by_pk(session.as_mut(), &[id1.into()])
        .unwrap()
        .expect("应按主键查到");
    assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("A-001"));
    // Decimal 按数值比较（scale 可能被补齐）
    assert_eq!(
        row.get_by_name("Amount").unwrap().as_decimal(),
        Some("12.5".parse().unwrap())
    );
    assert_eq!(
        row.get_by_name("Payload").unwrap(),
        &DbValue::Blob(vec![0x10, 0x20])
    );
    assert_eq!(
        row.get_by_name("CreateTime").unwrap(),
        &DbValue::DateTime(now)
    );

    // 条件统计 + 分页
    let filter = Where::new().like("Code", "A-%");
    assert_eq!(item.count(session.as_mut(), Some(&filter)).unwrap(), 1);
    let page = item
        .query(
            session.as_mut(),
            &Query::new().order_by("Code", false).page(1, 10),
        )
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(
        page.first().unwrap().get_by_name("Code").unwrap().as_str(),
        Some("A-001")
    );

    // 更新 / 删除
    let affected = item
        .update_by_pk(session.as_mut(), &[("Code", "A-999".into())], &[id1.into()])
        .unwrap();
    assert_eq!(affected, 1);
    let row = item
        .find_by_pk(session.as_mut(), &[id1.into()])
        .unwrap()
        .unwrap();
    assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("A-999"));

    assert_eq!(
        item.delete_by_pk(session.as_mut(), &[id2.into()]).unwrap(),
        1
    );
    assert!(!item.exists_by_pk(session.as_mut(), &[id2.into()]).unwrap());

    // 字符串主键表：无自增列时插入返回 0
    let key = dal.table("DuckKey").unwrap();
    let inserted = key
        .insert(
            session.as_mut(),
            &[("Key", "k-001".into()), ("Value", "v-001".into())],
        )
        .unwrap();
    assert_eq!(inserted, 0);
    let row = key
        .find_by_pk(session.as_mut(), &["k-001".into()])
        .unwrap()
        .unwrap();
    assert_eq!(row.get_by_name("Value").unwrap().as_str(), Some("v-001"));

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 事务回滚：未提交的写入应被丢弃。
#[test]
fn transaction_rollback() {
    let (dal, dir) = open_dal();
    dal.sync_schema().unwrap();
    let mut session = dal.open_session().unwrap();

    let item = dal.table("DuckItem").unwrap();
    let now = NaiveDate::from_ymd_opt(2026, 9, 27)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    session.begin().unwrap();
    item.insert(
        session.as_mut(),
        &[
            ("Code", "R-001".into()),
            ("Amount", "1".parse::<rust_decimal::Decimal>().unwrap().into()),
            ("CreateTime", now.into()),
        ],
    )
    .unwrap();
    session.rollback().unwrap();

    assert_eq!(item.count(session.as_mut(), None).unwrap(), 0);

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 连接串解析：provider=duckdb 应识别为 DuckDb。
#[test]
fn connection_string_kind() {
    let cs = ConnectionString::parse("Data Source=:memory:;provider=duckdb");
    assert_eq!(cs.kind().unwrap(), DatabaseKind::DuckDb);
}
