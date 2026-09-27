//! 真实 SQLite 历史库兼容性验证（**副本库**，环境变量门控，默认自动跳过）。
//!
//! 目的：用生产历史库的**副本**验证 Pek.RCode 与 C#/.NET 端“共库”的能力：
//! 1. 库中既存表全部可读（逐表 `COUNT(*)` 冒烟）
//! 2. `sync_schema` 只做增量（建缺失的表 / 补缺失的列），**不修改、不删除已有数据**
//! 3. 增量同步幂等（二次同步零变更）
//! 4. 同步前后数据行数完全一致（数据保护）
//! 5. 同步后模型中的全部表在库中就位
//! 6. 抽样表做真实 CRUD 往返（自增主键与字符串主键两条路径）
//!
//! 运行方式（务必指向**副本**，勿操作生产库）：
//!
//! ```powershell
//! Copy-Item <生产库路径> $env:TEMP\DG-live-copy.db -Force
//! $env:RCODE_LIVE_DB = "$env:TEMP\DG-live-copy.db"
//! $env:RCODE_MODEL   = "<项目>\Entity\Model.xml"
//! cargo test --test live_sqlite_e2e -- --nocapture
//! ```

use std::collections::{BTreeMap, BTreeSet};

use pek_rcode::{Dal, EntityModel};

/// 双引号标识符转义（表名来自 `sqlite_master`，可信，但仍按规则转义）。
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 读取单值计数（`COUNT(*)`）。
fn count_of(
    session: &mut dyn pek_rcode::session::SqlSession,
    table: &str,
) -> Option<i64> {
    let sql = format!("SELECT COUNT(*) FROM {}", quote_ident(table));
    session
        .query(&sql, &[])
        .ok()
        .and_then(|set| set.first().and_then(|row| row.get(0)).and_then(|v| v.as_i64()))
}

#[test]
fn live_sqlite_incremental_sync_and_smoke() {
    let (Ok(db_path), Ok(model_path)) =
        (std::env::var("RCODE_LIVE_DB"), std::env::var("RCODE_MODEL"))
    else {
        eprintln!("未设置 RCODE_LIVE_DB / RCODE_MODEL，跳过真实库验证");
        return;
    };
    // 防呆：本项目生产库位于 BinWeb/Data，验证必须使用副本
    assert!(
        !db_path.replace('/', "\\").contains("BinWeb"),
        "为保护生产库，RCODE_LIVE_DB 请指向副本路径（当前：{db_path}）"
    );

    let model = EntityModel::load(std::path::Path::new(&model_path)).expect("模型应可解析");
    let model_tables: BTreeSet<String> = model
        .tables
        .iter()
        .map(|t| t.effective_table_name().to_string())
        .collect();

    let dal =
        Dal::open_with_model(&format!("Data Source={db_path};Provider=SQLite"), model).unwrap();
    let mut session = dal.open_session().unwrap();

    // 1) 现有表清单（排除 SQLite 内部表）
    let set = session
        .query(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
            &[],
        )
        .unwrap();
    let existing: Vec<String> = set
        .rows
        .iter()
        .filter_map(|row| row.get(0).and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    eprintln!("库中现有表：{} 张", existing.len());

    // 2) 行数快照（同时作为“既存表全部可读”的冒烟验证）
    let mut snapshot: BTreeMap<String, i64> = BTreeMap::new();
    let mut smoke_failures: Vec<String> = Vec::new();
    for table in &existing {
        match count_of(session.as_mut(), table) {
            Some(total) => {
                snapshot.insert(table.clone(), total);
            }
            None => smoke_failures.push(table.clone()),
        }
    }
    assert!(
        smoke_failures.is_empty(),
        "既存表应全部可读，失败：{smoke_failures:?}"
    );
    let non_empty = snapshot.values().filter(|v| **v > 0).count();
    eprintln!(
        "冒烟通过：{} 张表可读（其中 {non_empty} 张有数据）",
        snapshot.len()
    );

    // 3) 增量同步（对副本执行：建缺失的表 / 补缺失的列）
    drop(session);
    let report = dal.sync_schema().expect("增量同步应成功");
    eprintln!("增量同步报告：{report}");

    // 4) 幂等：二次同步应零变更
    assert!(
        dal.sync_schema().unwrap().is_empty(),
        "二次同步应零变更（幂等）"
    );

    // 5) 数据保护：同步前后行数一致
    let mut session = dal.open_session().unwrap();
    for (table, before) in &snapshot {
        let after = count_of(session.as_mut(), table)
            .unwrap_or_else(|| panic!("同步后应仍可读：{table}"));
        assert_eq!(after, *before, "同步不应改变数据行数：{table}");
    }
    eprintln!("数据保护通过：{} 张表行数同步前后一致", snapshot.len());

    // 6) 模型覆盖：同步后全部模型表就位
    let missing: Vec<&String> = model_tables
        .iter()
        .filter(|t| !session.table_exists(t).unwrap())
        .collect();
    assert!(missing.is_empty(), "同步后模型表应全部存在：{missing:?}");
    eprintln!(
        "模型覆盖：{} / {} 张表已就位",
        model_tables.len() - missing.len(),
        model_tables.len()
    );

    // 7) 真实 CRUD 往返（副本库）
    let now = chrono::Local::now().naive_local();

    if model_tables.contains("DH_JiLiYu") {
        let jly = dal.table("JiLiYu").unwrap();
        let id = jly
            .insert(
                session.as_mut(),
                &[
                    ("Content", "rcode live smoke".into()),
                    ("CreateUser", "rcode".into()),
                    ("CreateUserID", 0.into()),
                    ("CreateTime", now.into()),
                    ("CreateIP", "127.0.0.1".into()),
                    ("UpdateUser", "rcode".into()),
                    ("UpdateUserID", 0.into()),
                    ("UpdateTime", now.into()),
                    ("UpdateIP", "127.0.0.1".into()),
                ],
            )
            .expect("自增表插入应成功");
        assert!(id > 0, "自增主键应回写");
        let row = jly
            .find_by_pk(session.as_mut(), &[id.into()])
            .unwrap()
            .expect("应能按自增主键查到");
        assert_eq!(
            row.get_by_name("Content").unwrap().as_str(),
            Some("rcode live smoke")
        );
        assert_eq!(jly.delete_by_pk(session.as_mut(), &[id.into()]).unwrap(), 1);
        eprintln!("DH_JiLiYu CRUD 往返通过（自增主键 {id}）");
    }

    if model_tables.contains("DH_VerifyCode") {
        let vc = dal.table("VerifyCode").unwrap();
        let end = now + chrono::TimeDelta::minutes(5);
        let inserted = vc
            .insert(
                session.as_mut(),
                &[
                    ("Key", "rcode-live-001".into()),
                    ("Code", "123456".into()),
                    ("EndTime", end.into()),
                    ("CreateTime", now.into()),
                ],
            )
            .unwrap();
        assert_eq!(inserted, 0, "字符串主键应返回 0");
        let row = vc
            .find_by_pk(session.as_mut(), &["rcode-live-001".into()])
            .unwrap()
            .expect("应能按主键查到");
        assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("123456"));
        assert_eq!(
            vc.delete_by_pk(session.as_mut(), &["rcode-live-001".into()])
                .unwrap(),
            1
        );
        eprintln!("DH_VerifyCode CRUD 往返通过");
    }

    // 8) 反向工程：从真实库读回结构（对应 C# 的 DAL.GetTables）
    let reversed = dal.read_model().expect("反向工程应成功");
    assert!(
        reversed.tables.len() >= model_tables.len(),
        "反向表数 {} 应不少于模型表数 {}",
        reversed.tables.len(),
        model_tables.len()
    );
    let reversed_names: BTreeSet<String> = reversed
        .tables
        .iter()
        .map(|t| t.effective_table_name().to_string())
        .collect();
    let reverse_missing: Vec<&String> = model_tables
        .iter()
        .filter(|t| !reversed_names.contains(*t))
        .collect();
    assert!(
        reverse_missing.is_empty(),
        "反向结果应包含模型全部表：{reverse_missing:?}"
    );

    // 抽样：DH_JiLiYu 的反向列名与顺序应与模型一致
    if let Some(src) = dal.model().and_then(|m| m.table("JiLiYu")) {
        let rev = reversed
            .tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(src.effective_table_name()))
            .expect("反向应包含 DH_JiLiYu");
        let src_cols: Vec<String> = src.columns.iter().map(|c| c.name.clone()).collect();
        let rev_cols: Vec<String> = rev.columns.iter().map(|c| c.name.clone()).collect();
        assert_eq!(rev_cols, src_cols, "DH_JiLiYu 列名与顺序应一致");
    }
    eprintln!(
        "反向工程：读回 {} 张表，模型 {} 张表全部覆盖",
        reversed.tables.len(),
        model_tables.len()
    );
}
