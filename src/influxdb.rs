//! InfluxDB 驱动：HTTP 接口（1.x 语义：行协议写入 + InfluxQL 查询）。
//!
//! 对应 DH.NCode 的 `InfluxDB.cs`：
//! - 时序库，无自增主键（Identity 回写为 0）、无建表 DDL（measurement 写入时自动创建）
//! - 写入使用**行协议**（measurement/tag/field/timestamp），主键/主列作为 tag、
//!   其余作为 field、`Time/CreateTime/UpdateTime` 列作为时间戳（与 DH.NCode 的批量写入规则一致）
//!
//! 实现要点：
//! - `INSERT` 由 [`crate::sqlbuild`] 直接生成行协议文本，会话把“非 SQL 关键字开头”的语句
//!   视为行协议并 POST 到 `/write`；其余语句（SELECT/DELETE/SHOW）以 InfluxQL 发到 `/query`
//! - 无绑定参数协议，参数由 [`crate::http::inline_params`] 内联为字面量（严格转义）
//! - UPDATE 不受支持（InfluxDB 数据模型所致），会返回可操作的错误
//! - 无事务：`begin/commit/rollback` 为空操作
//!
//! 查询说明：InfluxQL 的 `ORDER BY` 仅支持 `time`，驱动会把其它排序列改写为 `time`
//! （保持时间序语义，与 XCode 分页兜底排序兼容）。

use std::time::Duration;

use serde_json::Value as Json;

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::http::{LiteralStyle, inline_params, post_text};
use crate::session::{RowSet, SqlSession};
use crate::value::{DbValue, parse_datetime};

/// InfluxDB 会话。
pub struct InfluxDbSession {
    /// 连接设置
    settings: InfluxDbSettings,
}

/// 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct InfluxDbSettings {
    /// 主机
    host: String,
    /// HTTP 端口（默认 8086）
    port: u16,
    /// 数据库（必填；2.x 请填 bucket 名并自行确认兼容性）
    database: String,
    /// 用户名（可空）
    user: Option<String>,
    /// 密码（可空）
    password: Option<String>,
    /// 请求超时
    timeout: Duration,
}

impl InfluxDbSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        Ok(Self {
            settings: parse_settings(conn_str)?,
        })
    }

    /// Basic 认证信息。
    fn auth(&self) -> Option<(&str, &str)> {
        match (&self.settings.user, &self.settings.password) {
            (Some(user), Some(password)) => Some((user.as_str(), password.as_str())),
            _ => None,
        }
    }

    /// 写入行协议（`/write`）。
    fn write(&self, body: &str) -> Result<()> {
        let url = format!(
            "http://{}:{}/write?db={}",
            self.settings.host, self.settings.port, self.settings.database
        );
        post_text(&url, body, self.auth(), self.settings.timeout)?;
        Ok(())
    }

    /// 执行 InfluxQL（`/query`），返回 JSON（含 error 字段时报错）。
    fn query_json(&self, sql: &str) -> Result<Json> {
        let url = format!("http://{}:{}/query", self.settings.host, self.settings.port);
        let body = post_text(&url, sql, self.auth(), self.settings.timeout)?;

        // 1.x 的 /query 默认返回 JSON；带 error 的 200 响应在这里判错
        // 若配置了非 JSON 返回格式，响应会是 CSV，这里给出明确提示
        let json: Json = serde_json::from_str(&body).map_err(|_| {
            Error::Db(format!(
                "InfluxDB 查询响应不是 JSON（请确认服务器为 1.x 或返回 JSON 格式）：{}",
                &body[..body.len().min(200)]
            ))
        })?;
        if let Some(error) = json
            .get("results")
            .and_then(Json::as_array)
            .and_then(|results| results.first())
            .and_then(|first| first.get("error"))
            .and_then(Json::as_str)
        {
            return Err(Error::Db(format!("InfluxDB 错误：{error}")));
        }
        if let Some(error) = json.get("error").and_then(Json::as_str) {
            return Err(Error::Db(format!("InfluxDB 错误：{error}")));
        }
        Ok(json)
    }
}

/// 解析连接串（与 XCode 键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<InfluxDbSettings> {
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
        None => 8086,
    };
    let database = cs
        .get("database")
        .or(cs.get("db"))
        .ok_or_else(|| Error::Model("InfluxDB 连接串缺少 Database（1.x 为 db 名，2.x 为 bucket）".into()))?
        .to_string();
    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("username"))
        .map(str::to_string);
    let password = cs
        .get("pwd")
        .or(cs.get("password"))
        .or(cs.get("passwd"))
        .map(str::to_string);
    let timeout = match cs.get("timeout") {
        Some(v) => Duration::from_secs(
            v.parse::<u64>()
                .map_err(|e| Error::Model(format!("无效的 Timeout \"{v}\"：{e}")))?,
        ),
        None => Duration::from_secs(30),
    };
    Ok(InfluxDbSettings {
        host,
        port,
        database,
        user,
        password,
        timeout,
    })
}

/// 语句是否像 SQL（而非行协议）。
fn looks_like_sql(sql: &str) -> bool {
    matches!(
        sql.split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase()
            .as_str(),
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "CREATE" | "ALTER" | "DROP" | "SHOW" | "GRANT" | "REVOKE"
    )
}

/// InfluxQL 的 `ORDER BY` 只认 `time`：把其它排序列改写为 `time`。
fn normalize_order(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(pos) = upper.find(" ORDER BY ") else {
        return sql.to_string();
    };
    let rest = &sql[pos + " ORDER BY ".len()..];
    let split = rest.to_ascii_uppercase();
    let end = split.find(" LIMIT ").unwrap_or(rest.len());
    let order = &rest[..end];
    if order.to_ascii_lowercase().contains("time") {
        return sql.to_string();
    }
    let desc = order.to_ascii_uppercase().contains("DESC");
    let replacement = if desc { "time DESC" } else { "time ASC" };
    format!(
        "{}ORDER BY {replacement}{}",
        &sql[..pos + 1],
        &rest[end..]
    )
}

/// InfluxQL 查询结果 JSON → `RowSet`。
fn json_to_rowset(json: &Json) -> Result<RowSet> {
    let series = json
        .get("results")
        .and_then(Json::as_array)
        .and_then(|results| results.first())
        .and_then(|first| first.get("series"));

    let Some(series) = series else {
        // 空结果（无 series 或 results 为空）
        return Ok(RowSet::new(Vec::new()));
    };
    let series = series
        .as_array()
        .and_then(|items| items.first())
        .ok_or_else(|| Error::Db("InfluxDB 返回了非预期的 series 结构".into()))?;

    let names: Vec<String> = series
        .get("columns")
        .and_then(Json::as_array)
        .map(|cols| {
            cols.iter()
                .map(|c| c.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();

    let mut set = RowSet::new(names);
    if let Some(rows) = series.get("values").and_then(Json::as_array) {
        for row in rows {
            let cells = row.as_array().cloned().unwrap_or_default();
            let mut values = Vec::with_capacity(cells.len());
            for (index, cell) in cells.iter().enumerate() {
                let is_time = set
                    .columns
                    .get(index)
                    .is_some_and(|name| name.eq_ignore_ascii_case("time"));
                values.push(json_cell_to_dbvalue(cell, is_time));
            }
            set.push(values);
        }
    }
    Ok(set)
}

/// 单元值 → `DbValue`（`time` 列解析为时间）。
fn json_cell_to_dbvalue(cell: &Json, is_time: bool) -> DbValue {
    match cell {
        Json::Null => DbValue::Null,
        Json::Bool(v) => DbValue::Bool(*v),
        Json::Number(v) => {
            if let Some(i) = v.as_i64() {
                DbValue::Int(i)
            } else if let Some(f) = v.as_f64() {
                DbValue::Float(f)
            } else {
                DbValue::Text(v.to_string())
            }
        }
        Json::String(text) => {
            if is_time {
                parse_datetime(text)
                    .map(DbValue::DateTime)
                    .unwrap_or_else(|| DbValue::Text(text.clone()))
            } else {
                DbValue::Text(text.clone())
            }
        }
        other => DbValue::Text(other.to_string()),
    }
}

impl SqlSession for InfluxDbSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::InfluxDb
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        if !looks_like_sql(sql) {
            // 行协议（INSERT 由 sqlbuild 直接生成）
            let body = inline_params(sql, params, LiteralStyle::Influx)?;
            self.write(&body)?;
            return Ok(1);
        }

        let statement = sql
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        match statement.as_str() {
            "INSERT" => {
                // 带 SQL 语法的 INSERT 不做翻译（请通过实体/表接口写入以生成行协议）
                Err(Error::Unsupported(
                    "InfluxDB 写入请通过实体（Entity）或表（TableRef）接口，驱动会生成行协议；\
                     原始 SQL INSERT 不被支持"
                        .into(),
                ))
            }
            "UPDATE" => Err(Error::Unsupported(
                "InfluxDB 不支持 UPDATE（时序数据模型不支持原地更新；同 measurement+tags+time 再次写入即为覆盖）"
                    .into(),
            )),
            _ => {
                let sql = inline_params(sql, params, LiteralStyle::Influx)?;
                self.query_json(&sql)?;
                Ok(0)
            }
        }
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let sql = inline_params(sql, params, LiteralStyle::Influx)?;
        let sql = normalize_order(&sql);
        let json = self.query_json(&sql)?;
        json_to_rowset(&json)
    }

    fn begin(&mut self) -> Result<()> {
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        Ok(())
    }

    fn last_identity(&mut self) -> Result<i64> {
        Ok(0)
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let set = self.query("SHOW MEASUREMENTS", &[])?;
        Ok(set.rows.iter().any(|row| {
            row.get(0)
                .map(DbValue::to_text)
                .is_some_and(|name| name.eq_ignore_ascii_case(table))
        }))
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let quoted = DatabaseKind::InfluxDb.quote(table);
        let mut columns = Vec::new();
        for sql in [
            format!("SHOW FIELD KEYS FROM {quoted}"),
            format!("SHOW TAG KEYS FROM {quoted}"),
        ] {
            if let Ok(set) = self.query(&sql, &[]) {
                for row in &set.rows {
                    if let Some(name) = row.get(0).map(DbValue::to_text)
                        && !columns.iter().any(|c: &String| c.eq_ignore_ascii_case(&name))
                    {
                        columns.push(name);
                    }
                }
            }
        }
        Ok(columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parsing() {
        let cs = ConnectionString::parse(
            "Server=influx.local;Database=metrics;Uid=app;Pwd=***;provider=influxdb",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "influx.local");
        assert_eq!(s.port, 8086);
        assert_eq!(s.database, "metrics");
        assert_eq!(s.user.as_deref(), Some("app"));

        assert!(parse_settings(&ConnectionString::parse("Server=x;provider=influxdb")).is_err());
    }

    #[test]
    fn sql_detection_and_order_rewrite() {
        assert!(looks_like_sql("SELECT * FROM \"m\""));
        assert!(!looks_like_sql("Temperature,device=d1 value=36.5 1730000000000000000"));

        let sql = "SELECT * FROM \"m\" WHERE (\"k\" = 'v') ORDER BY \"Id\" LIMIT 10 OFFSET 0";
        assert_eq!(
            normalize_order(sql),
            "SELECT * FROM \"m\" WHERE (\"k\" = 'v') ORDER BY time ASC LIMIT 10 OFFSET 0"
        );
        let sql2 = "SELECT * FROM \"m\" ORDER BY time DESC LIMIT 5";
        assert_eq!(normalize_order(sql2), sql2);
    }

    #[test]
    fn rowset_from_query_json() {
        let json: Json = serde_json::from_str(
            r#"{"results":[{"series":[{"name":"m","columns":["time","value","name"],
               "values":[["2026-09-27T10:30:00.123Z",36.5,"温度"],[1727420000000000000,37,123]]}]}]}"#,
        )
        .unwrap();
        let set = json_to_rowset(&json).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set.columns.as_slice(), &["time", "value", "name"]);
        assert!(set.rows[0].get(0).unwrap().as_datetime().is_some());
        assert_eq!(set.rows[0].get(1).unwrap().as_f64(), Some(36.5));
        assert_eq!(set.rows[0].get(2).unwrap().to_text(), "温度");
        assert_eq!(set.rows[1].get(1).unwrap().as_i64(), Some(37));
        assert_eq!(set.rows[1].get(2).unwrap().as_i64(), Some(123));
    }
}
