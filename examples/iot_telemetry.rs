//! 设备状态上报（IoT 遥测）参考实现：**批量 Upsert + 定时 flush + 优雅关闭**。
//!
//! 为什么这样写（实测数据见 `docs/MySQL调优指南.md`）：
//! - MySQL 上单语句 ~92 µs 里框架只占 ~1 µs —— **往返次数与 COMMIT 次数才是成本**；
//! - 逐条 `save`/`update` ≈ 100~210 µs/行（1~2 次往返 + 每次提交刷盘）；
//! - 攒批后多行 `ON DUPLICATE KEY UPDATE` ≈ 6~10 µs/行（1 次往返/批，实测 105k~160k 行/秒）。
//!
//! 运行：
//!
//! ```powershell
//! # 本地 SQLite（默认，零依赖，便于先跑通链路）
//! cargo run --release --example iot_telemetry
//!
//! # 真实 MySQL（scripts/mysql-test-env.ps1 start 起本地实例）
//! $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None;PoolMinSize=4;PoolIdleTime=300"
//! cargo run --release --example iot_telemetry
//! ```

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use pek_rcode::{Dal, DbValue, EntityModel, entity_queue::EntityQueue};

/// 设备状态表（`DeviceId` 为业务主键：一台设备一行，反复上报即反复 Upsert）。
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

/// 每批行数（MySQL 实测最优区间 200~1000；框架默认 500）。
const BATCH: usize = 500;
/// flush 周期（攒批上限的“时间闸门”：流量小也不会等太久）。
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);
/// 本次演示产生的上报条数。
const TOTAL: i64 = 200_000;
/// 设备数（模拟重复上报：每台设备被反复 Upsert）。
const DEVICES: i64 = 1_000;

fn main() -> pek_rcode::Result<()> {
    // ① 打开连接：`RCODE_IOT_CONN` 显式指定 > `RCODE_MYSQL` > 本地 SQLite（同一套代码）
    let conn = std::env::var("RCODE_IOT_CONN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("RCODE_MYSQL").ok())
        .unwrap_or_else(|| {
            let dir = std::env::temp_dir().join("pek-rcode-iot");
            let _ = std::fs::create_dir_all(&dir);
            format!(
                "Data Source={};Provider=SQLite",
                dir.join("telemetry.db").display()
            )
        });
    let is_mysql = conn.contains("mysql");
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL)?)?;
    dal.sync_schema()?;
    println!(
        "后端：{}",
        if is_mysql { "MySQL" } else { "SQLite" }
    );

    // ② 预热连接池：建连 ~3~4 次往返，冷启动首批并发请求不该各自承担
    let warmed = dal.warmup_pool(4);
    if warmed > 0 {
        println!("连接池预热 {warmed} 个连接");
    }

    // ③ 数据接入（真实场景是 MQTT/HTTP/网关线程）→ 通过通道交给写入线程
    let (tx, rx) = mpsc::channel::<(String, i64, i64)>();
    let producer = std::thread::spawn(move || {
        for seq in 0..TOTAL {
            let device = format!("DEV-{:08}", seq % DEVICES);
            if tx.send((device, seq, seq % 100)).is_err() {
                break;
            }
        }
    });

    // ④ 写入线程（本线程）：攒批 → 达批大小或超时 → 一次多行 Upsert
    let mut queue = EntityQueue::new("DeviceTelemetry");
    queue.set_batch_size(BATCH);
    let mut session = dal.open_session()?;
    let mut pending = 0usize;
    let mut written: u64;
    let mut max_flush = Duration::ZERO;
    let started = Instant::now();
    let mut last_report = Instant::now();

    // 一批 = 一次 `INSERT ... ON DUPLICATE KEY UPDATE`（MySQL）/ `ON CONFLICT DO UPDATE`（SQLite）
    // 注意：统计要读 `queue.stats()`（队列内部累计，含“达批大小自动 flush”的部分）；
    // `flush()` 只返回本次刷入的统计，自动 flush 不经过我们的代码路径。
    let flush = |queue: &mut EntityQueue,
                 session: &mut Box<dyn pek_rcode::SqlSession>,
                 pending: &mut usize,
                 max_flush: &mut Duration|
     -> pek_rcode::Result<()> {
        if *pending == 0 {
            return Ok(());
        }
        let t = Instant::now();
        queue.flush(&dal, session.as_mut())?;
        let elapsed = t.elapsed();
        *max_flush = (*max_flush).max(elapsed);
        *pending = 0;
        Ok(())
    };

    loop {
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok((device, seq, status)) => {
                let now = DbValue::DateTime(chrono::Local::now().naive_local());
                let fields = [
                    ("DeviceId", DbValue::Text(device)),
                    ("Status", DbValue::Int(status)),
                    ("Amount", DbValue::Int(seq)),
                    ("ReportTime", now),
                ];
                // 主键（DeviceId）已在行内：队列会走「多行 Upsert」快速路径
                queue.upsert_tracked_static(
                    &dal,
                    session.as_mut(),
                    &[fields[0].1.clone()],
                    &fields,
                    None,
                )?;
                pending += 1;
                if pending >= BATCH {
                    // 队列已达批大小并在内部自动写过库，这里只重置计数
                    pending = 0;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // 时间闸门：流量低于批大小时也按时落库
                flush(
                    &mut queue,
                    &mut session,
                    &mut pending,
                    &mut max_flush,
                )?;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            written = queue.stats().total();
            let elapsed = started.elapsed().as_secs_f64();
            println!(
                "已上报 {written} 行 / {:.1}s（{:.0} 行/s）  pending {pending}",
                elapsed,
                written as f64 / elapsed
            );
            last_report = Instant::now();
        }
    }

    // ⑤ 优雅关闭：**必须**最终 flush，否则队列里的最后一批会丢
    flush(
        &mut queue,
        &mut session,
        &mut pending,
        &mut max_flush,
    )?;
    written = queue.stats().total();
    producer.join().ok();

    let elapsed = started.elapsed();
    println!(
        "完成：{written} 行 / {:.2}s（{:.0} 行/s），自动 flush 批次数 ≈ {}，单次显式 flush 最慢 {:.1} ms",
        elapsed.as_secs_f64(),
        written as f64 / elapsed.as_secs_f64(),
        written / BATCH as u64,
        max_flush.as_secs_f64() * 1000.0
    );
    Ok(())
}
