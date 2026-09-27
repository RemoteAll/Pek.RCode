//! 统计模型（对应 DH.NCode `Statistics` 模块的可移植部分）。
//!
//! - [`StatModes`]：统计方式（Max/Min/Avg/Sum/Count）；
//! - [`StatLevels`]：统计层级（All/Year/Month/Day/Hour/Minute/Quarter）；
//! - [`StatField`]：统计字段（以列名替代 C# 的 `FieldItem`）；
//! - [`StatModel`]：时间 + 层级的统计主键模型，支持层级格式化、显示文本与层级分割。
//!
//! C# `StatModel.Fill`（反射填充请求参数）与 `StatHelper.GetOrAdd`（实体查找）依赖反射与实体层，
//! 在 Rust 中由调用方显式构造，见迁移文档"机制差异"一节。

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

/// 统计方式（对齐 `StatModes`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatModes {
    /// 全部（0）。
    All = 0,
    /// 最大值（1）。
    Max = 1,
    /// 最小值（2）。
    Min = 2,
    /// 平均值（3）。
    Avg = 3,
    /// 求和（4）。
    Sum = 4,
    /// 计数（5）。
    Count = 5,
}

impl StatModes {
    /// 从数值解析统计方式。
    /// <param name="value">数值</param>
    /// <returns>统计方式，未知值返回 None</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::All),
            1 => Some(Self::Max),
            2 => Some(Self::Min),
            3 => Some(Self::Avg),
            4 => Some(Self::Sum),
            5 => Some(Self::Count),
            _ => None,
        }
    }
}

/// 统计层级（对齐 `StatLevels`；数值与 C# 一致，含 Quarter = 11）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatLevels {
    /// 全局（0）。
    All = 0,
    /// 年（1）。
    Year = 1,
    /// 月（2）。
    Month = 2,
    /// 日（3）。
    Day = 3,
    /// 小时（4）。
    Hour = 4,
    /// 分钟（5）。
    Minute = 5,
    /// 季度（11）。
    Quarter = 11,
}

impl StatLevels {
    /// 从数值解析统计层级。
    /// <param name="value">数值</param>
    /// <returns>统计层级，未知值返回 None</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::All),
            1 => Some(Self::Year),
            2 => Some(Self::Month),
            3 => Some(Self::Day),
            4 => Some(Self::Hour),
            5 => Some(Self::Minute),
            11 => Some(Self::Quarter),
            _ => None,
        }
    }
}

/// 统计字段（对齐 `StatField`，以列名替代 C# 的 `FieldItem`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatField {
    /// 列名。
    pub column: String,
    /// 统计方式。
    pub mode: StatModes,
}

impl StatField {
    /// 实例化。
    /// <param name="column">列名</param>
    /// <param name="mode">统计方式</param>
    pub fn new(column: impl Into<String>, mode: StatModes) -> Self {
        Self { column: column.into(), mode }
    }
}

/// 统计模型（对齐 `StatModel`）：时间 + 层级的组合构成统计主键。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatModel {
    /// 时间。
    pub time: NaiveDateTime,
    /// 层级。
    pub level: StatLevels,
}

impl StatModel {
    /// 实例化。
    /// <param name="level">统计层级</param>
    /// <param name="time">时间</param>
    pub fn new(level: StatLevels, time: NaiveDateTime) -> Self {
        Self { level, time }
    }

    /// 获取不同层级的时间。选择层级区间的开头（对齐 `GetDate`）。
    /// <param name="level">目标层级</param>
    /// <returns>格式化后的时间（Quarter 等未支持层级保持原值，对齐 C# default 分支）</returns>
    pub fn get_date(&self, level: StatLevels) -> NaiveDateTime {
        let dt = self.time;
        let (y, m, d, h, mi) = (dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute());
        match level {
            StatLevels::All => NaiveDateTime::MIN, // C# new DateTime(1, 1, 1)
            StatLevels::Year => date(y, 1, 1),
            StatLevels::Month => date(y, m, 1),
            StatLevels::Day => date(y, m, d),
            StatLevels::Hour => datetime(y, m, d, h, 0),
            StatLevels::Minute => datetime(y, m, d, h, mi),
            _ => dt,
        }
    }

    /// 数据库时间转显示字符串（对齐 `ToString`）。
    /// <returns>显示文本</returns>
    pub fn format_display(&self) -> String {
        let dt = self.time;
        match self.level {
            StatLevels::All => "全局".into(),
            StatLevels::Year => format!("{dt:?}").split('-').next().unwrap_or_default().to_string(),
            StatLevels::Month => format!("{}-{:02}", dt.year(), dt.month()),
            StatLevels::Day => format!("{}-{:02}-{:02}", dt.year(), dt.month(), dt.day()),
            StatLevels::Hour => format!("{}-{:02}-{:02} {:02}", dt.year(), dt.month(), dt.day(), dt.hour()),
            StatLevels::Minute => format!(
                "{}-{:02}-{:02} {:02}:{:02}",
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute()
            ),
            _ => format!("{:?}", self.level), // 对齐 C# `Level + ""`（枚举名）
        }
    }

    /// 分割为多个层级（对齐 `Split`）：按目标层级将时间格式化到区间开头。
    /// <param name="levels">目标层级集合</param>
    /// <returns>新的统计模型列表</returns>
    pub fn split(&self, levels: &[StatLevels]) -> Vec<StatModel> {
        levels
            .iter()
            .map(|level| StatModel {
                level: *level,
                time: self.get_date(*level),
            })
            .collect()
    }
}

/// 构造日期时间（0 点）。
fn date(y: i32, m: u32, d: u32) -> NaiveDateTime {
    datetime(y, m, d, 0, 0)
}

/// 构造日期时间。
fn datetime(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .and_then(|date| date.and_hms_opt(h, mi, 0))
        .unwrap_or(NaiveDateTime::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造时间（秒为 0）。
    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
    }

    #[test]
    fn enum_values_match_csharp() {
        assert_eq!(StatModes::All as i32, 0);
        assert_eq!(StatModes::Count as i32, 5);
        assert_eq!(StatLevels::Minute as i32, 5);
        assert_eq!(StatLevels::Quarter as i32, 11);
        assert_eq!(StatLevels::from_i32(11), Some(StatLevels::Quarter));
        assert_eq!(StatModes::from_i32(6), None);
    }

    #[test]
    fn get_date_formats_to_level_start() {
        let m = StatModel::new(StatLevels::Day, dt(2026, 9, 15, 10, 30));
        assert_eq!(m.get_date(StatLevels::All), NaiveDateTime::MIN);
        assert_eq!(m.get_date(StatLevels::Year), dt(2026, 1, 1, 0, 0));
        assert_eq!(m.get_date(StatLevels::Month), dt(2026, 9, 1, 0, 0));
        assert_eq!(m.get_date(StatLevels::Day), dt(2026, 9, 15, 0, 0));
        assert_eq!(m.get_date(StatLevels::Hour), dt(2026, 9, 15, 10, 0));
        assert_eq!(m.get_date(StatLevels::Minute), dt(2026, 9, 15, 10, 30));
        // Quarter 保持原值（对齐 C# default 分支）
        let origin = dt(2026, 9, 15, 10, 30);
        assert_eq!(m.get_date(StatLevels::Quarter), origin);
    }

    #[test]
    fn display_and_split() {
        let m = StatModel::new(StatLevels::Day, dt(2026, 9, 15, 10, 30));
        assert_eq!(m.format_display(), "2026-09-15");
        assert_eq!(StatModel::new(StatLevels::All, m.time).format_display(), "全局");
        assert_eq!(StatModel::new(StatLevels::Month, m.time).format_display(), "2026-09");
        assert_eq!(
            StatModel::new(StatLevels::Minute, m.time).format_display(),
            "2026-09-15 10:30"
        );
        assert_eq!(StatModel::new(StatLevels::Quarter, m.time).format_display(), "Quarter");

        let list = m.split(&[StatLevels::All, StatLevels::Day]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].level, StatLevels::All);
        assert_eq!(list[0].time, NaiveDateTime::MIN);
        assert_eq!(list[1].level, StatLevels::Day);
        assert_eq!(list[1].time, dt(2026, 9, 15, 0, 0));
    }
}
