//! 数据模拟/压测（对应 DH.NCode 的 `Common/DataSimulation.cs`）：批量生成并写入测试数据。
//!
//! 与 C# 行为一致：
//! - `Int32` → 随机整数；`String` → 随机 8 位字符串；`DateTime` → 当前时间 ±10000 秒
//! - 其余类型不填充（保持模型默认值）；自增列跳过
//! - 按批开启事务提交（默认批 1000，对齐 C# `BatchSize`），统计写入吞吐（TPS）
//!
//! 差异：C# 用 `Parallel.For` 多线程造数；Rust 版为单线程（连接池提供并发基础，
//! 需要多线程压测时可多任务并发调用，机制差异已记录）。

use std::time::{Duration, Instant};

use crate::dal::Dal;
use crate::error::Result;
use crate::types::DataType;
use crate::value::DbValue;

/// 模拟结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationReport {
    /// 写入行数
    pub inserted: usize,
    /// 耗时
    pub elapsed: Duration,
    /// 吞吐（行/秒）
    pub tps: i64,
}

/// 简易伪随机（xorshift64*，无需额外依赖；种子取自系统时间）。
struct SimpleRng(u64);

impl SimpleRng {
    /// 以当前时间播种。
    fn from_time() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self(nanos | 1)
    }

    /// 下一个 64 位随机数。
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 随机 8 位字符串（大小写字母 + 数字）。
    fn next_string(&mut self, len: usize) -> String {
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..len)
            .map(|_| CHARS[(self.next_u64() % CHARS.len() as u64) as usize] as char)
            .collect()
    }

    /// 当前时间偏移（±10000 秒，对齐 C#）。
    fn next_delta_seconds(&mut self) -> i64 {
        (self.next_u64() % 20_001) as i64 - 10_000
    }
}

/// 按模型批量生成并写入测试数据（对应 C# `DataSimulation.Run`）。
/// <param name="dal">数据访问层</param>
/// <param name="table">表名（模型实体名或物理表名）</param>
/// <param name="count">目标行数</param>
/// <param name="batch_size">每批事务行数（0 时使用默认 1000）</param>
/// <returns>压测结果</returns>
pub fn run(dal: &Dal, table: &str, count: usize, batch_size: usize) -> Result<SimulationReport> {
    let batch = if batch_size == 0 { 1000 } else { batch_size };
    let table_ref = dal.table(table)?;
    let meta = table_ref.meta();
    let mut rng = SimpleRng::from_time();
    let start = Instant::now();

    let mut inserted = 0usize;
    let mut session = dal.open_session()?;
    while inserted < count {
        let upper = (inserted + batch).min(count);
        session.begin()?;
        for _ in 0..(upper - inserted) {
            let mut fields: Vec<(&str, DbValue)> = Vec::new();
            for col in &meta.columns {
                if col.identity {
                    continue;
                }
                let name = meta.effective_column_name(col);
                match col.data_type {
                    DataType::Byte => {
                        fields.push((name, ((rng.next_u64() & 0xFF) as i64).into()));
                    }
                    DataType::Int16 => {
                        fields.push((name, ((rng.next_u64() & 0xFFFF) as i64).into()));
                    }
                    DataType::Int32 => {
                        fields.push((name, (rng.next_u64() as i32 as i64).into()));
                    }
                    DataType::Int64 => {
                        fields.push((name, ((rng.next_u64() >> 1) as i64).into()));
                    }
                    DataType::String => {
                        fields.push((name, rng.next_string(8).into()));
                    }
                    DataType::DateTime => {
                        let when = chrono::Local::now().naive_local()
                            + chrono::Duration::seconds(rng.next_delta_seconds());
                        fields.push((name, when.into()));
                    }
                    // 其余类型保持模型默认值（与 C# 一致）
                    _ => {}
                }
            }
            table_ref.insert(session.as_mut(), &fields)?;
        }
        inserted = upper;
        session.commit()?;
    }
    drop(session);

    let elapsed = start.elapsed();
    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
    Ok(SimulationReport {
        inserted,
        elapsed,
        tps: (inserted as f64 / seconds) as i64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="20" />
        <Column Name="Status" DataType="Int32" />
        <Column Name="Created" DataType="DateTime" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    #[test]
    fn simulation_inserts_rows_in_batches() {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-sim-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sim.db");

        let dal = Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db.display()),
            EntityModel::parse(MODEL).unwrap(),
        )
        .unwrap();
        dal.sync_schema().unwrap();

        let report = run(&dal, "Order", 250, 100).unwrap();
        assert_eq!(report.inserted, 250);
        assert!(report.tps > 0, "应统计吞吐");

        let table = dal.table("Order").unwrap();
        let mut session = dal.open_session().unwrap();
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 250);

        // 抽样：字符串为 8 位随机串
        let row = table
            .find_by_pk(session.as_mut(), &[1.into()])
            .unwrap()
            .expect("应有首行");
        let code = row.get_by_name("Code").unwrap().to_text();
        assert_eq!(code.len(), 8, "随机字符串长度应为 8：{code}");

        drop(session);
        dal.clear_pool();
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
