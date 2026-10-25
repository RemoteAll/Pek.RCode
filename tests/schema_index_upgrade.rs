//! 结构同步的**索引唯一性升级**：模型要求唯一、库里同列只有普通索引时必须自动升级。
//!
//! 历史坑：旧逻辑只用「同列且同序」判断索引是否存在，改模型 `Unique="True"` 后
//! `sync_schema` 会静默跳过，导致唯一约束永远不生效（只能手工执行 ALTER）。

use std::path::PathBuf;

use pek_rcode::model::EntityModel;
use pek_rcode::{Dal, DbValue};

/// 普通索引（升级前）。
const MODEL_PLAIN: &str = r#"<EntityModel><Tables><Table Name="DeviceOnline" TableName="DeviceOnline">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="DeviceId" DataType="Int32" />
    <Column Name="Pings" DataType="Int32" />
  </Columns>
  <Indexes>
    <Index Columns="DeviceId" />
  </Indexes>
</Table></Tables></EntityModel>"#;

/// 唯一索引（升级后）。
const MODEL_UNIQUE: &str = r#"<EntityModel><Tables><Table Name="DeviceOnline" TableName="DeviceOnline">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="DeviceId" DataType="Int32" />
    <Column Name="Pings" DataType="Int32" />
  </Columns>
  <Indexes>
    <Index Columns="DeviceId" Unique="True" />
  </Indexes>
</Table></Tables></EntityModel>"#;

fn temp_conn(name: &str) -> (PathBuf, String) {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!(
        "rcode-idx-{}-{}-{name}",
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = format!("Data Source={};Provider=SQLite", dir.join("t.db").display());
    (dir, conn)
}

/// 读取 `DeviceId` 索引的唯一性（找不到返回 `None`）。
fn device_index_unique(dal: &Dal) -> Option<bool> {
    let mut session = dal.open_session().unwrap();
    let rows = session.query("pragma index_list('DeviceOnline')", &[]).unwrap();
    for row in rows.rows.iter() {
        let name = row
            .get_by_name("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_uppercase();
        if name.contains("DEVICEID") {
            return Some(
                row.get_by_name("unique")
                    .and_then(DbValue::as_i64)
                    .is_some_and(|v| v != 0),
            );
        }
    }
    None
}

/// 已有普通索引 → 模型改唯一 → `sync_schema` 自动删旧建新（唯一）。
#[test]
fn sync_schema_upgrades_plain_index_to_unique() {
    let (dir, conn) = temp_conn("upgrade");

    {
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL_PLAIN).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        assert_eq!(device_index_unique(&dal), Some(false), "初始应为普通索引");
    }

    {
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL_UNIQUE).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert!(
            report
                .added_indexes
                .iter()
                .any(|(_, name)| name.to_uppercase().contains("DEVICEID")),
            "应报告补建/升级索引：{:?}",
            report.added_indexes
        );
        assert_eq!(
            device_index_unique(&dal),
            Some(true),
            "升级后必须是唯一索引（同一设备只允许一行在线记录）"
        );
    }

    // 幂等：再次同步不应重复建/报错，且保持唯一
    {
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL_UNIQUE).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert!(report.added_indexes.is_empty(), "已存在唯一索引时不应重复建");
        assert_eq!(device_index_unique(&dal), Some(true));
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 反向：库里是唯一索引、模型写普通索引时**不降级**（唯一约束不应被悄悄去掉）。
#[test]
fn sync_schema_never_downgrades_unique_index() {
    let (dir, conn) = temp_conn("no-downgrade");

    {
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL_UNIQUE).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        assert_eq!(device_index_unique(&dal), Some(true));
    }

    {
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL_PLAIN).unwrap()).unwrap();
        let report = dal.sync_schema().unwrap();
        assert!(report.added_indexes.is_empty());
        assert_eq!(
            device_index_unique(&dal),
            Some(true),
            "不应降级为普通索引"
        );
    }

 

/// 「该清的清」：模型里没有的索引由 `sync_indexes` 自动删除（主键/系统索引除外）。
#[test]
fn sync_indexes_drops_extra_index_not_in_model() {
    let (dir, conn) = temp_conn("drop-extra");

    {
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL_PLAIN).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        // 手工加一个模型里没有的索引，模拟历史遗留
        let mut session = dal.open_session().unwrap();
        session
            .execute("create index ix_extra_pings on DeviceOnline(Pings)", &[])
            .unwrap();
    }

    {
        // 模型只有 DeviceId 索引 → Pings 的索引属"多余"，应被清理
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL_PLAIN).unwrap()).unwrap();
        let report = dal.sync_indexes().unwrap();
        assert!(
            report
                .dropped_indexes
                .iter()
                .any(|(_, name)| name.to_lowercase().contains("extra")),
            "应删除模型外的索引：{:?}",
            report.dropped_indexes
        );

        let mut session = dal.open_session().unwrap();
        let rows = session.query("pragma index_list('DeviceOnline')", &[]).unwrap();
        let names: Vec<String> = rows
            .rows
            .iter()
            .filter_map(|r| r.get_by_name("name").and_then(|v| v.as_str().map(|s| s.to_lowercase())))
            .collect();
        assert!(
            !names.iter().any(|n| n.contains("extra")),
            "多余索引应已被删除：{names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("deviceid")),
            "模型里的索引必须保留：{names:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}   let _ = std::fs::remove_dir_all(&dir);
}
