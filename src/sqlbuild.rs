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

    let mut columns_sql = String::with_capacity(fields.len() * 16);
    let mut marks_sql = String::with_capacity(fields.len() * 4);
    let mut params = Vec::with_capacity(fields.len());
    let mut first = true;

    for (index, (field, value)) in fields.iter().enumerate() {
        let name = column_name(table, field)?;
        if !first {
            columns_sql.push_str(", ");
            marks_sql.push_str(", ");
        }
        first = false;
        kind.push_quoted(&mut columns_sql, name);
        kind.push_placeholder(&mut marks_sql, index);
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
        if !first {
            columns_sql.push_str(", ");
            marks_sql.push_str(", ");
        }
        kind.push_quoted(&mut columns_sql, table.effective_column_name(identity));
        marks_sql.push_str(&expression);
    }

    let mut sql =
        String::with_capacity(20 + table_name.len() + columns_sql.len() + marks_sql.len());
    sql.push_str("INSERT INTO ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" (");
    sql.push_str(&columns_sql);
    sql.push_str(") VALUES (");
    sql.push_str(&marks_sql);
    sql.push(')');
    Ok((sql, params))
}

/// 组装 INSERT 语句（列集合已拥有：值直接移入参数，热路径少一次克隆）。
pub fn insert_sql_owned_named<S: AsRef<str>>(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    fields: Vec<(S, DbValue)>,
) -> Result<(String, Vec<DbValue>)> {
    if fields.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的插入语句至少需要一个字段",
            table.name
        )));
    }

    // InfluxDB：直接生成行协议文本（非 SQL），写入由驱动 POST 到 /write
    if kind == DatabaseKind::InfluxDb {
        let borrowed: Vec<(&str, DbValue)> = fields
            .iter()
            .map(|(name, value)| (name.as_ref(), value.clone()))
            .collect();
        return Ok((influx_line_protocol(table, table_name, &borrowed)?, Vec::new()));
    }

    // 序列型自增（Oracle/DB2/Firebird，XCode 约定 SEQ_{表名}）：未显式提供自增列时补序列表达式
    let identity_name = table.identity().map(|c| c.name.clone());
    let add_sequence = matches!(
        kind,
        DatabaseKind::Oracle | DatabaseKind::Db2 | DatabaseKind::Firebird
    ) && identity_name
        .as_ref()
        .is_some_and(|name| !fields.iter().any(|(field, _)| field.as_ref().eq_ignore_ascii_case(name)));

    let mut columns_sql = String::with_capacity(fields.len() * 16);
    let mut marks_sql = String::with_capacity(fields.len() * 4);
    let mut params = Vec::with_capacity(fields.len());
    let mut first = true;

    for (index, (field, value)) in fields.into_iter().enumerate() {
        let name = column_name(table, field.as_ref())?;
        if !first {
            columns_sql.push_str(", ");
            marks_sql.push_str(", ");
        }
        first = false;
        kind.push_quoted(&mut columns_sql, name);
        kind.push_placeholder(&mut marks_sql, index);
        params.push(value);
    }

    if add_sequence && let Some(identity) = table.identity() {
        let sequence = oracle_identity_sequence(table_name);
        let expression = match kind {
            // Oracle："SEQ_x".NEXTVAL（引号保持大小写）
            DatabaseKind::Oracle => format!("{}.NEXTVAL", kind.quote(&sequence)),
            // DB2（Oracle 兼容模式）：与 DH.NCode 一致使用未引号 SEQ_表名.nextval
            DatabaseKind::Db2 => format!("{sequence}.nextval"),
            // Firebird：next value for "SEQ_x"
            _ => format!("next value for {}", kind.quote(&sequence)),
        };
        if !first {
            columns_sql.push_str(", ");
            marks_sql.push_str(", ");
        }
        kind.push_quoted(&mut columns_sql, table.effective_column_name(identity));
        marks_sql.push_str(&expression);
    }

    let mut sql =
        String::with_capacity(20 + table_name.len() + columns_sql.len() + marks_sql.len());
    sql.push_str("INSERT INTO ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" (");
    sql.push_str(&columns_sql);
    sql.push_str(") VALUES (");
    sql.push_str(&marks_sql);
    sql.push(')');
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
    multi_write_impl(kind, table, table_name, columns, rows, mode, None, None, &[])
}

/// 组装多行 Upsert，并**指定冲突列**（通常是某个唯一索引列，如业务主键 `DeviceId`）。
///
/// 与 [`multi_write_sql_named`] 的 `Upsert` 区别：
/// - SQLite / DuckDB / PostgreSQL：`ON CONFLICT (指定列) DO UPDATE`（不看表主键）；
/// - MySQL：仍为 `ON DUPLICATE KEY UPDATE`（会命中外键的任意唯一键，包括指定列）；
/// - 用途：心跳/状态表的「一台设备一行」批量 upsert（外部唯一键不是自增主键）。
///
/// `update_only` 同 [`multi_write_sql_upsert_named`]；传 `None` = 更新除主键/自增列外的全部插入列。
pub fn multi_write_sql_upsert_by_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    columns: &[&str],
    conflict_columns: &[&str],
    increment_columns: &[&str],
    rows: usize,
    update_only: Option<&[&str]>,
) -> Result<String> {
    if conflict_columns.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的 Upsert 必须指定冲突列",
            table.name
        )));
    }
    multi_write_impl(
        kind,
        table,
        table_name,
        columns,
        rows,
        BatchWriteMode::Upsert,
        update_only,
        Some(conflict_columns),
        increment_columns,
    )
}

/// 组装多行 Upsert 语句，并**指定冲突时只更新哪些列**（脏列追踪：只写变化列）。
///
/// `update_only` 为 `None` 等价于 [`multi_write_sql_named`] 的 `Upsert`（更新列 = 插入列排除主键与自增列）；
/// 为 `Some([..])` 时只会更新其中**同时出现在插入列里且不是主键/自增列**的列
/// （插入路径仍写全列，因为新行必须完整）。
///
/// 用途：`EntityQueue` 把「脏列集合相同的行」合并成一条多行 Upsert ——
/// 既保留「只写变化列」的语义，又拿到批量的性能（逐条 `find + update` 每行要 2 次往返）。
pub fn multi_write_sql_upsert_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    columns: &[&str],
    rows: usize,
    update_only: Option<&[&str]>,
) -> Result<String> {
    multi_write_impl(
        kind,
        table,
        table_name,
        columns,
        rows,
        BatchWriteMode::Upsert,
        update_only,
        None,
        &[],
    )
}

fn multi_write_impl(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    columns: &[&str],
    rows: usize,
    mode: BatchWriteMode,
    update_only: Option<&[&str]>,
    conflict_columns: Option<&[&str]>,
    increment_columns: &[&str],
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

    // 校验列名并解析物理名（不分配；后续直接按方言引用写入 SQL）
    let mut resolved: Vec<&str> = Vec::with_capacity(columns.len());
    for field in columns {
        resolved.push(column_name(table, field)?);
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
            // 更新列 = 插入列中排除主键与自增列（对齐 C# `On Conflict Do Update` 的字段过滤）；
            // 脏列追踪时再收窄到「本批的更新列集合」（`update_only`）
            let update_fields: Vec<&str> = columns
                .iter()
                .copied()
                .filter(|field| {
                    !pks.iter().any(|pk| pk.name.eq_ignore_ascii_case(field))
                        && !table
                            .identity()
                            .is_some_and(|id| id.name.eq_ignore_ascii_case(field))
                        && update_only.is_none_or(|only| {
                            only.iter().any(|col| col.eq_ignore_ascii_case(field))
                        })
                })
                .collect();

            match kind {
                DatabaseKind::Sqlite | DatabaseKind::DuckDb | DatabaseKind::PostgreSql => {
                    // 冲突列：显式指定优先（外部唯一键 upsert），否则用模型主键
                    let target: Vec<String> = match conflict_columns {
                        Some(cols) if !cols.is_empty() => cols
                            .iter()
                            .map(|c| Ok(kind.quote(column_name(table, c)?)))
                            .collect::<Result<_>>()?,
                        _ => pks
                            .iter()
                            .map(|pk| kind.quote(table.effective_column_name(pk)))
                            .collect(),
                    };
                    let tail = if update_fields.is_empty() {
                        format!(" ON CONFLICT ({}) DO NOTHING", target.join(", "))
                    } else {
                        let sets: Vec<String> = update_fields
                            .iter()
                            .map(|field| {
                                let col = kind.quote(column_name(table, field)?);
                                // 累计列（如 Pings）：冲突时在旧值上累加新增量，而不是覆盖
                                if increment_columns
                                    .iter()
                                    .any(|c| c.eq_ignore_ascii_case(field))
                                {
                                    Ok(format!("{col}={col}+excluded.{col}"))
                                } else {
                                    Ok(format!("{col}=excluded.{col}"))
                                }
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
                                if increment_columns
                                    .iter()
                                    .any(|c| c.eq_ignore_ascii_case(field))
                                {
                                    Ok(format!("{col}={col}+VALUES({col})"))
                                } else {
                                    Ok(format!("{col}=VALUES({col})"))
                                }
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

    // 零中间分配拼接：直接写入预分配缓冲（每行不再建 Vec/format 字符串）
    // 单个占位符最长约 8 字符（`@p123456` / `$123456`），其余方言 1 字符
    let slot = match kind {
        DatabaseKind::PostgreSql | DatabaseKind::Oracle | DatabaseKind::SqlServer => 8,
        _ => 1,
    };
    let mut sql = String::with_capacity(
        32 + resolved.len() * 16 + rows * (resolved.len() * (slot + 2) + 4) + tail.len(),
    );
    sql.push_str(head);
    sql.push(' ');
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" (");
    for (position, name) in resolved.iter().enumerate() {
        if position > 0 {
            sql.push_str(", ");
        }
        kind.push_quoted(&mut sql, name);
    }
    sql.push_str(") VALUES ");
    let mut index = 0usize;
    for row in 0..rows {
        if row > 0 {
            sql.push_str(", ");
        }
        sql.push('(');
        for column in 0..resolved.len() {
            if column > 0 {
                sql.push_str(", ");
            }
            kind.push_placeholder(&mut sql, index);
            index += 1;
        }
        sql.push(')');
    }
    sql.push_str(&tail);
    Ok(sql)
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
    let mut sql = String::with_capacity(24 + table_name.len() + sets.len() * 16);
    sql.push_str("UPDATE ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" SET ");
    for (position, (field, value)) in sets.iter().enumerate() {
        if position > 0 {
            sql.push_str(", ");
        }
        kind.push_quoted(&mut sql, column_name(table, field)?);
        sql.push_str(" = ");
        kind.push_placeholder(&mut sql, params.len());
        params.push(value.clone());
    }
    if !filter.is_empty() {
        sql.push_str(" WHERE ");
        filter.render_into(kind, &mut params, &mut sql);
    }

    Ok((sql, params))
}

/// 组装 UPDATE 语句（列集合已拥有：值直接移入参数，热路径少一次克隆）。
pub fn update_sql_owned_named<S: AsRef<str>>(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    sets: Vec<(S, DbValue)>,
    filter: &Where,
) -> Result<(String, Vec<DbValue>)> {
    if sets.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的更新语句至少需要一个待更新字段",
            table.name
        )));
    }

    let mut params = Vec::with_capacity(sets.len() + 4);
    // 直接拼接 SET（不建 String 列表、不 format!）：每列 0 次中间分配
    let mut sql = String::with_capacity(24 + table_name.len() + sets.len() * 16);
    sql.push_str("UPDATE ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" SET ");
    for (position, (field, value)) in sets.into_iter().enumerate() {
        if position > 0 {
            sql.push_str(", ");
        }
        kind.push_quoted(&mut sql, column_name(table, field.as_ref())?);
        sql.push_str(" = ");
        kind.push_placeholder(&mut sql, params.len());
        params.push(value);
    }
    if !filter.is_empty() {
        sql.push_str(" WHERE ");
        filter.render_into(kind, &mut params, &mut sql);
    }

    Ok((sql, params))
}

/// 组装**批量按主键更新**语句：一条 `UPDATE` 更新多行。
///
/// 形态：`UPDATE t SET c1 = CASE pk WHEN ? THEN ? ... END, ... WHERE pk IN (?, ? ...)`。
///
/// 用途：心跳/状态类高频写入的批量落库——把 N 条 `UPDATE` 合成 1 条语句，
/// 在 MySQL/PostgreSQL 这类网络库上把往返次数从 N 降到 1（往返是主要成本）。
///
/// 约束：
/// - 只支持**单列主键**（复合主键请改用逐行版本）；
/// - `columns` 为共用列集，`rows` 每行的值个数必须等于 `columns.len()`；
/// - 不经过拦截器（批量路径由调用方给全列，例如 `UpdateTime`）。
pub fn update_batch_by_pk_sql_named<S: AsRef<str>>(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    columns: &[S],
    pk_name: &str,
    rows: &[(DbValue, Vec<DbValue>)],
) -> Result<(String, Vec<DbValue>)> {
    if columns.is_empty() || rows.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的批量更新至少需要一个列与一行",
            table.name
        )));
    }
    let pk = column_name(table, pk_name)?;
    // 列名先全部校验，避免拼到一半才发现列不存在
    let mut names: Vec<&str> = Vec::with_capacity(columns.len());
    for field in columns {
        names.push(column_name(table, field.as_ref())?);
    }
    for (index, (_, values)) in rows.iter().enumerate() {
        if values.len() != names.len() {
            return Err(Error::Model(format!(
                "表 {} 批量更新第 {} 行的值个数 {} 与列数 {} 不一致",
                table.name,
                index + 1,
                values.len(),
                names.len()
            )));
        }
    }

    // 参数：每列每行 2 个（pk、值）+ WHERE IN 的 pk
    let mut params: Vec<DbValue> = Vec::with_capacity(rows.len() * names.len() * 2 + rows.len());
    let mut sql = String::with_capacity(32 + table_name.len() + names.len() * rows.len() * 24);
    sql.push_str("UPDATE ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" SET ");
    for (col_index, name) in names.iter().enumerate() {
        if col_index > 0 {
            sql.push_str(", ");
        }
        kind.push_quoted(&mut sql, name);
        sql.push_str(" = CASE ");
        kind.push_quoted(&mut sql, pk);
        for (pk_value, values) in rows {
            sql.push_str(" WHEN ");
            kind.push_placeholder(&mut sql, params.len());
            params.push(pk_value.clone());
            sql.push_str(" THEN ");
            kind.push_placeholder(&mut sql, params.len());
            params.push(values[col_index].clone());
        }
        sql.push_str(" END");
    }
    sql.push_str(" WHERE ");
    kind.push_quoted(&mut sql, pk);
    sql.push_str(" IN (");
    for (index, (pk_value, _)) in rows.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        kind.push_placeholder(&mut sql, params.len());
        params.push(pk_value.clone());
    }
    sql.push(')');

    Ok((sql, params))
}

/// 组装**按主键 UPDATE**（可选 CAS 期望条件）：主键条件直接拼接，**不构造 `Where`**。
///
/// - 比 [`update_sql_owned_named`] 少 3~4 次分配（`pk_filter` 的 `Cond` / 列名 `String` / 值 `Vec`）；
/// - `pk_names` / `pk_values` 长度必须一致（由调用方按模型主键提供）；
/// - `expected` 非空时追加 `AND 列 = 旧值`（CAS），列名经 [`column_name`] 校验并换成模型列名；
/// - 主键/期望列固定时 SQL 文本稳定，可命中语句缓存。
pub fn update_by_pk_sql_owned_named<S: AsRef<str>>(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    sets: Vec<(S, DbValue)>,
    pk_names: &[&str],
    pk_values: &[DbValue],
    expected: &[(&str, DbValue)],
) -> Result<(String, Vec<DbValue>)> {
    if sets.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的更新语句至少需要一个待更新字段",
            table.name
        )));
    }
    if pk_names.is_empty() || pk_names.len() != pk_values.len() {
        return Err(Error::Model(format!(
            "表 {} 主键列与值数量不匹配（列 {} 个，值 {} 个）",
            table.name,
            pk_names.len(),
            pk_values.len()
        )));
    }

    let mut params = Vec::with_capacity(sets.len() + pk_names.len() + expected.len() + 4);
    let mut set_sql = String::with_capacity(sets.len() * 16);
    let mut first = true;
    for (field, value) in sets {
        let name = column_name(table, field.as_ref())?;
        if !first {
            set_sql.push_str(", ");
        }
        first = false;
        kind.push_quoted(&mut set_sql, name);
        set_sql.push_str(" = ");
        kind.push_placeholder(&mut set_sql, params.len());
        params.push(value);
    }

    let mut where_sql =
        String::with_capacity(pk_names.len() * 16 + expected.len() * 18);
    for (index, (name, value)) in pk_names.iter().zip(pk_values.iter()).enumerate() {
        if index > 0 {
            where_sql.push_str(" AND ");
        }
        kind.push_quoted(&mut where_sql, name);
        where_sql.push_str(" = ");
        kind.push_placeholder(&mut where_sql, params.len());
        params.push(value.clone());
    }
    for (column, value) in expected {
        let name = column_name(table, column)?;
        where_sql.push_str(" AND ");
        kind.push_quoted(&mut where_sql, name);
        where_sql.push_str(" = ");
        kind.push_placeholder(&mut where_sql, params.len());
        params.push(value.clone());
    }

    let mut sql = String::with_capacity(16 + table_name.len() + set_sql.len() + where_sql.len());
    sql.push_str("UPDATE ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" SET ");
    sql.push_str(&set_sql);
    sql.push_str(" WHERE ");
    sql.push_str(&where_sql);

    Ok((sql, params))
}

/// 组装**按主键 UPDATE**（借用列集版本，供无拦截器快路使用）：
/// 与 [`update_by_pk_sql_owned_named`] 逐字节等价（单测护栏），差异仅为列集合是借用。
pub fn update_by_pk_sql_named(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    sets: &[(&str, DbValue)],
    pk_names: &[&str],
    pk_values: &[DbValue],
    expected: &[(&str, DbValue)],
) -> Result<(String, Vec<DbValue>)> {
    if sets.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的更新语句至少需要一个待更新字段",
            table.name
        )));
    }
    if pk_names.is_empty() || pk_names.len() != pk_values.len() {
        return Err(Error::Model(format!(
            "表 {} 主键列与值数量不匹配（列 {} 个，值 {} 个）",
            table.name,
            pk_names.len(),
            pk_values.len()
        )));
    }

    let mut params = Vec::with_capacity(sets.len() + pk_names.len() + expected.len() + 4);
    let mut set_sql = String::with_capacity(sets.len() * 16);
    let mut first = true;
    for (field, value) in sets {
        let name = column_name(table, field)?;
        if !first {
            set_sql.push_str(", ");
        }
        first = false;
        kind.push_quoted(&mut set_sql, name);
        set_sql.push_str(" = ");
        kind.push_placeholder(&mut set_sql, params.len());
        params.push(value.clone());
    }

    let mut where_sql =
        String::with_capacity(pk_names.len() * 16 + expected.len() * 18);
    for (index, (name, value)) in pk_names.iter().zip(pk_values.iter()).enumerate() {
        if index > 0 {
            where_sql.push_str(" AND ");
        }
        kind.push_quoted(&mut where_sql, name);
        where_sql.push_str(" = ");
        kind.push_placeholder(&mut where_sql, params.len());
        params.push(value.clone());
    }
    for (column, value) in expected {
        let name = column_name(table, column)?;
        where_sql.push_str(" AND ");
        kind.push_quoted(&mut where_sql, name);
        where_sql.push_str(" = ");
        kind.push_placeholder(&mut where_sql, params.len());
        params.push(value.clone());
    }

    let mut sql = String::with_capacity(16 + table_name.len() + set_sql.len() + where_sql.len());
    sql.push_str("UPDATE ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" SET ");
    sql.push_str(&set_sql);
    sql.push_str(" WHERE ");
    sql.push_str(&where_sql);

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
    let mut sql = String::with_capacity(24 + table_name.len());
    sql.push_str("DELETE FROM ");
    kind.push_quoted(&mut sql, table_name);
    if !filter.is_empty() {
        sql.push_str(" WHERE ");
        filter.render_into(kind, &mut params, &mut sql);
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
    let mut sql = String::with_capacity(32 + table_name.len());
    sql.push_str("SELECT COUNT(*) FROM ");
    kind.push_quoted(&mut sql, table_name);
    if let Some(filter) = filter
        && !filter.is_empty()
    {
        sql.push_str(" WHERE ");
        filter.render_into(kind, &mut params, &mut sql);
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
    // 查询列（直接写入缓冲：不建 `columns` 中的 String、不 join）
    let mut sql = String::with_capacity(64 + table.columns.len() * 8);
    sql.push_str("SELECT ");
    if query.select.is_empty() {
        sql.push('*');
    } else {
        for (position, column) in query.select.iter().enumerate() {
            if position > 0 {
                sql.push_str(", ");
            }
            match table.column(column) {
                Some(col) => kind.push_quoted(&mut sql, table.effective_column_name(col)),
                None => sql.push_str(column), // 原始表达式（函数、别名等）
            }
        }
    }
    sql.push_str(" FROM ");
    kind.push_quoted(&mut sql, table_name);

    let mut params = Vec::new();
    if let Some(filter) = &query.filter
        && !filter.is_empty()
    {
        sql.push_str(" WHERE ");
        filter.render_into(kind, &mut params, &mut sql);
    }

    // 排序
    let mut order_sql = render_order(kind, table, &query.order_by);

    // 分页/取前 N 条场景下，未显式排序时按主键兜底，保证结果稳定
    let paging = query.page_index >= 1 && query.page_size > 0;
    if order_sql.is_empty()
        && (paging || query.limit.is_some() || query.offset.is_some())
        && !table.primary_keys().is_empty()
    {
        order_sql = render_order_of(kind, table, &primary_key_names(table));
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

/// 渲染 `ORDER BY` 子句（空列表返回空串；列名直接写入缓冲，不建中间 String 列表）。
fn render_order(kind: DatabaseKind, table: &TableMeta, orders: &[OrderBy]) -> String {
    if orders.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(16 + orders.len() * 20);
    out.push_str("ORDER BY ");
    for (position, order) in orders.iter().enumerate() {
        if position > 0 {
            out.push_str(", ");
        }
        match table.column(&order.column) {
            Some(col) => kind.push_quoted(&mut out, table.effective_column_name(col)),
            None => out.push_str(&order.column),
        }
        if order.desc {
            out.push_str(" DESC");
        }
    }
    out
}

/// 按物理列名渲染升序 `ORDER BY`（不再克隆列名）。
fn render_order_of(kind: DatabaseKind, table: &TableMeta, columns: &[&str]) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(16 + columns.len() * 20);
    out.push_str("ORDER BY ");
    for (position, column) in columns.iter().enumerate() {
        if position > 0 {
            out.push_str(", ");
        }
        match table.column(column) {
            Some(col) => kind.push_quoted(&mut out, table.effective_column_name(col)),
            None => out.push_str(column),
        }
    }
    out
}

/// 主键物理列名（不分配 String）。
fn primary_key_names(table: &TableMeta) -> Vec<&str> {
    table
        .columns
        .iter()
        .filter(|column| column.primary_key)
        .map(|column| table.effective_column_name(column))
        .collect()
}

/// 组装「原子累加」UPDATE 语句：`SET 列 = 列 + @n`（对应 DH.NCode 的 `AdditionalFields` 语义）。
///
/// - `deltas`：累加列与增量（**列必须已存在**，类型由数据库隐式提升；文本列会报错，属预期）；
/// - `assigns`：普通赋值列（如拦截器补的 `UpdateTime`）；
/// - `filter`：条件（为空即全表，慎用；调用方通常传主键）。
///
/// 与 [`update_sql_owned_named`] 的区别：累加写成 `列 = 列 + 增量`，
/// 由数据库在**一条语句内**完成读-加-写，天然并发安全（不会丢更新）。
pub fn increment_sql_owned_named<S: AsRef<str>>(
    kind: DatabaseKind,
    table: &TableMeta,
    table_name: &str,
    deltas: Vec<(S, DbValue)>,
    assigns: Vec<(S, DbValue)>,
    filter: &Where,
) -> Result<(String, Vec<DbValue>)> {
    if deltas.is_empty() {
        return Err(Error::Model(format!(
            "表 {} 的累加语句至少需要一个累加字段",
            table.name
        )));
    }

    let mut params = Vec::with_capacity(deltas.len() + assigns.len() + 4);
    let mut set_sql = String::with_capacity((deltas.len() + assigns.len()) * 20);
    let mut first = true;

    for (field, value) in deltas {
        let name = column_name(table, field.as_ref())?;
        if !first {
            set_sql.push_str(", ");
        }
        first = false;
        kind.push_quoted(&mut set_sql, name);
        set_sql.push_str(" = ");
        kind.push_quoted(&mut set_sql, name);
        set_sql.push_str(" + ");
        kind.push_placeholder(&mut set_sql, params.len());
        params.push(value);
    }
    for (field, value) in assigns {
        let name = column_name(table, field.as_ref())?;
        if !first {
            set_sql.push_str(", ");
        }
        first = false;
        kind.push_quoted(&mut set_sql, name);
        set_sql.push_str(" = ");
        kind.push_placeholder(&mut set_sql, params.len());
        params.push(value);
    }

    let mut sql =
        String::with_capacity(16 + table_name.len() + set_sql.len() + 48);
    sql.push_str("UPDATE ");
    kind.push_quoted(&mut sql, table_name);
    sql.push_str(" SET ");
    sql.push_str(&set_sql);
    if !filter.is_empty() {
        sql.push_str(" WHERE ");
        filter.render_into(kind, &mut params, &mut sql);
    }

    Ok((sql, params))
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

