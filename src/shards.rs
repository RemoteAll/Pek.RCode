//! 分表（对应 DH.NCode 的 `Shards/IShardPolicy.cs` 与 `Shards/TimeShardPolicy.cs`，含跨表执行引擎）。
//!
//! 按时间把同一实体的数据落到不同物理表，如 `WmsLog` → `WmsLog_202609`，
//! **表名/连接名生成规则与 C# 版完全一致**（`String.Format` 模板 + .NET 日期格式子集），
//! 两边可直接共用同一批数据库表。
//!
//! ## 与 C# 的对应关系
//!
//! | C# / DH.NCode | Rust / pek-rcode |
//! |---------------|-----------------|
//! | `ShardModel(ConnName, TableName)` | [`ShardModel`]（字段可空 = 沿用基础连接 / 表名） |
//! | `TimeShardPolicy`（`ConnPolicy` / `TablePolicy` / `Step` / `Level`） | [`TimeShardPolicy`] |
//! | `Shard(DateTime / Int64 / IModel)` | [`shard_of_time`](TimeShardPolicy::shard_of_time) / [`shard_of_id`](TimeShardPolicy::shard_of_id) / [`shard_of_value`](TimeShardPolicy::shard_of_value) |
//! | `Shards(start, end)`（时间区间展开） | [`shards_between`](TimeShardPolicy::shards_between) |
//! | `Shards(expression)`（按条件推导） | [`shards_of`](TimeShardPolicy::shards_of) / [`shards_of_trim`](TimeShardPolicy::shards_of_trim) |
//! | `Meta.AutoShard(start, end, cb)` | [`TableRef::auto_shard`] |
//! | `FindAll` 分表分支（跨表分页） | [`TableRef::query_sharded`] |
//! | `FindCount` 分表求和 | [`TableRef::count_sharded`] |
//! | `Delete(expression)` 分表 | [`TableRef::delete_sharded`] |
//!
//! ## 与 C# 的差异（有意为之，均已在代码注释说明）
//!
//! - C# 通过全局可变的 `Meta.TableName` 切换分表上下文；Rust **不引入全局可变状态**，
//!   分表表名以显式表句柄（[`crate::dal::Dal::table_as`]）承载，由本模块的引擎方法自动切换；
//! - `Where` 中的 `BETWEEN` 按 SQL 闭区间语义处理（右端 +1 秒参与扫描），只会多扫一个边界分表，不会漏数据；
//! - 连接级分表（`ConnPolicy`）只参与**连接名 / 表名计算**（见 [`ShardModel::conn_name`]）；
//!   跨连接执行需要消费方按连接名自行路由多个 `Dal`——若分片连接与当前连接不同，引擎会
//!   **显式报错**（绝不静默落到当前库），不会自动切换连接；
//! - 日期格式只实现了 .NET 自定义格式的常用子集：`y/yy/yyyy`、`M/MM`、`d/dd`、`H/HH`、`m/mm`、`s/ss`、`f...`（小数秒截断）
//!   以及 `\x` / `'...'` 字面量转义；`ddd` / `MMMM` 等名称形式按数字处理。

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, TimeDelta, Timelike};

use crate::dal::TableRef;
use crate::error::{Error, Result};
use crate::model::TableMeta;
use crate::query::{Op, OrderBy, Query, Where};
use crate::session::{DbRow, RowSet, SqlSession};
use crate::snowflake::Snowflake;
use crate::value::DbValue;

/// 分表目标（连接名 + 物理表名，可空 = 沿用基础值；对应 C# `ShardModel` 记录）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShardModel {
    /// 连接名（配置了 `ConnPolicy` 时生成；None 表示沿用基础连接）
    pub conn_name: Option<String>,
    /// 物理表名（配置了 `TablePolicy` 时生成；None 表示沿用基础表）
    pub table_name: Option<String>,
}

impl ShardModel {
    /// 创建。
    pub fn new(conn_name: Option<String>, table_name: Option<String>) -> Self {
        Self {
            conn_name,
            table_name,
        }
    }

    /// 仅指定表名。
    pub fn table(table_name: impl Into<String>) -> Self {
        Self {
            conn_name: None,
            table_name: Some(table_name.into()),
        }
    }

    /// 去重键（`连接#表`，与 C# `TimeShardPolicy.GetModels` 的哈希键一致）。
    pub fn key(&self) -> String {
        format!(
            "{}#{}",
            self.conn_name.as_deref().unwrap_or(""),
            self.table_name.as_deref().unwrap_or("")
        )
    }
}

/// 分表粒度（对应 C# `StatLevels` 的 Year/Month/Day/Hour 四档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardLevel {
    /// 按年（`2026`）
    Year,
    /// 按月（`202609`）
    Month,
    /// 按日（`20260927`）
    Day,
    /// 按小时（`2026092710`）
    Hour,
}

/// 分表基础信息（基础连接名 + 基础表名；对应 C# `Factory.Table.ConnName/TableName`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardBase<'a> {
    /// 基础连接名（模型级 / 表级 ConnName；未配置时 None）
    pub conn_name: Option<&'a str>,
    /// 基础表名（物理表名）
    pub table_name: &'a str,
}

impl<'a> ShardBase<'a> {
    /// 仅指定基础表名。
    pub fn new(table_name: &'a str) -> Self {
        Self {
            conn_name: None,
            table_name,
        }
    }

    /// 指定基础连接名。
    pub fn with_conn(mut self, conn_name: Option<&'a str>) -> Self {
        self.conn_name = conn_name;
        self
    }
}

/// 时间分表策略（对应 C# `TimeShardPolicy`）。
///
/// - `conn_policy` / `table_policy`：`String.Format` 模板，`{0}` = 基础名、`{1:格式}` = 时间
///   （如 `"{0}_{1:yyyyMM}"`）；
/// - `step`：时间区间步进（默认 1 天），用于推断 `level`；
/// - `level`：粒度，未显式设置时按 `step` 推断（>=360 天→年、28~31 天→月、1 天→日、1 小时→时，
///   其余自定义步进直接按 `step` 推进）；
/// - `snow`：雪花解析器（分表字段为 Int64 雪花主键时必需，对应 C# `Factory.Snow`）。
#[derive(Debug, Clone)]
pub struct TimeShardPolicy {
    /// 分表字段名（实体的时间列或雪花 Id 列）
    pub field: String,
    /// 连接名策略模板
    pub conn_policy: Option<String>,
    /// 表名策略模板
    pub table_policy: Option<String>,
    /// 时间区间步进
    pub step: TimeDelta,
    /// 粒度（None = 按 step 推断）
    pub level: Option<ShardLevel>,
    /// 雪花解析器
    pub snow: Option<Arc<Snowflake>>,
}

impl TimeShardPolicy {
    /// 指定分表字段创建（默认步进 1 天）。
    pub fn new(field: &str) -> Self {
        Self {
            field: field.to_string(),
            conn_policy: None,
            table_policy: None,
            step: TimeDelta::days(1),
            level: None,
            snow: None,
        }
    }

    /// 设置连接名模板（如 `"{0}_{1:yyyy}"`）。
    pub fn with_conn_policy(mut self, policy: &str) -> Self {
        self.conn_policy = Some(policy.to_string());
        self
    }

    /// 设置表名模板（如 `"{0}_{1:yyyyMM}"`）。
    pub fn with_table_policy(mut self, policy: &str) -> Self {
        self.table_policy = Some(policy.to_string());
        self
    }

    /// 设置步进。
    pub fn with_step(mut self, step: TimeDelta) -> Self {
        self.step = step;
        self
    }

    /// 设置步进（天）。
    pub fn with_step_days(mut self, days: i64) -> Self {
        self.step = TimeDelta::days(days);
        self
    }

    /// 设置步进（小时）。
    pub fn with_step_hours(mut self, hours: i64) -> Self {
        self.step = TimeDelta::hours(hours);
        self
    }

    /// 显式指定粒度。
    pub fn with_level(mut self, level: ShardLevel) -> Self {
        self.level = Some(level);
        self
    }

    /// 设置雪花解析器（分表字段为雪花 Id 时必需）。
    pub fn with_snow(mut self, snow: Arc<Snowflake>) -> Self {
        self.snow = Some(snow);
        self
    }

    /// 是否配置了分表模板（两者皆空 = 不分表，对应 C# `Shard` 返回 null）。
    pub fn is_configured(&self) -> bool {
        self.conn_policy.is_some() || self.table_policy.is_some()
    }

    /// 生效粒度（显式设置优先，否则按 `step` 推断；推断不出的自定义步进返回 None）。
    pub fn resolved_level(&self) -> Option<ShardLevel> {
        if let Some(level) = self.level {
            return Some(level);
        }
        let days = self.step.num_days();
        if days >= 360 {
            Some(ShardLevel::Year)
        } else if (28..=31).contains(&days) {
            Some(ShardLevel::Month)
        } else if self.step == TimeDelta::days(1) {
            Some(ShardLevel::Day)
        } else if self.step == TimeDelta::hours(1) {
            Some(ShardLevel::Hour)
        } else {
            None
        }
    }

    /// 按时间计算分表（对应 C# `Shard(DateTime)`）；未配置模板时返回 None。
    pub fn resolve_time(&self, base: ShardBase<'_>, time: NaiveDateTime) -> Option<ShardModel> {
        if !self.is_configured() {
            return None;
        }
        let conn_name = self
            .conn_policy
            .as_ref()
            .map(|policy| format_policy(policy, base.conn_name.unwrap_or(""), time));
        let table_name = self
            .table_policy
            .as_ref()
            .map(|policy| format_policy(policy, base.table_name, time));
        Some(ShardModel {
            conn_name,
            table_name,
        })
    }

    /// 按时间计算分表（要求时间有效；对应 C# `Shard(DateTime)` 的入参校验）。
    pub fn shard_of_time(
        &self,
        base: ShardBase<'_>,
        time: NaiveDateTime,
    ) -> Result<Option<ShardModel>> {
        if time.year() <= 1 {
            return Err(Error::Argument("分表策略要求指定时间！".into()));
        }
        Ok(self.resolve_time(base, time))
    }

    /// 按雪花 Id 计算分表（对应 C# `Shard(Int64)`：解析时间 → 分表；1970 年前视为无效）。
    pub fn shard_of_id(&self, base: ShardBase<'_>, id: i64) -> Result<Option<ShardModel>> {
        let snow = self
            .snow
            .as_ref()
            .ok_or_else(|| Error::Model("分表策略要求指定雪花解析器（Snowflake）！".into()))?;
        let (time, _, _) = snow.parse(id);
        if time.year() <= 1970 {
            return Err(Error::Model("雪花Id解析时间失败，无法用于分表".into()));
        }
        Ok(self.resolve_time(base, time))
    }

    /// 按字段值计算分表（对应 C# `Shard(IModel)`）：
    /// 时间列（`DbValue::DateTime`）→ 直接分表；雪花 Id 列（`DbValue::Int`）→ 解析后分表。
    pub fn shard_of_value(
        &self,
        base: ShardBase<'_>,
        value: &DbValue,
    ) -> Result<Option<ShardModel>> {
        match value {
            DbValue::DateTime(time) => {
                if time.year() <= 1970 {
                    return Err(Error::Model("实体对象时间字段为空，无法用于分表".into()));
                }
                Ok(self.resolve_time(base, *time))
            }
            DbValue::Int(id) => self.shard_of_id(base, *id),
            other => Err(Error::Model(format!(
                "时间分表策略不支持[{other:?}]类型字段"
            ))),
        }
    }

    /// 从时间区间计算多个分表（对应 C# `Shards(start, end)`，支持倒序）。
    pub fn shards_between(
        &self,
        base: ShardBase<'_>,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Result<Vec<ShardModel>> {
        if start.year() <= 1 {
            return Err(Error::Argument("分表策略要求指定时间！".into()));
        }
        if end.year() <= 1 {
            return Err(Error::Argument("分表策略要求指定时间！".into()));
        }
        if start <= end {
            Ok(self.get_models(base, start, end))
        } else {
            let mut models = self.get_models(base, end, start);
            models.reverse();
            Ok(models)
        }
    }

    /// 从查询条件推导分表（对应 C# `Shards(expression)`）。
    ///
    /// 语义与 C# 一致：
    /// - 条件中没有分表字段时返回空（调用方回退单表查询）；
    /// - `=` → 单表；`>` / `>=` → 起点（时间字段的 `>` 再 +1 秒）；`<` / `<=` → 终点（缺省为当前时间）；
    /// - 条件不足（存在分表字段条件但无法推导区间）时返回错误。
    pub fn shards_of(&self, base: ShardBase<'_>, filter: &Where) -> Result<Vec<ShardModel>> {
        let mut scratch = filter.clone();
        self.shard_models_of(base, &mut scratch, false)
    }

    /// 同 [`shards_of`](Self::shards_of)，但在"单个分表且区间恰好覆盖整个分片"时
    /// **移除条件中的起止时间条件**（对应 C# 的 `Trim`，查询走整表、条件不再重复过滤）。
    pub fn shards_of_trim(
        &self,
        base: ShardBase<'_>,
        filter: &mut Where,
    ) -> Result<Vec<ShardModel>> {
        self.shard_models_of(base, filter, true)
    }

    /// 条件推导实现（`trim` 控制是否执行 C# 的 Trim 优化）。
    fn shard_models_of(
        &self,
        base: ShardBase<'_>,
        filter: &mut Where,
        trim: bool,
    ) -> Result<Vec<ShardModel>> {
        // 收集分表字段条件（与 C# 一致：Eq 取首个、区间取首个有效上下界）
        let mut eq: Option<(NaiveDateTime, bool)> = None;
        let mut lower: Option<(usize, Op, NaiveDateTime, bool)> = None;
        let mut upper: Option<(usize, Op, NaiveDateTime, bool)> = None;
        let mut matched = false;

        for (index, cond) in filter.conds().iter().enumerate() {
            if !cond.column.eq_ignore_ascii_case(&self.field) {
                continue;
            }
            matched = true;
            match cond.op {
                Op::Eq => {
                    if eq.is_none()
                        && let Some(value) = cond.values.first()
                    {
                        eq = Some(self.as_time(value)?);
                    }
                }
                Op::Gt | Op::Ge => {
                    if lower.is_none()
                        && let Some(value) = cond.values.first()
                    {
                        let (time, is_time) = self.as_time(value)?;
                        lower = Some((index, cond.op, time, is_time));
                    }
                }
                Op::Lt | Op::Le => {
                    if upper.is_none()
                        && let Some(value) = cond.values.first()
                    {
                        let (time, is_time) = self.as_time(value)?;
                        upper = Some((index, cond.op, time, is_time));
                    }
                }
                Op::Between => {
                    // SQL 闭区间：右端 +1 秒扫描，保证边界分表被覆盖（只会多扫、不会漏）
                    if let (Some(low), Some(high)) = (cond.values.first(), cond.values.get(1)) {
                        if lower.is_none() {
                            let (time, is_time) = self.as_time(low)?;
                            lower = Some((index, Op::Ge, time, is_time));
                        }
                        if upper.is_none() {
                            let (time, is_time) = self.as_time(high)?;
                            upper = Some((index, Op::Lt, time + TimeDelta::seconds(1), is_time));
                        }
                    }
                }
                // 其余运算符不参与区间推导（与 C# 一致）
                _ => {}
            }
        }

        if !matched {
            // 条件中没有分表字段：不属于分表查询
            return Ok(Vec::new());
        }

        // 1) 等值：时间列按 1 秒窗口取单表；雪花 Id 直接解析单表
        if let Some((time, is_time)) = eq {
            if is_time {
                if time.year() > 1 {
                    return Ok(self.get_models(base, time, time + TimeDelta::seconds(1)));
                }
            } else if let Some(model) = self.resolve_time(base, time) {
                return Ok(vec![model]);
            }
        }

        // 2) 区间
        if let Some((lower_index, lower_op, lower_time, lower_is_time)) = lower {
            let start = if lower_is_time && lower_op == Op::Gt {
                lower_time + TimeDelta::seconds(1)
            } else {
                lower_time
            };
            let mut end = Local::now().naive_local();
            if let Some((_, _, upper_time, _)) = upper {
                end = upper_time;
            }

            let models = self.get_models(base, start, end);

            // Trim：单个分表且区间恰好完整覆盖该分片时，移除起止条件（与 C# 一致）
            if trim
                && lower_op == Op::Ge
                && let Some((upper_index, Op::Lt, _, _)) = upper
                && models.len() == 1
                && self.is_full(start, end)
            {
                filter.retain_conds(|index, _| index != lower_index && index != upper_index);
            }

            return Ok(models);
        }

        Err(Error::Model(
            "分表策略因条件不足无法执行分表查询操作！".into(),
        ))
    }

    /// 时间区间展开为分表集合（对应 C# `GetModels`）。
    fn get_models(
        &self,
        base: ShardBase<'_>,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Vec<ShardModel> {
        let mut models = Vec::new();
        let mut hash: HashSet<String> = HashSet::new();

        let step = self.step;
        let level = self.resolved_level();

        // 根据步进对齐起点：整天及以上对齐到日期，小时级对齐到整点（与 C# 一致）
        let mut dt = if step.num_days() >= 1 {
            start.date().and_hms_opt(0, 0, 0).unwrap_or(start)
        } else if start.hour() >= 1 {
            start
                .date()
                .and_hms_opt(start.hour(), 0, 0)
                .unwrap_or(start)
        } else {
            start
        };

        while dt < end {
            if let Some(model) = self.resolve_time(base, dt) {
                let key = model.key();
                if key != "#" && hash.insert(key) {
                    models.push(model);
                }
            }

            let next = get_next(level, dt, step);
            if next <= dt {
                // 防御：步进不前进（异常配置）直接结束，避免死循环
                break;
            }
            dt = next;
        }

        models
    }

    /// 区间是否恰好覆盖一个完整分片（对应 C# `IsFull`）。
    fn is_full(&self, start: NaiveDateTime, end: NaiveDateTime) -> bool {
        let level = self.resolved_level();
        let next = get_next(level, start, self.step);
        match level {
            Some(ShardLevel::Year) => next == end && add_months_floor(next, -12) == start,
            Some(ShardLevel::Month) => next == end && add_months_floor(next, -1) == start,
            Some(ShardLevel::Day) => next == end && next - TimeDelta::days(1) == start,
            Some(ShardLevel::Hour) => next == end && next - TimeDelta::hours(1) == start,
            None => false,
        }
    }

    /// 条件值 → 时间（时间列直接取值；雪花 Id 经解析；文本按多格式解析）。
    fn as_time(&self, value: &DbValue) -> Result<(NaiveDateTime, bool)> {
        match value {
            DbValue::DateTime(time) => Ok((*time, true)),
            DbValue::Int(id) => {
                let snow = self
                    .snow
                    .as_ref()
                    .ok_or_else(|| Error::Model("分表策略要求指定雪花解析器（Snowflake）！".into()))?;
                let (time, _, _) = snow.parse(*id);
                Ok((time, false))
            }
            DbValue::Text(text) => crate::value::parse_datetime(text)
                .map(|time| (time, true))
                .ok_or_else(|| Error::Model(format!("分表条件值[{text}]无法解析为时间或雪花Id"))),
            other => Err(Error::Model(format!(
                "分表条件值[{other:?}]无法解析为时间或雪花Id"
            ))),
        }
    }
}

/// 步进到下一个分片起点（对应 C# `GetNext`）。
fn get_next(level: Option<ShardLevel>, dt: NaiveDateTime, step: TimeDelta) -> NaiveDateTime {
    match level {
        Some(ShardLevel::Year) => NaiveDate::from_ymd_opt(dt.year() + 1, 1, 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .unwrap_or(dt + step),
        Some(ShardLevel::Month) => add_months_floor(dt, 1),
        Some(ShardLevel::Day) => (dt + TimeDelta::days(1))
            .date()
            .and_hms_opt(0, 0, 0)
            .unwrap_or(dt + TimeDelta::days(1)),
        Some(ShardLevel::Hour) => {
            let next = dt + TimeDelta::hours(1);
            next.date().and_hms_opt(next.hour(), 0, 0).unwrap_or(next)
        }
        None => dt + step,
    }
}

/// 按月平移（结果对齐到 1 号 0 点；对应 C# `AddMonths` + 归整）。
fn add_months_floor(dt: NaiveDateTime, months: i32) -> NaiveDateTime {
    let mut year = dt.year();
    let mut month = dt.month() as i32 + months;
    while month < 1 {
        month += 12;
        year -= 1;
    }
    while month > 12 {
        month -= 12;
        year += 1;
    }
    NaiveDate::from_ymd_opt(year, month as u32, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .unwrap_or(dt)
}

/// `String.Format` 风格分表模板渲染：`{0}` = 基础名、`{1:格式}` = 时间（.NET 日期格式子集）。
///
/// 未识别的占位符原样保留；`{1}`（无格式）按 `yyyy-MM-dd HH:mm:ss` 输出。
pub fn format_policy(template: &str, base: &str, time: NaiveDateTime) -> String {
    let mut out = String::with_capacity(template.len() + 8);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(offset) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &rest[start + 1..start + offset];
        match inner.split_once(':') {
            Some(("0", format)) => out.push_str(&dotnet_date_format(format, time)),
            Some(("1", format)) => out.push_str(&dotnet_date_format(format, time)),
            None if inner == "0" => out.push_str(base),
            None if inner == "1" => out.push_str(&dotnet_date_format("yyyy-MM-dd HH:mm:ss", time)),
            _ => {
                out.push('{');
                out.push_str(inner);
                out.push('}');
            }
        }
        rest = &rest[start + offset + 1..];
    }
    out.push_str(rest);
    out
}

/// .NET 自定义日期格式渲染（常用子集）：
///
/// - `y` 年（`yy` 两位、`yyy+` 四位）、`M` 月、`d` 日、`H` 时、`m` 分、`s` 秒（单字母不补零、双字母补两位）；
/// - `f...` 小数秒（按纳秒前 N 位截断，最多 9 位）；
/// - `\x` 转义下一个字符、`'...'` 内为字面量；其余字符原样输出。
pub fn dotnet_date_format(format: &str, time: NaiveDateTime) -> String {
    let chars: Vec<char> = format.chars().collect();
    let mut out = String::with_capacity(format.len() + 8);
    let mut index = 0;
    let mut literal = false;

    while index < chars.len() {
        let ch = chars[index];

        // 转义与字面量
        if ch == '\\' {
            if index + 1 < chars.len() {
                out.push(chars[index + 1]);
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if ch == '\'' {
            literal = !literal;
            index += 1;
            continue;
        }
        if literal {
            out.push(ch);
            index += 1;
            continue;
        }

        // 同字符连续段
        let mut run = 1;
        while index + run < chars.len() && chars[index + run] == ch {
            run += 1;
        }

        match ch {
            'y' => {
                if run == 1 {
                    out.push_str(&time.year().to_string());
                } else if run == 2 {
                    out.push_str(&format!("{:02}", time.year().rem_euclid(100)));
                } else {
                    out.push_str(&format!("{:04}", time.year()));
                }
            }
            'M' => push_number(&mut out, time.month() as i64, run),
            'd' => push_number(&mut out, time.day() as i64, run),
            'H' => push_number(&mut out, time.hour() as i64, run),
            'm' => push_number(&mut out, time.minute() as i64, run),
            's' => push_number(&mut out, time.second() as i64, run),
            'f' => {
                let nanos = format!("{:09}", time.nanosecond());
                let take = run.min(9);
                out.push_str(&nanos[..take]);
                for _ in take..run {
                    out.push('0');
                }
            }
            _ => {
                for _ in 0..run {
                    out.push(ch);
                }
            }
        }

        index += run;
    }

    out
}

/// 数字输出：单个符号不补零，两个及以上补两位（对应 .NET `M/d/H/m/s` 的重复规则）。
fn push_number(out: &mut String, value: i64, run: usize) {
    if run >= 2 {
        out.push_str(&format!("{value:02}"));
    } else {
        out.push_str(&value.to_string());
    }
}

/// 校验分片连接：分片连接名与基础连接一致（或未产生连接名）才能在当前 `Dal` 内执行。
///
/// 连接级分片（`ConnPolicy`）在 C# 中通过全局连接串注册表（`DAL.Create(connName)`）切换连接；
/// Rust 侧不引入全局连接注册表，**跨连接执行需消费方按连接名路由**。为避免静默把数据写到
/// 当前库，这里对"分片连接 ≠ 当前连接"显式返回错误（而非忽略连接名）。
pub(crate) fn ensure_conn_in_sync(base: ShardBase<'_>, model: &ShardModel) -> Result<()> {
    match &model.conn_name {
        Some(conn) if Some(conn.as_str()) != base.conn_name => Err(Error::Unsupported(format!(
            "分片连接 [{conn}] 与当前连接 [{}] 不同：跨连接（分库）执行需要消费方按连接名路由（参见 shards 模块文档），当前连接内无法安全执行",
            base.conn_name.unwrap_or("（未配置）")
        ))),
        _ => Ok(()),
    }
}

/// 调整分表顺序（对应 C# `FixOrder`）：按分表字段排序时，分表按"连接名、表名"升/降序排列。
fn fix_shard_order(
    shards: Vec<ShardModel>,
    table: &TableMeta,
    policy: &TimeShardPolicy,
    orders: &[OrderBy],
) -> Vec<ShardModel> {
    let field = &policy.field;
    let matches_field = |order: &OrderBy| -> bool {
        if order.column.eq_ignore_ascii_case(field) {
            return true;
        }
        // 排序项可能是列名（而策略字段可能是实体名），做双向匹配
        table
            .column(&order.column)
            .map(|c| {
                c.name.eq_ignore_ascii_case(field)
                    || table.effective_column_name(c).eq_ignore_ascii_case(field)
            })
            .unwrap_or(false)
    };

    let mut result = shards;
    // 与 C# 一致：遍历全部排序项，最后一个匹配项决定方向
    for order in orders {
        if !matches_field(order) {
            continue;
        }
        result.sort_by(|a, b| {
            let left = (
                a.conn_name.as_deref().unwrap_or(""),
                a.table_name.as_deref().unwrap_or(""),
            );
            let right = (
                b.conn_name.as_deref().unwrap_or(""),
                b.table_name.as_deref().unwrap_or(""),
            );
            let cmp = left.cmp(&right);
            if order.desc { cmp.reverse() } else { cmp }
        });
    }
    result
}

/// 分表执行引擎：表句柄上的跨表操作（对应 C# `Entity.FindAll/FindCount/Delete/AutoShard` 的分表分支）。
impl<'a> TableRef<'a> {
    /// 分表基础信息（连接名取表级 / 模型级配置，表名取模型物理名）。
    pub fn shard_base(&self) -> ShardBase<'a> {
        let conn = self
            .meta()
            .conn_name
            .as_deref()
            .or_else(|| self.dal().model().and_then(|m| m.options.conn_name()));
        ShardBase {
            conn_name: conn,
            table_name: self.meta().effective_table_name(),
        }
    }

    /// 跨分表查询（对齐 C# `FindAll` 分表分支的完整语义）：
    ///
    /// - 条件无法推导分表区间时按单表查询（使用原查询）；
    /// - 逐个分表查询并按 `query` 的 `offset` / `limit`（或 `page`/`take`）跨表续页，
    ///   跳过行数按前序分表总行数扣减（对应 C# 的 `row -= skipCount`）；
    /// - **不存在的分表自动跳过**；不会自动建表（对齐 C# `dal.TableNames.Contains` 检查）；
    /// - 跨表分页要求排序稳定（建议按分表字段或主键排序；C# 依赖各表自然序，本库同样不强制）。
    pub fn query_sharded(
        &self,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        query: &Query,
    ) -> Result<RowSet> {
        let Some(filter) = query.filter.clone() else {
            return self.query(session, query);
        };

        let base = self.shard_base();
        let mut trimmed = filter;
        let shards = policy.shards_of_trim(base, &mut trimmed)?;
        if shards.is_empty() {
            return self.query(session, query);
        }

        let meta = self.meta();
        let shards = fix_shard_order(shards, meta, policy, &query.order_by);

        // 原始偏移/上限：分页参数优先，其次 offset/limit
        let (mut row, original_limit): (usize, Option<usize>) =
            if query.page_index >= 1 && query.page_size > 0 {
                (
                    (query.page_index - 1) * query.page_size,
                    Some(query.page_size),
                )
            } else {
                (query.offset.unwrap_or(0), query.limit)
            };
        let mut remaining = original_limit;

        let mut columns: Option<Arc<Vec<String>>> = None;
        let mut rows: Vec<DbRow> = Vec::new();

        for (index, shard) in shards.iter().enumerate() {
            ensure_conn_in_sync(base, shard)?;
            let physical = shard
                .table_name
                .clone()
                .unwrap_or_else(|| meta.effective_table_name().to_string());
            if !session.table_exists(&physical)? {
                continue;
            }
            let shard_table = self.dal().table_as(&meta.name, &physical)?;

            let mut per_shard = query.clone();
            per_shard.filter = Some(trimmed.clone());
            per_shard.page_index = 0;
            per_shard.page_size = 0;
            // 未跨表且无上限时不追加分页子句，保持 SQL 简洁
            per_shard.offset = (row > 0).then_some(row);
            per_shard.limit = remaining;

            let set = shard_table.query(session, &per_shard)?;
            if columns.is_none() {
                columns = Some(set.columns.clone());
            }
            let fetched = set.rows.len();
            rows.extend(set.rows);

            if let Some(max) = original_limit
                && max > 0
                && rows.len() >= max
            {
                break;
            }

            // 前序分表消耗的偏移量：当前分表满足条件的总行数（对应 C# `row -= skipCount`）
            if row > 0 && index + 1 < shards.len() {
                let skip = shard_table.count(session, Some(&trimmed))?;
                row = row.saturating_sub(skip.max(0) as usize);
            }

            if let Some(left) = remaining {
                remaining = Some(left.saturating_sub(fetched));
            }
        }

        Ok(RowSet {
            columns: columns.unwrap_or_default(),
            rows,
        })
    }

    /// 跨分表计数（对应 C# `FindCount` 分表分支：逐表计数求和，不存在的分表跳过）。
    pub fn count_sharded(
        &self,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        filter: Option<&Where>,
    ) -> Result<i64> {
        let Some(filter) = filter else {
            return self.count(session, None);
        };

        let base = self.shard_base();
        let mut trimmed = filter.clone();
        let shards = policy.shards_of_trim(base, &mut trimmed)?;
        if shards.is_empty() {
            return self.count(session, Some(filter));
        }

        let meta = self.meta();
        let mut total = 0i64;
        for shard in shards {
            ensure_conn_in_sync(base, &shard)?;
            let physical = shard
                .table_name
                .clone()
                .unwrap_or_else(|| meta.effective_table_name().to_string());
            if !session.table_exists(&physical)? {
                continue;
            }
            let shard_table = self.dal().table_as(&meta.name, &physical)?;
            total += shard_table.count(session, Some(&trimmed))?;
        }
        Ok(total)
    }

    /// 跨分表条件删除（对应 C# `Delete(Expression)` 分表分支）。
    ///
    /// 与 C# 的差异：**不存在的分表直接跳过**（C# 未做检查，会对缺失表报错——跳过的结果等价且更稳）；
    /// 空条件返回 0（与 C# `where.IsEmpty` 短路一致）。
    pub fn delete_sharded(
        &self,
        session: &mut dyn SqlSession,
        policy: &TimeShardPolicy,
        filter: &Where,
    ) -> Result<u64> {
        if filter.is_empty() {
            return Ok(0);
        }

        let base = self.shard_base();
        let mut trimmed = filter.clone();
        let shards = policy.shards_of_trim(base, &mut trimmed)?;
        if shards.is_empty() {
            return self.delete_where(session, filter);
        }

        let meta = self.meta();
        let mut total = 0u64;
        for shard in shards {
            ensure_conn_in_sync(base, &shard)?;
            let physical = shard
                .table_name
                .clone()
                .unwrap_or_else(|| meta.effective_table_name().to_string());
            if !session.table_exists(&physical)? {
                continue;
            }
            let shard_table = self.dal().table_as(&meta.name, &physical)?;
            total += shard_table.delete_where(session, &trimmed)?;
        }
        Ok(total)
    }

    /// 按时间区间自动分表遍历（对应 C# `Meta.AutoShard`）：
    ///
    /// 对区间内**已存在**的每个分表依次执行回调（回调拿到该分表表句柄 + 独立会话），
    /// 返回各次回调结果（顺序与分表扫描顺序一致；`start > end` 时倒序）。
    /// 与 C# 一致：未生成表名的连接级分表会被跳过。
    pub fn auto_shard<T, F>(
        &self,
        policy: &TimeShardPolicy,
        start: NaiveDateTime,
        end: NaiveDateTime,
        mut func: F,
    ) -> Result<Vec<T>>
    where
        F: FnMut(&TableRef<'a>, &mut dyn SqlSession) -> Result<T>,
    {
        let base = self.shard_base();
        let shards = policy.shards_between(base, start, end)?;
        let meta = self.meta();

        let mut results = Vec::new();
        for shard in shards {
            ensure_conn_in_sync(base, &shard)?;
            // 与 C# `AutoShard` 一致：未计算表名的分表跳过
            let Some(physical) = shard.table_name else {
                continue;
            };
            let mut session = self.dal().open_session()?;
            if !session.table_exists(&physical)? {
                continue;
            }
            let shard_table = self.dal().table_as(&meta.name, &physical)?;
            results.push(func(&shard_table, session.as_mut())?);
        }
        Ok(results)
    }

    /// 删除时间区间内的分表物理表（对应 C# 生成代码的 `DropWith`，用于清理历史分表数据）。
    ///
    /// - 只删除**已存在**的分表：基础表不受影响；未生成表名的连接级分表跳过；
    /// - **不受迁移档位限制**（与 C# `DropWith` 一致，DDL 直接执行）——请谨慎调用，建议先用
    ///   [`TableRef::count_sharded`] 或 [`TableRef::auto_shard`] 评估数据；
    /// - 返回实际删除的表数量。
    pub fn drop_shards(
        &self,
        policy: &TimeShardPolicy,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Result<usize> {
        let base = self.shard_base();
        let shards = policy.shards_between(base, start, end)?;

        let mut session = self.dal().open_session()?;
        let mut dropped = 0;
        for shard in shards {
            ensure_conn_in_sync(base, &shard)?;
            let Some(physical) = shard.table_name else {
                continue;
            };
            if !session.table_exists(&physical)? {
                continue;
            }
            let sql = format!("DROP TABLE {}", self.dal().kind().quote(&physical));
            self.dal().log_sql(&sql);
            session.execute(&sql, &[])?;
            self.dal().invalidate_cache(&physical);
            dropped += 1;
        }
        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(y: i32, m: u32, d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn base() -> ShardBase<'static> {
        ShardBase {
            conn_name: Some("test"),
            table_name: "Log2",
        }
    }

    #[test]
    fn dotnet_formats_cover_common_patterns() {
        let t = at(2026, 9, 27, 10, 30).with_second(5).unwrap();
        assert_eq!(dotnet_date_format("yyyyMMdd", t), "20260927");
        assert_eq!(dotnet_date_format("yy", t), "26");
        assert_eq!(dotnet_date_format("M-d H:m:s", t), "9-27 10:30:5");
        assert_eq!(dotnet_date_format("MM/dd HH:mm", t), "09/27 10:30");
        assert_eq!(dotnet_date_format("yyyy_MM", t), "2026_09");
        assert_eq!(dotnet_date_format("'T'yyyy", t), "T2026");
        // `\M` 转义为字面量 M，后续单个 M 渲染月份（与 .NET 自定义格式一致）
        assert_eq!(dotnet_date_format("yyyy\\MM", t), "2026M9");
        assert_eq!(dotnet_date_format("fff", t), "000");
    }

    #[test]
    fn format_policy_renders_table_names() {
        let t = at(2026, 9, 27, 10, 30);
        assert_eq!(format_policy("{0}_{1:yyyyMM}", "Log2", t), "Log2_202609");
        assert_eq!(format_policy("{0}_{1:yyyy}", "Log2", t), "Log2_2026");
        assert_eq!(format_policy("{0}_{1:yyyyMMdd}", "Log2", t), "Log2_20260927");
        assert_eq!(format_policy("{0}_{1:dd}", "Log2", t), "Log2_27");
        assert_eq!(format_policy("t_{1:yyyy}_{0}", "Log2", t), "t_2026_Log2");
        assert_eq!(format_policy("{1:yyyy}_{1:MM}", "Log2", t), "2026_09");
    }

    #[test]
    fn resolve_time_month_and_conn() {
        let policy = TimeShardPolicy::new("CreateTime")
            .with_conn_policy("{0}_{1:yyyy}")
            .with_table_policy("{0}_{1:yyyyMM}");
        let model = policy.resolve_time(base(), at(2026, 9, 27, 10, 30)).unwrap();
        assert_eq!(model.conn_name.as_deref(), Some("test_2026"));
        assert_eq!(model.table_name.as_deref(), Some("Log2_202609"));

        // 未配置模板 → 不分表
        let empty = TimeShardPolicy::new("CreateTime");
        assert!(empty.resolve_time(base(), at(2026, 9, 27, 10, 30)).is_none());
    }

    #[test]
    fn shards_between_scans_days_months_years() {
        // C# `Shards(start, end)` 为左闭右开：4 天覆盖（07-30 ~ 08-02 含首尾）需 end=08-03
        // （C# `fi.Between(start, end)` 对纯日期自动 +1 天，见 `FieldExtension.Between`）
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
        let shards = policy
            .shards_between(base(), time(2024, 7, 30), time(2024, 8, 3))
            .unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(
            names,
            ["Log2_20240730", "Log2_20240731", "Log2_20240801", "Log2_20240802"]
        );

        // 左闭右开：end=08-02 时仅扫到 08-01
        let shards = policy
            .shards_between(base(), time(2024, 7, 30), time(2024, 8, 2))
            .unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(names, ["Log2_20240730", "Log2_20240731", "Log2_20240801"]);

        // 跨月 = 2 分片（对齐 C# `跨月Shards`：fi.Between(7/25, 8/1) 会因纯日期 +1 天，
        // 等价于这里 end=8/2 的左闭右开区间）
        let policy = TimeShardPolicy::new("CreateTime")
            .with_table_policy("{0}_{1:yyyyMM}")
            .with_step_days(31);
        let shards = policy
            .shards_between(base(), time(2024, 7, 25), time(2024, 8, 2))
            .unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(names, ["Log2_202407", "Log2_202408"]);

        // 跨年 = 2 分片（对齐 C# `跨年Shards`）
        let policy = TimeShardPolicy::new("CreateTime")
            .with_table_policy("{0}_{1:yyyy}")
            .with_step_days(365);
        let shards = policy
            .shards_between(
                base(),
                time(2023, 12, 31),
                time(2023, 12, 31) + TimeDelta::days(300),
            )
            .unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(names, ["Log2_2023", "Log2_2024"]);

        // 循环天表（对齐 C# `循环天表Shards`）：{1:dd}，end=08-03（含 08-02）
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:dd}");
        let shards = policy
            .shards_between(base(), time(2024, 7, 30), time(2024, 8, 3))
            .unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(names, ["Log2_30", "Log2_31", "Log2_01", "Log2_02"]);
    }

    #[test]
    fn shards_between_reverses_when_descending() {
        // 倒序 = 左闭右开区间 [08-02, 07-30) 的分表倒排（与 C# `Shards(start>end)` 一致）
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
        let shards = policy
            .shards_between(base(), time(2024, 8, 2), time(2024, 7, 30))
            .unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(names, ["Log2_20240801", "Log2_20240731", "Log2_20240730"]);
    }

    #[test]
    fn shards_of_equals_gives_single_table() {
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
        let where_ = Where::new().eq("CreateTime", time(2024, 5, 29));
        let shards = policy.shards_of(base(), &where_).unwrap();
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].table_name.as_deref(), Some("Log2_20240529"));
    }

    #[test]
    fn shards_of_range_and_trim() {
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");

        // 整天区间 → 单分表 + Trim 移除条件（对齐 C# `ExpressionShardsFullDay`）
        let mut where_ = Where::new()
            .ge("CreateTime", time(2024, 5, 29))
            .lt("CreateTime", time(2024, 5, 30));
        let shards = policy.shards_of_trim(base(), &mut where_).unwrap();
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].table_name.as_deref(), Some("Log2_20240529"));
        assert!(where_.is_empty(), "Trim 后起止条件应被移除");

        // 非 trim 版本不改条件
        let where_ = Where::new()
            .ge("CreateTime", time(2024, 5, 29))
            .lt("CreateTime", time(2024, 5, 30));
        let _ = policy.shards_of(base(), &where_).unwrap();
        assert!(!where_.is_empty());

        // > 起点：25 日 00:00 之后（+1 秒）→ 首表仍为 25 日
        let where_ = Where::new().gt("CreateTime", time(2024, 5, 25));
        let shards = policy.shards_of(base(), &where_).unwrap();
        assert!(!shards.is_empty());
        assert_eq!(shards[0].table_name.as_deref(), Some("Log2_20240525"));
    }

    #[test]
    fn shards_of_between_is_inclusive() {
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
        // BETWEEN 2024-05-30 00:00:00 与 2024-05-31 00:00:00（闭区间）→ 30/31 两张表
        let where_ = Where::new().between("CreateTime", time(2024, 5, 30), time(2024, 5, 31));
        let shards = policy.shards_of(base(), &where_).unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(names, ["Log2_20240530", "Log2_20240531"]);
    }

    #[test]
    fn shards_of_without_field_returns_empty() {
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
        let where_ = Where::new().eq("Status", 1);
        assert!(policy.shards_of(base(), &where_).unwrap().is_empty());

        // 有分表字段但条件不足 → 报错
        let where_ = Where::new().like("CreateTime", "2024%");
        assert!(policy.shards_of(base(), &where_).is_err());
    }

    #[test]
    fn shards_of_snow_id_conditions() {
        let snow = Arc::new(Snowflake::with_worker_id(9));
        let policy = TimeShardPolicy::new("Id")
            .with_table_policy("{0}_{1:yyyyMMdd}")
            .with_snow(snow.clone());

        // 等值：直接解析 id → 单表
        let id = snow.new_id_at(at(2024, 5, 29, 10, 0));
        let where_ = Where::new().eq("Id", id);
        let shards = policy.shards_of(base(), &where_).unwrap();
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].table_name.as_deref(), Some("Log2_20240529"));

        // 区间：>= id1 且 < id2
        let id1 = snow.id_at(time(2024, 5, 28));
        let id2 = snow.id_at(time(2024, 5, 30));
        let where_ = Where::new().ge("Id", id1).lt("Id", id2);
        let shards = policy.shards_of(base(), &where_).unwrap();
        let names: Vec<_> = shards
            .iter()
            .map(|s| s.table_name.clone().unwrap())
            .collect();
        assert_eq!(names, ["Log2_20240528", "Log2_20240529"]);
    }

    #[test]
    fn shard_of_value_dispatches_by_type() {
        let snow = Arc::new(Snowflake::with_worker_id(3));
        let policy = TimeShardPolicy::new("Id")
            .with_table_policy("{0}_{1:yyyyMM}")
            .with_snow(snow.clone());

        let model = policy
            .shard_of_value(base(), &DbValue::Int(snow.new_id_at(at(2026, 9, 1, 0, 0))))
            .unwrap()
            .unwrap();
        assert_eq!(model.table_name.as_deref(), Some("Log2_202609"));

        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMM}");
        let model = policy
            .shard_of_value(base(), &DbValue::DateTime(at(2026, 10, 1, 8, 0)))
            .unwrap()
            .unwrap();
        assert_eq!(model.table_name.as_deref(), Some("Log2_202610"));

        // 空时间（0001-01-01）→ 报错（对齐 C# 实体对象时间字段为空）
        assert!(
            policy
                .shard_of_value(base(), &DbValue::DateTime(NaiveDateTime::default()))
                .is_err()
        );
    }

    #[test]
    fn fix_order_sorts_shards_by_table_name() {
        let policy = TimeShardPolicy::new("CreateTime").with_table_policy("{0}_{1:yyyyMMdd}");
        let shards = policy
            .shards_between(base(), time(2024, 5, 29), time(2024, 6, 1))
            .unwrap();
        let table = TableMeta {
            name: "Log2".into(),
            table_name: String::new(),
            description: String::new(),
            conn_name: None,
            migration: None,
            columns: Vec::new(),
            indexes: Vec::new(),
        };

        let asc = fix_shard_order(
            shards.clone(),
            &table,
            &policy,
            &[OrderBy {
                column: "CreateTime".into(),
                desc: false,
            }],
        );
        assert_eq!(asc[0].table_name.as_deref(), Some("Log2_20240529"));

        let desc = fix_shard_order(
            shards,
            &table,
            &policy,
            &[OrderBy {
                column: "CreateTime".into(),
                desc: true,
            }],
        );
        assert_eq!(desc[0].table_name.as_deref(), Some("Log2_20240531"));
    }
}
