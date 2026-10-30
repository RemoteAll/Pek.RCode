//! MySQL「往返经济学」基准：把「每行 N 次往返」的路径量与批量路径摆在一起对比。
//!
//! ```powershell
//! pwsh scripts/mysql-test-env.ps1 start
//! $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None"
//! cargo test --release --test mysql_perf_roundtrip -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel};

const MODEL: &str = r#"<EntityModel><Tables>
  <Table Name="RCodeRtItem" TableName="rcode_rt_item" Description="往返基准表">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
      <Column Name="DeviceId" DataType="String" Length="40" />
      <Column Name="Code" DataType="String" Length="50" />
      <Column Name="Amount" DataType="Int64" />
      <Column Name="Status" DataType="Int32" />
      <Column Name="CreateTime" DataType="DateTime" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

/// 与生成代码同构的实体。
#[derive(Debug, Clone, PartialEq)]
struct RtItem {
    id: i32,
    device_id: String,
    code: String,
    amount: i64,
    status: i32,
    create_time: chrono::NaiveDateTime,
}

impl RtItem {
    fn new(index: i32) -> Self {
        Self {
            id: index,
            device_id: format!("DEV-{index:08}"),
            code: "PAYLOAD-0123456789".into(),
            amount: i64::from(index),
            status: 1,
            create_time: chrono::Local::now().naive_local().with_nanosecond_zero(),
        }
    }
}

/// 秒级时间（MySQL DATETIME 精度）。
trait WithNanoZero {
    fn with_nanosecond_zero(self) -> Self;
}

impl WithNanoZero for chrono::NaiveDateTime {
    fn with_nanosecond_zero(self) -> Self {
        chrono::NaiveDateTime::new(
            self.date(),
            self.time().with_nanosecond(0).unwrap_or(self.time()),
        )
    }
}

use chrono::Timelike;

impl Entity for RtItem {
    fn table() -> &'static str {
        "RCodeRtItem"
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "DeviceId", "Code", "Amount", "Status", "CreateTime"]
    }

    fn primary_keys() -> &'static [&'static str] {
        &["Id"]
    }

    fn identity_column() -> Option<&'static str> {
        Some("Id")
    }

    fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
        vec![
            ("Id", DbValue::Int(i64::from(self.id))),
            ("DeviceId", DbValue::Text(self.device_id.clone())),
            ("Code", DbValue::Text(self.code.clone())),
            ("Amount", DbValue::Int(self.amount)),
            ("Status", DbValue::Int(i64::from(self.status))),
            ("CreateTime", DbValue::DateTime(self.create_time)),
        ]
    }

    fn from_row(row: &DbRow) -> pek_rcode::Result<Self> {
        Ok(Self {
            id: row
                .get_by_name("Id")
                .and_then(DbValue::as_i32)
                .unwrap_or_default(),
            device_id: row
                .get_by_name("DeviceId")
                .and_then(DbValue::as_str)
                .unwrap_or_default()
                .to_string(),
            code: row
                .get_by_name("Code")
                .and_then(DbValue::as_str)
                .unwrap_or_default()
                .to_string(),
            amount: row
                .get_by_name("Amount")
                .and_then(DbValue::as_i64)
                .unwrap_or_default(),
            status: row
                .get_by_name("Status")
                .and_then(DbValue::as_i32)
                .unwrap_or_default(),
            create_time: row
                .get_by_name("CreateTime")
                .and_then(DbValue::as_datetime)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc()),
        })
    }

    fn set_identity(&mut self, value: i64) -> pek_rcode::Result<()> {
        self.id = value as i32;
        Ok(())
    }
}

fn open() -> Option<Dal> {
    let conn = std::env::var("RCODE_MYSQL").ok()?;
    let model = EntityModel::parse(MODEL).expect("模型解析");
    Some(Dal::open_with_model(&conn, model).expect("连接 MySQL"))
}

fn report(label: &str, n: usize, elapsed: Duration) {
    let ns = elapsed.as_nanos() as f64 / n as f64;
    let per_sec = if ns > 0.0 { 1e9 / ns } else { 0.0 };
    println!(
        "{label:<48} {ns:>11.0} ns/行   {per_sec:>10.0} 行/s   ≈{:.1} 次往返/行",
        ns / 92_000.0
    );
}

#[test]
#[ignore]
fn bench_mysql_roundtrip_paths() {
    let Some(dal) = open() else {
        println!("跳过：未设置 RCODE_MYSQL");
        return;
    };
    let _table = dal.table("RCodeRtItem").expect("表定义");
    dal.sync_schema().expect("建表");
    let mut session = dal.open_session().unwrap();
    session.execute("DELETE FROM rcode_rt_item", &[]).unwrap();

    // 先批量灌入 3000 行（后续 upsert/update 针对已存在行）
    const SEED: usize = 3_000;
    let seed: Vec<RtItem> = (1..=SEED as i32).map(RtItem::new).collect();
    session.begin().unwrap();
    RtItem::insert_batch(&dal, session.as_mut(), &seed, None).unwrap();
    session.commit().unwrap();
    println!("已灌入 {SEED} 行");

    // ① 逐条 upsert（EntityQueue Upsert 段现行实现：find + update/insert = 2 次往返/行）
    {
        const N: usize = 200;
        let mut queue = pek_rcode::entity_queue::EntityQueue::new("RCodeRtItem");
        queue.set_batch_size(N);
        let items: Vec<RtItem> = (1..=N as i32).map(RtItem::new).collect();
        session.begin().unwrap();
        let t = Instant::now();
        for item in &items {
            let fields = item.to_fields();
            queue
                .upsert(
                    &dal,
                    session.as_mut(),
                    &[DbValue::Int(i64::from(item.id))],
                    &fields,
                )
                .unwrap();
        }
        queue.flush(&dal, session.as_mut()).unwrap();
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report("① EntityQueue::upsert（逐条 find+update）", N, elapsed);
    }

    // ② 批量 upsert（多行 `ON DUPLICATE KEY UPDATE`：1 次往返/批）
    {
        const N: usize = 3_000;
        let items: Vec<RtItem> = (1..=N as i32)
            .map(|i| {
                let mut item = RtItem::new(i);
                item.status = 2;
                item
            })
            .collect();
        session.begin().unwrap();
        let t = Instant::now();
        RtItem::upsert_batch(&dal, session.as_mut(), &items, None).unwrap();
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report(
            "② Entity::upsert_batch（多行 ON DUPLICATE KEY）",
            N,
            elapsed,
        );
    }

    // ③ save_batch（当前：已存在行逐条 update → N 次往返 + 1 次批量 IN 查询）
    {
        const N: usize = 1_000;
        let items: Vec<RtItem> = (1..=N as i32)
            .map(|i| {
                let mut item = RtItem::new(i);
                item.status = 3;
                item
            })
            .collect();
        session.begin().unwrap();
        let t = Instant::now();
        RtItem::save_batch(&dal, session.as_mut(), &items, None).unwrap();
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report("③ Entity::save_batch（已存在行逐条 update）", N, elapsed);
    }

    // ④ 逐条 update（基线：1 次往返/行）
    {
        const N: usize = 500;
        session.begin().unwrap();
        let t = Instant::now();
        for i in 1..=N as i32 {
            let mut item = RtItem::new(i);
            item.status = 4;
            item.update(&dal, session.as_mut()).unwrap();
        }
        let elapsed = t.elapsed();
        session.commit().unwrap();
        report("④ Entity::update 逐条（基线 1 往返/行）", N, elapsed);
    }

    // 清理
    let _ = session.execute("DROP TABLE IF EXISTS rcode_rt_item", &[]);
    println!("已清理基准表 rcode_rt_item");
}
