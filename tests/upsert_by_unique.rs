//! 按业务唯一键批量 Upsert（`TableRef::upsert_batch_by_unique`）。
//!
//! 场景：设备在线表「一台设备一行」——表主键是自增 `Id`，业务唯一键是 `DeviceId`（唯一索引）。
//! 心跳聚合后用一条语句写入多行，冲突按 `DeviceId` 更新，避免「先查再插/改」的 N 次往返。

use std::path::PathBuf;

use pek_rcode::model::EntityModel;
use pek_rcode::{Dal, DbValue, Query, Where};

const MODEL: &str = r#"<EntityModel><Tables><Table Name="DeviceOnline" TableName="DeviceOnline">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="DeviceId" DataType="Int32" />
    <Column Name="Memory" DataType="Int64" />
    <Column Name="Delay" DataType="Int32" />
    <Column Name="AccessMode" DataType="String" Length="20" />
  </Columns>
  <Indexes>
    <Index Columns="DeviceId" Unique="True" />
  </Indexes>
</Table></Tables></EntityModel>"#;

fn setup(name: &str) -> (PathBuf, Dal) {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!(
        "rcode-upsert-{}-{}-{name}",
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = format!("Data Source={};Provider=SQLite", dir.join("t.db").display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();
    (dir, dal)
}

/// 首批插入、第二批按 DeviceId 更新；自增 Id 由数据库生成，不会重复插入。
#[test]
fn upsert_batch_by_unique_inserts_then_updates() {
    let (dir, dal) = setup("upsert");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();
    let columns = ["DeviceId", "Memory", "Delay", "AccessMode"];

    let first = vec![
        vec![
            DbValue::Int(10),
            DbValue::Int(1024),
            DbValue::Int(5),
            DbValue::Text("Mqtt".into()),
        ],
        vec![
            DbValue::Int(11),
            DbValue::Int(2048),
            DbValue::Int(6),
            DbValue::Text("Mqtt".into()),
        ],
    ];
    let affected = table
        .upsert_batch_by_unique(session.as_mut(), &columns, &["DeviceId"], &first, None)
        .unwrap();
    assert_eq!(affected, 2, "首批应插入两行");

    // 第二批：同 DeviceId 只更新指标，不新增行
    let second = vec![
        vec![
            DbValue::Int(10),
            DbValue::Int(9999),
            DbValue::Int(9),
            DbValue::Text("Mqtt".into()),
        ],
        vec![
            DbValue::Int(12),
            DbValue::Int(4096),
            DbValue::Int(7),
            DbValue::Text("Mqtt".into()),
        ],
    ];
    table
        .upsert_batch_by_unique(session.as_mut(), &columns, &["DeviceId"], &second, None)
        .unwrap();

    let count = table.count(session.as_mut(), None).unwrap();
    assert_eq!(count, 3, "同 DeviceId 应更新而非新增");

    let mut read = |device_id: i32| {
        let rs = table
            .query(
                session.as_mut(),
                &Query::new()
                    .filter(Where::new().eq("DeviceId", device_id))
                    .take(1),
            )
            .unwrap();
        let row = rs.first().expect("行存在");
        (
            row.get_by_name("Memory").and_then(DbValue::as_i64).unwrap_or(-1),
            row.get_by_name("Delay").and_then(DbValue::as_i64).unwrap_or(-1),
        )
    };
    assert_eq!(read(10), (9999, 9), "同 DeviceId 应被更新");
    assert_eq!(read(11), (2048, 6), "未列出的行保持原值");
    assert_eq!(read(12), (4096, 7), "新设备应插入");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 入参校验：空列/空行/值个数不匹配都要报错且不发 SQL。
#[test]
fn upsert_batch_by_unique_validates_inputs() {
    let (dir, dal) = setup("validate");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();

    assert!(table
        .upsert_batch_by_unique(session.as_mut(), &[], &["DeviceId"], &[vec![DbValue::Int(1)]], None)
        .is_err());
    assert!(table
        .upsert_batch_by_unique(session.as_mut(), &["DeviceId"], &["DeviceId"], &[], None)
        .is_err());
    assert!(table
        .upsert_batch_by_unique(
            session.as_mut(),
            &["DeviceId", "Memory"],
            &["DeviceId"],
            &[vec![DbValue::Int(1)]],
            None,
        )
        .is_err());
    assert!(table
        .upsert_batch_by_unique(
            session.as_mut(),
            &["DeviceId"],
            &[],
            &[vec![DbValue::Int(1)]],
            None,
        )
        .is_err());

    let _ = std::fs::remove_dir_all(&dir);
}
