//! 数据访问层辅助件（对应 DH.NCode `DataAccessLayer` 模块的可移植部分）。
//!
//! - [`TimeRegion`]/[`ReadWriteStrategy`]：读写分离策略（忽略时间区间与表名、只读库轮询）；
//! - [`SaveModes`]：实体保存模式（配合 `Dal` 的 `TableRef::save`）；
//! - [`ModelSortModes`]：模型字段排序模式（对应 `Attributes/ModelSortMode.cs`）；
//! - [`row_number`]/[`rank`]/[`dense_rank`]/[`aggregate`]：窗口函数 SQL 片段生成（对应 `WindowFunction`）；
//! - [`extract_table_names`]：从 SQL 提取表名（`DAL.GetTables` 的简化版）。
//!
//! `MSPageSplit`（SQL Server 2000/2005 历史分页算法）未端口：Rust 方言统一使用
//! `OFFSET .. FETCH`/`LIMIT` 现代分页（见 `sqlbuild` 的分页支持）。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::NaiveTime;

use crate::error::{Error, Result};

/// 时间区间（当天内的时间段，闭开区间 `[start, end)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRegion {
    /// 开始时间。
    pub start: NaiveTime,
    /// 结束时间。
    pub end: NaiveTime,
}

/// 读写分离策略（对齐 `ReadWriteStrategy`）。忽略时间区间和表名。
#[derive(Debug, Default)]
pub struct ReadWriteStrategy {
    /// 要忽略的时间区间（这些时段内不走只读库）。
    pub ignore_times: Vec<TimeRegion>,
    /// 要忽略的表名（小写存储，比较时大小写不敏感）。
    pub ignore_tables: BTreeSet<String>,
    /// 轮询下标。
    index: AtomicUsize,
}

impl ReadWriteStrategy {
    /// 实例化。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置不走读写分离的时间段，如 `00:30-00:50,01:00-02:00`（多段以逗号分隔）。
    /// <param name="regions">时间段文本</param>
    pub fn add_ignore_times(&mut self, regions: &str) {
        for item in regions.split(',').filter(|s| !s.trim().is_empty()) {
            let mut parts = item.split('-');
            if let (Some(start), Some(end)) = (parts.next(), parts.next()) {
                let start = start.trim().parse::<NaiveTime>().ok();
                let end = end.trim().parse::<NaiveTime>().ok();
                if let (Some(start), Some(end)) = (start, end)
                    && start < end
                {
                    self.ignore_times.push(TimeRegion { start, end });
                }
            }
        }
    }

    /// 检查是否支持读写分离（对齐 `Validate`）。
    ///
    /// 条件：非事务中；动作属于 `Select`/`SelectCount`/`Query`（`ExecuteScalar` 分支
    /// 在 C# 中先被前一条件拒绝而不可达，此处忠实转写）；不在忽略时间区间；SQL 未引用忽略表。
    /// <param name="action">操作名</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="in_transaction">是否处于事务中</param>
    /// <param name="day_time">当前时刻（当天时间）</param>
    /// <returns>是否可切换到只读库</returns>
    pub fn validate(&self, action: &str, sql: &str, in_transaction: bool, day_time: NaiveTime) -> bool {
        // 事务中不支持分离
        if in_transaction {
            return false;
        }
        if !["select", "selectcount", "query"]
            .iter()
            .any(|a| action.eq_ignore_ascii_case(a))
        {
            return false;
        }
        if action.eq_ignore_ascii_case("executescalar")
            && !sql.trim_start().to_ascii_lowercase().starts_with("select ")
        {
            return false;
        }
        // 忽略的时间区间（闭开区间）
        for region in &self.ignore_times {
            if day_time >= region.start && day_time < region.end {
                return false;
            }
        }
        // 忽略的表名
        if !sql.is_empty() && !self.ignore_tables.is_empty() {
            for table in extract_table_names(sql) {
                if self.ignore_tables.contains(&table.to_ascii_lowercase()) {
                    return false;
                }
            }
        }
        true
    }

    /// 取下一个只读库下标（轮询；对齐 C# `Interlocked.Increment` 的先增后取语义）。
    /// <param name="slave_count">只读库数量（为 0 时返回 0，调用方应先判断是否可用）</param>
    /// <returns>只读库下标</returns>
    pub fn next_slave(&self, slave_count: usize) -> usize {
        if slave_count == 0 {
            return 0;
        }
        (self.index.fetch_add(1, Ordering::Relaxed) + 1) % slave_count
    }
}

/// 实体保存模式（对齐 C# `SaveModes`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveModes {
    /// 插入（0）。
    Insert = 0,
    /// 存在则更新，否则插入（1）。
    Upsert = 1,
    /// 存在则忽略（2）。
    InsertIgnore = 2,
    /// 存在则删除后插入（3）。
    Replace = 3,
}

/// 模型字段排序模式（对齐 `ModelSortModes`；影响数据字段在数据表中的先后顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelSortModes {
    /// 基类优先。默认值。一般用于扩展某个实体类增加若干数据字段。
    #[default]
    BaseFirst,
    /// 派生类优先。一般用于具有某些公共数据字段的基类。
    DerivedFirst,
}

/// 模型检查模式（对齐 `ModelCheckModes`；对应 `Attributes/ModelCheckModeAttribute.cs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelCheckModes {
    /// 初始化时检查所有表。默认值。具有最好性能。
    #[default]
    CheckAllTablesWhenInit,
    /// 第一次使用时检查表。适用于存在大量实体类但不会同时使用的场合。
    CheckTableWhenFirstUse,
}

/// 生成 `ROW_NUMBER() OVER(ORDER BY {order_by}) AS {alias}`（对齐 `WindowFunction.RowNumber`）。
/// <param name="order_by">排序表达式</param>
/// <param name="alias">别名（None 时使用 `RowNum`）</param>
/// <returns>SQL 片段</returns>
pub fn row_number(order_by: &str, alias: Option<&str>) -> Result<String> {
    if order_by.is_empty() {
        return Err(Error::Argument("排序字段不能为空".into()));
    }
    Ok(format!(
        "ROW_NUMBER() OVER(ORDER BY {order_by}) AS {}",
        alias.unwrap_or("RowNum")
    ))
}

/// 生成 `ROW_NUMBER() OVER(PARTITION BY {partition_by} ORDER BY {order_by}) AS {alias}`。
///
/// `partition_by` 为空时退化为单排序版本（对齐 C# 行为）。
/// <param name="partition_by">分区表达式</param>
/// <param name="order_by">排序表达式</param>
/// <param name="alias">别名（None 时使用 `RowNum`）</param>
/// <returns>SQL 片段</returns>
pub fn row_number_partition(partition_by: &str, order_by: &str, alias: Option<&str>) -> Result<String> {
    if partition_by.is_empty() {
        return row_number(order_by, alias);
    }
    if order_by.is_empty() {
        return Err(Error::Argument("排序字段不能为空".into()));
    }
    Ok(format!(
        "ROW_NUMBER() OVER(PARTITION BY {partition_by} ORDER BY {order_by}) AS {}",
        alias.unwrap_or("RowNum")
    ))
}

/// 生成 `RANK() OVER(ORDER BY {order_by}) AS {alias}`（对齐 `WindowFunction.Rank`）。
/// <param name="order_by">排序表达式</param>
/// <param name="alias">别名（None 时使用 `Rank`）</param>
/// <returns>SQL 片段</returns>
pub fn rank(order_by: &str, alias: Option<&str>) -> Result<String> {
    if order_by.is_empty() {
        return Err(Error::Argument("排序字段不能为空".into()));
    }
    Ok(format!("RANK() OVER(ORDER BY {order_by}) AS {}", alias.unwrap_or("Rank")))
}

/// 生成 `DENSE_RANK() OVER(ORDER BY {order_by}) AS {alias}`（对齐 `WindowFunction.DenseRank`）。
/// <param name="order_by">排序表达式</param>
/// <param name="alias">别名（None 时使用 `DenseRank`）</param>
/// <returns>SQL 片段</returns>
pub fn dense_rank(order_by: &str, alias: Option<&str>) -> Result<String> {
    if order_by.is_empty() {
        return Err(Error::Argument("排序字段不能为空".into()));
    }
    Ok(format!(
        "DENSE_RANK() OVER(ORDER BY {order_by}) AS {}",
        alias.unwrap_or("DenseRank")
    ))
}

/// 生成聚合窗口函数片段（对齐 `WindowFunction.Aggregate`）。
///
/// `partition_by` 为空时输出 `{func}({column}) OVER() AS {alias}`；
/// 别名 None 时输出空字符串（对齐 C# 默认参数行为）。
/// <param name="func">聚合函数名（如 `SUM`/`COUNT`）</param>
/// <param name="column">列表达式</param>
/// <param name="partition_by">分区表达式</param>
/// <param name="alias">别名</param>
/// <returns>SQL 片段</returns>
pub fn aggregate(func: &str, column: &str, partition_by: Option<&str>, alias: Option<&str>) -> Result<String> {
    if func.is_empty() {
        return Err(Error::Argument("聚合函数名不能为空".into()));
    }
    if column.is_empty() {
        return Err(Error::Argument("聚合列不能为空".into()));
    }
    let alias = alias.unwrap_or_default();
    match partition_by {
        Some(p) if !p.is_empty() => Ok(format!("{func}({column}) OVER(PARTITION BY {p}) AS {alias}")),
        _ => Ok(format!("{func}({column}) OVER() AS {alias}")),
    }
}

/// 从 SQL 中提取表名（对应 `DAL.GetTables` 的简化版本）。
///
/// 识别 `FROM`/`JOIN`/`UPDATE`/`INTO` 之后的第一个标识符，并去除方括号/双引号/反引号引用；
/// 不做完整 SQL 解析（识别到 `SELECT` 关键字时跳过，覆盖常见的子查询场景）。
/// <param name="sql">SQL 语句</param>
/// <returns>表名列表（出现顺序）</returns>
pub fn extract_table_names(sql: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut expect = false;
    for raw in sql.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == ';') {
        if raw.is_empty() {
            continue;
        }
        if expect {
            expect = false;
            let name = raw.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`' | '\''));
            if !name.is_empty() && !name.to_ascii_lowercase().starts_with("select") {
                names.push(name.to_string());
            }
            continue;
        }
        if matches!(raw.to_ascii_lowercase().as_str(), "from" | "join" | "update" | "into") {
            expect = true;
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造时间。
    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn ignore_time_regions_parsing() {
        let mut s = ReadWriteStrategy::new();
        s.add_ignore_times("00:30-00:50, 01:00-02:00");
        assert_eq!(s.ignore_times.len(), 2);
        assert_eq!(s.ignore_times[0].start, time(0, 30));
        assert_eq!(s.ignore_times[0].end, time(0, 50));
        // 非法时间段被忽略（起点不小于终点 / 无法解析）
        s.add_ignore_times("03:00-02:00,abc-def");
        assert_eq!(s.ignore_times.len(), 2);
    }

    #[test]
    fn validate_rules() {
        let mut s = ReadWriteStrategy::new();
        s.add_ignore_times("00:30-00:50");
        s.ignore_tables.insert("syslog".into());

        // 正常放行
        assert!(s.validate("Select", "SELECT * FROM [DH_Order]", false, time(10, 0)));
        // 事务中拒绝
        assert!(!s.validate("Select", "SELECT * FROM [DH_Order]", true, time(10, 0)));
        // 非查询动作拒绝
        assert!(!s.validate("Update", "UPDATE T SET x=1", false, time(10, 0)));
        // 忽略时间段内拒绝（闭开区间）
        assert!(!s.validate("Select", "SELECT 1", false, time(0, 30)));
        assert!(s.validate("Select", "SELECT 1", false, time(0, 50)));
        // 忽略表名拒绝（大小写不敏感）
        assert!(!s.validate("Select", "SELECT * FROM SysLog WHERE ID>0", false, time(10, 0)));
        // 子查询中的 SELECT 不误判为表名
        assert!(s.validate("Select", "SELECT * FROM (SELECT * FROM DH_Order) t", false, time(10, 0)));
    }

    #[test]
    fn slave_round_robin() {
        let s = ReadWriteStrategy::new();
        assert_eq!(s.next_slave(0), 0);
        // 先增后取：首次返回 1
        assert_eq!(s.next_slave(3), 1);
        assert_eq!(s.next_slave(3), 2);
        assert_eq!(s.next_slave(3), 0);
        assert_eq!(s.next_slave(3), 1);
    }

    #[test]
    fn save_modes_and_model_sort_values() {
        assert_eq!(SaveModes::Insert as i32, 0);
        assert_eq!(SaveModes::Upsert as i32, 1);
        assert_eq!(SaveModes::InsertIgnore as i32, 2);
        assert_eq!(SaveModes::Replace as i32, 3);
        assert_eq!(ModelSortModes::default(), ModelSortModes::BaseFirst);
        assert_ne!(ModelSortModes::BaseFirst, ModelSortModes::DerivedFirst);
        assert_eq!(ModelCheckModes::default(), ModelCheckModes::CheckAllTablesWhenInit);
        assert_ne!(
            ModelCheckModes::CheckAllTablesWhenInit,
            ModelCheckModes::CheckTableWhenFirstUse
        );
    }

    #[test]
    fn window_function_snippets() {
        assert_eq!(
            row_number("ID DESC", None).unwrap(),
            "ROW_NUMBER() OVER(ORDER BY ID DESC) AS RowNum"
        );
        assert_eq!(
            row_number("ID", Some("Rn")).unwrap(),
            "ROW_NUMBER() OVER(ORDER BY ID) AS Rn"
        );
        assert_eq!(
            row_number_partition("DeptID", "Salary DESC", None).unwrap(),
            "ROW_NUMBER() OVER(PARTITION BY DeptID ORDER BY Salary DESC) AS RowNum"
        );
        // 分区为空时退化为单排序
        assert_eq!(
            row_number_partition("", "ID", None).unwrap(),
            "ROW_NUMBER() OVER(ORDER BY ID) AS RowNum"
        );
        assert!(row_number("", None).is_err());

        assert_eq!(rank("Score DESC", None).unwrap(), "RANK() OVER(ORDER BY Score DESC) AS Rank");
        assert_eq!(
            dense_rank("Score DESC", None).unwrap(),
            "DENSE_RANK() OVER(ORDER BY Score DESC) AS DenseRank"
        );
        assert_eq!(
            aggregate("SUM", "Amount", None, Some("Total")).unwrap(),
            "SUM(Amount) OVER() AS Total"
        );
        assert_eq!(
            aggregate("SUM", "Amount", Some("DeptID"), Some("Total")).unwrap(),
            "SUM(Amount) OVER(PARTITION BY DeptID) AS Total"
        );
        // 别名缺省（对齐 C# 默认参数 null）
        assert_eq!(aggregate("COUNT", "*", None, None).unwrap(), "COUNT(*) OVER() AS ");
        assert!(aggregate("", "Amount", None, None).is_err());
    }

    #[test]
    fn table_name_extraction() {
        assert_eq!(
            extract_table_names("SELECT * FROM [DH_Order] o JOIN `SysUser` u ON o.UserID=u.ID"),
            vec!["DH_Order", "SysUser"]
        );
        assert_eq!(
            extract_table_names("UPDATE \"Order\" SET Status=1 WHERE ID=2"),
            vec!["Order"]
        );
        assert_eq!(extract_table_names("INSERT INTO DH_Log(Content) VALUES('x')"), vec!["DH_Log"]);
    }
}
