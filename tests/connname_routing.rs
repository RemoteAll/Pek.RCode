//! 表级 `ConnName` 自动分库路由（对齐 C# `TableItem.ConnName` → `Meta.ConnName`）。
//!
//! 验证点：
//! 1. 表配了 `ConnName="IoTData"` → `Dal::with_table_session` 自动落到该连接；
//! 2. 未注册的连接名按 C# `DAL.Init` 规则自动建 `{自动目录}/IoTData.db`（SQLite）；
//! 3. 写入只落在 `IoTData.db`，主库同名表保持 0 行；
//! 4. 未配 ConnName 的表仍走主库；
//! 5. 表级 ConnName 与模型级同名时视为同一连接，不切换。
//!
//! 说明：`shards` 的连接注册表与自动库目录是**进程级全局状态**，因此本文件只用一个
//! `#[test]` 顺序执行全部场景（多测试并行会互相覆盖全局目录，导致假失败）。

use std::path::Path;

use pek_rcode::{Dal, EntityModel, Query, shards};

const MODEL: &str = r#"
<EntityModel>
  <Tables>
    <Table Name="LocalTable" TableName="LocalTable">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Name" DataType="String" Length="50" Nullable="True" />
      </Columns>
    </Table>
    <Table Name="RemoteTable" TableName="RemoteTable" ConnName="IoTData">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Name" DataType="String" Length="50" Nullable="True" />
      </Columns>
    </Table>
  </Tables>
</EntityModel>
"#;

fn conn(path: &Path) -> String {
    format!("Data Source={};Provider=SQLite", path.display())
}

fn count_rows(path: &Path, table: &str, model_xml: &str) -> usize {
    let model = EntityModel::parse(model_xml).expect("模型解析");
    let dal = Dal::open_with_model(&conn(path), model).expect("打开库");
    let mut session = dal.open_session().expect("会话");
    let t = dal.table(table).expect("取表");
    t.query(session.as_mut(), &Query::new().column("Id"))
        .expect("查询")
        .len()
}

#[test]
fn table_conn_name_routing_matrix() {
    // —— 场景 A：表级 ConnName 自动分库 ——
    let dir = std::env::temp_dir().join(format!("pek-rcode-connname-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    shards::set_auto_db_dir(&dir);
    shards::clear_connections();

    let main_db = dir.join("main.db");
    let remote_db = dir.join("IoTData.db");
    let model = EntityModel::parse(MODEL).expect("模型解析");
    let dal = Dal::open_with_model(&conn(&main_db), model).expect("打开主库");
    dal.sync_schema().expect("同步主库结构");

    let target = dal
        .table_conn_dal("RemoteTable")
        .expect("解析连接")
        .expect("应解析出 IoTData 连接");
    assert!(
        shards::registered_connection("iotdata").is_some(),
        "自动连接应已注册"
    );
    target.sync_schema().expect("同步 IoTData 结构");
    assert!(
        remote_db.exists(),
        "应自动创建 IoTData.db：{}（自动目录={}）",
        remote_db.display(),
        shards::auto_db_dir().display()
    );

    dal.with_table_session("RemoteTable", |_dal, table, session| {
        table.insert(session, &[("Name", "remote-1".into())])?;
        Ok(())
    })
    .expect("写入 RemoteTable");
    dal.with_table_session("LocalTable", |_dal, table, session| {
        table.insert(session, &[("Name", "local-1".into())])?;
        Ok(())
    })
    .expect("写入 LocalTable");

    assert_eq!(
        count_rows(&remote_db, "RemoteTable", MODEL),
        1,
        "IoTData 库应有 1 行"
    );
    assert_eq!(
        count_rows(&main_db, "RemoteTable", MODEL),
        0,
        "主库不应有 RemoteTable 数据"
    );
    assert_eq!(
        count_rows(&main_db, "LocalTable", MODEL),
        1,
        "主库应有 LocalTable 数据"
    );

    shards::clear_connections();

    // —— 场景 B：表级 ConnName 与模型级同名 → 不切换 ——
    let model_xml = MODEL.replace(
        "<EntityModel>",
        "<EntityModel><Option><ConnName>IoTData</ConnName></Option>",
    );
    let model = EntityModel::parse(&model_xml).expect("模型解析");
    let db = dir.join("same.db");
    let dal = Dal::open_with_model(&conn(&db), model).expect("打开库");
    assert!(
        dal.table_conn_dal("RemoteTable")
            .expect("解析连接")
            .is_none(),
        "表级 ConnName 与模型级相同 → 不切换连接"
    );

    shards::clear_connections();

    // —— 场景 C：按连接建表（多库模型的正确姿势）——
    // 模型级 Main，RemoteTable 指向 IoTData：主库只能有 LocalTable，IoTData 库只建 RemoteTable
    let multi = MODEL.replace(
        "<EntityModel>",
        "<EntityModel><Option><ConnName>Main</ConnName></Option>",
    );
    let model = EntityModel::parse(&multi).expect("模型解析");

    let main_db = dir.join("c-main.db");
    let main_dal = Dal::open_with_model(&conn(&main_db), model.clone()).expect("打开主库");
    let report = main_dal
        .sync_schema_for(Some("Main"), true, false)
        .expect("按连接同步主库");
    assert!(
        report.created_tables.iter().any(|t| t == "LocalTable"),
        "主库应建 LocalTable：{report:?}"
    );
    assert!(
        !report.created_tables.iter().any(|t| t == "RemoteTable"),
        "主库**不应**建 RemoteTable（它属于 IoTData 连接）：{report:?}"
    );
    // 主库里确实查不到 RemoteTable（表不存在 → 查询报错）
    {
        let mut session = main_dal.open_session().expect("会话");
        let table = main_dal.table("RemoteTable").expect("取表");
        assert!(
            table.query(session.as_mut(), &Query::new().column("Id")).is_err(),
            "主库不应存在 RemoteTable 表"
        );
    }

    // 表级连接：解析即自动建库 + 按该连接建表（无需调用方手动 sync_schema）
    let auto = main_dal
        .table_conn_dal("RemoteTable")
        .expect("解析连接")
        .expect("应解析出 IoTData 连接");
    let mut auto_session = auto.open_session().expect("会话");
    let auto_table = auto.table("RemoteTable").expect("取表");
    assert!(
        auto_table
            .query(auto_session.as_mut(), &Query::new().column("Id"))
            .is_ok(),
        "自动建的表级连接库里应有 RemoteTable 表（不需要调用方手动 sync_schema）"
    );

    shards::clear_connections();
    let _ = std::fs::remove_dir_all(&dir);
}
