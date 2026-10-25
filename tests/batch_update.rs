//! 批量按主键更新（`TableRef::update_batch_by_pk`）：一条语句更新多行。
//!
//! 目的：心跳/状态类高频写入在 MySQL/PostgreSQL 上的主要成本是「每条语句一次往返」，
//! 把 N 行合成 1 条 `UPDATE ... SET c = CASE pk WHEN ? THEN ? ... END` 后，往返次数 N → 1。

use std::path::PathBuf;

use pek_rcode::model::EntityModel;
use pek_rcode::{Dal, DbValue, Query, Where};

const MODEL: &str = r#"<EntityModel><Tables><Table Name="DeviceOnline" TableName="DeviceOnline">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="DeviceId" DataType="Int32" />
    <Column Name="Memory" DataType="Int64" />
    <Column Name="CpuRate" DataType="Double" />
    <Column Name="Delay" DataType="Int32" />
  </Columns>
  <Indexes>
    <Index Columns="DeviceId" />
  </Indexes>
</Table></Tables></EntityModel>"#;

fn setup(name: &str) -> (PathBuf, Dal) {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!(
        "rcode-batch-{}-{}-{name}",
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = format!("Data Source={};Provider=SQLite", dir.join("t.db").display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();
    (dir, dal)
}

/// 一条语句更新多行：逐行值各不相同，未命中的主键不受影响。
#[test]
fn batch_update_writes_multiple_rows_in_one_statement() {
    let (dir, dal) = setup("multi");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();

    for device_id in 1..=3 {
        table
            .insert(
                session.as_mut(),
                &[
                    ("DeviceId", DbValue::Int(device_id)),
                    ("Memory", DbValue::Int(0)),
                    ("CpuRate", DbValue::Float(0.0)),
                    ("Delay", DbValue::Int(0)),
                ],
            )
            .unwrap();
    }

    let columns = ["Memory", "CpuRate", "Delay"];
    let rows = vec![
        (
            DbValue::Int(1),
            vec![DbValue::Int(1024), DbValue::Float(0.25), DbValue::Int(12)],
        ),
        (
            DbValue::Int(3),
            vec![DbValue::Int(4096), DbValue::Float(0.75), DbValue::Int(30)],
        ),
    ];
    let affected = table
        .update_batch_by_pk(session.as_mut(), &columns, &rows)
        .unwrap();
    assert_eq!(affected, 2, "应命中两行");

    let mut read = |pk: i32| {
        let rs = table
            .query(
                session.as_mut(),
                &Query::new().filter(Where::new().eq("Id", pk)).take(1),
            )
            .unwrap();
        let row = rs.first().expect("行存在");
        (
            row.get_by_name("Memory").and_then(DbValue::as_i64).unwrap_or(-1),
            row.get_by_name("Delay").and_then(DbValue::as_i64).unwrap_or(-1),
        )
    };
    assert_eq!(read(1), (1024, 12));
    assert_eq!(read(2), (0, 0), "未列出的主键不应被改动");
    assert_eq!(read(3), (4096, 30));

    let _ = std::fs::remove_dir_all(&dir);
}

/// 值个数与列数不一致、列不存在、空行都应报错（不发 SQL）。
#[test]
fn batch_update_validates_inputs() {
    let (dir, dal) = setup("validate");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();

    assert!(table
        .update_batch_by_pk(session.as_mut(), &["Memory"], &[])
        .is_err());
    assert!(table
        .update_batch_by_pk(session.as_mut(), &[], &[(DbValue::Int(1), vec![])])
        .is_err());
    // 值个数不匹配
    assert!(table
        .update_batch_by_pk(
            session.as_mut(),
            &["Memory", "Delay"],
            &[(DbValue::Int(1), vec![DbValue::Int(1)])],
        )
        .is_err());
    // 列不存在
    assert!(table
        .update_batch_by_pk(
            session.as_mut(),
            &["NotExists"],
            &[(DbValue::Int(1), vec![DbValue::Int(1)])],
        )
        .is_err());

    let _ = std::fs::remove_dir_all(&dir);
}
