//! 按业务唯一键批量 Upsert（`TableRef::upsert_batch_by_unique`）。
//!
//! 场景：设备在线表「一台设备一行」——表主键是自增 `Id`，业务唯一键是 `DeviceId`（唯一索引）。
//! 心跳聚合后用一条语句写入多行，冲突按 `DeviceId` 更新；`Pings` 这类计数列按增量**累加**而非覆盖。

use std::path::PathBuf;

use pek_rcode::model::EntityModel;
use pek_rcode::{Dal, DbValue, Query, Where};

const MODEL: &str = r#"<EntityModel><Tables><Table Name="DeviceOnline" TableName="DeviceOnline">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="DeviceId" DataType="Int32" />
    <Column Name="Memory" DataType="Int64" />
    <Column Name="Delay" DataType="Int32" />
    <Column Name="Pings" DataType="Int32" />
    <Column Name="AccessMode" DataType="String" Length="20" />
    <Column Name="CreateTime" DataType="DateTime" Nullable="True" />
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

/// 首批插入、第二批按 DeviceId 更新；`Pings` 累加而不覆盖。
#[test]
fn upsert_batch_by_unique_inserts_then_updates() {
    let (dir, dal) = setup("upsert");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();
    let columns = ["DeviceId", "Memory", "Delay", "Pings", "AccessMode"];

    let first = vec![
        vec![
            DbValue::Int(10),
            DbValue::Int(1024),
            DbValue::Int(5),
            DbValue::Int(1),
            DbValue::Text("Mqtt".into()),
        ],
        vec![
            DbValue::Int(11),
            DbValue::Int(2048),
            DbValue::Int(6),
            DbValue::Int(1),
            DbValue::Text("Mqtt".into()),
        ],
    ];
    let affected = table
        .upsert_batch_by_unique(
            session.as_mut(),
            &columns,
            &["DeviceId"],
            &["Pings"],
            &first,
            None,
        )
        .unwrap();
    assert_eq!(affected, 2, "首批应插入两行");

    // 第二批：同 DeviceId 更新指标，Pings 在旧值上累加
    let second = vec![
        vec![
            DbValue::Int(10),
            DbValue::Int(9999),
            DbValue::Int(9),
            DbValue::Int(1),
            DbValue::Text("Mqtt".into()),
        ],
        vec![
            DbValue::Int(12),
            DbValue::Int(4096),
            DbValue::Int(7),
            DbValue::Int(1),
            DbValue::Text("Mqtt".into()),
        ],
    ];
    table
        .upsert_batch_by_unique(
            session.as_mut(),
            &columns,
            &["DeviceId"],
            &["Pings"],
            &second,
            None,
        )
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
            row.get_by_name("Memory")
                .and_then(DbValue::as_i64)
                .unwrap_or(-1),
            row.get_by_name("Delay")
                .and_then(DbValue::as_i64)
                .unwrap_or(-1),
            row.get_by_name("Pings")
                .and_then(DbValue::as_i64)
                .unwrap_or(-1),
        )
    };
    assert_eq!(
        read(10),
        (9999, 9, 2),
        "同 DeviceId 应更新且 Pings 累加为 2"
    );
    assert_eq!(read(11), (2048, 6, 1), "未列出的行保持原值");
    assert_eq!(read(12), (4096, 7, 1), "新设备应插入");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 入参校验：空列/空行/值个数不匹配/未指定冲突列都要报错且不发 SQL。
#[test]
fn upsert_batch_by_unique_validates_inputs() {
    let (dir, dal) = setup("validate");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();
    let one = vec![DbValue::Int(1)];

    // 空列
    assert!(
        table
            .upsert_batch_by_unique(
                session.as_mut(),
                &[],
                &["DeviceId"],
                &[],
                &[one.clone()],
                None
            )
            .is_err()
    );
    // 空行
    assert!(
        table
            .upsert_batch_by_unique(
                session.as_mut(),
                &["DeviceId"],
                &["DeviceId"],
                &[],
                &[],
                None
            )
            .is_err()
    );
    // 值个数与列数不一致
    assert!(
        table
            .upsert_batch_by_unique(
                session.as_mut(),
                &["DeviceId", "Memory"],
                &["DeviceId"],
                &[],
                &[one.clone()],
                None,
            )
            .is_err()
    );
    // 未指定冲突列
    assert!(
        table
            .upsert_batch_by_unique(session.as_mut(), &["DeviceId"], &[], &[], &[one], None)
            .is_err()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 创建审计列（`CreateTime`）只在首次插入时写：冲突时不得被覆盖，也不需调用方额外传参。
#[test]
fn upsert_batch_by_unique_keeps_create_time() {
    let (dir, dal) = setup("only");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();

    let columns = ["DeviceId", "Memory", "Delay", "Pings", "AccessMode", "CreateTime"];
    let first_time = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let first = vec![vec![
        DbValue::Int(20),
        DbValue::Int(100),
        DbValue::Int(1),
        DbValue::Int(1),
        DbValue::Text("Mqtt".into()),
        DbValue::DateTime(first_time),
    ]];
    table
        .upsert_batch_by_unique(
            session.as_mut(),
            &columns,
            &["DeviceId"],
            &["Pings"],
            &first,
            None,
        )
        .unwrap();

    // 第二批：CreateTime 给一个新值——但创建列属于「只写一次」，库里必须保持首次时间
    let later = first_time + chrono::Duration::hours(5);
    let second = vec![vec![
        DbValue::Int(20),
        DbValue::Int(200),
        DbValue::Int(2),
        DbValue::Int(3),
        DbValue::Text("Mqtt".into()),
        DbValue::DateTime(later),
    ]];
    table
        .upsert_batch_by_unique(
            session.as_mut(),
            &columns,
            &["DeviceId"],
            &["Pings"],
            &second,
            None,
        )
        .unwrap();

    let rows = table
        .query(
            session.as_mut(),
            &Query::new().filter(Where::new().eq("DeviceId", 20)),
        )
        .unwrap();
    let row = rows.rows.first().expect("应有一行");
    assert_eq!(
        row.get_by_name("CreateTime")
            .and_then(|value| value.as_datetime()),
        Some(first_time),
        "CreateTime 属于创建列，不应被冲突更新覆盖"
    );
    assert_eq!(
        row.get_by_name("Memory").and_then(|value| value.as_i64()),
        Some(200),
        "Memory 是普通列，应更新"
    );
    assert_eq!(
        row.get_by_name("Pings").and_then(|value| value.as_i32()),
        Some(4),
        "Pings 是累加列，应在旧值上累加"
    );
    assert_eq!(
        row.get_by_name("Delay").and_then(|value| value.as_i32()),
        Some(2),
        "Delay 是普通列，应更新为新值"
    );

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}
