//! 真实 MySQL 实例上的能力回归：JSON 列 + `AS _new` 行别名 Upsert。
//!
//! 为什么必须跑真机：这两项都只在服务端版本满足条件时才生效，SQLite/MariaDB 都验证不到——
//! - `upsert_row_alias`：MySQL `8.0.19+` 支持 `INSERT ... VALUES (...) AS _new ON DUPLICATE KEY UPDATE c = _new.c`；
//!   MariaDB 不支持（只能 `VALUES(c)`），版本探测失败时也保持关闭。
//! - JSON 列：MySQL 5.7.8+ 的原生 `JSON` 类型（紧凑二进制 + 服务端校验），
//!   读取时驱动给的是字符集 binary 的字节（带 `BINARY_FLAG`），必须还原成文本。
//!
//! 运行方式（本机绿色版见 `scripts/mysql-test-env.ps1`）：
//! ```text
//! pwsh scripts\mysql-test-env.ps1 start
//! $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None"
//! cargo test --test mysql_features_live
//! ```
//!
//! 安全说明：只创建/删除 `rcode_live_` 前缀的专用表，不触碰其它表。

use pek_rcode::model::EntityModel;
use pek_rcode::{Dal, DbRow, DbValue, Query, Where};

const MODEL: &str = r#"<EntityModel><Tables><Table Name="RCodeLiveDoc" TableName="rcode_live_doc">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="DeviceId" DataType="Int32" />
    <Column Name="Payload" DataType="Json" Nullable="True" />
    <Column Name="Pings" DataType="Int32" />
    <Column Name="CreateTime" DataType="DateTime" Nullable="True" />
  </Columns>
  <Indexes>
    <Index Columns="DeviceId" Unique="True" />
  </Indexes>
</Table></Tables></EntityModel>"#;

const PAYLOAD: &str = r#"{"tags":["a","b"],"count":3,"title":"清单","meta":{"ok":true}}"#;

fn conn() -> Option<String> {
    match std::env::var("RCODE_MYSQL") {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => {
            eprintln!("未设置 RCODE_MYSQL，跳过真实 MySQL 能力回归（参见文件头运行说明）");
            None
        }
    }
}

fn open() -> Dal {
    let conn = conn().expect("调用方应先检查环境变量");
    Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap()
}

fn cleanup(dal: &Dal) {
    if let Ok(mut session) = dal.open_session() {
        let _ = session.execute("DROP TABLE IF EXISTS rcode_live_doc", &[]);
    }
}

/// 读取单元格文本：MySQL 8 的 `information_schema` 列是 **binary charset**，
/// 驱动会返回 `DbValue::Blob`，与 `catalog.rs` 的 `text_any` 保持同一口径。
fn cell_text(row: &DbRow, i: usize) -> String {
    match row.get(i) {
        Some(value) => match value.as_str() {
            Some(text) => text.to_string(),
            None => value
                .as_blob()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .unwrap_or_default()
                .to_string(),
        },
        None => String::new(),
    }
}

/// 版本探测 → 行别名分流必须与真实服务端一致。
#[test]
fn mysql_server_uses_row_alias_and_reports_version() {
    let Some(_) = conn() else { return };
    let dal = open();
    dal.detect_server().unwrap();

    let info = dal.server_info().expect("连接成功后应有服务端版本信息");
    assert!(!info.is_mariadb, "绿色版应是 MySQL 而非 MariaDB：{}", info.raw);
    assert!(
        info.at_least(8, 0, 19),
        "本机 MySQL 应 >= 8.0.19 才能验证行别名：{}",
        info.raw
    );
    assert!(
        dal.sql_features().upsert_row_alias,
        "8.0.19+ 应启用 `AS _new` 行别名：{}",
        info.raw
    );

    let label = info.label();
    assert!(
        label.starts_with("Server 8."),
        "8.x 的版本摘要应为 `Server 8.x.y`：{label}"
    );
    cleanup(&dal);
}

/// 建表用原生 `JSON` 类型，写入/读回保持 JSON 原文。
#[test]
fn mysql_json_column_stores_native_type_and_roundtrips() {
    let Some(_) = conn() else { return };
    let dal = open();
    dal.sync_schema().unwrap();
    let table = dal.table("RCodeLiveDoc").unwrap();
    let mut session = dal.open_session().unwrap();

    // 建表类型必须是原生 json（而不是 longtext）
    let db_name = session
        .query("SELECT DATABASE()", &[])
        .unwrap()
        .first()
        .map(|row| cell_text(row, 0))
        .unwrap_or_default();
    assert!(!db_name.is_empty(), "连接应有默认库（连接串里的 Database）");

    let columns = session
        .query(
            "SELECT DATA_TYPE FROM information_schema.columns \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'rcode_live_doc' \
               AND COLUMN_NAME = 'Payload'",
            &[DbValue::Text(db_name)],
        )
        .unwrap();
    let data_type = columns
        .first()
        .map(|row| cell_text(row, 0))
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert_eq!(data_type, "json", "Payload 应为 MySQL 原生 JSON 列");

    let id = table
        .insert(
            session.as_mut(),
            &[
                ("DeviceId", DbValue::Int(1)),
                ("Payload", DbValue::Text(PAYLOAD.to_string())),
                ("Pings", DbValue::Int(1)),
            ],
        )
        .unwrap();

    let rows = table
        .query(
            session.as_mut(),
            &Query::new().filter(Where::new().eq("Id", id)).take(1),
        )
        .unwrap();
    let read = rows
        .first()
        .and_then(|row| row.get_by_name("Payload"))
        .and_then(DbValue::as_str)
        .expect("JSON 列应读回文本");
    // MySQL 存储 JSON 会规范化键序与空白，这里按语义比较（解析后重新序列化不可行，
    // 故只断言关键片段都在、且不是被当成 Blob 的十六进制）
    assert!(read.contains("\"count\""), "应保留键 count：{read}");
    assert!(read.contains("清单"), "应保留中文取值：{read}");
    assert!(!read.starts_with("0x"), "不应被当成二进制读取：{read}");

    // 服务端 JSON 函数能直接用在该列上（`CAST ... AS SIGNED` 让返回值是整数而不是 JSON）
    let stats = session
        .query(
            "SELECT CAST(JSON_EXTRACT(Payload, '$.count') AS SIGNED) AS n \
             FROM rcode_live_doc WHERE Id = ?",
            &[DbValue::Int(id)],
        )
        .unwrap();
    assert_eq!(
        stats.first().and_then(|row| row.get(0)).and_then(DbValue::as_i64),
        Some(3),
        "JSON_EXTRACT 应能取到数字 3"
    );

    cleanup(&dal);
}

/// `AS _new` 行别名 Upsert：累加列用新值相加、`CreateTime` 保持首次插入值。
#[test]
fn mysql_upsert_batch_by_unique_uses_row_alias() {
    let Some(_) = conn() else { return };
    let dal = open();
    dal.sync_schema().unwrap();
    let table = dal.table("RCodeLiveDoc").unwrap();
    let mut session = dal.open_session().unwrap();

    let first_time = chrono::NaiveDate::from_ymd_opt(2026, 9, 1)
        .unwrap()
        .and_hms_opt(1, 0, 0)
        .unwrap();
    let second_time = chrono::NaiveDate::from_ymd_opt(2026, 9, 2)
        .unwrap()
        .and_hms_opt(5, 0, 0)
        .unwrap();

    let columns = ["DeviceId", "Payload", "Pings", "CreateTime"];
    let first = vec![
        vec![
            DbValue::Int(10),
            DbValue::Text(PAYLOAD.to_string()),
            DbValue::Int(1),
            DbValue::DateTime(first_time),
        ],
        vec![
            DbValue::Int(11),
            DbValue::Text(PAYLOAD.to_string()),
            DbValue::Int(2),
            DbValue::DateTime(first_time),
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

    // 第二批：Pings 在旧值上累加（1+3=4 / 2+4=6），CreateTime 不被覆盖
    let second = vec![
        vec![
            DbValue::Int(10),
            DbValue::Text(PAYLOAD.to_string()),
            DbValue::Int(3),
            DbValue::DateTime(second_time),
        ],
        vec![
            DbValue::Int(11),
            DbValue::Text(PAYLOAD.to_string()),
            DbValue::Int(4),
            DbValue::DateTime(second_time),
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

    for (device, pings) in [(10, 4), (11, 6)] {
        let rows = table
            .query(
                session.as_mut(),
                &Query::new()
                    .filter(Where::new().eq("DeviceId", device))
                    .take(1),
            )
            .unwrap();
        let row = rows.first().expect("Upsert 后行应存在");
        assert_eq!(
            row.get_by_name("Pings").and_then(DbValue::as_i32),
            Some(pings),
            "设备 {device} 的 Pings 应在旧值上累加"
        );
        assert_eq!(
            row.get_by_name("CreateTime").and_then(DbValue::as_datetime),
            Some(first_time),
            "设备 {device} 的 CreateTime 应保持首次插入值"
        );
    }

    cleanup(&dal);
}
