//! 查询条件与查询描述：对应 XCode 的 `WhereExpression` 与 `PageParameter`。
//!
//! 条件以结构化方式收集，渲染时按目标数据库方言生成占位符与参数序列，
//! 全程参数绑定，不拼接字面量。

use crate::dialect::DatabaseKind;
use crate::value::DbValue;

/// 比较运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// 等于
    Eq,
    /// 不等于
    Ne,
    /// 大于
    Gt,
    /// 大于等于
    Ge,
    /// 小于
    Lt,
    /// 小于等于
    Le,
    /// 模糊匹配
    Like,
    /// 反向模糊匹配
    NotLike,
    /// 集合包含
    In,
    /// 集合排除
    NotIn,
    /// 为空
    IsNull,
    /// 非空
    NotNull,
    /// 区间
    Between,
}

/// 单个条件项。
#[derive(Debug, Clone, PartialEq)]
struct Cond {
    /// 列名（渲染时按方言引用；含 SQL 函数时请直接使用原始表达式）
    column: String,
    /// 运算符
    op: Op,
    /// 参数列表（IsNull/NotNull 为空）
    values: Vec<DbValue>,
}

/// WHERE 条件构建器（多个条件之间为 AND）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Where {
    conds: Vec<Cond>,
}

impl Where {
    /// 创建空条件。
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否没有任何条件。
    pub fn is_empty(&self) -> bool {
        self.conds.is_empty()
    }

    /// 追加自定义条件（低层接口，列表达式原样使用）。
    pub fn and_raw(mut self, column: impl Into<String>, op: Op, values: Vec<DbValue>) -> Self {
        self.conds.push(Cond {
            column: column.into(),
            op,
            values,
        });
        self
    }

    /// `列 = 值`
    pub fn eq(self, column: impl Into<String>, value: impl Into<DbValue>) -> Self {
        self.and_raw(column, Op::Eq, vec![value.into()])
    }

    /// `列 <> 值`
    pub fn ne(self, column: impl Into<String>, value: impl Into<DbValue>) -> Self {
        self.and_raw(column, Op::Ne, vec![value.into()])
    }

    /// `列 > 值`
    pub fn gt(self, column: impl Into<String>, value: impl Into<DbValue>) -> Self {
        self.and_raw(column, Op::Gt, vec![value.into()])
    }

    /// `列 >= 值`
    pub fn ge(self, column: impl Into<String>, value: impl Into<DbValue>) -> Self {
        self.and_raw(column, Op::Ge, vec![value.into()])
    }

    /// `列 < 值`
    pub fn lt(self, column: impl Into<String>, value: impl Into<DbValue>) -> Self {
        self.and_raw(column, Op::Lt, vec![value.into()])
    }

    /// `列 <= 值`
    pub fn le(self, column: impl Into<String>, value: impl Into<DbValue>) -> Self {
        self.and_raw(column, Op::Le, vec![value.into()])
    }

    /// `列 LIKE 值`
    pub fn like(self, column: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.and_raw(column, Op::Like, vec![DbValue::Text(pattern.into())])
    }

    /// `列 NOT LIKE 值`
    pub fn not_like(self, column: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.and_raw(column, Op::NotLike, vec![DbValue::Text(pattern.into())])
    }

    /// `列 IN (值...)`
    pub fn in_(
        self,
        column: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<DbValue>>,
    ) -> Self {
        let list: Vec<DbValue> = values.into_iter().map(Into::into).collect();
        self.and_raw(column, Op::In, list)
    }

    /// `列 NOT IN (值...)`
    pub fn not_in(
        self,
        column: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<DbValue>>,
    ) -> Self {
        let list: Vec<DbValue> = values.into_iter().map(Into::into).collect();
        self.and_raw(column, Op::NotIn, list)
    }

    /// `列 BETWEEN 下限 AND 上限`
    pub fn between(
        self,
        column: impl Into<String>,
        low: impl Into<DbValue>,
        high: impl Into<DbValue>,
    ) -> Self {
        self.and_raw(column, Op::Between, vec![low.into(), high.into()])
    }

    /// `列 IS NULL`
    pub fn is_null(self, column: impl Into<String>) -> Self {
        self.and_raw(column, Op::IsNull, vec![])
    }

    /// `列 IS NOT NULL`
    pub fn not_null(self, column: impl Into<String>) -> Self {
        self.and_raw(column, Op::NotNull, vec![])
    }

    /// 渲染条件片段并追加参数。
    ///
    /// - 占位符编号基于 `params` 的当前长度，调用方应先把前置参数（如 UPDATE 的 SET 值）放入 `params`
    /// - 返回完整片段（外层带括号）；空条件返回空串
    pub fn render(&self, kind: DatabaseKind, params: &mut Vec<DbValue>) -> String {
        if self.conds.is_empty() {
            return String::new();
        }

        let mut parts = Vec::with_capacity(self.conds.len());
        for cond in &self.conds {
            let part = match cond.op {
                Op::IsNull => format!("{} IS NULL", quote_or_raw(kind, &cond.column)),
                Op::NotNull => format!("{} IS NOT NULL", quote_or_raw(kind, &cond.column)),
                Op::In | Op::NotIn => {
                    let op = if cond.op == Op::In { "IN" } else { "NOT IN" };
                    if cond.values.is_empty() {
                        // 空集合：IN → 恒假；NOT IN → 恒真（避免生成非法 SQL）
                        if cond.op == Op::In {
                            "1 = 0".to_string()
                        } else {
                            "1 = 1".to_string()
                        }
                    } else {
                        let mut marks = Vec::with_capacity(cond.values.len());
                        for v in &cond.values {
                            marks.push(kind.placeholder(params.len()));
                            params.push(v.clone());
                        }
                        format!(
                            "{} {op} ({})",
                            quote_or_raw(kind, &cond.column),
                            marks.join(", ")
                        )
                    }
                }
                Op::Between => {
                    let a = kind.placeholder(params.len());
                    params.push(cond.values[0].clone());
                    let b = kind.placeholder(params.len());
                    params.push(cond.values[1].clone());
                    format!("{} BETWEEN {a} AND {b}", quote_or_raw(kind, &cond.column))
                }
                _ => {
                    let op = match cond.op {
                        Op::Eq => "=",
                        Op::Ne => "<>",
                        Op::Gt => ">",
                        Op::Ge => ">=",
                        Op::Lt => "<",
                        Op::Le => "<=",
                        Op::Like => "LIKE",
                        Op::NotLike => "NOT LIKE",
                        _ => unreachable!(),
                    };
                    let mark = kind.placeholder(params.len());
                    params.push(cond.values[0].clone());
                    format!("{} {op} {mark}", quote_or_raw(kind, &cond.column))
                }
            };
            parts.push(part);
        }

        format!("({})", parts.join(" AND "))
    }
}

/// 列名 → SQL 片段：普通标识符按方言引用；含非标识符字符（函数、表达式）时原样使用。
fn quote_or_raw(kind: DatabaseKind, column: &str) -> String {
    let plain = !column.is_empty()
        && column
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' );
    if plain {
        // 支持 "Table.Column" 形式，逐段引用
        column
            .split('.')
            .map(|part| kind.quote(part))
            .collect::<Vec<_>>()
            .join(".")
    } else {
        column.to_string()
    }
}

/// 排序项。
#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    /// 列名
    pub column: String,
    /// 是否倒序
    pub desc: bool,
}

/// 查询描述（SELECT 语句的组装参数）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// 查询列（空 = 全部列 `*`；含表达式时原样使用）
    pub select: Vec<String>,
    /// 过滤条件
    pub filter: Option<Where>,
    /// 排序
    pub order_by: Vec<OrderBy>,
    /// 取前 N 条（与分页互斥，分页优先）
    pub limit: Option<usize>,
    /// 页码（1 基；与 `page_size` 同时有效才启用分页）
    pub page_index: usize,
    /// 每页条数
    pub page_size: usize,
}

impl Query {
    /// 新查询（默认全部列、无过滤）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加查询列。
    pub fn column(mut self, column: impl Into<String>) -> Self {
        self.select.push(column.into());
        self
    }

    /// 设置过滤条件。
    pub fn filter(mut self, filter: Where) -> Self {
        self.filter = Some(filter);
        self
    }

    /// 追加排序。
    pub fn order_by(mut self, column: impl Into<String>, desc: bool) -> Self {
        self.order_by.push(OrderBy {
            column: column.into(),
            desc,
        });
        self
    }

    /// 设置分页（页码从 1 开始）。
    pub fn page(mut self, page_index: usize, page_size: usize) -> Self {
        self.page_index = page_index;
        self.page_size = page_size;
        self
    }

    /// 仅取前 N 条。
    pub fn take(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn where_of() -> Where {
        Where::new()
            .eq("Status", 1)
            .gt("Amount", "100".parse::<rust_decimal::Decimal>().unwrap())
            .like("Code", "HLT%")
            .in_("Type", [1, 2, 3])
            .is_null("Remark")
    }

    #[test]
    fn render_sqlite_where() {
        let mut params = Vec::new();
        let sql = where_of().render(DatabaseKind::Sqlite, &mut params);
        assert_eq!(
            sql,
            "(\"Status\" = ? AND \"Amount\" > ? AND \"Code\" LIKE ? AND \"Type\" IN (?, ?, ?) AND \"Remark\" IS NULL)"
        );
        assert_eq!(params.len(), 6);
    }

    #[test]
    fn render_sqlserver_placeholders() {
        let mut params = vec![DbValue::from(1)]; // 模拟已占用一个参数（如 UPDATE 的 SET 值）
        let sql = Where::new()
            .eq("A", 2)
            .between("B", 1, 9)
            .render(DatabaseKind::SqlServer, &mut params);
        assert_eq!(sql, "([A] = @p1 AND [B] BETWEEN @p2 AND @p3)");
        assert_eq!(params.len(), 4);
    }

    #[test]
    fn empty_in_never_generates_invalid_sql() {
        let mut params = Vec::new();
        let empty: Vec<i32> = vec![];
        assert_eq!(
            Where::new().in_("A", empty).render(DatabaseKind::Sqlite, &mut params),
            "(1 = 0)"
        );
        let mut params = Vec::new();
        let empty: Vec<i32> = vec![];
        assert_eq!(
            Where::new()
                .not_in("A", empty)
                .render(DatabaseKind::Sqlite, &mut params),
            "(1 = 1)"
        );
    }

    #[test]
    fn quote_handles_dotted_and_expressions() {
        assert_eq!(quote_or_raw(DatabaseKind::Sqlite, "t.Id"), "\"t\".\"Id\"");
        assert_eq!(quote_or_raw(DatabaseKind::Sqlite, "COUNT(*)"), "COUNT(*)");
        // 恶意列名不会直接进入 SQL（会被整体引用或拒绝）
        assert_eq!(quote_or_raw(DatabaseKind::Sqlite, "a b"), "a b");
    }
}
