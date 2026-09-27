//! 数据库远程服务（对应 DH.NCode `Services` 模块的可移植部分）。
//!
//! - [`DbRequest`]：SQL 执行请求（sql + 参数）；
//! - [`LoginInfo`]：登录响应（数据库类型与版本）；
//! - [`DbService`]：核心服务层（令牌校验 + 库白名单 + SQL 查询/执行/自增/计数/表结构），
//!   不绑定具体 HTTP 框架，宿主可将其挂接到任意路由（对齐 C# `DbService` 的设计定位）；
//! - [`DbClient`]：HTTP 客户端（调用远端 `/Db/*` 接口），协议与 C# 对齐：
//!   `POST Db/Login`、`POST Db/Query`、`POST Db/Execute`、`POST Db/InsertAndGetIdentity`、
//!   `GET Db/QueryCount`、`GET Db/GetTables`；响应采用 NewLife 信封 `{code, data, msg}`。
//!
//! 与 C# 的协议互通：
//! - `POST Db/Query` 的应答为 **DbTable v3 二进制**（[`crate::dbtable`]），与 C#
//!   `DbController.Query` 的 `rs.ToPacket()` 一致：C# `DbClient.QueryAsync` 可直接解析，
//!   Rust 侧用 [`DbClient::query_rowset`] 解析；
//! - 其它接口（Login/Execute/InsertAndGetIdentity/QueryCount/GetTables）为 JSON 信封
//!   `{code, data, msg}`（与 C# `ApiHelper.ProcessResponse` 兼容，无 `code` 时按原样返回）；
//! - C# 的 `DbServer`/`DbController` 绑定 NewLife.Http/MVC；Rust 版提供不绑定框架的
//!   [`DbService`]，路由参数到方法的映射由宿主完成；
//! - Rust 服务端按参数字典的字母序绑定占位符（serde_json Map 语义），详见 [`DbService::query`]。

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::dal::Dal;
use crate::error::{Error, Result};
use crate::http;
use crate::model::TableMeta;
use crate::session::RowSet;
use crate::value::DbValue;

/// 默认请求超时。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// 数据库请求参数模型（对齐 `DbRequest`）。
#[derive(Debug, Clone, Default)]
pub struct DbRequest {
    /// SQL 语句。
    pub sql: Option<String>,
    /// SQL 参数字典（按字母序绑定占位符）。
    pub parameters: BTreeMap<String, Value>,
}

/// 登录信息（对齐 `LoginInfo`；服务端返回给客户端的数据库信息）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoginInfo {
    /// 数据库类型名（Rust 枚举调试名，如 `Sqlite`/`MySql`/`SqlServer`）。
    pub db_type: String,
    /// 服务端数据库版本（Rust 版不主动连接探测，通常为空）。
    pub version: String,
}

/// 数据库服务层（对齐 `DbService`）：令牌校验与 SQL 操作，可被任意 HTTP 宿主复用。
#[derive(Debug, Default)]
pub struct DbService {
    /// 令牌字典：令牌 → 允许访问的数据库连接名列表。
    ///
    /// 空字典表示不校验；某令牌的列表为空表示允许所有数据库（对齐 C# 语义）。
    /// 令牌查找与库名比较均忽略大小写。
    pub tokens: BTreeMap<String, Vec<String>>,
}

impl DbService {
    /// 实例化。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记令牌及其可访问的数据库列表（空列表表示允许所有）。
    /// <param name="token">令牌</param>
    /// <param name="databases">允许访问的数据库连接名</param>
    pub fn set_token(&mut self, token: &str, databases: &[&str]) {
        self.tokens
            .insert(token.to_string(), databases.iter().map(|s| s.to_string()).collect());
    }

    /// 验证令牌是否可访问指定数据库（对齐 `ValidateToken`）。
    /// <param name="token">令牌</param>
    /// <param name="db">数据库连接名</param>
    /// <returns>是否允许访问</returns>
    pub fn validate_token(&self, token: &str, db: &str) -> Result<()> {
        if token.is_empty() {
            return Err(Error::Argument("令牌不能为空".into()));
        }
        if db.is_empty() {
            return Err(Error::Argument("数据库名称不能为空".into()));
        }
        // 未配置令牌时不校验
        if self.tokens.is_empty() {
            return Ok(());
        }
        let Some(dbs) = self.lookup_token(token) else {
            return Err(Error::Db("无效令牌".into()));
        };
        if !dbs.is_empty() && !dbs.iter().any(|d| d.eq_ignore_ascii_case(db)) {
            return Err(Error::Db(format!("令牌无权访问数据库[{db}]")));
        }
        Ok(())
    }

    /// 登录信息（对齐控制器 `Login`；Rust 版不主动建立连接，版本号留空）。
    /// <param name="dal">数据访问层</param>
    /// <returns>登录信息</returns>
    pub fn login_info(&self, dal: &Dal) -> LoginInfo {
        LoginInfo {
            db_type: format!("{:?}", dal.kind()),
            version: String::new(),
        }
    }

    /// 执行 SQL 查询，返回结果集（对齐 `Query`）。
    ///
    /// 参数按 [`BTreeMap`] 的键序（字母序）绑定到 SQL 的位置占位符；
    /// 跨语言调用建议使用命名占位符的方言（如 SQL Server `@name`）。
    /// <param name="dal">数据访问层</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>结果集</returns>
    pub fn query(&self, dal: &Dal, sql: &str, parameters: Option<&BTreeMap<String, Value>>) -> Result<RowSet> {
        if sql.is_empty() {
            return Err(Error::Argument("SQL不能为空".into()));
        }
        let params = to_params(parameters);
        let mut session = dal.open_session()?;
        session.query(sql, &params)
    }

    /// 执行 SQL 查询并返回 **DbTable v3 二进制报文**（对齐 C# `DbController.Query` 的 `rs.ToPacket()`）。
    ///
    /// 宿主应把结果作为 `application/octet-stream` 响应体返回给 `POST Db/Query`，
    /// C# `DbClient.QueryAsync` 可直接解析；Rust 侧可用 [`DbClient::query_rowset`] 解析。
    /// <param name="dal">数据访问层</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>DbTable 二进制报文</returns>
    pub fn query_packet(
        &self,
        dal: &Dal,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<Vec<u8>> {
        let set = self.query(dal, sql, parameters)?;
        Ok(crate::dbtable::encode_rowset(&set))
    }

    /// 执行 SQL 语句，返回受影响行数（对齐 `Execute`）。
    /// <param name="dal">数据访问层</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>受影响行数</returns>
    pub fn execute(&self, dal: &Dal, sql: &str, parameters: Option<&BTreeMap<String, Value>>) -> Result<u64> {
        if sql.is_empty() {
            return Err(Error::Argument("SQL不能为空".into()));
        }
        let params = to_params(parameters);
        let mut session = dal.open_session()?;
        session.execute(sql, &params)
    }

    /// 执行插入语句并返回自增标识（对齐 `InsertAndGetIdentity`）。
    /// <param name="dal">数据访问层</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>自增主键值</returns>
    pub fn insert_and_get_identity(
        &self,
        dal: &Dal,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<i64> {
        if sql.is_empty() {
            return Err(Error::Argument("SQL不能为空".into()));
        }
        let params = to_params(parameters);
        let mut session = dal.open_session()?;
        session.execute(sql, &params)?;
        session.last_identity()
    }

    /// 快速查询单表记录数（对齐 `QueryCount`）。
    /// <param name="dal">数据访问层</param>
    /// <param name="table">表名（模型名）</param>
    /// <returns>记录数</returns>
    pub fn query_count(&self, dal: &Dal, table: &str) -> Result<i64> {
        if table.is_empty() {
            return Err(Error::Argument("表名不能为空".into()));
        }
        let mut session = dal.open_session()?;
        dal.table(table)?.count(session.as_mut(), None)
    }

    /// 获取远端数据库的表结构（对齐 `GetTables`）。
    /// <param name="dal">数据访问层</param>
    /// <returns>表结构列表</returns>
    pub fn get_tables(&self, dal: &Dal) -> Result<Vec<TableMeta>> {
        let mut session = dal.open_session()?;
        dal.read_tables(session.as_mut())
    }

    /// 查找令牌（忽略大小写，对齐 C# 的 `StringComparer.OrdinalIgnoreCase` 字典）。
    fn lookup_token(&self, token: &str) -> Option<&Vec<String>> {
        self.tokens.get(token).or_else(|| {
            self.tokens
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(token))
                .map(|(_, v)| v)
        })
    }
}

/// 数据库 HTTP 客户端（对齐 `DbClient`）。
///
/// 所有数据库操作通过 HTTP 转发到远端服务执行；使用 [`http::post_json`]/[`http::get_text`]，
/// 响应按 NewLife 信封 `{code, data, msg}` 解包。
#[derive(Debug, Clone)]
pub struct DbClient {
    /// 服务端地址（如 `http://127.0.0.1:3305`）。
    pub server: String,
    /// 数据库连接名。
    pub db: String,
    /// 令牌。
    pub token: Option<String>,
    /// 请求超时。
    pub timeout: Duration,
}

impl DbClient {
    /// 实例化。
    /// <param name="server">服务端地址</param>
    /// <param name="db">数据库连接名</param>
    /// <param name="token">令牌</param>
    pub fn new(server: impl Into<String>, db: impl Into<String>, token: Option<&str>) -> Self {
        Self {
            server: server.into().trim_end_matches('/').to_string(),
            db: db.into(),
            token: token.map(str::to_string),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 登录到远端数据库服务（对齐 `LoginAsync`）。
    /// <returns>登录信息</returns>
    pub fn login(&self) -> Result<LoginInfo> {
        let body = json!({ "db": self.db, "token": self.token }).to_string();
        let text = http::post_json(&self.url("Db/Login"), &body, None, self.timeout)?;
        let data = parse_envelope(&text)?;
        Ok(LoginInfo {
            db_type: get_str_field(&data, &["dbType", "DbType"]),
            version: get_str_field(&data, &["version", "Version"]),
        })
    }

    /// 执行 SQL 查询，返回结果集（自动识别二进制与 JSON 应答）。
    ///
    /// C# `DbServer` 的 `Db/Query` 返回 DbTable 二进制（[`crate::dbtable`]）；
    /// Rust 宿主若返回 JSON 行集（`{columns, rows}`，可带 `{code,data,msg}` 信封）也同样支持。
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>结果集</returns>
    pub fn query_rowset(&self, sql: &str, parameters: Option<&BTreeMap<String, Value>>) -> Result<RowSet> {
        let args = self.build_args(sql, parameters);
        let bytes = http::post_bytes(
            &self.url("Db/Query"),
            &args.to_string(),
            true,
            None,
            self.timeout,
        )?;
        if bytes.is_empty() {
            return Ok(RowSet::new(Vec::new()));
        }
        if crate::dbtable::is_dbtable(&bytes) {
            return crate::dbtable::decode_rowset(&bytes);
        }
        let text = String::from_utf8_lossy(&bytes);
        let data = parse_envelope(&text)?;
        rowset_from_json(&data)
    }

    /// 执行 SQL 查询，返回 JSON 行集（对 C# / Rust 两种服务端均可用）。
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>JSON 行集（`{columns, rows}`）</returns>
    pub fn query(&self, sql: &str, parameters: Option<&BTreeMap<String, Value>>) -> Result<Value> {
        let set = self.query_rowset(sql, parameters)?;
        Ok(rowset_to_json(&set))
    }

    /// 执行 SQL 语句，返回受影响行数（对齐 `ExecuteAsync`）。
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>受影响行数</returns>
    pub fn execute(&self, sql: &str, parameters: Option<&BTreeMap<String, Value>>) -> Result<i64> {
        let args = self.build_args(sql, parameters);
        let text = http::post_json(&self.url("Db/Execute"), &args.to_string(), None, self.timeout)?;
        parse_envelope_i64(&text)
    }

    /// 执行插入语句并返回自增标识（对齐 `InsertAndGetIdentityAsync`）。
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>自增主键值</returns>
    pub fn insert_and_get_identity(&self, sql: &str, parameters: Option<&BTreeMap<String, Value>>) -> Result<i64> {
        let args = self.build_args(sql, parameters);
        let text = http::post_json(
            &self.url("Db/InsertAndGetIdentity"),
            &args.to_string(),
            None,
            self.timeout,
        )?;
        parse_envelope_i64(&text)
    }

    /// 查询单表记录数（对齐 `QueryCountAsync`）。
    /// <param name="table_name">表名</param>
    /// <returns>记录数</returns>
    pub fn query_count(&self, table_name: &str) -> Result<i64> {
        let url = format!(
            "{}?tableName={}&db={}&token={}",
            self.url("Db/QueryCount"),
            percent_encode(table_name),
            percent_encode(&self.db),
            percent_encode(self.token.as_deref().unwrap_or_default())
        );
        let text = http::get_text(&url, None, self.timeout)?;
        parse_envelope_i64(&text)
    }

    /// 获取表结构（对齐 `GetTablesAsync`；返回 JSON 数组）。
    /// <returns>JSON 表结构数组</returns>
    pub fn get_tables(&self) -> Result<Value> {
        let url = format!(
            "{}?db={}&token={}",
            self.url("Db/GetTables"),
            percent_encode(&self.db),
            percent_encode(self.token.as_deref().unwrap_or_default())
        );
        let text = http::get_text(&url, None, self.timeout)?;
        parse_envelope(&text)
    }

    /// 拼接路由地址。
    fn url(&self, route: &str) -> String {
        format!("{}/{route}", self.server)
    }

    /// 构建请求参数（sql/parameters/db/token；parameters 为 JSON 字符串，对齐 C# `BuildArgs`）。
    fn build_args(&self, sql: &str, parameters: Option<&BTreeMap<String, Value>>) -> Value {
        match parameters {
            None => json!({ "sql": sql, "db": self.db, "token": self.token }),
            Some(ps) => {
                let map: serde_json::Map<String, Value> =
                    ps.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                json!({
                    "sql": sql,
                    "parameters": Value::Object(map).to_string(),
                    "db": self.db,
                    "token": self.token,
                })
            }
        }
    }
}

/// 参数转换：JSON 字典 → 位置参数数组（按字母序，对齐 `DbService` 的绑定说明）。
/// <param name="parameters">参数字典</param>
/// <returns>数据库值数组</returns>
fn to_params(parameters: Option<&BTreeMap<String, Value>>) -> Vec<DbValue> {
    parameters
        .map(|ps| ps.values().map(json_to_db_value).collect())
        .unwrap_or_default()
}

/// JSON 值转换为数据库值。
/// <param name="value">JSON 值</param>
/// <returns>数据库值</returns>
pub fn json_to_db_value(value: &Value) -> DbValue {
    match value {
        Value::Null => DbValue::Null,
        Value::Bool(b) => DbValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                DbValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                DbValue::Float(f)
            } else {
                DbValue::Text(n.to_string())
            }
        }
        Value::String(s) => DbValue::Text(s.clone()),
        other => DbValue::Text(other.to_string()),
    }
}

/// 数据库值转换为 JSON 值（二进制转十六进制字符串，时间转 ISO 文本）。
/// <param name="value">数据库值</param>
/// <returns>JSON 值</returns>
pub fn db_value_to_json(value: &DbValue) -> Value {
    match value {
        DbValue::Null => Value::Null,
        DbValue::Bool(b) => Value::Bool(*b),
        DbValue::Int(i) => Value::Number((*i).into()),
        DbValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        DbValue::Decimal(d) => Value::String(d.to_string()),
        DbValue::Text(s) => Value::String(s.clone()),
        DbValue::Blob(b) => Value::String(http::to_hex(b)),
        DbValue::DateTime(dt) => Value::String(dt.format("%Y-%m-%d %H:%M:%S%.f").to_string()),
    }
}

/// 将本地结果集编码为 JSON 行集（`{columns, rows}`；替代 C# 的 NewLife Packet 编码）。
/// <param name="set">结果集</param>
/// <returns>JSON 行集</returns>
pub fn rowset_to_json(set: &RowSet) -> Value {
    let rows: Vec<Value> = set
        .rows
        .iter()
        .map(|row| Value::Array(row.values().iter().map(db_value_to_json).collect()))
        .collect();
    json!({ "columns": set.columns.as_ref(), "rows": rows })
}

/// 将 JSON 行集解码为本地结果集。
/// <param name="value">JSON 行集</param>
/// <returns>结果集</returns>
pub fn rowset_from_json(value: &Value) -> Result<RowSet> {
    let columns: Vec<String> = value
        .get("columns")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .ok_or_else(|| Error::Db("行集缺少 columns".into()))?;
    let mut set = RowSet::new(columns);
    if let Some(rows) = value.get("rows").and_then(Value::as_array) {
        for row in rows {
            let values: Vec<DbValue> = row
                .as_array()
                .ok_or_else(|| Error::Db("行集行格式错误".into()))?
                .iter()
                .map(json_to_db_value)
                .collect();
            set.push(values);
        }
    }
    Ok(set)
}

/// 解包 NewLife 响应信封 `{code, data, msg}`：无 `code` 字段时视为直接返回数据。
/// <param name="text">响应文本</param>
/// <returns>数据部分的 JSON</returns>
fn parse_envelope(text: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(text).map_err(|e| Error::Db(format!("响应解析失败：{e}")))?;
    match value.get("code").and_then(Value::as_i64) {
        Some(0) | None => match value.get("data") {
            Some(data) => Ok(data.clone()),
            None => Ok(value),
        },
        Some(_) => {
            let msg = value
                .get("msg")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("远程服务返回错误");
            Err(Error::Db(msg.to_string()))
        }
    }
}

/// 解包信封并取整数结果。
/// <param name="text">响应文本</param>
/// <returns>整数结果</returns>
fn parse_envelope_i64(text: &str) -> Result<i64> {
    parse_envelope(text)?
        .as_i64()
        .ok_or_else(|| Error::Db("响应不是整数".into()))
}

/// 读取字符串字段（兼容多种大小写写法）。
/// <param name="data">JSON 对象</param>
/// <param name="keys">候选键</param>
/// <returns>字段值（缺失时为空字符串）</returns>
fn get_str_field(data: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| data.get(*k).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// 对查询串参数做百分号编码（unreserved 字符不编码）。
/// <param name="text">原文</param>
/// <returns>编码文本</returns>
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use serde_json::json;

    use super::*;
    use crate::dal::Dal;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="User" TableName="DH_User" Description="用户">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Name" DataType="String" Length="50" Nullable="True" />
        <Column Name="Enable" DataType="Int32" Nullable="True" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    /// 建临时库。
    fn temp_dal(name: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now().format("%H%M%S%.6f").to_string().replace('.', "");
        let dir = std::env::temp_dir().join(format!("rcode-dbsvc-{}-{}-{name}", std::process::id(), stamp));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        (dal, dir)
    }

    #[test]
    fn token_validation_matrix() {
        // 未配置令牌：放行
        let svc = DbService::new();
        assert!(svc.validate_token("any", "Demo").is_ok());
        // 空令牌/空库名：参数错误
        assert!(svc.validate_token("", "Demo").is_err());
        assert!(svc.validate_token("tk", "").is_err());

        let mut svc = DbService::new();
        svc.set_token("tk1", &["Membership", "Mes"]);
        svc.set_token("tk2", &[]); // 允许所有
        assert!(svc.validate_token("tk1", "membership").is_ok()); // 库名大小写不敏感
        assert!(svc.validate_token("TK1", "Mes").is_ok()); // 令牌大小写不敏感
        assert!(svc.validate_token("tk1", "Other").is_err());
        assert!(svc.validate_token("unknown", "Mes").is_err());
        assert!(svc.validate_token("tk2", "Whatever").is_ok());
    }

    #[test]
    fn db_service_crud_over_sqlite() {
        let (dal, dir) = temp_dal("crud");
        let svc = DbService::new();

        // 登录信息
        let info = svc.login_info(&dal);
        assert_eq!(info.db_type, "Sqlite");
        assert!(info.version.is_empty());

        // 执行插入（无参数）
        assert_eq!(
            svc.execute(&dal, "INSERT INTO DH_User(Name, Enable) VALUES('a', 1)", None).unwrap(),
            1
        );
        // 带参数插入（按字母序绑定：Enable → ?1，Name → ?2）
        let mut ps = BTreeMap::new();
        ps.insert("Enable".to_string(), json!(1));
        ps.insert("Name".to_string(), json!("b"));
        assert_eq!(
            svc.execute(&dal, "INSERT INTO DH_User(Enable, Name) VALUES(?, ?)", Some(&ps)).unwrap(),
            1
        );

        // 查询
        let set = svc.query(&dal, "SELECT Name FROM DH_User ORDER BY Id", None).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set.first().unwrap().get(0).unwrap().as_str(), Some("a"));

        // 自增标识
        let id = svc
            .insert_and_get_identity(&dal, "INSERT INTO DH_User(Name, Enable) VALUES('c', 1)", None)
            .unwrap();
        assert!(id > 0);

        // 计数
        assert_eq!(svc.query_count(&dal, "User").unwrap(), 3);

        // 表结构
        let tables = svc.get_tables(&dal).unwrap();
        assert!(tables.iter().any(|t| t.name.contains("User")));

        // 空 SQL 报错
        assert!(svc.query(&dal, "", None).is_err());
        assert!(svc.execute(&dal, "", None).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rowset_json_roundtrip() {
        let mut ps = RowSet::new(vec!["Id".into(), "Name".into(), "Score".into()]);
        ps.push(vec![DbValue::Int(1), DbValue::Text("张三".into()), DbValue::Null]);
        ps.push(vec![DbValue::Int(2), DbValue::Text("李四".into()), DbValue::Float(9.5)]);

        let value = rowset_to_json(&ps);
        assert_eq!(value["columns"][0], "Id");
        assert_eq!(value["rows"][0][1], "张三");
        assert_eq!(value["rows"][0][2], Value::Null);

        let back = rowset_from_json(&value).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.columns.as_ref(), &ps.columns.as_ref().clone());
        assert_eq!(back.first().unwrap().get(0).unwrap().as_i64(), Some(1));
        assert_eq!(back.rows[1].get(2).unwrap().as_f64(), Some(9.5));

        // 非法输入
        assert!(rowset_from_json(&json!({})).is_err());
    }

    #[test]
    fn envelope_parsing() {
        // 正常信封
        assert_eq!(parse_envelope(r#"{"code":0,"data":5}"#).unwrap(), json!(5));
        // 无信封：直接返回
        assert_eq!(parse_envelope("3").unwrap(), json!(3));
        // 错误信封
        let e = parse_envelope(r#"{"code":401,"msg":"未登录或令牌无效"}"#);
        assert!(e.is_err());
        assert!(e.unwrap_err().to_string().contains("未登录"));
        // 非 JSON
        assert!(parse_envelope("oops").is_err());

        assert_eq!(percent_encode("a b/中"), "a%20b%2F%E4%B8%AD");
    }

    #[test]
    fn db_client_execute_against_mock_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        data.extend_from_slice(&buf[..n]);
                        // 头部完整且 Content-Length 声明的主体已到齐则结束（ureq 可能分两段发送）
                        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&data[..pos]);
                            let content_length = head
                                .lines()
                                .filter_map(|l| {
                                    let lower = l.to_ascii_lowercase();
                                    lower
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .next()
                                .unwrap_or(0);
                            if data.len() >= pos + 4 + content_length {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let request = String::from_utf8_lossy(&data).to_string();
            let body = r#"{"code":0,"data":3}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            request
        });

        let client = DbClient::new(format!("http://127.0.0.1:{port}"), "Demo", Some("tk"));
        let n = client.execute("UPDATE X SET A=1", None).unwrap();
        assert_eq!(n, 3);

        let request = handle.join().unwrap();
        assert!(request.starts_with("POST /Db/Execute"));
        assert!(request.contains(r#""sql":"UPDATE X SET A=1""#));
        assert!(request.contains(r#""db":"Demo""#));
        assert!(request.contains(r#""token":"tk""#));
    }

    /// 启动固定应答的 mock HTTP 服务（可处理多个连接），返回端口与请求文本。
    fn spawn_mock_response(
        content_type: &'static str,
        body: Vec<u8>,
        times: usize,
    ) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..times {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();

                // 读取请求（头部 + Content-Length 声明的主体）
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            data.extend_from_slice(&buf[..n]);
                            if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&data[..pos]);
                                let content_length = head
                                    .lines()
                                    .filter_map(|l| {
                                        let lower = l.to_ascii_lowercase();
                                        lower
                                            .strip_prefix("content-length:")
                                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                    })
                                    .next()
                                    .unwrap_or(0);
                                if data.len() >= pos + 4 + content_length {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                requests.push(String::from_utf8_lossy(&data).to_string());

                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                stream.write_all(&response).unwrap();
                stream.flush().unwrap();
            }
            requests
        });

        (port, handle)
    }

    #[test]
    fn db_client_query_parses_binary_dbtable_response() {
        // 模拟 C# DbServer：/Db/Query 返回 DbTable 二进制（两列一次查询、再走一次 JSON 视图）
        let mut set = RowSet::new(vec!["Id".into(), "Name".into(), "Score".into()]);
        set.push(vec![
            DbValue::Int(7),
            DbValue::Text("张三".into()),
            DbValue::Float(9.5),
        ]);
        let payload = crate::dbtable::encode_rowset(&set);
        let (port, handle) = spawn_mock_response("application/octet-stream", payload, 2);

        let client = DbClient::new(format!("http://127.0.0.1:{port}"), "Demo", Some("tk"));

        // 二进制应答 → RowSet
        let back = client.query_rowset("SELECT Id,Name,Score FROM T", None).unwrap();
        assert_eq!(back.columns.as_ref(), set.columns.as_ref());
        assert_eq!(back.rows[0].get(0), Some(&DbValue::Int(7)));
        assert_eq!(back.rows[0].get(1), Some(&DbValue::Text("张三".into())));
        assert_eq!(back.rows[0].get(2), Some(&DbValue::Float(9.5)));

        // 同一接口的 JSON 视图同样可用（内部自动转 JSON 行集）
        let json = client.query("SELECT Id,Name,Score FROM T", None).unwrap();
        assert_eq!(json["columns"][0], "Id");
        assert_eq!(json["rows"][0][1], "张三");

        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("POST /Db/Query"));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("accept: application/octet-stream"),
            "应声明二进制 Accept：{}",
            requests[0]
        );
    }

    #[test]
    fn db_client_query_falls_back_to_json_response() {
        // 模拟 Rust 宿主：返回 JSON 行集
        let body = br#"{"columns":["A"],"rows":[[1],[2]]}"#.to_vec();
        let (port, _handle) = spawn_mock_response("application/json", body, 1);

        let client = DbClient::new(format!("http://127.0.0.1:{port}"), "Demo", None);
        let set = client.query_rowset("SELECT A", None).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set.rows[0].get(0), Some(&DbValue::Int(1)));
        assert_eq!(set.rows[1].get(0), Some(&DbValue::Int(2)));
    }
}
