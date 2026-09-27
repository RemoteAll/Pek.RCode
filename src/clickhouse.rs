//! ClickHouse 驱动：通过 HTTP 接口（默认 8123 端口）执行 SQL。
//!
//! 对应 DH.NCode 的 `ClickHouse.cs`：
//! - 无自增主键（`InsertAndGetIdentity` 返回 0）
//! - 分页 `LIMIT n OFFSET m`
//! - 反引号引用标识符；布尔用 1/0；时间文本 `yyyy-MM-dd HH:mm:ss.fffffff`
//!
//! 实现要点：
//! - SQL 经 HTTP POST 发送（`?database=` 指定库），查询使用
//!   `FORMAT TSVWithNamesAndTypes` 回读列名与类型，再按类型转换为 [`DbValue`]
//! - HTTP 接口无绑定参数协议，参数由 [`crate::http::inline_params`] 内联为字面量
//!   （值全部来自 `DbValue`，并严格转义）
//! - ClickHouse 无事务：`begin/commit/rollback` 为空操作
//!
//! 建表说明：方言会在 `CREATE TABLE` 末尾补 `ENGINE = MergeTree() ORDER BY tuple()`
//! （ClickHouse 建表必须有引擎）。

use std::time::Duration;

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::http::{LiteralStyle, inline_params, post_text};
use crate::session::{RowSet, SqlSession};
use crate::value::{DbValue, parse_datetime};

/// ClickHouse 会话。
pub struct ClickHouseSession {
    /// 连接设置
    settings: ClickHouseSettings,
}

/// 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct ClickHouseSettings {
    /// 主机
    host: String,
    /// HTTP 端口（默认 8123）
    port: u16,
    /// 数据库（可空，缺省用服务端默认）
    database: Option<String>,
    /// 用户名（默认 default）
    user: String,
    /// 密码
    password: String,
    /// 请求超时
    timeout: Duration,
}

impl ClickHouseSession {
    /// 根据 XCode 风格连接串打开连接（HTTP 无状态，连接设置即全部状态）。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        Ok(Self {
            settings: parse_settings(conn_str)?,
        })
    }

    /// 执行 SQL 并返回响应文本。
    fn request(&self, sql: &str) -> Result<String> {
        let url = match &self.settings.database {
            Some(db) => format!(
                "http://{}:{}/?database={}",
                self.settings.host, self.settings.port, db
            ),
            None => format!("http://{}:{}/", self.settings.host, self.settings.port),
        };
        post_text(
            &url,
            sql,
            Some((&self.settings.user, &self.settings.password)),
            self.settings.timeout,
        )
    }
}

/// 解析连接串（与 XCode 键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<ClickHouseSettings> {
    let host = cs
        .get("server")
        .or(cs.get("host"))
        .or(cs.get("data source"))
        .unwrap_or("127.0.0.1")
        .to_string();
    let port = match cs.get("port") {
        Some(v) => v
            .parse::<u16>()
            .map_err(|e| Error::Model(format!("无效的 Port \"{v}\"：{e}")))?,
        None => 8123,
    };
    let database = cs
        .get("database")
        .or(cs.get("db"))
        .or(cs.get("initial catalog"))
        .map(str::to_string);
    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("username"))
        .unwrap_or("default")
        .to_string();
    let password = cs
        .get("pwd")
        .or(cs.get("password"))
        .or(cs.get("passwd"))
        .unwrap_or("")
        .to_string();
    let timeout = match cs.get("timeout") {
        Some(v) => Duration::from_secs(
            v.parse::<u64>()
                .map_err(|e| Error::Model(format!("无效的 Timeout \"{v}\"：{e}")))?,
        ),
        None => Duration::from_secs(30),
    };
    Ok(ClickHouseSettings {
        host,
        port,
        database,
        user,
        password,
        timeout,
    })
}

/// TSV 解析结果：列名、类型、数据行。
type TsvTable = (Vec<String>, Vec<String>, Vec<Vec<DbValue>>);

/// 解析 `TSVWithNamesAndTypes` 输出：第 1 行列名，第 2 行类型，其后数据行。
fn parse_tsv(text: &str) -> Result<TsvTable> {
    let mut lines = text.lines();
    let names: Vec<String> = lines
        .next()
        .unwrap_or_default()
        .split('\t')
        .map(clean_tsv_name)
        .collect();
    let types: Vec<String> = lines
        .next()
        .unwrap_or_default()
        .split('\t')
        .map(str::to_string)
        .collect();

    let mut rows = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let cells: Vec<&str> = line.split('\t').collect();
        let mut values = Vec::with_capacity(cells.len());
        for (index, cell) in cells.iter().enumerate() {
            let ty = types.get(index).map(String::as_str).unwrap_or("");
            values.push(tsv_cell_to_dbvalue(cell, ty));
        }
        rows.push(values);
    }
    Ok((names, types, rows))
}

/// TSV 单元格（含转移义）→ `DbValue`。
fn tsv_cell_to_dbvalue(cell: &str, ty: &str) -> DbValue {
    // ClickHouse TSV：\N 表示 NULL，反斜杠转义
    if cell == "\\N" {
        return DbValue::Null;
    }
    let text = unescape_tsv(cell);

    if ty.starts_with("UInt") || ty.starts_with("Int") {
        if let Ok(v) = text.parse::<i64>() {
            return DbValue::Int(v);
        }
        if let Ok(v) = text.parse::<u64>() {
            return DbValue::Int(v as i64);
        }
        return DbValue::Text(text);
    }
    if ty.starts_with("Float") {
        return text.parse::<f64>().map(DbValue::Float).unwrap_or(DbValue::Text(text));
    }
    if ty.starts_with("Decimal") {
        return text
            .parse::<rust_decimal::Decimal>()
            .map(DbValue::Decimal)
            .unwrap_or(DbValue::Text(text));
    }
    if ty.starts_with("DateTime") || ty.starts_with("Date") {
        // 形如 2026-09-27 10:30:00.123（可能带时区后缀）
        let trimmed = text.split('.').next().map(str::to_string);
        if let Some(dt) = parse_datetime(&text).or_else(|| trimmed.and_then(|t| parse_datetime(&t))) {
            return DbValue::DateTime(dt);
        }
        return DbValue::Text(text);
    }
    if ty == "UUID" || ty.starts_with("IPv") || ty.starts_with("Enum") {
        return DbValue::Text(text);
    }
    // String / FixedString / Array / 其它：文本；含非法 UTF-8 转义时按二进制保留
    DbValue::Text(text)
}

/// 清洗 TSV 表头列名（ClickHouse 输出形如 `"Id"`，含转义与双写引号）。
fn clean_tsv_name(raw: &str) -> String {
    let text = unescape_tsv(raw);
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].replace("\"\"", "\"")
    } else {
        text
    }
}

/// 反转义 ClickHouse TSV 单元（`\\`、`\t`、`\n`、`\r`、`\'` 等）。
fn unescape_tsv(cell: &str) -> String {
    if !cell.contains('\\') {
        return cell.to_string();
    }
    let mut out = String::with_capacity(cell.len());
    let mut chars = cell.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 驱动错误 → 统一错误。
fn map_err(context: &str, e: Error) -> Error {
    Error::Db(format!("ClickHouse {context}：{e}"))
}

impl SqlSession for ClickHouseSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::ClickHouse
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let sql = inline_params(sql, params, LiteralStyle::ClickHouse)?;
        self.request(&sql).map_err(|e| map_err("执行失败", e))?;
        // ClickHouse 不返回受影响行数
        Ok(0)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let sql = inline_params(sql, params, LiteralStyle::ClickHouse)?;
        // 附加 FORMAT 以获得列名与类型（语句自带 FORMAT 时不重复）
        let sql = if sql.to_ascii_uppercase().contains(" FORMAT ") {
            sql
        } else {
            format!("{sql} FORMAT TSVWithNamesAndTypes")
        };
        let text = self.request(&sql).map_err(|e| map_err("查询失败", e))?;

        let (names, _types, rows) = parse_tsv(&text)?;
        let mut set = RowSet::new(names);
        for row in rows {
            set.push(row);
        }
        Ok(set)
    }

    fn begin(&mut self) -> Result<()> {
        // ClickHouse 无事务
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        Ok(())
    }

    fn last_identity(&mut self) -> Result<i64> {
        // ClickHouse 不支持自增主键（与 DH.NCode 一致）
        Ok(0)
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let set = self.query(
            "SELECT count() FROM system.tables WHERE database = currentDatabase() AND name = ?",
            &[DbValue::Text(table.to_string())],
        )?;
        Ok(set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0)
            > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let set = self.query(
            "SELECT name FROM system.columns WHERE database = currentDatabase() AND table = ? ORDER BY position",
            &[DbValue::Text(table.to_string())],
        )?;
        Ok(set
            .rows
            .iter()
            .filter_map(|row| row.get(0).map(DbValue::to_text))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parsing() {
        let cs = ConnectionString::parse(
            "Server=ch.local;Port=9000;Database=metrics;Uid=app;Pwd=***;provider=clickhouse",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "ch.local");
        assert_eq!(s.port, 9000);
        assert_eq!(s.database.as_deref(), Some("metrics"));
        assert_eq!(s.user, "app");

        let cs = ConnectionString::parse("provider=clickhouse");
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.port, 8123);
        assert_eq!(s.user, "default");
    }

    #[test]
    fn tsv_parsing() {
        let text = "\"Id\"\t\"Code\"\t\"Amount\"\nInt32\tString\tDecimal(18, 4)\n1\tA-001\t12.3400\n\\N\tit\\'s\t-1.5000\n";
        let (names, types, rows) = parse_tsv(text).unwrap();
        assert_eq!(names, vec!["Id", "Code", "Amount"]);
        assert_eq!(types.len(), 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], DbValue::Int(1));
        assert_eq!(rows[0][1], DbValue::Text("A-001".into()));
        assert_eq!(
            rows[0][2],
            DbValue::Decimal("12.3400".parse::<rust_decimal::Decimal>().unwrap())
        );
        assert!(rows[1][0].is_null());
        assert_eq!(rows[1][1], DbValue::Text("it's".into()));
        assert_eq!(
            rows[1][2],
            DbValue::Decimal("-1.5000".parse::<rust_decimal::Decimal>().unwrap())
        );
    }

    #[test]
    fn tsv_datetime_conversion() {
        let value = tsv_cell_to_dbvalue("2026-09-27 10:30:00.123", "DateTime64(6)");
        assert!(value.as_datetime().is_some(), "{value:?}");
    }
}
