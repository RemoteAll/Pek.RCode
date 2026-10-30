//! 视图表（`IsView`）端到端测试：写保护 + 跳过结构迁移 + 查询可用。
//!
//! 对齐 DH.NCode `TableItem.IsView`：`EntitySession` 中所有增删改都抛
//! 「视图无法添删改！」，且 `CheckModel`/`InitData` 等结构迁移全部跳过。

use std::path::PathBuf;

use pek_rcode::Dal;
use pek_rcode::dal::ConnectionString;
use pek_rcode::model::EntityModel;
use pek_rcode::query::{Query, Where};
use pek_rcode::value::DbValue;

const MODEL: &str = r#"<EntityModel><Tables>
<Table Name="V_Order" TableName="v_order" Description="订单视图" IsView="True">
  <Columns>
    <Column Name="Id" DataType="Int32" PrimaryKey="True" />
    <Column Name="Code" DataType="String" Length="50" Nullable="True" />
  </Columns>
</Table>
<Table Name="Order" TableName="DH_Order" Description="订单">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="Code" DataType="String" Length="50" Nullable="True" />
  </Columns>
</Table>
</Tables></EntityModel>"#;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pek-rcode-view-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 建库：实体表 + 真实 SQLite 视图 + 两行数据。
fn setup(tag: &str) -> (PathBuf, Dal) {
    let dir = temp_dir(tag);
    let db = dir.join("test.db");
    let conn = format!("Data Source={};Provider=SQLite", db.display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    let report = dal.sync_schema().unwrap();

    // 视图不参与结构迁移（对齐 C#：`if (DataTable.IsView) return`）
    assert!(
        !report.created_tables.iter().any(|t| t == "v_order"),
        "视图不应被建表：{report}"
    );
    assert!(
        !report.added_columns.iter().any(|(t, _)| t == "v_order"),
        "视图不应补列：{report}"
    );
    assert!(
        report.created_tables.iter().any(|t| t == "DH_Order"),
        "{report}"
    );

    let mut s = dal.open_session().unwrap();
    s.execute("create view v_order as select Id, Code from DH_Order", &[])
        .unwrap();
    s.execute(
        "insert into DH_Order (Code) values (?), (?)",
        &[DbValue::Text("A".into()), DbValue::Text("B".into())],
    )
    .unwrap();
    (dir, dal)
}

#[test]
fn view_table_is_marked_and_readable() {
    let (dir, dal) = setup("read");
    let table = dal.table("V_Order").unwrap();
    assert!(table.is_view());
    assert!(table.meta().is_view);

    let mut s = dal.open_session().unwrap();
    let rows = table.query(&mut *s, &Query::new()).unwrap();
    assert_eq!(rows.len(), 2, "视图可以正常查询");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn view_table_rejects_all_writes() {
    let (dir, dal) = setup("write");
    let table = dal.table("V_Order").unwrap();
    let mut s = dal.open_session().unwrap();

    // 单行插入
    let err = table
        .insert(&mut *s, &[("Code", DbValue::Text("X".into()))])
        .unwrap_err()
        .to_string();
    assert!(err.contains("视图无法添删改"), "{err}");

    // 批量写入
    let rows = vec![vec![("Code", DbValue::Text("Y".into()))]];
    let err = table
        .write_batch(
            &mut *s,
            &rows,
            pek_rcode::sqlbuild::BatchWriteMode::Insert,
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("视图无法添删改"), "{err}");

    // 按主键更新
    let err = table
        .update_by_pk(&mut *s, &[("Code", DbValue::Text("Z".into()))], &[1.into()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("视图无法添删改"), "{err}");

    // 按主键删除 / 条件删除 / 分批删除
    let err = table
        .delete_by_pk(&mut *s, &[1.into()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("视图无法添删改"), "{err}");
    let err = table
        .delete_where(&mut *s, &Where::new().eq("Code", "A"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("视图无法添删改"), "{err}");
    let err = table
        .delete_where_batched(&mut *s, &Where::new().eq("Code", "A"), None, None, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("视图无法添删改"), "{err}");

    // 视图不建分表
    assert!(
        !dal.ensure_shard_table("V_Order", "v_order_20260101")
            .unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn name_starting_with_hash_is_view() {
    // 对齐 C# `bt.IsView || bt.Name[0] == '#'`
    let xml = r##"<EntityModel><Tables>
<Table Name="#V_Stat" TableName="v_stat">
  <Columns><Column Name="Id" DataType="Int32" PrimaryKey="True" /></Columns>
</Table>
</Tables></EntityModel>"##;
    let model = EntityModel::parse(xml).unwrap();
    assert!(model.table("#V_Stat").unwrap().is_view);

    // 往返保留 IsView
    let out = model.to_xml();
    assert!(out.contains("IsView=\"true\""), "{out}");
    assert!(
        EntityModel::parse(&out)
            .unwrap()
            .table("#V_Stat")
            .unwrap()
            .is_view
    );
}

#[test]
fn connection_string_readonly_flag_is_parsed() {
    // 连接级只读（DbBase.Readonly）与视图（表级）是两回事，互不影响
    let conn = ConnectionString::parse("Data Source=a.db;Provider=SQLite;Readonly=true");
    assert_eq!(conn.get("readonly"), Some("true"));
}
