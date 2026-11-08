//! JSON 列（`DataType="Json"`）：建表类型映射、模型往返与真实 SQLite 读写。
//!
//! 设计口径：
//! - JSON 在值层面就是文本（`DbValue::Text`），实体字段仍是 `String`，
//!   因此 C# 侧把该列当 `String` 用完全等价；
//! - 差别只在「建表时用各库的原生 JSON 类型」与「反向工程把 `json`/`jsonb` 识别为 `Json`」；
//! - SQLite 没有 JSON 类型：`json` 类型名会得到 NUMERIC 亲和度，把 JSON 数字字面量转成数字，
//!   所以 SQLite 一律建 `text`（JSON1 / JSONB 函数对 TEXT 同样生效）。

use std::path::PathBuf;

use pek_rcode::dialect::DatabaseKind;
use pek_rcode::model::{ColumnMeta, EntityModel};
use pek_rcode::{Dal, DbValue, Query, Where};

const MODEL: &str = r#"<EntityModel><Tables><Table Name="Doc" TableName="Doc">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="Payload" DataType="Json" />
    <Column Name="Note" DataType="String" Length="50" Nullable="True" />
  </Columns>
</Table></Tables></EntityModel>"#;

/// 测试用 JSON 原文（含数组、数字、中文与嵌套对象）。
const PAYLOAD: &str = r#"{"tags":["a","b"],"count":3,"title":"清单","meta":{"ok":true}}"#;

fn payload_column() -> ColumnMeta {
    let model = EntityModel::parse(MODEL).unwrap();
    model.tables[0]
        .column("Payload")
        .cloned()
        .expect("模型应含 Payload 列")
}

fn setup(name: &str) -> (PathBuf, Dal) {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!(
        "rcode-json-{}-{}-{name}",
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = format!("Data Source={};Provider=SQLite", dir.join("t.db").display());
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    dal.sync_schema().unwrap();
    (dir, dal)
}

/// 每种数据库各自的建表类型：有原生 JSON 就映射原生，没有则退化为最合适的大文本。
#[test]
fn json_maps_to_native_type_per_database() {
    let col = payload_column();
    let cases = [
        // MySQL 5.7.8+ 原生 JSON（紧凑二进制 + 服务端校验）
        (DatabaseKind::MySql, "JSON"),
        // PostgreSQL 用 jsonb（二进制 + 去重键 + 可建索引）
        (DatabaseKind::PostgreSql, "jsonb"),
        (DatabaseKind::KingBase, "jsonb"),
        (DatabaseKind::HighGo, "jsonb"),
        (DatabaseKind::VastBase, "jsonb"),
        // SQLite 必须 TEXT（`json` 类型名会得到 NUMERIC 亲和度，破坏 JSON 字面量）
        (DatabaseKind::Sqlite, "text"),
        // DuckDB 有原生 JSON 逻辑类型
        (DatabaseKind::DuckDb, "JSON"),
        // 无原生类型的库退化为大文本 / 大对象
        (DatabaseKind::SqlServer, "nvarchar(max)"),
        (DatabaseKind::Oracle, "clob"),
        (DatabaseKind::Firebird, "BLOB SUB_TYPE TEXT"),
        (DatabaseKind::ClickHouse, "String"),
        (DatabaseKind::TDengine, "TEXT"),
        (DatabaseKind::Hana, "NCLOB"),
        (DatabaseKind::Db2, "CLOB"),
        (DatabaseKind::DaMeng, "CLOB"),
        (DatabaseKind::Iris, "LONGVARCHAR"),
        (DatabaseKind::Access, "MEMO"),
    ];
    for (kind, expected) in cases {
        assert_eq!(kind.field_type(&col), expected, "{kind:?} 的 JSON 列类型");
    }
}

/// SQLite 端到端：JSON 原文原样落库、原样读回（不经重排序或转义）。
#[test]
fn json_column_roundtrips_on_sqlite() {
    let (dir, dal) = setup("roundtrip");
    let table = dal.table("Doc").unwrap();
    let mut session = dal.open_session().unwrap();

    let id = table
        .insert(
            session.as_mut(),
            &[
                ("Payload", DbValue::Text(PAYLOAD.to_string())),
                ("Note", DbValue::Text("首条".into())),
            ],
        )
        .unwrap();

    let rows = table
        .query(
            session.as_mut(),
            &Query::new().filter(Where::new().eq("Id", id)).take(1),
        )
        .unwrap();
    let row = rows.first().expect("刚插入的行应存在");
    let read = row
        .get_by_name("Payload")
        .and_then(DbValue::as_str)
        .expect("Payload 应为文本");
    assert_eq!(read, PAYLOAD, "JSON 原文应原样往返");

    // 服务端 JSON 函数可直接作用在该列上（SQLite 的 TEXT 同样支持 JSON1）
    let mut query = Query::new().filter(Where::new().eq("Id", id));
    query
        .select
        .push("json_extract(\"Payload\", '$.count')".to_string());
    let count = table.query(session.as_mut(), &query).unwrap();
    let extracted = count
        .first()
        .and_then(|row| row.get_by_name("json_extract(\"Payload\", '$.count')").cloned())
        .and_then(|v| v.as_i64());
    assert_eq!(extracted, Some(3), "json_extract 应能取到数字 3");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 模型 XML 往返：`Json` 不会被降级成 `String`，`jsonb` 别名也能解析。
#[test]
fn json_type_survives_model_xml_roundtrip() {
    let model = EntityModel::parse(MODEL).unwrap();
    let xml = model.to_xml();
    assert!(
        xml.contains(r#"DataType="Json""#),
        "写出模型应保留 Json：{xml}"
    );

    let again = EntityModel::parse(&xml).unwrap();
    assert_eq!(
        again.tables[0].column("Payload").unwrap().data_type.name(),
        "Json",
        "二次解析后仍是 Json"
    );

    // 反向工程/手写模型用 `jsonb` 写法也能识别
    let alias = MODEL.replace(r#"DataType="Json""#, r#"DataType="jsonb""#);
    let parsed = EntityModel::parse(&alias).unwrap();
    assert_eq!(
        parsed.tables[0].column("Payload").unwrap().data_type.name(),
        "Json"
    );
}

/// C# 兼容口径：JSON 列的 Rust 基础类型与 `String` 相同，实体按字符串承载。
#[test]
fn json_column_uses_string_storage_type() {
    let col = payload_column();
    // 与 String 无长度列的存储类型一致（SQLite 下均为 text）
    assert_eq!(col.data_type.rust_type(), "String");
    assert_eq!(col.data_type.name(), "Json");
}
