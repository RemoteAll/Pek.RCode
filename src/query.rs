//! 查询条件与查询描述：对应 XCode 的 `WhereExpression` 与 `PageParameter`。
//!
//! 条件以结构化方式收集，渲染时按目标数据库方言生成占位符与参数序列，
//! 全程参数绑定，不拼接字面量。

use crate::dialect::{DatabaseKind, PageStyle};
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
    /// 原始 SQL 片段（`column` 原样输出，`?` 按顺序由 `values` 填占位；仅限可信字面量）
    Raw,
}

/// 单个条件项。
#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    /// 列名（渲染时按方言引用；含 SQL 函数时请直接使用原始表达式）
    pub column: String,
    /// 运算符
    pub op: Op,
    /// 参数列表（IsNull/NotNull 为空）
    pub values: Vec<DbValue>,
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

    /// 条件列表（只读；分表推导等结构化分析使用）。
    pub fn conds(&self) -> &[Cond] {
        &self.conds
    }

    /// 按条件保留（回调收到 `(序号, 条件)`；返回 false 的条件被移除）。
    ///
    /// 供分表 `Trim` 等需要从条件集中移除特定项的场景使用。
    pub fn retain_conds<F>(&mut self, mut keep: F)
    where
        F: FnMut(usize, &Cond) -> bool,
    {
        let mut index = 0;
        self.conds.retain(|cond| {
            let keep_it = keep(index, cond);
            index += 1;
            keep_it
        });
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

    /// 追加一段**原始 SQL 条件**（`?` 占位按顺序由 `values` 填充）。
    ///
    /// 用于表达 `AND` 无法组合的语义（如多列 `OR`）。`fragment` 必须是**可信字面量**，
    /// 严禁拼接用户输入（值一律走 `values` 参数化）。
    pub fn raw(self, fragment: impl Into<String>, values: Vec<DbValue>) -> Self {
        self.and_raw(fragment, Op::Raw, values)
    }

    /// 任一列模糊匹配：`(c1 LIKE ? OR c2 LIKE ? ...)`。
    ///
    /// 对齐 C# `_.Name.Contains(k) | _.Code.Contains(k) | ...` 这种「关键字搜索」语义
    /// （`Where` 的条件之间固定为 `AND`，多列任一命中需 `OR`）。
    /// `columns` 须为受信任的列名标识符。
    pub fn any_like(self, columns: &[&str], pattern: impl Into<String>) -> Self {
        if columns.is_empty() {
            return self;
        }
        let pattern = pattern.into();
        let expr = columns
            .iter()
            .map(|c| format!("{c} LIKE ?"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let values = vec![DbValue::Text(pattern); columns.len()];
        self.raw(format!("({expr})"), values)
    }

    /// 渲染条件片段并追加参数。
    ///
    /// - 占位符编号基于 `params` 的当前长度，调用方应先把前置参数（如 UPDATE 的 SET 值）放入 `params`
    /// - 返回完整片段（外层带括号）；空条件返回空串
    pub fn render(&self, kind: DatabaseKind, params: &mut Vec<DbValue>) -> String {
        if self.conds.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(32 + self.conds.len() * 24);
        self.render_into(kind, params, &mut out);
        out
    }

    /// 渲染条件片段并**直接追加**到 `out`（零中间分配：不建 `Vec<String>`、不 `join`、不 `format!`）。
    ///
    /// 语义与 [`Where::render`] 完全一致（外层带括号；空条件不追加任何内容）。
    pub fn render_into(&self, kind: DatabaseKind, params: &mut Vec<DbValue>, out: &mut String) {
        if self.conds.is_empty() {
            return;
        }
        out.push('(');
        for (index, cond) in self.conds.iter().enumerate() {
            if index > 0 {
                out.push_str(" AND ");
            }
            match cond.op {
                Op::IsNull => {
                    push_quoted_or_raw(kind, out, &cond.column);
                    out.push_str(" IS NULL");
                }
                Op::NotNull => {
                    push_quoted_or_raw(kind, out, &cond.column);
                    out.push_str(" IS NOT NULL");
                }
                Op::In | Op::NotIn => {
                    if cond.values.is_empty() {
                        // 空集合：IN → 恒假；NOT IN → 恒真（避免生成非法 SQL）
                        out.push_str(if cond.op == Op::In { "1 = 0" } else { "1 = 1" });
                    } else {
                        push_quoted_or_raw(kind, out, &cond.column);
                        out.push_str(if cond.op == Op::In { " IN (" } else { " NOT IN (" });
                        for (position, value) in cond.values.iter().enumerate() {
                            if position > 0 {
                                out.push_str(", ");
                            }
                            kind.push_placeholder(out, params.len());
                            params.push(value.clone());
                        }
                        out.push(')');
                    }
                }
                Op::Between => {
                    push_quoted_or_raw(kind, out, &cond.column);
                    out.push_str(" BETWEEN ");
                    kind.push_placeholder(out, params.len());
                    params.push(cond.values[0].clone());
                    out.push_str(" AND ");
                    kind.push_placeholder(out, params.len());
                    params.push(cond.values[1].clone());
                }
                Op::Raw => {
                    // 片段原样输出，`?` 逐个替换为方言占位符并填参数
                    let mut parts = cond.column.split('?');
                    if let Some(first) = parts.next() {
                        out.push_str(first);
                    }
                    let mut it = cond.values.iter();
                    for part in parts {
                        match it.next() {
                            Some(value) => {
                                kind.push_placeholder(out, params.len());
                                params.push(value.clone());
                            }
                            // 占位符多于值：按原文输出，不静默吞掉（便于暴露调用方错误）
                            None => out.push('?'),
                        }
                        out.push_str(part);
                    }
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
                    push_quoted_or_raw(kind, out, &cond.column);
                    out.push(' ');
                    out.push_str(op);
                    out.push(' ');
                    kind.push_placeholder(out, params.len());
                    params.push(cond.values[0].clone());
                }
            }
        }
        out.push(')');
    }
}

/// 列名 → SQL 片段**追加**（普通标识符按方言引用，`Table.Column` 逐段引用；含非标识符字符时原样）。
fn push_quoted_or_raw(kind: DatabaseKind, out: &mut String, column: &str) {
    let plain = !column.is_empty()
        && column
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if !plain {
        out.push_str(column);
        return;
    }
    for (index, part) in column.split('.').enumerate() {
        if index > 0 {
            out.push('.');
        }
        kind.push_quoted(out, part);
    }
}

/// 列名 → SQL 片段（`String` 版本，保留给测试与外部调用）。
#[cfg(test)]
fn quote_or_raw(kind: DatabaseKind, column: &str) -> String {
    let mut out = String::with_capacity(column.len() + 4);
    push_quoted_or_raw(kind, &mut out, column);
    out
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
    /// 跳过行数（原始偏移；与 `limit` 搭配可表达"跳过 N 取 M"，
    /// 分页参数（`page_index`/`page_size`）优先。留空时不跳过）
    pub offset: Option<usize>,
    /// 页码（1 基；与 `page_size` 同时有效才启用分页）
    pub page_index: usize,
    /// 每页条数
    pub page_size: usize,
    /// 分页风格（仅 SQL Server 生效：默认 2012+ `OFFSET..FETCH`，2005/2008 可切 `ROW_NUMBER`）
    pub page_style: PageStyle,
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

    /// 跳过 N 行（原始偏移；与 [`Query::take`] 搭配表达"跳过 N 取 M"；分页参数优先）。
    pub fn offset(mut self, n: usize) -> Self {
        self.offset = Some(n);
        self
    }

    /// 设置分页风格（仅 SQL Server 生效；连接 SQL Server 2005/2008 时用 [`PageStyle::RowNumber`]）。
    /// <param name="style">分页风格</param>
    /// <returns>查询描述</returns>
    pub fn page_style(mut self, style: PageStyle) -> Self {
        self.page_style = style;
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

    #[test]
    fn raw_fragment_renders_verbatim_with_params() {
        let mut params = Vec::new();
        let sql = Where::new()
            .eq("Id", 1)
            .raw("(Name LIKE ? OR Code LIKE ?)", vec!["k".into(), "k".into()])
            .render(DatabaseKind::Sqlite, &mut params);
        assert_eq!(sql, "(\"Id\" = ? AND (Name LIKE ? OR Code LIKE ?))");
        assert_eq!(params.len(), 3);

        // SQL Server 占位符按序号递增，且沿用调用方已占用的下标
        let mut params = vec![DbValue::from(9)];
        let sql = Where::new()
            .raw("LEN(Code) > ?", vec![3.into()])
            .render(DatabaseKind::SqlServer, &mut params);
        assert_eq!(sql, "(LEN(Code) > @p1)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn any_like_builds_or_group() {
        let mut params = Vec::new();
        let sql = Where::new()
            .eq("Enable", true)
            .any_like(&["Name", "Code", "Remark"], "%hlk%")
            .render(DatabaseKind::Sqlite, &mut params);
        assert_eq!(
            sql,
            "(\"Enable\" = ? AND (Name LIKE ? OR Code LIKE ? OR Remark LIKE ?))"
        );
        assert_eq!(params.len(), 4, "3 列各一个同名模式参数");
        // 模式作用于每一列
        assert!(params[1..].iter().all(|v| v.as_str() == Some("%hlk%")));

        // 空列集合不生成条件（避免恒假/恒真）
        let mut params = Vec::new();
        assert_eq!(Where::new().any_like(&[], "%x%").render(DatabaseKind::Sqlite, &mut params), "");
        assert!(params.is_empty());
    }

    /// 端到端：`any_like` 与 `raw` 在真实 SQLite 上按预期命中。
    #[test]
    fn any_like_matches_any_column_on_sqlite() {
        use crate::dal::Dal;

        let dir = std::env::temp_dir().join(format!(
            "pek-where-or-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = format!(
            "Data Source={};Provider=SQLite",
            dir.join("where.db").display()
        );
        let dal = Dal::open(&conn).expect("打开 SQLite");
        let mut s = dal.open_session().expect("会话");
        s.execute("create table T (Id integer primary key, Name text, Code text, Remark text)", &[])
            .expect("建表");
        for (id, name, code, remark) in [
            (1, "alpha", "A1", ""),
            (2, "beta", "HLK-9", ""),
            (3, "gamma", "G3", "hlk 备注"),
            (4, "delta", "D4", ""),
        ] {
            s.execute(
                "insert into T (Id, Name, Code, Remark) values (?, ?, ?, ?)",
                &[id.into(), name.into(), code.into(), remark.into()],
            )
            .expect("插行");
        }

        let mut hit = |list: &[&str], pat: &str| -> Vec<i64> {
            let rows = s
                .query(
                    &format!(
                        "select Id from T where {} order by Id",
                        Where::new().any_like(list, pat).render(DatabaseKind::Sqlite, &mut Vec::new())
                    ),
                    &{
                        let mut p = Vec::new();
                        Where::new().any_like(list, pat).render(DatabaseKind::Sqlite, &mut p);
                        p
                    },
                )
                .expect("查询");
            rows.rows
                .iter()
                .map(|r| r.get_by_name("Id").and_then(|v| v.as_i64()).unwrap_or(0))
                .collect()
        };

        assert_eq!(hit(&["Name", "Code", "Remark"], "%hlk%"), vec![2, 3], "Code 与 Remark 任一命中");
        assert_eq!(hit(&["Name"], "%ta"), vec![2, 4], "只有 beta/delta 命中");
        assert!(hit(&["Code"], "%zzz%").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
