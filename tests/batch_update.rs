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
            row.get_by_name("Memory")
                .and_then(DbValue::as_i64)
                .unwrap_or(-1),
            row.get_by_name("Delay")
                .and_then(DbValue::as_i64)
                .unwrap_or(-1),
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

    assert!(
        table
            .update_batch_by_pk(session.as_mut(), &["Memory"], &[])
            .is_err()
    );
    assert!(
        table
            .update_batch_by_pk(session.as_mut(), &[], &[(DbValue::Int(1), vec![])])
            .is_err()
    );
    // 值个数不匹配
    assert!(
        table
            .update_batch_by_pk(
                session.as_mut(),
                &["Memory", "Delay"],
                &[(DbValue::Int(1), vec![DbValue::Int(1)])],
            )
            .is_err()
    );
    // 列不存在
    assert!(
        table
            .update_batch_by_pk(
                session.as_mut(),
                &["NotExists"],
                &[(DbValue::Int(1), vec![DbValue::Int(1)])],
            )
            .is_err()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **分片守护**：超过数据库变量上限的行数也必须成功（分片前会整批失败）。
///
/// 实测依据（2026-10-29，SQLite，本用例的列集 = 3 列 → 参数 = `行数 × 3 × 2 + 行数`）：
/// - 6000 行 = 30000 参数 → OK；8000 行 = 40000 参数 → `too many SQL variables`
///   （SQLite 的 `SQLITE_MAX_VARIABLE_NUMBER` = 32766；MySQL 协议上限 65535）。
/// - 同一批行走单条 `UPDATE ... CASE` 是 O(行数²)：实测 5000 行单片 1030ms，
///   分片 500 后 1 万行只要 67.7ms。
///
/// 这条用例不验证性能（CI 机器差异大），只守住「不因分片丢行/漏更新」。
#[test]
fn batch_update_spans_rows_beyond_variable_limit() {
    let (dir, dal) = setup("chunked");
    let table = dal.table("DeviceOnline").unwrap();
    let mut session = dal.open_session().unwrap();

    // 12000 行 × 3 列 = 84000 参数，远超 SQLite 上限
    const N: usize = 12_000;
    let seed: Vec<Vec<(&str, DbValue)>> = (0..N)
        .map(|i| {
            vec![
                ("DeviceId", DbValue::Int(i as i64 + 1)),
                ("Memory", DbValue::Int(0)),
                ("CpuRate", DbValue::Float(0.0)),
                ("Delay", DbValue::Int(0)),
            ]
        })
        .collect();
    table
        .insert_batch(session.as_mut(), &seed, Some(500))
        .unwrap();

    let data: Vec<(DbValue, Vec<DbValue>)> = (0..N)
        .map(|i| {
            (
                DbValue::Int(i as i64 + 1),
                vec![
                    DbValue::Int(i as i64 + 100),
                    DbValue::Float(0.5),
                    DbValue::Int(7),
                ],
            )
        })
        .collect();
    let affected = table
        .update_batch_by_pk(session.as_mut(), &["Memory", "CpuRate", "Delay"], &data)
        .expect("分片后应成功（分片前会报 too many SQL variables）");
    assert_eq!(affected, N as u64, "分片不能丢行");

    // 首尾 + 中间各抽查一行，确认值真的写进去了
    for probe in [1i64, (N / 2) as i64, N as i64] {
        let rows = table
            .query(
                session.as_mut(),
                &Query::new()
                    .column("Memory")
                    .column("Delay")
                    .filter(Where::new().eq("DeviceId", probe)),
            )
            .unwrap();
        let row = rows.first().expect("设备行应存在");
        assert_eq!(
            row.get_by_name("Memory").and_then(|v| v.as_i64()),
            Some(probe - 1 + 100),
            "第 {probe} 行的 Memory 应被更新"
        );
        assert_eq!(row.get_by_name("Delay").and_then(|v| v.as_i64()), Some(7));
    }

    let _ = std::fs::remove_dir_all(&dir);
}
