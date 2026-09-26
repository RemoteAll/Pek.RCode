//! SQL 语句组装：INSERT / UPDATE / DELETE / SELECT / COUNT。
//!
//! 对应 DH.NCode 的 `InsertBuilder` / `SelectBuilder`：语句完全由参数绑定生成，
//! 列名一律经方言引用，杜绝拼接注入。

use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::model::TableMeta;
use crate::query::{OrderBy, Query, Where};
use crate::value::DbValue;

/// 字段名 → 数据库列名（同时校验列存在，尽早发现拼写错误）。
fn column_name<'a>(table: &'a TableMeta, field: &str) -> Result<&'a str> {
    table
        .column(field)
        .map(|c| table.effective_column_name(c))
        .ok_or_else(|| Error::Model(format!("表 {} 不存在列 {}", table.name, field)))
}

/// 组装 INSERT 语句。
pub fn insert_sql(
    kind: DatabaseKind,
    table: &TableMeta,
    fields: &[(&str, DbValue)],
) -> Result<(String, Vec<DbValue>)> {
    if fields.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的插入语句至少需要一个字段",
            table.name
        )));
    }

    let mut columns = Vec::with_capacity(fields.len());
    let mut marks = Vec::with_capacity(fields.len());
    let mut params = Vec::with_capacity(fields.len());

    for (index, (field, value)) in fields.iter().enumerate() {
        columns.push(kind.quote(column_name(table, field)?));
        marks.push(kind.placeholder(index));
        params.push(value.clone());
    }

    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        kind.quote(table.effective_table_name()),
        columns.join(", "),
        marks.join(", ")
    );
    Ok((sql, params))
}

/// 组装 UPDATE 语句（`sets` 为 SET 子句，`filter` 为空时更新全部行，慎用）。
pub fn update_sql(
    kind: DatabaseKind,
    table: &TableMeta,
    sets: &[(&str, DbValue)],
    filter: &Where,
) -> Result<(String, Vec<DbValue>)> {
    if sets.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的更新语句至少需要一个待更新字段",
            table.name
        )));
    }

    let mut params = Vec::with_capacity(sets.len() + 4);
    let mut parts = Vec::with_capacity(sets.len());

    for (field, value) in sets {
        let mark = kind.placeholder(params.len());
        parts.push(format!("{} = {mark}", kind.quote(column_name(table, field)?)));
        params.push(value.clone());
    }

    let where_sql = filter.render(kind, &mut params);
    let mut sql = format!(
        "UPDATE {} SET {}",
        kind.quote(table.effective_table_name()),
        parts.join(", ")
    );
    if !where_sql.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_sql);
    }

    Ok((sql, params))
}

/// 组装 DELETE 语句（`filter` 为空时清空全表，慎用）。
pub fn delete_sql(kind: DatabaseKind, table: &TableMeta, filter: &Where) -> (String, Vec<DbValue>) {
    let mut params = Vec::with_capacity(4);
    let where_sql = filter.render(kind, &mut params);

    let mut sql = format!("DELETE FROM {}", kind.quote(table.effective_table_name()));
    if !where_sql.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_sql);
    }
    (sql, params)
}

/// 组装 COUNT 语句。
pub fn count_sql(
    kind: DatabaseKind,
    table: &TableMeta,
    filter: Option<&Where>,
) -> (String, Vec<DbValue>) {
    let mut params = Vec::new();
    let mut sql = format!(
        "SELECT COUNT(*) FROM {}",
        kind.quote(table.effective_table_name())
    );
    if let Some(filter) = filter {
        let where_sql = filter.render(kind, &mut params);
        if !where_sql.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_sql);
        }
    }
    (sql, params)
}

/// 组装 SELECT 语句（含排序、分页、取前 N 条）。
///
/// 说明：
/// - `query.select` 中的列名会校验并引用；不是列名的内容按原始表达式使用（如 `COUNT(*)`）
/// - 分页：`page_index >= 1 && page_size > 0` 时启用；SQL Server/Oracle 无排序时按主键兜底
pub fn select_sql(kind: DatabaseKind, table: &TableMeta, query: &Query) -> (String, Vec<DbValue>) {
    // 查询列
    let columns = if query.select.is_empty() {
        "*".to_string()
    } else {
        query
            .select
            .iter()
            .map(|c| match table.column(c) {
                Some(col) => kind.quote(table.effective_column_name(col)),
                None => c.clone(), // 原始表达式（函数、别名等）
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut params = Vec::new();
    let mut sql = format!(
        "SELECT {columns} FROM {}",
        kind.quote(table.effective_table_name())
    );

    if let Some(filter) = &query.filter {
        let where_sql = filter.render(kind, &mut params);
        if !where_sql.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_sql);
        }
    }

    // 排序
    let mut order_sql = render_order(kind, table, &query.order_by);

    // 分页/取前 N 条场景下，未显式排序时按主键兜底，保证结果稳定
    let paging = query.page_index >= 1 && query.page_size > 0;
    if order_sql.is_empty() && (paging || query.limit.is_some()) && !table.primary_keys().is_empty() {
        order_sql = render_order_of(kind, table, &default_order_columns(table));
    }

    // 分页优先于取前 N
    if paging {
        let offset = (query.page_index - 1) * query.page_size;
        sql = kind.apply_paging(&sql, &order_sql, offset, query.page_size);
    } else if let Some(limit) = query.limit {
        sql = kind.apply_paging(&sql, &order_sql, 0, limit);
    } else if !order_sql.is_empty() {
        sql.push(' ');
        sql.push_str(&order_sql);
    }

    (sql, params)
}

/// 渲染 `ORDER BY` 子句（空列表返回空串）。
fn render_order(kind: DatabaseKind, table: &TableMeta, orders: &[OrderBy]) -> String {
    if orders.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = orders
        .iter()
        .map(|o| {
            let name = match table.column(&o.column) {
                Some(col) => kind.quote(table.effective_column_name(col)),
                None => o.column.clone(),
            };
            if o.desc {
                format!("{name} DESC")
            } else {
                name.to_string()
            }
        })
        .collect();
    format!("ORDER BY {}", parts.join(", "))
}

/// 按列名渲染升序 `ORDER BY`。
fn render_order_of(kind: DatabaseKind, table: &TableMeta, columns: &[String]) -> String {
    let orders: Vec<OrderBy> = columns
        .iter()
        .map(|c| OrderBy {
            column: c.clone(),
            desc: false,
        })
        .collect();
    render_order(kind, table, &orders)
}

/// 默认排序列：主键（用于分页时保证结果稳定）。
fn default_order_columns(table: &TableMeta) -> Vec<String> {
    table
        .primary_keys()
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
        <Column Name="Status" DataType="Int32" />
        <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    fn table() -> TableMeta {
        EntityModel::parse(MODEL).unwrap().tables.remove(0)
    }

    #[test]
    fn insert_generation() {
        let t = table();
        let (sql, params) = insert_sql(
            DatabaseKind::Sqlite,
            &t,
            &[("Code", "A1".into()), ("Amount", "10.5".parse::<rust_decimal::Decimal>().unwrap().into())],
        )
        .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"DH_Order\" (\"Code\", \"Amount\") VALUES (?, ?)"
        );
        assert_eq!(params.len(), 2);

        let (sql, _) = insert_sql(DatabaseKind::SqlServer, &t, &[("Code", "A1".into())]).unwrap();
        assert_eq!(sql, "INSERT INTO [DH_Order] ([Code]) VALUES (@p0)");

        // 未知列应尽早报错
        assert!(insert_sql(DatabaseKind::Sqlite, &t, &[("Nope", 1.into())]).is_err());
    }

    #[test]
    fn update_generation_with_params_order() {
        let t = table();
        let filter = Where::new().eq("Id", 7);
        let (sql, params) =
            update_sql(DatabaseKind::SqlServer, &t, &[("Status", 2.into()), ("Code", "B".into())], &filter)
                .unwrap();
        assert_eq!(
            sql,
            "UPDATE [DH_Order] SET [Status] = @p0, [Code] = @p1 WHERE ([Id] = @p2)"
        );
        assert_eq!(params.len(), 3, "SET 参数应排在 WHERE 参数之前");
    }

    #[test]
    fn delete_generation() {
        let t = table();
        let (sql, params) = delete_sql(DatabaseKind::Sqlite, &t, &Where::new().eq("Id", 1));
        assert_eq!(sql, "DELETE FROM \"DH_Order\" WHERE (\"Id\" = ?)");
        assert_eq!(params, vec![DbValue::Int(1)]);
    }

    #[test]
    fn count_and_select_with_paging() {
        let t = table();
        let (sql, _) = count_sql(DatabaseKind::Sqlite, &t, Some(&Where::new().eq("Status", 1)));
        assert_eq!(sql, "SELECT COUNT(*) FROM \"DH_Order\" WHERE (\"Status\" = ?)");

        let q = Query::new().filter(Where::new().eq("Status", 1)).page(2, 10);
        let (sql, params) = select_sql(DatabaseKind::Sqlite, &t, &q);
        assert_eq!(
            sql,
            "SELECT * FROM \"DH_Order\" WHERE (\"Status\" = ?) ORDER BY \"Id\" LIMIT 10 OFFSET 10"
        );
        assert_eq!(params.len(), 1);

        // SQL Server 分页使用 OFFSET..FETCH，并以主键兜底排序
        let (sql, _) = select_sql(DatabaseKind::SqlServer, &t, &q);
        assert_eq!(
            sql,
            "SELECT * FROM [DH_Order] WHERE ([Status] = @p0) ORDER BY [Id] OFFSET 10 ROWS FETCH NEXT 10 ROWS ONLY"
        );
    }

    #[test]
    fn select_expression_and_take() {
        let t = table();
        let q = Query::new().column("Status").column("COUNT(*) AS Cnt").take(5);
        let (sql, _) = select_sql(DatabaseKind::Sqlite, &t, &q);
        assert_eq!(
            sql,
            "SELECT \"Status\", COUNT(*) AS Cnt FROM \"DH_Order\" ORDER BY \"Id\" LIMIT 5 OFFSET 0"
        );
    }
}
