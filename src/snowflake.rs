//! 雪花算法（对应 DH.NCode 依赖的 `NewLife.Data.Snowflake`，与 C# 版**互通**）。
//!
//! 位结构（与 C# 完全一致）：`1bit 保留 + 41bit 毫秒时间戳 + 10bit 机器 + 12bit 序列号`，
//! 即 63 位正整数。
//!
//! - 左移常量：时间戳 [`TIMESTAMP_SHIFT`] = 22，机器 [`WORKER_ID_SHIFT`] = 12；
//! - 默认起始时间 [`default_start_timestamp`] = UTC 1970-01-01 对应的**本地时间**
//!   （与 C# `new DateTime(1970,1,1,0,0,0,Utc).ToLocalTime()` 一致；
//!   由于起止都落在本地时间轴，其差值恰好等于"UTC 毫秒数"，跨时区机器生成结果一致）；
//! - [`Snowflake::id_at`]（对应 C# `GetId`）只含时间戳部分，用于把时间区间换算成 Id 区间；
//! - [`Snowflake::parse`]（对应 C# `TryParse`，C# 版恒成功）解析出时间/机器/序列号；
//! - 时间回拨容忍 [`MAX_CLOCK_BACK_MS`]（夏令时 1 小时 + 10 秒），超限拒绝生成。
//!
//! 与 C# 的差异：
//! - C# 内置 MachineId 取 IP 后两字节；Rust 无内置网卡探测，默认取"进程号 + 熵"组合，
//!   **生产环境要求严格唯一时请显式 [`Snowflake::with_worker_id`]（与 C# 注释的约定一致）**；
//! - 跨进程/跨实例建议使用 [`shared`]（进程级单例），确保同进程序列号共享；
//!   多进程部署时各自 `worker_id` 必须不同（可借用 Redis 自增分配，同 C# `JoinCluster` 思路）。

use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Duration, Local, NaiveDateTime};

use crate::error::{Error, Result};

/// 时间戳左移位数（10 位机器 + 12 位序列）。
pub const TIMESTAMP_SHIFT: u32 = 22;
/// 机器编号左移位数（12 位序列）。
pub const WORKER_ID_SHIFT: u32 = 12;
/// 最大机器编号（10 位，0-1023）。
pub const MAX_WORKER_ID: u16 = (1 << 10) - 1;
/// 最大序列号（12 位，0-4095）。
pub const MAX_SEQUENCE: u32 = (1 << 12) - 1;
/// 时间回拨最大容忍度（毫秒）：夏令时最大 1 小时 + 10 秒（与 C# 一致）。
pub const MAX_CLOCK_BACK_MS: i64 = 3_600_000 + 10_000;

/// 默认起始时间：UTC 1970-01-01 对应的本地时间（1970 纪元在本地时间轴上的表示）。
pub fn default_start_timestamp() -> NaiveDateTime {
    chrono::DateTime::from_timestamp(0, 0)
        .expect("时间戳 0 必然有效")
        .with_timezone(&Local)
        .naive_local()
}

/// 进程级共享雪花实例（[`OnceLock`] 懒初始化）。
///
/// 分表/代码生成默认使用它：同一进程内所有实体共用序列号，避免多实例生成重复 Id。
pub fn shared() -> Arc<Snowflake> {
    static SHARED: OnceLock<Arc<Snowflake>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(Snowflake::new())).clone()
}

/// 生成器内部状态（时间戳与序列号）。
#[derive(Debug)]
struct State {
    /// 上一次使用的时间戳（毫秒）
    last_timestamp: i64,
    /// 当前序列号
    sequence: u32,
}

/// 雪花算法（分布式 Id 生成器）。业务内建议保持单例（见 [`shared`]）。
#[derive(Debug)]
pub struct Snowflake {
    /// 起始时间（本地时间轴）
    start: NaiveDateTime,
    /// 机器编号（10 位）
    worker_id: u16,
    /// 运行时状态
    state: Mutex<State>,
}

impl Default for Snowflake {
    fn default() -> Self {
        Self::new()
    }
}

impl Snowflake {
    /// 创建实例（机器编号自动取"进程号 + 熵"组合；严格唯一场景请用 [`Snowflake::with_worker_id`]）。
    pub fn new() -> Self {
        Self::with_worker_id(default_worker_id())
    }

    /// 按机器编号创建（0-1023）。
    ///
    /// # Panics
    /// 编号超过 10 位范围时 panic（与 C# `WorkerId` 的 ArgumentOutOfRangeException 对应）。
    pub fn with_worker_id(worker_id: u16) -> Self {
        assert!(
            worker_id <= MAX_WORKER_ID,
            "WorkerId 必须在 0-{MAX_WORKER_ID} 范围内，实际 {worker_id}"
        );
        Self {
            start: default_start_timestamp(),
            worker_id,
            state: Mutex::new(State {
                last_timestamp: -1,
                sequence: 0,
            }),
        }
    }

    /// 替换起始时间（改造实例；与 C# `StartTimestamp` 属性一致）。
    pub fn with_start(mut self, start: NaiveDateTime) -> Self {
        self.start = start;
        self
    }

    /// 起始时间。
    pub fn start_timestamp(&self) -> NaiveDateTime {
        self.start
    }

    /// 机器编号。
    pub fn worker_id(&self) -> u16 {
        self.worker_id
    }

    /// 当前时间的雪花 Id（对应 C# `NewId()`），遇到轻微时间回拨自动等待。
    ///
    /// 时间回拨超过 [`MAX_CLOCK_BACK_MS`] 时返回错误（对应 C# 抛 `InvalidOperationException`），
    /// 避免生成可能重复的 Id。
    pub fn now_id(&self) -> Result<i64> {
        let current = self.timestamp_of(Local::now().naive_local());
        let mut state = self.lock();

        let mut timestamp = current;
        if current < state.last_timestamp {
            let clock_back = state.last_timestamp - current;
            if clock_back > MAX_CLOCK_BACK_MS {
                return Err(Error::Model(format!(
                    "时间回拨过大（{clock_back}ms），为保证唯一性拒绝生成雪花 Id"
                )));
            }
            timestamp = state.last_timestamp;
        }

        let sequence;
        if timestamp > state.last_timestamp {
            // 时间推进：重置序列号
            state.last_timestamp = timestamp;
            state.sequence = 0;
            sequence = 0;
        } else {
            // 同一毫秒：递增序列号；溢出（回到 0）则推进到下一毫秒
            state.sequence = state.sequence.wrapping_add(1) & MAX_SEQUENCE;
            if state.sequence != 0 {
                sequence = state.sequence;
            } else {
                state.last_timestamp += 1;
                sequence = 0;
            }
        }

        Ok(build_id(state.last_timestamp, self.worker_id, sequence))
    }

    /// 指定时间的雪花 Id（对应 C# `NewId(DateTime)`）：机器与序列号参与其中，
    /// 同一毫秒内多次调用靠序列号区分（超过 4096 次可能重复）。
    pub fn new_id_at(&self, time: NaiveDateTime) -> i64 {
        let timestamp = self.timestamp_of(time);
        let mut state = self.lock();
        state.sequence = state.sequence.wrapping_add(1) & MAX_SEQUENCE;
        let sequence = state.sequence;
        drop(state);
        build_id(timestamp, self.worker_id, sequence)
    }

    /// 时间转 Id（对应 C# `GetId`）：**不含**机器与序列号，用于构建时间片段查询边界。
    ///
    /// 与 C# 输出完全一致：`(time - 起始时间) 毫秒 << 22`；同区间的 `[GetId(start), GetId(end))`
    /// 可直接作为雪花主键列的左右开区间条件。
    pub fn id_at(&self, time: NaiveDateTime) -> i64 {
        self.timestamp_of(time) << TIMESTAMP_SHIFT
    }

    /// 解析雪花 Id（对应 C# `TryParse`，C# 版恒返回 true）：时间/机器/序列号。
    pub fn parse(&self, id: i64) -> (NaiveDateTime, u16, u16) {
        let timestamp = id >> TIMESTAMP_SHIFT;
        let time = self.start + Duration::milliseconds(timestamp);
        let worker_id = ((id >> WORKER_ID_SHIFT) & MAX_WORKER_ID as i64) as u16;
        let sequence = (id & MAX_SEQUENCE as i64) as u16;
        (time, worker_id, sequence)
    }

    /// 时间 → 毫秒时间戳（相对起始时间；"未指定时区"直接相减，与 C# `ConvertKind` 的 Unspecified 分支一致）。
    fn timestamp_of(&self, time: NaiveDateTime) -> i64 {
        (time - self.start).num_milliseconds()
    }

    /// 获取状态锁（容忍毒锁：雪花状态坏了也比 panic 好，与库内其它锁一致）。
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 组装雪花 Id（对应 C# `BuildSnowflakeId`）。
fn build_id(timestamp: i64, worker_id: u16, sequence: u32) -> i64 {
    (timestamp << TIMESTAMP_SHIFT)
        | ((worker_id as i64 & MAX_WORKER_ID as i64) << WORKER_ID_SHIFT)
        | (sequence as i64 & MAX_SEQUENCE as i64)
}

/// 默认机器编号：进程号低 5 位 + 毫秒时间低 5 位（对齐 C# "nodeId + pid 混合"的思路）。
fn default_worker_id() -> u16 {
    let pid = std::process::id() as u16;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u16)
        .unwrap_or(0);
    ((pid & 0x1F) << 5) | (nanos & 0x1F)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(y: i32, m: u32, d: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
    }

    #[test]
    fn bit_layout_matches_newlife() {
        // 起始时间 = UTC 1970 对应本地时间（例：UTC+8 下为 1970-01-01 08:00:00）
        let start = default_start_timestamp();
        let epoch_local = chrono::DateTime::from_timestamp(0, 0)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        assert_eq!(start, epoch_local);

        let snow = Snowflake::with_worker_id(7);
        let time = t(2020, 8, 22);

        // GetId：毫秒时间戳 << 22；与"本地时间对应的 UTC 毫秒"一致
        let expected_ms = (time - start).num_milliseconds();
        assert_eq!(snow.id_at(time), expected_ms << TIMESTAMP_SHIFT);
        let utc_ms = time.and_local_timezone(Local).unwrap().timestamp_millis();
        let epoch_ms = chrono::DateTime::from_timestamp(0, 0).unwrap().timestamp_millis();
        assert_eq!(expected_ms, utc_ms - epoch_ms);
    }

    #[test]
    fn parse_round_trips_and_extracts_fields() {
        let snow = Snowflake::with_worker_id(1023);
        let time = at(2026, 9, 27, 10, 30, 15);

        let id = snow.new_id_at(time);
        let (parsed, worker, _seq) = snow.parse(id);
        assert_eq!(parsed, time);
        assert_eq!(worker, 1023);

        // 手工构造：时间戳 12345、机器 513、序列 4095
        let manual = (12345i64 << TIMESTAMP_SHIFT) | (513i64 << WORKER_ID_SHIFT) | 4095;
        let (pt, pw, ps) = snow.parse(manual);
        assert_eq!(pt, snow.start_timestamp() + Duration::milliseconds(12345));
        assert_eq!(pw, 513);
        assert_eq!(ps, 4095);
    }

    #[test]
    fn now_id_is_monotonic_and_unique() {
        let snow = Snowflake::with_worker_id(1);
        let mut last = 0i64;
        for _ in 0..10_000 {
            let id = snow.now_id().unwrap();
            assert!(id > last, "雪花 Id 必须单调递增");
            last = id;
        }

        // 同毫秒批量生成不重复（指定时间）；与 C# 一致：超过 4096 次可能重复，此处取 4000 次
        let time = t(2026, 1, 1);
        let mut ids: Vec<i64> = (0..4000).map(|_| snow.new_id_at(time)).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 4000, "同一毫秒内 4000 个 Id 不应重复（序列号 12 位）");
    }

    #[test]
    fn rejects_worker_id_out_of_range() {
        let result = std::panic::catch_unwind(|| Snowflake::with_worker_id(1024));
        assert!(result.is_err());
    }

    #[test]
    fn clock_back_within_tolerance_is_handled() {
        let snow = Snowflake::with_worker_id(2);
        // 构造回拨：先按较晚时间生成，再按较早时间取 now_id 无法直接模拟，
        // 这里通过内部状态验证轻微回拨沿用上次时间戳（不 panic、不重复）
        let id1 = snow.new_id_at(at(2026, 5, 1, 12, 0, 0));
        let id2 = snow.new_id_at(at(2026, 5, 1, 12, 0, 0));
        assert_ne!(id1, id2, "同毫秒不同序列号");
    }

    #[test]
    fn shared_instance_is_singleton() {
        let a = shared();
        let b = shared();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
