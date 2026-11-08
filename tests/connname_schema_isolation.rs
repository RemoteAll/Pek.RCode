//! **表绝不能建错库**：多库模型（模型级 `<Option><ConnName>` + 表级 `<Table ConnName="...">`）下，
//! 每个连接只允许拥有属于自己的表。

// `Databases` 在 `database` feature 下；默认 features 不含它，测试整体按 feature 门跳过
#![cfg(feature = "database")]
//!
//! 语义（与 C# `TableItem.ConnName` → `Meta.ConnName` 一致）：
//! - 表上写着 `ConnName="X"` → 该表**只属于 X 连接**；
//! - 表上没写 → 属于**模型级** `<Option><ConnName>` 那个连接。
//!
//! 验证方式是**物理级**的：用同一份模型打开目标 `.db` 文件并真的执行 SQL，
//! 能查到 = 这张表真在那个文件里；查报错 = 真不在。不依赖本库的内部状态或缓存。
//!
//! 注意：本文件只覆盖「配置里已声明的连接」；未注册连接自动建库建表的场景
//! 依赖 `shards` 的**进程级全局**自动目录，放在 `connname_routing.rs` 里串行验证（避免并行互相覆盖）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pek_rcode::database::{ConnectionEntry, DatabaseFile, Databases};
use pek_rcode::{Dal, EntityModel, Query};

/// 模型级 Main；`LocalTable` 不写 ConnName（归 Main），`RemoteTable` 写 IoTData
const MODEL: &str = r#"
<EntityModel>
  <Option><ConnName>Main</ConnName></Option>
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

/// 三个表级连接各占一张表，用来验证「多表级连接互不串库」
const MULTI_MODEL: &str = r#"
<EntityModel>
  <Option><ConnName>Main</ConnName></Option>
  <Tables>
    <Table Name="LocalOnly" TableName="LocalOnly">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      </Columns>
    </Table>
    <Table Name="TableA" TableName="TableA" ConnName="DbA">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      </Columns>
    </Table>
    <Table Name="TableB" TableName="TableB" ConnName="DbB">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      </Columns>
    </Table>
  </Tables>
</EntityModel>
"#;

fn temp_base(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pek-schema-isolation-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("Data")).expect("建临时数据目录");
    dir
}

fn db_file(base: &Path, default: &str, conns: &[(&str, &str)]) -> DatabaseFile {
    let mut connections = BTreeMap::new();
    for (name, file) in conns {
        connections.insert(
            (*name).to_string(),
            ConnectionEntry::Simple(format!(
                "Data Source={};Provider=SQLite",
                base.join("Data").join(file).display()
            )),
        );
    }
    DatabaseFile {
        default: default.to_string(),
        connections,
        ..DatabaseFile::default()
    }
}

/// **物理验证**：这张表是否真的存在于该 `.db` 文件里。
///
/// 做法：用同一份模型另开一个 `Dal`（**不调用 sync_schema**，所以不会「为了检查而建表」），
/// 然后对该表真的执行一次 `SELECT`。能查通 = 表在文件里；报错 = 表不在。
fn table_exists(db_path: &Path, model_xml: &str, table: &str) -> bool {
    assert!(
        db_path.is_file(),
        "库文件不存在：{}（表 {} 无从谈起）",
        db_path.display(),
        table
    );
    let model = EntityModel::parse(model_xml).expect("模型解析");
    let conn = format!("Data Source={};Provider=SQLite", db_path.display());
    let dal = Dal::open_with_model(&conn, model).expect("打开库");
    let mut session = dal.open_session().expect("会话");
    let handle = dal.table(table).expect("取表");
    handle
        .query(session.as_mut(), &Query::new().column("Id"))
        .is_ok()
}

#[test]
fn tables_land_in_their_own_database() {
    let base = temp_base("split");
    let file = db_file(&base, "Main", &[("Main", "main.db"), ("IoTData", "iot.db")]);
    let model = EntityModel::parse(MODEL).expect("模型解析");
    let dbs = Databases::new(&base, file, Some(model));

    // 打开主连接：只允许有 LocalTable
    let main_store = dbs.store("Main").expect("打开主连接");
    let main_db = base.join("Data/main.db");
    assert!(
        table_exists(&main_db, MODEL, "LocalTable"),
        "主库必须有 LocalTable"
    );
    assert!(
        !table_exists(&main_db, MODEL, "RemoteTable"),
        "**主库绝不能有 RemoteTable** —— 它在模型里写死了 ConnName=IoTData"
    );

    // 打开 IoTData 连接：只允许有 RemoteTable
    let iot_store = dbs.store("IoTData").expect("打开 IoTData 连接");
    let iot_db = base.join("Data/iot.db");
    assert!(
        table_exists(&iot_db, MODEL, "RemoteTable"),
        "IoTData 库必须有 RemoteTable"
    );
    assert!(
        !table_exists(&iot_db, MODEL, "LocalTable"),
        "**IoTData 库绝不能有 LocalTable** —— 它没写 ConnName，归模型级 Main"
    );

    drop((main_store, iot_store));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn legacy_conn_name_still_gets_model_level_tables() {
    // 旧配置场景（Pek.Proxy 线上就是这种）：连接名叫 main，而模型级 ConnName 是 Main。
    // 这时 main 就是业务库 → 模型级（无表级 ConnName）的表必须建在它里面，
    // 否则升级后「一张表都没有」；但表级 ConnName 的表仍然不许进来。
    let base = temp_base("legacy");
    let file = db_file(&base, "main", &[("main", "main.db")]);
    let model = EntityModel::parse(MODEL).expect("模型解析");
    let dbs = Databases::new(&base, file, Some(model));

    let store = dbs.store("main").expect("打开旧连接名");
    let db = base.join("Data/main.db");

    assert!(
        table_exists(&db, MODEL, "LocalTable"),
        "模型级连接没有同名配置时，业务库的表要建在当前连接上（否则升级后无表可用）"
    );
    assert!(
        !table_exists(&db, MODEL, "RemoteTable"),
        "表级 ConnName=IoTData 的表绝不能进业务库"
    );

    drop(store);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn multiple_table_level_connections_do_not_cross_contaminate() {
    let base = temp_base("multi");
    let file = db_file(
        &base,
        "Main",
        &[("Main", "main.db"), ("DbA", "a.db"), ("DbB", "b.db")],
    );
    let model = EntityModel::parse(MULTI_MODEL).expect("模型解析");
    let dbs = Databases::new(&base, file, Some(model));

    let main_store = dbs.store("Main").expect("打开 Main");
    let a_store = dbs.store("DbA").expect("打开 DbA");
    let b_store = dbs.store("DbB").expect("打开 DbB");

    let main_db = base.join("Data/main.db");
    let a_db = base.join("Data/a.db");
    let b_db = base.join("Data/b.db");

    // Main：只有 LocalOnly
    assert!(table_exists(&main_db, MULTI_MODEL, "LocalOnly"), "Main 应有 LocalOnly");
    assert!(!table_exists(&main_db, MULTI_MODEL, "TableA"), "Main 不应有 TableA");
    assert!(!table_exists(&main_db, MULTI_MODEL, "TableB"), "Main 不应有 TableB");

    // DbA：只有 TableA
    assert!(table_exists(&a_db, MULTI_MODEL, "TableA"), "DbA 应有 TableA");
    assert!(!table_exists(&a_db, MULTI_MODEL, "LocalOnly"), "DbA 不应有 LocalOnly");
    assert!(!table_exists(&a_db, MULTI_MODEL, "TableB"), "DbA 不应有 TableB");

    // DbB：只有 TableB
    assert!(table_exists(&b_db, MULTI_MODEL, "TableB"), "DbB 应有 TableB");
    assert!(!table_exists(&b_db, MULTI_MODEL, "LocalOnly"), "DbB 不应有 LocalOnly");
    assert!(!table_exists(&b_db, MULTI_MODEL, "TableA"), "DbB 不应有 TableA");

    drop((main_store, a_store, b_store));
    let _ = std::fs::remove_dir_all(&base);
}
