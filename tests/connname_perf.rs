//! 表级 `ConnName` 路由的**开销基线**（手动跑，不进默认测试）。
//!
//! 目的：回答“新增路由对现有热路径有无影响、资源占用如何”。
//!
//! 运行：
//! ```text
//! cargo test --release --test connname_perf -- --ignored --nocapture
//! ```
//!
//! 结论口径：
//! - `Dal::table` / CRUD **未改动**（零影响）；
//! - 只有调用 `table_conn_dal` / `with_table_session` 才付出解析成本；
//! - 无 ConnName 的表走最快路径（不分配、不取锁）。

use std::path::Path;
use std::time::Instant;

use pek_rcode::{shards, Dal, EntityModel, Query};

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

fn bench(name: &str, iters: u32, mut f: impl FnMut()) {
    for _ in 0..2000 {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let per = t.elapsed().as_nanos() as f64 / iters as f64;
    println!("{name:<46} {per:>10.1} ns/op");
}

#[test]
#[ignore = "性能基线，手动运行：cargo test --release --test connname_perf -- --ignored --nocapture"]
fn routing_overhead_baseline() {
    let dir = std::env::temp_dir().join(format!("pek-rcode-connperf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    shards::set_auto_db_dir(&dir);
    shards::clear_connections();

    let model = EntityModel::parse(MODEL).expect("模型解析");
    let dal = Dal::open_with_model(&conn(&dir.join("main.db")), model).expect("打开主库");
    dal.sync_schema().expect("同步结构");
    // 预热 IoTData 连接（未注册时首次会建库，属于一次性成本）
    let _ = dal.table_conn_dal("RemoteTable").expect("解析连接");

    let n = 200_000;
    bench("① Dal::table（既有路径，未改动）", n, || {
        let _ = dal.table("LocalTable");
    });
    bench("② table_conn_dal（无 ConnName → 快速返回）", n, || {
        let _ = dal.table_conn_dal("LocalTable");
    });
    bench("③ table_conn_dal（有 ConnName → 命中注册表）", n, || {
        let _ = dal.table_conn_dal("RemoteTable");
    });
    bench("④ open_session 借还（既有连接池）", n, || {
        let s = dal.open_session().expect("会话");
        drop(s);
    });
    bench("⑤ with_table_session（无 ConnName 表，空闭包）", n, || {
        dal.with_table_session("LocalTable", |_d, _t, _s| Ok(())).expect("with_table_session");
    });
    bench("⑥ with_table_session（ConnName 表，空闭包）", n, || {
        dal.with_table_session("RemoteTable", |_d, _t, _s| Ok(())).expect("with_table_session");
    });

    // 一次真实查询（含 SQL），看端到端量级
    let mut session = dal.open_session().expect("会话");
    let t = dal.table("LocalTable").expect("取表");
    bench("⑦ TableRef::query（LocalTable，空表）", 20_000, || {
        let _ = t.query(session.as_mut(), &Query::new().column("Id")).expect("查询");
    });

    shards::clear_connections();
    let _ = std::fs::remove_dir_all(&dir);
}
