//! 反向工程端到端：生产固件模型 → 临时 SQLite 建库 → 反向读回 → 与源模型逐表逐列对比。
//!
//! 验证链路（与 C# 端 `DAL.GetTables` 对照）：
//! ```text
//! Model.xml ──sync_schema──▶ SQLite 库 ──read_model──▶ 反向模型 ──to_xml──▶ Model.xml ──codegen──▶ 实体
//! ```

use std::path::PathBuf;

use pek_rcode::{Dal, EntityModel, codegen, types::DataType};

/// 测试固件：生产 WMS 模型快照样本（7 张真实表，覆盖全部数据类型）。
const SAMPLE_MODEL: &str = include_str!("fixtures/wms_model_sample.xml");

/// 临时目录（每次唯一，避免并行冲突）。
fn temp_dir(name: &str) -> PathBuf {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!(
        "rcode-reverse-e2e-{}-{stamp}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn reverse_roundtrip_with_production_fixture() {
    let dir = temp_dir("fixture");
    let db = dir.join("reverse.db");

    let source = EntityModel::parse(SAMPLE_MODEL).expect("固件模型应可解析");
    let table_count = source.tables.len();

    // 正向：建库
    let dal = Dal::open_with_model(
        &format!("Data Source={};Provider=SQLite", db.display()),
        source.clone(),
    )
    .unwrap();
    let report = dal.sync_schema().unwrap();
    assert_eq!(report.created_tables.len(), table_count, "{report}");

    // 反向：读回结构
    let reversed = dal.read_model().unwrap();
    assert_eq!(reversed.tables.len(), table_count);

    // 逐表逐列对比（SQLite 的 DECIMAL 不带精度参数，Precision/Scale 不参与对比）
    for src in &source.tables {
        let table_name = src.effective_table_name();
        let got = reversed
            .tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(table_name))
            .unwrap_or_else(|| panic!("反向结果缺少表 {table_name}"));
        assert_eq!(
            got.columns.len(),
            src.columns.len(),
            "表 {table_name} 列数不一致"
        );

        for col in &src.columns {
            let actual = got.column(&col.name).unwrap_or_else(|| {
                panic!("表 {table_name} 反向结果缺少列 {}", col.name)
            });
            assert_eq!(
                actual.data_type, col.data_type,
                "列 {table_name}.{} 类型",
                col.name
            );
            assert_eq!(
                actual.primary_key, col.primary_key,
                "列 {table_name}.{} 主键",
                col.name
            );
            assert_eq!(
                actual.identity, col.identity,
                "列 {table_name}.{} 自增",
                col.name
            );
            assert_eq!(
                actual.nullable, col.nullable,
                "列 {table_name}.{} 可空",
                col.name
            );
            if col.data_type == DataType::String {
                assert_eq!(
                    actual.length, col.length,
                    "列 {table_name}.{} 长度",
                    col.name
                );
            }
        }
    }

    // 反向模型写出 XML 后可再次解析，且能直接生成实体代码（反向链路闭环）
    let xml = reversed.to_xml();
    let again = EntityModel::parse(&xml).expect("反向模型应可再次解析");
    assert_eq!(again.tables.len(), table_count);
    let files = codegen::generate_all(&again);
    assert_eq!(files.len(), table_count);

    let _ = std::fs::remove_dir_all(&dir);
}
