//! TDengine 驱动：通过 REST 接口（默认 6041 端口，`/rest/sql`）执行 SQL。
//!
//! 对应 DH.NCode 的 `TDengine.cs`：
//! - 无自增主键（`InsertAndGetIdentity` 抛 NotSupported；本实现按 0 回写）
//! - 布尔用 1/0；时间文本 `yyyy-MM-dd HH:mm:ss.fffffff`
//! - 反引号引用标识符；分页 `LIMIT n OFFSET m`
//!
//! 实现要点：
//! - REST 响应为 JSON：`{ code, column_meta: [[name, type, len]], data: [[..]], rows }`
//! - 无绑定参数协议，参数由 [`crate::http::inline_params`] 内联为字面量（严格转义）
//! - TDengine 无事务：`begin/commit/rollback` 为空操作
//!
//! 建表说明：TDengine 3.x 要求首列必须是 TIMESTAMP，方言在生成 DDL 时会把
//! 第一个时间列调整到首列（见 [`crate::dialect::DatabaseKind::create_table_sql`]）。

use std::time::Duration;

use serde_json::Value as Json;

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::http::{LiteralStyle, inline_params, post_text};
use crate::session::{RowSet, SqlSession};
use crate::value::{DbValue, parse_datetime};

/// TDengine 会话。
pub struct TDengineSession {
    /// 连接设置
    settings: TDengineSettings,
}

/// 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct TDengineSettings {
    /// 主机
    host: String,
    /// REST 端口（默认 6041）
    port: u16,
    /// 数据库（必填：TDengine 的 REST 路径需要库名）
    database: String,
    /// 用户名（默认 root）
    user: String,
    /// 密码（默认 taosdata）
    password: String,
    /// 请求超时
    timeout: Duration,
}

impl TDengineSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        Ok(Self {
            settings: parse_settings(conn_str)?,
        })
    }

    /// 发送 SQL（REST），返回解析后的 JSON（`code != 0` 时报错）。
    fn request(&self, sql: &str) -> Result<Json> {
        let url = format!(
            "http://{}:{}/rest/sql/{}",
            self.settings.host, self.settings.port, self.settings.database
        );
        let body = post_text(
            &url,
            sql,
            Some((&self.settings.user, &self.settings.password)),
            self.settings.timeout,
        )?;

        let json: Json = serde_json::from_str(&body)
            .map_err(|e| Error::Db(format!("TDengine 响应不是合法 JSON：{e}；内容：{}", trim(&body))))?;
        let code = json.get("code").and_then(Json::as_i64).unwrap_or(-1);
        if code != 0 {
            let desc = json
                .get("desc")
                .and_then(Json::as_str)
                .unwrap_or("未知错误");
            return Err(Error::Db(format!("TDengine 错误（code={code}）：{desc}")));
        }
        Ok(json)
    }
}

/// 截断超长响应文本（错误信息可读性）。
fn trim(text: &str) -> String {
    if text.chars().count() > 200 {
        format!("{}…", text.chars().take(200).collect::<String>())
    } else {
        text.to_string()
    }
}

/// 解析连接串（与 XCode 键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<TDengineSettings> {
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
        None => 6041,
    };
    let database = cs
        .get("database")
        .or(cs.get("db"))
        .or(cs.get("initial catalog"))
        .ok_or_else(|| Error::Model("TDengine 连接串缺少 Database".into()))?
        .to_string();
    let user = cs
        .get("uid")
        .or(cs.get("user"))
        .or(cs.get("username"))
        .unwrap_or("root")
        .to_string();
    let password = cs
        .get("pwd")
        .or(cs.get("password"))
        .or(cs.get("passwd"))
        .unwrap_or("taosdata")
        .to_string();
    let timeout = match cs.get("timeout") {
        Some(v) => Duration::from_secs(
            v.parse::<u64>()
                .map_err(|e| Error::Model(format!("无效的 Timeout \"{v}\"：{e}")))?,
        ),
        None => Duration::from_secs(30),
    };
    Ok(TDengineSettings {
        host,
        port,
        database,
        user,
        password,
        timeout,
    })
}

/// TDengine REST JSON → `RowSet`。
fn json_to_rowset(json: &Json) -> Result<RowSet> {
    let names: Vec<String> = json
        .get("column_meta")
        .and_then(Json::as_array)
        .map(|meta| {
            meta.iter()
                .map(|item| {
                    item.get(0)
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    let types: Vec<String> = json
        .get("column_meta")
        .and_then(Json::as_array)
        .map(|meta| {
            meta.iter()
                .map(|item| {
                    item.get(1)
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();

    let mut set = RowSet::new(names);
    if let Some(rows) = json.get("data").and_then(Json::as_array) {
        for row in rows {
            let cells = row.as_array().cloned().unwrap_or_default();
            let mut values = Vec::with_capacity(cells.len());
            for (index, cell) in cells.iter().enumerate() {
                let ty = types.get(index).map(String::as_str).unwrap_or("");
                values.push(json_cell_to_dbvalue(cell, ty));
            }
            set.push(values);
        }
    }
    Ok(set)
}

/// 单元值 → `DbValue`（按 TDengine 列类型）。
fn json_cell_to_dbvalue(cell: &Json, ty: &str) -> DbValue {
    if cell.is_null() {
        return DbValue::Null;
    }
    match ty {
        "TIMESTAMP" => cell
            .as_str()
            .and_then(parse_datetime)
            .map(DbValue::DateTime)
            .unwrap_or_else(|| DbValue::Text(cell.to_string())),
        "BOOL" => cell
            .as_bool()
            .or_else(|| cell.as_i64().map(|v| v != 0))
            .map(DbValue::Bool)
            .unwrap_or(DbValue::Null),
        "TINYINT" | "SMALLINT" | "INT" | "BIGINT" => {
            cell.as_i64().map(DbValue::Int).unwrap_or(DbValue::Null)
        }
        "FLOAT" | "DOUBLE" => cell.as_f64().map(DbValue::Float).unwrap_or(DbValue::Null),
        "DECIMAL" => cell
            .as_str()
            .and_then(|s| s.parse::<rust_decimal::Decimal>().ok())
            .or_else(|| cell.as_f64().and_then(rust_decimal::Decimal::from_f64_retain))
            .map(DbValue::Decimal)
            .unwrap_or(DbValue::Null),
        _ => match cell {
            Json::String(s) => DbValue::Text(s.clone()),
            other => DbValue::Text(other.to_string()),
        },
    }
}

impl SqlSession for TDengineSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::TDengine
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let sql = inline_params(sql, params, LiteralStyle::TDengine)?;
        let json = self.request(&sql)?;
        Ok(json.get("rows").and_then(Json::as_u64).unwrap_or(0))
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let sql = inline_params(sql, params, LiteralStyle::TDengine)?;
        let json = self.request(&sql)?;
        json_to_rowset(&json)
    }

    fn begin(&mut self) -> Result<()> {
        // TDengine 无事务
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        Ok(())
    }

    fn last_identity(&mut self) -> Result<i64> {
        // TDengine 不支持自增主键
        Ok(0)
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let set = self.query(
            "SELECT count(*) FROM information_schema.ins_tables WHERE db_name = ? AND table_name = ?",
            &[
                DbValue::Text(self.settings.database.clone()),
                DbValue::Text(table.to_string()),
            ],
        )?;
        Ok(set
            .first()
            .and_then(|row| row.get(0))
            .and_then(DbValue::as_i64)
            .unwrap_or(0)
            > 0)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        // DESCRIBE 返回 (field, type, length, note)
        let set = self.query(
            &format!("DESCRIBE {}", DatabaseKind::TDengine.quote(table)),
            &[],
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
            "Server=td.local;Port=6041;Database=iot;Uid=root;Pwd=taosdata;provider=tdengine",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.host, "td.local");
        assert_eq!(s.port, 6041);
        assert_eq!(s.database, "iot");
        assert_eq!(s.password, "taosdata");

        // Database 缺失时报错
        assert!(parse_settings(&ConnectionString::parse("Server=x;provider=tdengine")).is_err());
    }

    #[test]
    fn json_rowset_conversion() {
        let json: Json = serde_json::from_str(
            r#"{"code":0,"column_meta":[["ts","TIMESTAMP",8],["value","DOUBLE",8],["ok","BOOL",1],["name","NCHAR",64]],
               "data":[["2026-09-27 10:30:00.123",12.5,true,"温度"],["2026-09-27 10:31:00.000",null,false,"湿度"]],"rows":2}"#,
        )
        .unwrap();
        let set = json_to_rowset(&json).unwrap();
        assert_eq!(set.columns.as_slice(), &["ts", "value", "ok", "name"]);
        assert_eq!(set.len(), 2);
        let first = set.first().unwrap();
        assert!(first.get(0).unwrap().as_datetime().is_some());
        assert_eq!(first.get(1).and_then(DbValue::as_f64), Some(12.5));
        assert_eq!(first.get(2).and_then(DbValue::as_bool), Some(true));
        assert_eq!(first.get(3).map(DbValue::to_text), Some("温度".into()));
        assert!(set.rows[1].get(1).unwrap().is_null());
    }
}
