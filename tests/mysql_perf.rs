//! MySQL / MariaDB 性能基准（真实实例，`RCODE_MYSQL` 门控）。
//!
//! ```powershell
//! pwsh scripts/mysql-test-env.ps1 start
//! $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None"
//! cargo test --release --test mysql_perf -- --ignored --nocapture
//! ```
//!
//! 关注点：**网络往返 + 存储刷盘** 与 **框架侧开销** 的比例——
//! 物联网写入路径的优化方向由此决定（批量/事务/参数、而不是抠 SQL 字符串）。
//!
//! 表名统一 `rcode_perf_` 前缀，测试结束自动清理。

use std::time::{Duration, Instant};

use pek_rcode::{Dal, DbValue, EntityModel};

const MODEL: &str = r#"<EntityModel><Tables>
  <Table Name="RCodePerfItem" TableName="rcode_perf_item" Description="MySQL 基准表">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      <Column Name="DeviceId" DataType="String" Length="40" />
      <Column Name="Code" DataType="String" Length="50" />
      <Column Name="C1" DataType="String" Length="50" />
      <Column Name="C2" DataType="String" Length="50" />
      <Column Name="C3" DataType="String" Length="50" />
      <Column Name="C4" DataType="String" Length="50" />
      <Column Name="C5" DataType="String" Length="50" />
      <Column Name="Amount" DataType="Int64" />
      <Column Name="Status" DataType="Int32" />
      <Column Name="CreateTime" DataType="DateTime" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

fn open() -> Option<Dal> {
    let conn = std::env::var("RCODE_MYSQL").ok()?;
    let model = EntityModel::parse(MODEL).expect("模型解析");
    let dal = Dal::open_with_model(&conn, model).expect("连接 MySQL");
    Some(dal)
}

fn report(label: &str, n: usize, elapsed: Duration) {
    let ns = elapsed.as_nanos() as f64 / n as f64;
    let per_sec = if ns > 0.0 { 1e9 / ns } else { 0.0 };
    println!("{label:<46} {ns:>10.0} ns/op   {per_sec:>10.0} op/s");
}

fn row_fields() -> Vec<(&'static str, DbValue)> {
    vec![
        ("DeviceId", DbValue::Text("DEV-000000001".into())),
        ("Code", DbValue::Text("PAYLOAD-0123456789".into())),
        ("C1", DbValue::Text("0123456789abcdef".into())),
        ("C2", DbValue::Text("0123456789abcdef".into())),
        ("C3", DbValue::Text("0123456789abcdef".into())),
        ("C4", DbValue::Text("0123456789abcdef".into())),
        ("C5", DbValue::Text("0123456789abcdef".into())),
        ("Amount", DbValue::Int(123456)),
        ("Status", DbValue::Int(1)),
        (
            "CreateTime",
            DbValue::DateTime(chrono::Local::now().naive_local()),
        ),
    ]
}

#[test]
#[ignore]
fn bench_mysql_write_read() {
    let Some(dal) = open() else {
        println!("跳过：未设置 RCODE_MYSQL");
        return;
    };
    let table = dal.table("RCodePerfItem").expect("表定义");
    dal.sync_schema().expect("建表");
    // 清空上一轮数据（基准可重复）
    table
        .delete_where(&mut *dal.open_session().unwrap(), &Default::default())
        .ok();

    // ① 网络往返基线
    {
        const N: usize = 2_000;
        let mut session = dal.open_session().unwrap();
        let t = Instant::now();
        for _ in 0..N {
            session.query("SELECT 1", &[]).unwrap();
        }
        report("① SELECT 1（网络往返基线）", N, t.elapsed());
    }

    // ② 单行插入 · 事务外（每次 autocommit = 一次刷盘）
    {
        const N: usize = 500;
        let fields = row_fields();
        let mut session = dal.open_session().unwrap();
        let t = Instant::now();
        for _ in 0..N {
            table.insert(session.as_mut(), &fields).unwrap();
        }
        report("② 单行插入 · 事务外（含 COMMIT 刷盘）", N, t.elapsed());
    }

    // ③ 单行插入 · 事务内
    {
        const N: usize = 2_000;
        let fields = row_fields();
        let mut session = dal.open_session().unwrap();
        session.begin().unwrap();
        let t = Instant::now();
        for _ in 0..N {
            table.insert(session.as_mut(), &fields).unwrap();
        }
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report("③ 单行插入 · 事务内", N, elapsed);
    }

    // ④ 批量插入（批大小对比：500 / 2000 / 5000）
    {
        const ROWS: usize = 10_000;
        let fields = row_fields();
        let rows: Vec<Vec<(&str, DbValue)>> = (0..ROWS).map(|_| fields.clone()).collect();
        for batch in [100usize, 200, 500, 1_000, 2_000, 5_000] {
            let mut session = dal.open_session().unwrap();
            session.begin().unwrap();
            let t = Instant::now();
            table
                .write_batch(
                    session.as_mut(),
                    &rows,
                    pek_rcode::sqlbuild::BatchWriteMode::Insert,
                    Some(batch),
                )
                .unwrap();
            let elapsed = t.elapsed();
            session.commit().unwrap();
            report(&format!("④ 批量插入（{batch}/批，事务内）"), ROWS, elapsed);
        }
    }

    // ④b 默认批大小（`None` → 按方言：MySQL 取 500）
    {
        const ROWS: usize = 10_000;
        let fields = row_fields();
        let rows: Vec<Vec<(&str, DbValue)>> = (0..ROWS).map(|_| fields.clone()).collect();
        let mut session = dal.open_session().unwrap();
        session.begin().unwrap();
        let t = Instant::now();
        table
            .write_batch(
                session.as_mut(),
                &rows,
                pek_rcode::sqlbuild::BatchWriteMode::Insert,
                None,
            )
            .unwrap();
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report("④b 批量插入（默认批大小 = 方言最优）", ROWS, elapsed);
    }

    // ⑤ 按主键更新（事务内）
    {
        const N: usize = 2_000;
        let mut session = dal.open_session().unwrap();
        let set = [("Status", DbValue::Int(2))];
        session.begin().unwrap();
        let t = Instant::now();
        for id in 1..=N as i64 {
            table
                .update_by_pk(session.as_mut(), &set, &[DbValue::Int(id)])
                .unwrap();
        }
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report("⑤ 按主键更新（事务内）", N, elapsed);
    }

    // ⑥ 按主键查询（事务内）
    {
        const N: usize = 2_000;
        let mut session = dal.open_session().unwrap();
        session.begin().unwrap();
        let t = Instant::now();
        let mut sink = 0usize;
        for id in 1..=N as i64 {
            let row = table
                .find_by_pk(session.as_mut(), &[DbValue::Int(id)])
                .unwrap();
            sink += usize::from(row.is_some());
        }
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report("⑥ 按主键查询（事务内）", N, elapsed);
        assert!(sink > 0);
    }

    // ⑦ 条件查询（10 行）
    {
        const N: usize = 500;
        let mut session = dal.open_session().unwrap();
        let query = pek_rcode::Query::new()
            .filter(pek_rcode::Where::new().eq("Status", DbValue::Int(2)))
            .take(10);
        let t = Instant::now();
        let mut sink = 0usize;
        for _ in 0..N {
            sink += table.query(session.as_mut(), &query).unwrap().len();
        }
        report("⑦ 条件查询（LIMIT 10）", N, t.elapsed());
        assert!(sink > 0);
    }

    // 清理
    let mut session = dal.open_session().unwrap();
    let _ = session.execute("DROP TABLE IF EXISTS rcode_perf_item", &[]);
    println!("已清理基准表 rcode_perf_item");
}
