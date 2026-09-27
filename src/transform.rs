//! 数据抽取器（对应 DH.NCode `Transform` 模块）。
//!
//! C# 版本直接依赖 `DAL.Query` 迭代返回 `DbTable`；Rust 版拆分为「游标推进纯逻辑」与「调用方执行查询」，
//! 便于测试与复用。三个游标分别对齐 `IdExtracter`（自增主键）、`TimeExtracter`（时间窗口）
//! 与 `PagingExtracter`（分页），批次大小对齐 C# 默认 5000。

use chrono::{Duration, NaiveDateTime};

/// 默认批次大小（对齐 C# `BatchSize = 5000`）。
pub const DEFAULT_BATCH_SIZE: i32 = 5000;

/// 整数主键抽取游标（对齐 `IdExtracter`）。
///
/// 每批条件：`{column} > {row}`，`ORDER BY {column} ASC`，取 `batch_size` 条；
/// 推进规则：取本批最后一行的主键作为下一批起点，不足一批即结束。
#[derive(Debug, Clone)]
pub struct IdCursor {
    /// 已抽取到的最大主键（下一批从此值之后开始）。
    pub row: i64,
    /// 批次大小。
    pub batch_size: i32,
    /// 累计行数。
    pub total_count: i64,
}

impl IdCursor {
    /// 实例化。
    /// <param name="batch_size">批次大小（&lt;=0 时使用默认 5000）</param>
    pub fn new(batch_size: i32) -> Self {
        Self {
            row: 0,
            batch_size: if batch_size > 0 { batch_size } else { DEFAULT_BATCH_SIZE },
            total_count: 0,
        }
    }

    /// 生成当前批次的过滤片段（`{column} > {row}`）。
    /// <param name="column">主键列名</param>
    /// <returns>SQL 条件片段</returns>
    pub fn where_clause(&self, column: &str) -> String {
        format!("{column} > {}", self.row)
    }

    /// 一批数据返回后推进游标。
    /// <param name="last_id">本批最后一行的主键（0 行时忽略）</param>
    /// <param name="count">本批行数</param>
    /// <returns>是否应继续抽取（本批满批时继续，对齐 C# `count == BatchSize`）</returns>
    pub fn advance(&mut self, last_id: i64, count: usize) -> bool {
        if count == 0 {
            return false;
        }
        self.row = last_id;
        self.total_count += count as i64;
        count == self.batch_size as usize
    }
}

/// 时间窗口抽取游标（对齐 `TimeExtracter`）。
///
/// 首查取第一条数据的时间作为起点；随后按 `[start, end)` 窗口分批，
/// 空转时步进加倍（60 秒起，上限 1 天），逼近当前时间（`start + 60s >= now`）即止步，
/// 下次调用从上次停止处继续。
#[derive(Debug, Clone)]
pub struct TimeCursor {
    /// 当前进度（本批起点）。
    pub start_time: NaiveDateTime,
    /// 批次大小。
    pub batch_size: i32,
    /// 累计行数。
    pub total_count: i64,
    /// 当前步进（初始 60 秒，空转加倍，上限 1 天）。
    pub step: Duration,
}

impl TimeCursor {
    /// 最小步进（对齐 C# `minStep = 60s`）。
    pub const MIN_STEP: Duration = Duration::seconds(60);
    /// 最大步进（对齐 C# `maxStep = 1 天`）。
    pub const MAX_STEP: Duration = Duration::days(1);

    /// 实例化。
    /// <param name="start_time">起始时间（通常为首条数据的时间）</param>
    /// <param name="batch_size">批次大小（&lt;=0 时使用默认 5000）</param>
    pub fn new(start_time: NaiveDateTime, batch_size: i32) -> Self {
        Self {
            start_time,
            batch_size: if batch_size > 0 { batch_size } else { DEFAULT_BATCH_SIZE },
            total_count: 0,
            step: Self::MIN_STEP,
        }
    }

    /// 计算当前分片窗口 `[start, end)`（`end` 不超过当前时间，对齐 C# `end > now → end = now`）。
    /// <param name="now">当前时间</param>
    /// <returns>窗口起止时间</returns>
    pub fn window(&self, now: NaiveDateTime) -> (NaiveDateTime, NaiveDateTime) {
        let mut end = self.start_time + self.step;
        if end > now {
            end = now;
        }
        (self.start_time, end)
    }

    /// 生成当前窗口的过滤片段（`{column} >= start AND {column} < end`，时间为 ISO 文本）。
    /// <param name="column">时间列名</param>
    /// <param name="now">当前时间</param>
    /// <returns>SQL 条件片段</returns>
    pub fn where_clause(&self, column: &str, now: NaiveDateTime) -> String {
        let (start, end) = self.window(now);
        let fmt = |t: NaiveDateTime| t.format("%Y-%m-%d %H:%M:%S").to_string();
        format!("{column} >= '{}' AND {column} < '{}'", fmt(start), fmt(end))
    }

    /// 一批数据返回后推进（`start = last_time + 1 秒`，多次赋值时以最后一批为准）。
    /// <param name="last_time">本批最后一行的值（0 行时忽略）</param>
    /// <param name="count">本批行数</param>
    pub fn advance(&mut self, last_time: NaiveDateTime, count: usize) {
        if count == 0 {
            return;
        }
        self.start_time = last_time + Duration::seconds(1);
        self.total_count += count as i64;
    }

    /// 空转扩大步进（踏空：一批也没有时步进加倍，上限 1 天）。
    pub fn widen(&mut self) {
        if self.step < Self::MAX_STEP {
            self.step = (self.step * 2).min(Self::MAX_STEP);
        }
    }

    /// 是否止步（对齐 C# `StartTime.Add(minStep) >= now`）。
    /// <param name="now">当前时间</param>
    /// <returns>是否应停止抽取</returns>
    pub fn should_stop(&self, now: NaiveDateTime) -> bool {
        self.start_time + Self::MIN_STEP >= now
    }
}

/// 分页抽取游标（对齐 `PagingExtracter`）：`offset = row`，每批后 `row += batch_size`。
#[derive(Debug, Clone)]
pub struct PagingCursor {
    /// 当前偏移量。
    pub row: i64,
    /// 批次大小。
    pub batch_size: i32,
}

impl PagingCursor {
    /// 实例化。
    /// <param name="batch_size">批次大小（&lt;=0 时使用默认 5000）</param>
    pub fn new(batch_size: i32) -> Self {
        Self {
            row: 0,
            batch_size: if batch_size > 0 { batch_size } else { DEFAULT_BATCH_SIZE },
        }
    }

    /// 当前偏移量。
    /// <returns>查询偏移量</returns>
    pub fn offset(&self) -> i64 {
        self.row
    }

    /// 一批数据返回后推进偏移量（对齐 C# `Row += BatchSize`）。
    /// <param name="count">本批行数</param>
    /// <returns>是否应继续抽取（本批满批时继续）</returns>
    pub fn advance(&mut self, count: usize) -> bool {
        if count == 0 {
            return false;
        }
        self.row += self.batch_size as i64;
        count == self.batch_size as usize
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    /// 构造时间。
    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, s).unwrap()
    }

    #[test]
    fn id_cursor_advance_rules() {
        let mut c = IdCursor::new(10);
        assert_eq!(c.where_clause("ID"), "ID > 0");
        // 满批：继续
        assert!(c.advance(100, 10));
        assert_eq!(c.row, 100);
        assert_eq!(c.total_count, 10);
        assert_eq!(c.where_clause("ID"), "ID > 100");
        // 不满批：结束
        assert!(!c.advance(105, 5));
        assert_eq!(c.total_count, 15);
        // 0 行：结束且不推进
        let mut c2 = IdCursor::new(0);
        assert_eq!(c2.batch_size, DEFAULT_BATCH_SIZE);
        assert!(!c2.advance(0, 0));
        assert_eq!(c2.row, 0);
    }

    #[test]
    fn time_cursor_window_and_step() {
        let start = dt(2026, 9, 15, 10, 0, 0);
        let c = TimeCursor::new(start, 100);
        // 窗口正常：start + 60s
        let now = dt(2026, 9, 15, 12, 0, 0);
        assert_eq!(c.window(now).1, dt(2026, 9, 15, 10, 1, 0));
        // 窗口被 now 截断
        let near = dt(2026, 9, 15, 10, 0, 30);
        assert_eq!(c.window(near), (start, near));
        // 空转加倍封顶 1 天
        let mut c2 = c.clone();
        for _ in 0..20 {
            c2.widen();
        }
        assert_eq!(c2.step, TimeCursor::MAX_STEP);
        // 推进：最后时间 + 1 秒
        let mut c3 = c.clone();
        c3.advance(dt(2026, 9, 15, 10, 0, 59), 50);
        assert_eq!(c3.start_time, dt(2026, 9, 15, 10, 1, 0));
        assert_eq!(c3.total_count, 50);
        // 止步：接近当前时间
        assert!(c3.should_stop(dt(2026, 9, 15, 10, 1, 30)));
        assert!(!c3.should_stop(dt(2026, 9, 15, 10, 2, 1)));
    }

    #[test]
    fn paging_cursor_advance_rules() {
        let mut c = PagingCursor::new(10);
        assert_eq!(c.offset(), 0);
        assert!(c.advance(10));
        assert_eq!(c.offset(), 10);
        assert!(!c.advance(3));
        assert_eq!(c.offset(), 20);
        assert!(!c.advance(0));
        assert_eq!(c.offset(), 20);
    }
}
