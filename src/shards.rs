//! 分表策略（对应 DH.NCode 的 `Shards/IShardPolicy.cs` 与 `Shards/TimeShardPolicy.cs`）。
//!
//! 按时间把数据落到不同表（如 `WmsOrder` → `WmsOrder_202609`）。
//! 与 C# 版的差异记录：C# 通过全局可变的实体 `Meta.TableName` 切换实现 `EntitySplit`；
//! Rust 的 `TableRef` 绑定不可变模型，分表使用“**基础表名 + 策略后缀**”显式组名
//! （配合反向/多模型生成分表结构），不引入全局可变状态。

use chrono::{Datelike, NaiveDateTime, Timelike};

/// 分表策略。
pub trait ShardPolicy: Send + Sync {
    /// 基于时间计算分表后缀（如 `2026` / `202609` / `20260927`）；返回 `None` 表示主表。
    fn resolve(&self, time: NaiveDateTime) -> Option<String>;

    /// 计算实际表名（`base` + 后缀，后缀为空时使用 `base`）。
    fn table_name(&self, base: &str, time: NaiveDateTime) -> String {
        match self.resolve(time) {
            Some(suffix) if !suffix.is_empty() => format!("{base}_{suffix}"),
            _ => base.to_string(),
        }
    }
}

/// 时间分表粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardLevel {
    /// 按年：`2026`
    Year,
    /// 按月：`202609`
    Month,
    /// 按日：`20260927`
    Day,
    /// 按小时：`2026092710`
    Hour,
}

/// 时间分表策略（对应 `TimeShardPolicy`）。
pub struct TimeShardPolicy {
    /// 粒度
    level: ShardLevel,
    /// 基础表名的分表格式（默认 `{0}_{1}`：基础名 + 后缀）
    format: String,
}

impl TimeShardPolicy {
    /// 按粒度创建。
    pub fn new(level: ShardLevel) -> Self {
        Self {
            level,
            format: "{0}_{1}".to_string(),
        }
    }

    /// 自定义表名格式（`{0}`=基础名、`{1}`=后缀）。
    pub fn set_format(&mut self, format: &str) {
        self.format = format.to_string();
    }

    /// 计算表名（使用自定义格式）。
    pub fn shard_table(&self, base: &str, time: NaiveDateTime) -> String {
        match self.resolve(time) {
            Some(suffix) if !suffix.is_empty() => {
                self.format.replace("{0}", base).replace("{1}", &suffix)
            }
            _ => base.to_string(),
        }
    }
}

impl ShardPolicy for TimeShardPolicy {
    fn resolve(&self, time: NaiveDateTime) -> Option<String> {
        Some(match self.level {
            ShardLevel::Year => format!("{:04}", time.year()),
            ShardLevel::Month => format!("{:04}{:02}", time.year(), time.month()),
            ShardLevel::Day => format!("{:04}{:02}{:02}", time.year(), time.month(), time.day()),
            ShardLevel::Hour => format!(
                "{:04}{:02}{:02}{:02}",
                time.year(),
                time.month(),
                time.day(),
                time.hour()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 27)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap()
    }

    #[test]
    fn resolves_by_level() {
        let policy = TimeShardPolicy::new(ShardLevel::Year);
        assert_eq!(policy.resolve(time()).as_deref(), Some("2026"));
        assert_eq!(policy.table_name("WmsOrder", time()), "WmsOrder_2026");

        let policy = TimeShardPolicy::new(ShardLevel::Month);
        assert_eq!(policy.table_name("WmsOrder", time()), "WmsOrder_202609");

        let policy = TimeShardPolicy::new(ShardLevel::Day);
        assert_eq!(policy.table_name("WmsOrder", time()), "WmsOrder_20260927");

        let policy = TimeShardPolicy::new(ShardLevel::Hour);
        assert_eq!(policy.table_name("WmsOrder", time()), "WmsOrder_2026092710");
    }

    #[test]
    fn custom_format() {
        let mut policy = TimeShardPolicy::new(ShardLevel::Month);
        policy.set_format("{0}{1}");
        assert_eq!(policy.shard_table("Log", time()), "Log202609");

        // 将 {0}/{1} 都替换回自身，验证普通格式
        policy.set_format("shard_{1}_{0}");
        assert_eq!(policy.shard_table("Log", time()), "shard_202609_Log");
    }
}
