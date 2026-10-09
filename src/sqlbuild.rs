//! SQL 语句组装：INSERT / UPDATE / DELETE / SELECT / COUNT。
//!
//! 对应 DH.NCode 的 `InsertBuilder` / `SelectBuilder`：语句完全由参数绑定生成，
//! 列名一律经方言引用，杜绝拼接注入。

use crate::dialect::{DatabaseKind, oracle_identity_sequence};
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
    insert_sql_named(kind, table, table.effective_table_name(), fields)
}

/// 组装 INSERT 语句（指定物理表名；分表场景由 [`crate::dal::Dal::table_as`] 的句柄自动传入）。
pub fn insert_sql_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    fields: &[(&str, DbValue)],
) -> Result<(String, Vec<DbValue>)> {
    if fields.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的插入语句至少需要一个字段",
            table.name
        )));
    }

    // InfluxDB：直接生成行协议文本（非 SQL），写入由驱动 POST 到 /write
    if kind == DatabaseKind::InfluxDb {
        return Ok((influx_line_protocol(table, table_name, fields)?, Vec::new()));
    }

    let mut columns = Vec::with_capacity(fields.len() + 1);
    let mut marks = Vec::with_capacity(fields.len() + 1);
    let mut params = Vec::with_capacity(fields.len());

    for (index, (field, value)) in fields.iter().enumerate() {
        columns.push(kind.quote(column_name(table, field)?));
        marks.push(kind.placeholder(index));
        params.push(value.clone());
    }

    // 序列型自增（Oracle/DB2/Firebird，XCode 约定 SEQ_{表名}）：
    // 未显式提供自增列时补上序列表达式，插入后由驱动读取序列当前值回写
    if matches!(
        kind,
        DatabaseKind::Oracle | DatabaseKind::Db2 | DatabaseKind::Firebird
    ) && let Some(identity) = table.identity()
        && !fields
            .iter()
            .any(|(field, _)| field.eq_ignore_ascii_case(&identity.name))
    {
        let sequence = oracle_identity_sequence(table_name);
        let expression = match kind {
            // Oracle："SEQ_x".NEXTVAL（引号保持大小写）
            DatabaseKind::Oracle => format!("{}.NEXTVAL", kind.quote(&sequence)),
            // DB2（Oracle 兼容模式）：与 DH.NCode 一致使用未引号 SEQ_表名.nextval
            DatabaseKind::Db2 => format!("{sequence}.nextval"),
            // Firebird：next value for "SEQ_x"
            _ => format!("next value for {}", kind.quote(&sequence)),
        };
        columns.push(kind.quote(table.effective_column_name(identity)));
        marks.push(expression);
    }

    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        kind.quote(table_name),
        columns.join(", "),
        marks.join(", ")
    );
    Ok((sql, params))
}

/// 批量写入模式（对应 C# `IDbSession.Insert / InsertIgnore / Replace / Upsert` 的多行实现）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchWriteMode {
    /// 普通插入（多行 `VALUES`）
    Insert,
    /// 忽略重复插入（SQLite/DuckDB `Insert Or Ignore`、MySQL `Insert Ignore`、PostgreSQL `On Conflict Do Nothing`）
    InsertIgnore,
    /// 替换插入（SQLite/DuckDB `Insert Or Replace`、MySQL `Replace Into`）
    Replace,
    /// 插入或更新（SQLite/DuckDB/PostgreSQL `On Conflict(pk) Do Update`、MySQL `On Duplicate Key Update`）
    Upsert,
}

/// 组装多行写入语句（对应 C# 各库 `IDbSession.Insert / InsertIgnore / Replace / Upsert` 的批量实现）。
///
/// 生成 `INSERT ... VALUES (..), (..), ...` 及方言去重后缀；列集由调用方统一，占位符按行展开，
/// 参数由调用方按行、列顺序拍平传入（与 [`insert_sql_named`] 的单行占位符规则一致）。
///
/// - `Upsert` 的冲突目标取**表主键**（无主键报错）；更新列 = 插入列中排除主键与自增列
///   （对齐 C# `On Conflict Do Update` 的字段过滤；无更新列时 SQLite 系 `Do Nothing`、MySQL no-op）；
/// - 方言不支持对应模式时返回 [`Error::Unsupported`]（由调用方回退或报错）。
pub fn multi_write_sql_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    columns: &[&str],
    rows: usize,
    mode: BatchWriteMode,
) -> Result<String> {
    if columns.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的批量写入语句至少需要一个字段",
            table.name
        )));
    }
    if rows == 0 {
        return Err(Error::Model("批量写入的行数不能为 0".into()));
    }
    // InfluxDB 行协议、MongoDB 子集翻译不属于 SQL 多行语法（调用方先行回退）
    if !kind.supports_multi_row_insert() {
        return Err(Error::Unsupported(format!(
            "{kind:?} 不支持多行 VALUES 批量写入，请回退逐行执行"
        )));
    }

    let cols: Vec<String> = columns
        .iter()
        .map(|field| column_name(table, field).map(|name| kind.quote(name)))
        .collect::<Result<_>>()?;

    let mut tuples = Vec::with_capacity(rows);
    let mut index = 0usize;
    for _ in 0..rows {
        let mut marks = Vec::with_capacity(columns.len());
        for _ in columns {
            marks.push(kind.placeholder(index));
            index += 1;
        }
        tuples.push(format!("({})", marks.join(", ")));
    }

    let (head, tail): (&str, String) = match mode {
        BatchWriteMode::Insert => ("INSERT INTO", String::new()),
        BatchWriteMode::InsertIgnore => match kind {
            DatabaseKind::Sqlite | DatabaseKind::DuckDb => ("INSERT OR IGNORE INTO", String::new()),
            DatabaseKind::MySql => ("INSERT IGNORE INTO", String::new()),
            DatabaseKind::PostgreSql => ("INSERT INTO", " ON CONFLICT DO NOTHING".into()),
            _ => {
                return Err(Error::Unsupported(format!(
                    "{kind:?} 不支持多行 InsertIgnore 批量写入"
                )));
            }
        },
        BatchWriteMode::Replace => match kind {
            DatabaseKind::Sqlite | DatabaseKind::DuckDb => ("INSERT OR REPLACE INTO", String::new()),
            DatabaseKind::MySql => ("REPLACE INTO", String::new()),
            _ => {
                return Err(Error::Unsupported(format!(
                    "{kind:?} 不支持多行 Replace 批量写入"
                )));
            }
        },
        BatchWriteMode::Upsert => {
            let pks = table.primary_keys();
            if pks.is_empty() {
                return Err(Error::Model(format!(
                    "表 {} 没有主键，无法批量 Upsert",
                    table.name
                )));
            }
            // 更新列 = 插入列中排除主键与自增列（对齐 C# `On Conflict Do Update` 的字段过滤）
            let update_fields: Vec<&str> = columns
                .iter()
                .copied()
                .filter(|field| {
                    !pks.iter().any(|pk| pk.name.eq_ignore_ascii_case(field))
                        && !table
                            .identity()
                            .is_some_and(|id| id.name.eq_ignore_ascii_case(field))
                })
                .collect();

            match kind {
                DatabaseKind::Sqlite | DatabaseKind::DuckDb | DatabaseKind::PostgreSql => {
                    let target: Vec<String> = pks
                        .iter()
                        .map(|pk| kind.quote(table.effective_column_name(pk)))
                        .collect();
                    let tail = if update_fields.is_empty() {
                        format!(" ON CONFLICT ({}) DO NOTHING", target.join(", "))
                    } else {
                        let sets: Vec<String> = update_fields
                            .iter()
                            .map(|field| {
                                let col = kind.quote(column_name(table, field)?);
                                Ok(format!("{col}=excluded.{col}"))
                            })
                            .collect::<Result<_>>()?;
                        format!(
                            " ON CONFLICT ({}) DO UPDATE SET {}",
                            target.join(", "),
                            sets.join(", ")
                        )
                    };
                    ("INSERT INTO", tail)
                }
                DatabaseKind::MySql => {
                    let tail = if update_fields.is_empty() {
                        // 无更新列：no-op 保持"存在即忽略"语义（MySQL 惯用写法）
                        let pk = kind.quote(table.effective_column_name(pks[0]));
                        format!(" ON DUPLICATE KEY UPDATE {pk}={pk}")
                    } else {
                        let sets: Vec<String> = update_fields
                            .iter()
                            .map(|field| {
                                let col = kind.quote(column_name(table, field)?);
                                Ok(format!("{col}=VALUES({col})"))
                            })
                            .collect::<Result<_>>()?;
                        format!(" ON DUPLICATE KEY UPDATE {}", sets.join(", "))
                    };
                    ("INSERT INTO", tail)
                }
                _ => {
                    return Err(Error::Unsupported(format!(
                        "{kind:?} 不支持多行 Upsert 批量写入"
                    )));
                }
            }
        }
    };

    Ok(format!(
        "{head} {} ({}) VALUES {}{tail}",
        kind.quote(table_name),
        cols.join(", "),
        tuples.join(", ")
    ))
}

/// 组装多行 INSERT 语句（[`BatchWriteMode::Insert`] 的便捷入口，保留原签名）。
pub fn insert_multi_sql_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    columns: &[&str],
    rows: usize,
) -> Result<String> {
    multi_write_sql_named(kind, table, table_name, columns, rows, BatchWriteMode::Insert)
}

/// InfluxDB 行协议：`measurement,tag=.. field=.. timestamp`。
///
/// 与 DH.NCode 的批量写入规则一致：主键/主列（`PrimaryKey`/`Master`）作为 tag，
/// 其余作为 field；名为 `Time`/`CreateTime`/`UpdateTime` 的时间列作为时间戳（纳秒）。
fn influx_line_protocol(
    table: &TableMeta,
    table_name: &str,
    fields: &[(&str, DbValue)],
) -> Result<String> {
    let time_column = fields.iter().find_map(|(name, value)| {
        let is_time = name.eq_ignore_ascii_case("Time")
            || name.eq_ignore_ascii_case("CreateTime")
            || name.eq_ignore_ascii_case("UpdateTime");
        (is_time && matches!(value, DbValue::DateTime(_))).then_some(*name)
    });

    let mut line = escape_influx(table_name);
    let mut values = String::new();
    let mut timestamp: Option<i64> = None;

    for (name, value) in fields {
        if Some(*name) == time_column {
            if let DbValue::DateTime(v) = value {
                timestamp = v.and_utc().timestamp_nanos_opt();
            }
            continue;
        }
        let col = table
            .column(name)
            .ok_or_else(|| Error::Model(format!("表 {} 不存在列 {}", table.name, name)))?;
        let key = escape_influx(table.effective_column_name(col));

        if col.primary_key || col.master {
            // tag：值不能为 NULL（NULL 时跳过该 tag）
            if !value.is_null() {
                line.push_str(&format!(",{key}={}", escape_influx(&influx_tag_value(value))));
            }
        } else {
            if !values.is_empty() {
                values.push(',');
            }
            values.push_str(&format!("{key}={}", influx_field_value(value)?));
        }
    }

    if values.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的插入除了 tag/时间戳外至少需要一个 field（InfluxDB 行协议要求）",
            table.name
        )));
    }

    line.push(' ');
    line.push_str(&values);
    if let Some(ts) = timestamp {
        line.push(' ');
        line.push_str(&ts.to_string());
    }
    Ok(line)
}

/// tag 值（仅数值/布尔/文本有意义）。
fn influx_tag_value(value: &DbValue) -> String {
    match value {
        DbValue::Bool(v) => v.to_string(),
        other => other.to_text(),
    }
}

/// field 值（字符串需引号包裹；整数带 `i` 后缀；DECIMAL 降级为浮点文本）。
fn influx_field_value(value: &DbValue) -> Result<String> {
    Ok(match value {
        DbValue::Bool(v) => v.to_string(),
        DbValue::Int(v) => format!("{v}i"),
        DbValue::Float(v) => {
            if v.is_finite() {
                v.to_string()
            } else {
                return Err(Error::Model("InfluxDB 不接受 NaN/Inf 浮点值".into()));
            }
        }
        DbValue::Decimal(v) => v.to_string(),
        DbValue::Text(v) => format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")),
        DbValue::DateTime(v) => v.and_utc().timestamp_nanos_opt().unwrap_or(0).to_string(),
        DbValue::Blob(_) => {
            return Err(Error::Model(
                "InfluxDB 行协议不支持二进制 field（请改用文本/数值列）".into(),
            ));
        }
        DbValue::Null => "NULL".into(),
    })
}

/// 行协议标识符转义（逗号/空格/等号/反斜杠）。
fn escape_influx(ident: &str) -> String {
    ident
        .replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(' ', "\\ ")
        .replace('=', "\\=")
}

/// 组装 UPDATE 语句（`sets` 为 SET 子句，`filter` 为空时更新全部行，慎用）。
pub fn update_sql(
    kind: DatabaseKind,
    table: &TableMeta,
    sets: &[(&str, DbValue)],
    filter: &Where,
) -> Result<(String, Vec<DbValue>)> {
    update_sql_named(kind, table, table.effective_table_name(), sets, filter)
}

/// 组装 UPDATE 语句（指定物理表名；分表场景使用）。
pub fn update_sql_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
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
        kind.quote(table_name),
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
    delete_sql_named(kind, table.effective_table_name(), filter)
}

/// 组装 DELETE 语句（指定物理表名；分表场景使用）。
pub fn delete_sql_named(
    kind: DatabaseKind,
    table_name: &str,
    filter: &Where,
) -> (String, Vec<DbValue>) {
    let mut params = Vec::with_capacity(4);
    let where_sql = filter.render(kind, &mut params);

    let mut sql = format!("DELETE FROM {}", kind.quote(table_name));
    if !where_sql.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_sql);
    }
    (sql, params)
}

/// 组装**分批删除**语句（每批最多 `batch_size` 行；对应 C# `IDbDatabase.BuildDeleteSql`）。
///
/// 支持分批删除的方言（与 C# 对齐，另含 SQLite 增强）：
/// - MySQL / IRIS / NovaDb（MySQL 协议）：`DELETE ... LIMIT n`
/// - SQL Server：`DELETE TOP (n) FROM ...`
/// - PostgreSQL 系（含 HighGo/KingBase/VastBase）：`WITH _to_delete AS (SELECT ctid ... LIMIT n) DELETE ... WHERE ctid IN (...)`
/// - Oracle：`DELETE ... WHERE ROWID IN (SELECT ROWID ... WHERE (where) AND ROWNUM<=n)`
/// - SQLite（**C# 未支持，本库增强**）：`DELETE ... WHERE rowid IN (SELECT rowid FROM ... LIMIT n)`（`WITHOUT ROWID` 表不可用）
///
/// 其余方言返回 `None`（调用方回退为一次性删除，与 C# 相同）。
pub fn delete_batched_sql_named(
    kind: DatabaseKind,
    table_name: &str,
    filter: &Where,
    batch_size: usize,
) -> Option<(String, Vec<DbValue>)> {
    if batch_size == 0 {
        return None;
    }
    let mut params = Vec::with_capacity(4);
    let where_sql = filter.render(kind, &mut params);
    let has_where = !where_sql.is_empty();
    let t = kind.quote(table_name);

    let sql = match kind {
        DatabaseKind::MySql | DatabaseKind::Iris => {
            let mut sql = format!("DELETE FROM {t}");
            if has_where {
                sql.push_str(" WHERE ");
                sql.push_str(&where_sql);
            }
            sql.push_str(&format!(" LIMIT {batch_size}"));
            sql
        }
        DatabaseKind::SqlServer => {
            let mut sql = format!("DELETE TOP ({batch_size}) FROM {t}");
            if has_where {
                sql.push_str(" WHERE ");
                sql.push_str(&where_sql);
            }
            sql
        }
        DatabaseKind::PostgreSql => {
            let mut inner = format!("SELECT ctid FROM {t}");
            if has_where {
                inner.push_str(" WHERE ");
                inner.push_str(&where_sql);
            }
            inner.push_str(&format!(" LIMIT {batch_size}"));
            format!(
                "WITH _to_delete AS ({inner}) DELETE FROM {t} WHERE ctid IN (SELECT ctid FROM _to_delete)"
            )
        }
        DatabaseKind::Oracle => {
            let cond = if has_where {
                format!("{where_sql} AND ")
            } else {
                String::new()
            };
            format!(
                "DELETE FROM {t} WHERE ROWID IN (SELECT ROWID FROM {t} WHERE {cond}ROWNUM<={batch_size})"
            )
        }
        DatabaseKind::Sqlite => {
            let mut inner = format!("SELECT rowid FROM {t}");
            if has_where {
                inner.push_str(" WHERE ");
                inner.push_str(&where_sql);
            }
            inner.push_str(&format!(" LIMIT {batch_size}"));
            format!("DELETE FROM {t} WHERE rowid IN ({inner})")
        }
        _ => return None,
    };
    Some((sql, params))
}

/// 组装 COUNT 语句。
pub fn count_sql(
    kind: DatabaseKind,
    table: &TableMeta,
    filter: Option<&Where>,
) -> (String, Vec<DbValue>) {
    count_sql_named(kind, table.effective_table_name(), filter)
}

/// 组装 COUNT 语句（指定物理表名；分表场景使用）。
pub fn count_sql_named(
    kind: DatabaseKind,
    table_name: &str,
    filter: Option<&Where>,
) -> (String, Vec<DbValue>) {
    let mut params = Vec::new();
    let mut sql = format!("SELECT COUNT(*) FROM {}", kind.quote(table_name));
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
    select_sql_named(kind, table, table.effective_table_name(), query)
}

/// 组装 SELECT 语句（指定物理表名；分表场景使用）。
pub fn select_sql_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    query: &Query,
) -> (String, Vec<DbValue>) {
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
    let mut sql = format!("SELECT {columns} FROM {}", kind.quote(table_name));

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
    if order_sql.is_empty()
        && (paging || query.limit.is_some() || query.offset.is_some())
        && !table.primary_keys().is_empty()
    {
        order_sql = render_order_of(kind, table, &default_order_columns(table));
    }

    // 分页优先于 offset/limit；offset 无 limit 时按"跳到末尾取全部"处理（BIGINT 上限）
    if paging {
        let offset = (query.page_index - 1) * query.page_size;
        sql = kind.apply_paging_with_style(&sql, &order_sql, offset, query.page_size, query.page_style);
    } else if let Some(offset) = query.offset {
        let size = query.limit.unwrap_or(i64::MAX as usize);
        sql = kind.apply_paging_with_style(&sql, &order_sql, offset, size, query.page_style);
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
    fn multi_row_insert_generation() {
        let t = table();
        // SQLite/MySQL：占位符 `?` 按行展开
        let sql = insert_multi_sql_named(
            DatabaseKind::Sqlite,
            &t,
            "DH_Order_2026",
            &["Code", "Amount"],
            2,
        )
        .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"DH_Order_2026\" (\"Code\", \"Amount\") VALUES (?, ?), (?, ?)"
        );

        // SQL Server：命名占位符连续编号
        let sql =
            insert_multi_sql_named(DatabaseKind::SqlServer, &t, "DH_Order", &["Code"], 2).unwrap();
        assert_eq!(sql, "INSERT INTO [DH_Order] ([Code]) VALUES (@p0), (@p1)");

        // 不支持多行的方言 / 非法参数：调用方先行回退
        assert!(insert_multi_sql_named(DatabaseKind::Oracle, &t, "T", &["Code"], 1).is_err());
        assert!(insert_multi_sql_named(DatabaseKind::InfluxDb, &t, "T", &["Code"], 1).is_err());
        assert!(insert_multi_sql_named(DatabaseKind::Sqlite, &t, "T", &["Nope"], 1).is_err());
        assert!(insert_multi_sql_named(DatabaseKind::Sqlite, &t, "T", &[], 1).is_err());
        assert!(insert_multi_sql_named(DatabaseKind::Sqlite, &t, "T", &["Code"], 0).is_err());
    }

    #[test]
    fn multi_write_modes_generation() {
        let t = table();
        let cols = ["Id", "Code", "Amount"];

        // 忽略重复：SQLite/MySQL 前缀式、PostgreSQL On Conflict Do Nothing
        let sql = multi_write_sql_named(
            DatabaseKind::Sqlite,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::InsertIgnore,
        )
        .unwrap();
        assert!(sql.starts_with("INSERT OR IGNORE INTO \"DH_Order\""));
        let sql = multi_write_sql_named(
            DatabaseKind::MySql,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::InsertIgnore,
        )
        .unwrap();
        assert!(sql.starts_with("INSERT IGNORE INTO `DH_Order`"));
        let sql = multi_write_sql_named(
            DatabaseKind::PostgreSql,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::InsertIgnore,
        )
        .unwrap();
        assert!(sql.ends_with(" ON CONFLICT DO NOTHING"), "{sql}");

        // 替换插入：SQLite/DuckDB 前缀式、MySQL Replace Into
        let sql = multi_write_sql_named(
            DatabaseKind::Sqlite,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::Replace,
        )
        .unwrap();
        assert!(sql.starts_with("INSERT OR REPLACE INTO \"DH_Order\""));
        let sql = multi_write_sql_named(
            DatabaseKind::MySql,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::Replace,
        )
        .unwrap();
        assert!(sql.starts_with("REPLACE INTO `DH_Order`"));

        // Upsert：SQLite/PG 用 On Conflict(pk) Do Update（更新列排除主键与自增列）
        let sql = multi_write_sql_named(
            DatabaseKind::Sqlite,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::Upsert,
        )
        .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"DH_Order\" (\"Id\", \"Code\", \"Amount\") VALUES (?, ?, ?) \
             ON CONFLICT (\"Id\") DO UPDATE SET \"Code\"=excluded.\"Code\", \"Amount\"=excluded.\"Amount\""
        );
        let sql = multi_write_sql_named(
            DatabaseKind::PostgreSql,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::Upsert,
        )
        .unwrap();
        assert!(sql.contains("ON CONFLICT (\"Id\") DO UPDATE SET"), "{sql}");
        let sql = multi_write_sql_named(
            DatabaseKind::MySql,
            &t,
            "DH_Order",
            &cols,
            1,
            BatchWriteMode::Upsert,
        )
        .unwrap();
        assert!(
            sql.contains("ON DUPLICATE KEY UPDATE `Code`=VALUES(`Code`), `Amount`=VALUES(`Amount`)"),
            "{sql}"
        );

        // 仅主键列（无更新列）：SQLite 系 Do Nothing、MySQL no-op
        let sql = multi_write_sql_named(
            DatabaseKind::Sqlite,
            &t,
            "DH_Order",
            &["Id"],
            1,
            BatchWriteMode::Upsert,
        )
        .unwrap();
        assert!(sql.ends_with("ON CONFLICT (\"Id\") DO NOTHING"), "{sql}");
        let sql = multi_write_sql_named(
            DatabaseKind::MySql,
            &t,
            "DH_Order",
            &["Id"],
            1,
            BatchWriteMode::Upsert,
        )
        .unwrap();
        assert!(sql.ends_with("ON DUPLICATE KEY UPDATE `Id`=`Id`"), "{sql}");

        // 方言不支持：报错（无安全回退）
        assert!(
            multi_write_sql_named(
                DatabaseKind::Oracle,
                &t,
                "T",
                &cols,
                1,
                BatchWriteMode::InsertIgnore
            )
            .is_err()
        );
        assert!(
            multi_write_sql_named(
                DatabaseKind::SqlServer,
                &t,
                "T",
                &cols,
                1,
                BatchWriteMode::Replace
            )
            .is_err()
        );
        assert!(
            multi_write_sql_named(
                DatabaseKind::Oracle,
                &t,
                "T",
                &cols,
                1,
                BatchWriteMode::Upsert
            )
            .is_err()
        );
    }

    #[test]
    fn batched_delete_generation() {
        let filter = Where::new().eq("Status", 0);

        // MySQL：DELETE ... LIMIT
        let (sql, params) =
            delete_batched_sql_named(DatabaseKind::MySql, "DH_Order", &filter, 100).unwrap();
        assert_eq!(sql, "DELETE FROM `DH_Order` WHERE (`Status` = ?) LIMIT 100");
        assert_eq!(params.len(), 1);

        // SQL Server：DELETE TOP (n)
        let (sql, _) =
            delete_batched_sql_named(DatabaseKind::SqlServer, "DH_Order", &filter, 100).unwrap();
        assert_eq!(sql, "DELETE TOP (100) FROM [DH_Order] WHERE ([Status] = @p0)");

        // PostgreSQL：ctid 子查询
        let (sql, _) =
            delete_batched_sql_named(DatabaseKind::PostgreSql, "DH_Order", &filter, 100).unwrap();
        assert_eq!(
            sql,
            "WITH _to_delete AS (SELECT ctid FROM \"DH_Order\" WHERE (\"Status\" = $1) LIMIT 100) \
             DELETE FROM \"DH_Order\" WHERE ctid IN (SELECT ctid FROM _to_delete)"
        );

        // Oracle：ROWID + ROWNUM
        let (sql, _) =
            delete_batched_sql_named(DatabaseKind::Oracle, "DH_Order", &filter, 100).unwrap();
        assert_eq!(
            sql,
            "DELETE FROM \"DH_Order\" WHERE ROWID IN (SELECT ROWID FROM \"DH_Order\" \
             WHERE (\"Status\" = :1) AND ROWNUM<=100)"
        );

        // SQLite：rowid 子查询（本库增强；C# 未支持）
        let (sql, _) =
            delete_batched_sql_named(DatabaseKind::Sqlite, "DH_Order", &filter, 100).unwrap();
        assert_eq!(
            sql,
            "DELETE FROM \"DH_Order\" WHERE rowid IN \
             (SELECT rowid FROM \"DH_Order\" WHERE (\"Status\" = ?) LIMIT 100)"
        );

        // 无 WHERE：全表分批删除（不带 WHERE 子句）
        let (sql, params) = delete_batched_sql_named(
            DatabaseKind::MySql,
            "DH_Order",
            &Where::default(),
            7,
        )
        .unwrap();
        assert_eq!(sql, "DELETE FROM `DH_Order` LIMIT 7");
        assert!(params.is_empty());

        // 其余方言：None（调用方回退一次性删除）；批大小 0：None
        assert!(delete_batched_sql_named(DatabaseKind::SqlServer, "T", &filter, 0).is_none());
        assert!(delete_batched_sql_named(DatabaseKind::DuckDb, "T", &filter, 100).is_none());
        assert!(delete_batched_sql_named(DatabaseKind::Firebird, "T", &filter, 100).is_none());
    }

    #[test]
    fn oracle_insert_injects_sequence_expression() {
        let t = table();
        // 自增列未显式提供时，自动补上序列表达式（XCode 约定 SEQ_{表名}，不占绑定参数）
        let (sql, params) = insert_sql(DatabaseKind::Oracle, &t, &[("Code", "A1".into())]).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"DH_Order\" (\"Code\", \"Id\") VALUES (:1, \"SEQ_DH_Order\".NEXTVAL)"
        );
        assert_eq!(params.len(), 1);

        // 显式提供自增列时不注入（保持调用方语义）
        let (sql, params) = insert_sql(
            DatabaseKind::Oracle,
            &t,
            &[("Id", 9.into()), ("Code", "A1".into())],
        )
        .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"DH_Order\" (\"Id\", \"Code\") VALUES (:1, :2)"
        );
        assert_eq!(params.len(), 2);
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

    #[test]
    fn sqlserver_row_number_style_paging() {
        let t = table();

        // 默认风格仍为 2012+ OFFSET..FETCH（与 DH.NCode 现行行为一致）
        let q = Query::new().filter(Where::new().eq("Status", 1)).page(3, 10);
        let (sql, _) = select_sql(DatabaseKind::SqlServer, &t, &q);
        assert!(sql.contains("OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"), "{sql}");

        // 切换 MSPageSplit 的 ROW_NUMBER 风格（SQL Server 2005/2008）
        let q = q.page_style(crate::dialect::PageStyle::RowNumber);
        let (sql, _) = select_sql(DatabaseKind::SqlServer, &t, &q);
        assert_eq!(
            sql,
            "SELECT * FROM (SELECT *, row_number() over(Order By [Id]) as rowNumber FROM (SELECT * FROM [DH_Order] WHERE ([Status] = @p0)) AS XCode_T0) AS XCode_T1 WHERE rowNumber BETWEEN 21 And 30"
        );

        // 其它库不受风格影响
        let (sql, _) = select_sql(DatabaseKind::Sqlite, &t, &q);
        assert_eq!(
            sql,
            "SELECT * FROM \"DH_Order\" WHERE (\"Status\" = ?) ORDER BY \"Id\" LIMIT 10 OFFSET 20"
        );
    }

    #[test]
    fn offset_paging_supports_raw_skip() {
        let t = table();

        // offset + take：原始"跳过 N 取 M"
        let q = Query::new().offset(10).take(5);
        let (sql, _) = select_sql(DatabaseKind::Sqlite, &t, &q);
        assert_eq!(
            sql,
            "SELECT * FROM \"DH_Order\" ORDER BY \"Id\" LIMIT 5 OFFSET 10"
        );

        // 仅 offset：跳过 N 后取全部（LIMIT 取 BIGINT 上限）
        let q = Query::new().offset(10);
        let (sql, _) = select_sql(DatabaseKind::Sqlite, &t, &q);
        assert_eq!(
            sql,
            "SELECT * FROM \"DH_Order\" ORDER BY \"Id\" LIMIT 9223372036854775807 OFFSET 10"
        );

        // 分页参数优先于 offset
        let q = Query::new().offset(10).take(5).page(2, 10);
        let (sql, _) = select_sql(DatabaseKind::Sqlite, &t, &q);
        assert_eq!(
            sql,
            "SELECT * FROM \"DH_Order\" ORDER BY \"Id\" LIMIT 10 OFFSET 10"
        );
    }

    #[test]
    fn named_variants_target_physical_table() {
        let t = table();

        // 分表物理表名覆盖（模型元数据不变）
        let (sql, _) = select_sql_named(DatabaseKind::Sqlite, &t, "DH_Order_202609", &Query::new());
        assert_eq!(sql, "SELECT * FROM \"DH_Order_202609\"");

        let (sql, _) = count_sql_named(DatabaseKind::Sqlite, "DH_Order_202609", None);
        assert_eq!(sql, "SELECT COUNT(*) FROM \"DH_Order_202609\"");

        let (sql, params) = delete_sql_named(
            DatabaseKind::Sqlite,
            "DH_Order_202609",
            &Where::new().eq("Id", 1),
        );
        assert_eq!(sql, "DELETE FROM \"DH_Order_202609\" WHERE (\"Id\" = ?)");
        assert_eq!(params.len(), 1);

        let (sql, _) = update_sql_named(
            DatabaseKind::Sqlite,
            &t,
            "DH_Order_202609",
            &[("Status", 2.into())],
            &Where::new().eq("Id", 1),
        )
        .unwrap();
        assert_eq!(
            sql,
            "UPDATE \"DH_Order_202609\" SET \"Status\" = ? WHERE (\"Id\" = ?)"
        );

        let (sql, _) = insert_sql_named(
            DatabaseKind::Sqlite,
            &t,
            "DH_Order_202609",
            &[("Code", "A".into())],
        )
        .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"DH_Order_202609\" (\"Code\") VALUES (?)"
        );
    }
}
