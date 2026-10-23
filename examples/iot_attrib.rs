//! 分层归因：一批 500 行 Upsert 的成本拆解 —— 客户端构造 vs 网络往返 vs 服务端。
//!
//! 用途：判断「还能往哪里优化」。如果客户端构造（SQL 拼接 + 参数）占比很小，
//! 说明框架侧已无空间，继续优化只能落在「减少往返次数」或「减少服务端工作量」上。
//!
//! ```powershell
//! $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None;PoolMinSize=4;PoolIdleTime=300"
//! cargo run --release --example iot_attrib
//! ```

use std::time::{Duration, Instant};

use pek_rcode::entity_queue::EntityQueue;
use pek_rcode::sqlbuild::{self, BatchWriteMode};
use pek_rcode::{Dal, DbValue, EntityModel};

const MODEL: &str = r#"<EntityModel><Tables>
  <Table Name="DeviceTelemetry" TableName="iot_device_telemetry" Description="设备状态上报">
    <Columns>
      <Column Name="DeviceId" DataType="String" Length="40" PrimaryKey="True" />
      <Column Name="Status" DataType="Int32" />
      <Column Name="Amount" DataType="Int64" />
      <Column Name="ReportTime" DataType="DateTime" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

/// 每批行数（与 `iot_telemetry` 一致，MySQL 默认 500）
const BATCH: usize = 500;
/// 批次数（500 × 400 = 20 万行）
const ROUNDS: usize = 400;

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn per_batch(duration: Duration) -> f64 {
    ms(duration) / ROUNDS as f64
}

fn main() -> pek_rcode::Result<()> {
    let conn = std::env::var("RCODE_MYSQL")
        .expect("请先设置 RCODE_MYSQL（本地实例见 scripts/mysql-test-env.ps1）");
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL)?)?;
    dal.sync_schema()?;
    let table = dal.table("DeviceTelemetry")?;
    let columns = ["DeviceId", "Status", "Amount", "ReportTime"];

    // 预热连接池，避免把建连成本算进第一轮
    dal.warmup_pool(2);

    // ① 客户端：SQL 拼接（每批都重新组装 500 行占位符）
    let started = Instant::now();
    let mut sql_len = 0usize;
    for _ in 0..ROUNDS {
        let sql = sqlbuild::multi_write_sql_named(
            dal.kind(),
            table.meta(),
            table.physical_name(),
            &columns,
            BATCH,
            BatchWriteMode::Upsert,
        )?;
        sql_len = sql.len();
    }
    let sql_cost = started.elapsed();

    // ② 客户端：参数构造（每行 4 个值，含一次 `Text` 分配 + 一次 `DateTime` clone）
    let now = DbValue::DateTime(chrono::Local::now().naive_local());
    let started = Instant::now();
    for _ in 0..ROUNDS {
        let mut params = Vec::with_capacity(BATCH * columns.len());
        for row in 0..BATCH {
            params.push(DbValue::Text(format!("DEV-{:08}", row % 1_000)));
            params.push(DbValue::Int(row as i64 % 100));
            params.push(DbValue::Int(row as i64));
            params.push(now.clone());
        }
        std::hint::black_box(&params);
    }
    let param_cost = started.elapsed();

    // ③ 完整路径：每轮重新构造参数（值必须变化，否则 MySQL 判「值未变」跳过写入）+ 发送
    let sql = sqlbuild::multi_write_sql_named(
        dal.kind(),
        table.meta(),
        table.physical_name(),
        &columns,
        BATCH,
        BatchWriteMode::Upsert,
    )?;
    let mut session = dal.open_session()?;
    let build = |round: usize| {
        let mut params = Vec::with_capacity(BATCH * columns.len());
        for row in 0..BATCH {
            params.push(DbValue::Text(format!("DEV-{:08}", row % 1_000)));
            params.push(DbValue::Int(row as i64 % 100));
            // 每轮换一个值 → 确实产生写入（否则 500 行都是「设为当前值」，MySQL 直接跳过）
            params.push(DbValue::Int(round as i64 * BATCH as i64 + row as i64));
            params.push(now.clone());
        }
        params
    };
    // 先跑 1 轮建立语句缓存与连接状态
    session.execute_owned(&sql, build(0))?;
    let started = Instant::now();
    for round in 1..=ROUNDS {
        session.execute_owned(&sql, build(round))?;
    }
    let exec_cost = started.elapsed();
    drop(session);

    // ④ 纯往返基线（无写入）
    let mut session = dal.open_session()?;
    let started = Instant::now();
    for _ in 0..ROUNDS {
        let _ = session.query_owned("SELECT 1", Vec::new())?;
    }
    let ping_cost = started.elapsed();
    drop(session);

    // ⑤ 带脏列的 Upsert（`EntityQueue::upsert_tracked` 带 update_columns）：
    //    这条路径目前**不走多行 Upsert**（各行脏列可能不同 → 逐条 find + update）
    const TRACKED_ROWS: usize = 1_000;
    let mut queue = EntityQueue::new("DeviceTelemetry");
    queue.set_batch_size(BATCH);
    let dirty = vec!["Status".to_string(), "Amount".to_string()];
    let mut session = dal.open_session()?;
    let started = Instant::now();
    for row in 0..TRACKED_ROWS {
        let fields = vec![
            ("DeviceId", DbValue::Text(format!("DEV-{:08}", row % 1_000))),
            ("Status", DbValue::Int(row as i64 % 100)),
            ("Amount", DbValue::Int(9_000_000 + row as i64)),
            ("ReportTime", now.clone()),
        ];
        let pk = vec![fields[0].1.clone()];
        queue.upsert_tracked(&dal, session.as_mut(), &pk, &fields, Some(&dirty))?;
    }
    queue.flush(&dal, session.as_mut())?;
    let tracked_cost = started.elapsed();
    drop(session);

    println!("每批 {BATCH} 行 × {ROUNDS} 批 = {} 行", BATCH * ROUNDS);
    println!("SQL 文本长度 {sql_len} 字节");
    println!();
    println!("① 客户端 SQL 拼接      {:>8.2} ms  每批 {:>7.1} µs", ms(sql_cost), per_batch(sql_cost) * 1000.0);
    println!("② 客户端参数构造+分配  {:>8.2} ms  每批 {:>7.1} µs", ms(param_cost), per_batch(param_cost) * 1000.0);
    println!("③ 完整路径(构造+发送+服务端) {:>8.2} ms  每批 {:>7.1} µs", ms(exec_cost), per_batch(exec_cost) * 1000.0);
    println!("④ `SELECT 1` 往返基线  {:>8.2} ms  每次 {:>7.1} µs", ms(ping_cost), per_batch(ping_cost) * 1000.0);
    println!();
    let batch_us = per_batch(exec_cost) * 1000.0;
    println!(
        "→ 一批 500 行的 {} µs 中：",
        batch_us.round()
    );
    println!(
        "   · SQL 拼接      {:>7.1} µs（{:.1}%）",
        per_batch(sql_cost) * 1000.0,
        per_batch(sql_cost) * 1000.0 / batch_us * 100.0
    );
    println!(
        "   · 参数构造      {:>7.1} µs（{:.1}%）",
        per_batch(param_cost) * 1000.0,
        per_batch(param_cost) * 1000.0 / batch_us * 100.0
    );
    println!(
        "   · 网络往返      {:>7.1} µs（{:.1}%）",
        per_batch(ping_cost) * 1000.0,
        per_batch(ping_cost) * 1000.0 / batch_us * 100.0
    );
    println!(
        "   · 服务端执行    {:>7.1} µs（{:.1}%）",
        (batch_us - per_batch(sql_cost) * 1000.0 - per_batch(param_cost) * 1000.0 - per_batch(ping_cost) * 1000.0).max(0.0),
        (1.0 - (per_batch(sql_cost) + per_batch(param_cost) + per_batch(ping_cost)) / per_batch(exec_cost)) * 100.0
    );
    println!();
    println!(
        "⑤ 带脏列的 Upsert（`upsert_tracked` + update_columns）：{TRACKED_ROWS} 行 {:>8.2} ms → 每行 {:>7.1} µs",
        ms(tracked_cost),
        ms(tracked_cost) * 1000.0 / TRACKED_ROWS as f64
    );
    println!(
        "   （对比多行 Upsert：每行 {:>5.1} µs；差距 {:>5.1} 倍）",
        per_batch(exec_cost) * 1000.0 / BATCH as f64,
        (ms(tracked_cost) * 1000.0 / TRACKED_ROWS as f64) / (per_batch(exec_cost) * 1000.0 / BATCH as f64)
    );
    Ok(())
}
