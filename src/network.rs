//! `provider=network` 驱动：把数据库操作转发到远端 XCode DbServer（对应 DH.NCode `Network.cs`）。
//!
//! 连接串格式（与 C# 一致）：
//!
//! ```text
//! Server=http://127.0.0.1:3305;Database=Membership;Password=token123;provider=network
//! ```
//!
//! - `Server`：远端 `DbServer` 地址；`Database`：远端数据库连接名；`Password`：访问令牌（兼容 `Token=`）；
//! - 打开即登录：`Dal::open` 登录远端（`POST Db/Login`）取得数据库类型，之后本端按**远端类型**
//!   套用方言规则（占位符/分页/引用符），对齐 C# `Network.RawType`/`Server` 的委托格式化；
//! - SQL 转发：查询 `POST Db/Query`（应答为 DbTable v3 二进制，见 [`crate::dbtable`]）、
//!   执行 `POST Db/Execute`、插入自增 `POST Db/InsertAndGetIdentity`；
//! - 占位符：本端生成的位置占位符（`?`/`$N`/`:N`/`@pN`）改写为带远端前缀的命名占位符
//!   （`@p0`/`:p0`/`?p0`，与 C# `FormatParameterName` 一致），参数字典键与之同名；
//! - 事务：**不支持转发**（远端 `DbServer` 未提供事务接口，C# 侧同样无法真正生效）——
//!   `begin`/`commit`/`rollback` 返回 [`Error::Unsupported`]；
//! - 表结构：不在本端建表（对齐 C# `NetworkMetaData.OnSetTables` 空实现，`sync_schema` 返回空清单）；
//!   `table_exists`/`table_columns` 通过远端 `GET Db/GetTables` 探测（结果缓存）；
//! - 限制：二进制**参数**经 JSON 字符串传递（本端按十六进制），与 C# 的编码可能不同，
//!   跨语言时建议避免传 BLOB 参数；二进制**结果列**走 DbTable 二进制通道不受影响。
//!
//! 与 C# 的差异：
//! - C# 首次使用时惰性登录；Rust 在 `Dal::open` 即登录（配置错误快速暴露）；
//! - C# `NetworkSession.QueryCountFast` 走 `GET Db/QueryCount`（Rust 可用
//!   [`DbClient::query_count`]，但会话内计数走 `select count(*)` 转发）。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::dal::ConnectionString;
use crate::db_service::{DbClient, db_value_to_json};
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// 是否 `provider=network`（兼容 `net`）。
pub(crate) fn is_network(conn_str: &ConnectionString) -> bool {
    conn_str
        .provider()
        .map(|p| matches!(p.trim().to_ascii_lowercase().as_str(), "network" | "net"))
        .unwrap_or(false)
}

/// 登录远端，探明远端数据库类型（对应 C# `Network.GetClient` 的登录逻辑）。
pub(crate) fn probe_remote_kind(conn_str: &ConnectionString) -> Result<DatabaseKind> {
    let client = DbClient::from_connection_string(conn_str)?;
    let info = client
        .login()
        .map_err(|e| Error::Db(format!("network 驱动登录远端失败：{e}")))?;
    DatabaseKind::from_remote_name(&info.db_type)
}

/// 远端数据库的 C# 参数前缀（对齐各驱动 `ParamPrefix` 覆写）。
///
/// `:` → Oracle/DaMeng/DB2；`?` → MySql/Hana/IRIS/TDengine；其余（SqlServer/SQLite/PG 系等）→ `@`。
fn remote_prefix(kind: DatabaseKind) -> char {
    match kind {
        DatabaseKind::Oracle | DatabaseKind::DaMeng | DatabaseKind::Db2 => ':',
        DatabaseKind::MySql | DatabaseKind::Hana | DatabaseKind::Iris | DatabaseKind::TDengine => '?',
        _ => '@',
    }
}

/// 把本地方言 SQL 改写为远端命名占位符形式，并生成参数字典。
///
/// - `?` → `{prefix}p{顺序号}`（按出现顺序消耗位置参数）；
/// - `$N` / `:N`（1 基）→ `{prefix}p{N-1}`；
/// - `@pN`（SqlServer 本地风格）原样保留；
/// - 字符串字面量内的占位符不转换。
fn translate(sql: &str, params: &[DbValue], kind: DatabaseKind) -> (String, BTreeMap<String, Value>) {
    let prefix = remote_prefix(kind);
    let mut out = String::with_capacity(sql.len() + params.len() * 5);
    let mut index = 0usize;
    let mut chars = sql.char_indices().peekable();
    let mut in_string = false;

    while let Some((pos, ch)) = chars.next() {
        if ch == '\'' {
            in_string = !in_string;
            out.push(ch);
            continue;
        }
        if in_string {
            out.push(ch);
            continue;
        }
        match ch {
            '?' => {
                out.push(prefix);
                out.push_str(&format!("p{index}"));
                index += 1;
            }
            '$' | ':' => {
                // 跳过 PostgreSQL 的 `::` 类型转换
                if ch == ':' && sql[pos + 1..].starts_with(':') {
                    out.push_str("::");
                    chars.next();
                    continue;
                }
                let rest = &sql[pos + ch.len_utf8()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if digits.is_empty() {
                    out.push(ch);
                    continue;
                }
                // 消耗数字字符（ASCII，按字符数即可）
                for _ in 0..digits.len() {
                    chars.next();
                }
                if let Ok(n) = digits.parse::<usize>() {
                    let idx = n.saturating_sub(1);
                    out.push(prefix);
                    out.push_str(&format!("p{idx}"));
                    index = index.max(idx + 1);
                }
            }
            _ => out.push(ch),
        }
    }

    let dict: BTreeMap<String, Value> = (0..params.len())
        .map(|i| (format!("{prefix}p{i}"), db_value_to_json(&params[i])))
        .collect();
    (out, dict)
}

/// 网络数据库会话：把 SQL 操作转为 HTTP 接口调用（对应 C# `NetworkSession`）。
pub struct NetworkSession {
    /// HTTP 客户端
    client: DbClient,
    /// 远端数据库类型（本地套用其方言规则）
    kind: DatabaseKind,
    /// 远端表结构缓存（首次访问时拉取，对应远端 `GET Db/GetTables`）
    tables: Option<Value>,
}

impl NetworkSession {
    /// 依据连接串与远端类型创建会话。
    /// <param name="conn_str">连接串（`Server`/`Database`/`Password`）</param>
    /// <param name="kind">远端数据库类型（由 [`probe_remote_kind`] 登录取得）</param>
    pub fn new(conn_str: &ConnectionString, kind: DatabaseKind) -> Result<Self> {
        Ok(Self {
            client: DbClient::from_connection_string(conn_str)?,
            kind,
            tables: None,
        })
    }

    /// 远端 HTTP 客户端（供诊断或扩展接口如 `query_count` 使用）。
    pub fn client(&self) -> &DbClient {
        &self.client
    }

    /// 远端表结构 JSON（首次访问拉取并缓存）。
    fn remote_tables(&mut self) -> Result<&Value> {
        if self.tables.is_none() {
            self.tables = Some(self.client.get_tables()?);
        }
        Ok(self.tables.as_ref().expect("tables 已填充"))
    }
}

/// 事务不支持的统一错误。
fn tx_unsupported() -> Error {
    Error::Unsupported("network 驱动不支持事务转发：远端 DbServer 未提供事务接口".into())
}

/// 从远端表结构 JSON 提取列表（兼容裸数组、`{data:[...]}`、`{items:[...]}` 信封）。
fn table_items(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .or_else(|| value.get("data").and_then(Value::as_array))
        .or_else(|| value.get("items").and_then(Value::as_array))
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// 表名（兼容 `Name`/`name`/`TableName`/`tableName` 与字符串元素）。
fn item_name(item: &Value) -> Option<&str> {
    if let Some(s) = item.as_str() {
        return Some(s);
    }
    ["Name", "name", "TableName", "tableName"]
        .iter()
        .find_map(|k| item.get(*k).and_then(Value::as_str))
}

/// 列名（兼容 `Columns`/`columns` 数组，元素为字符串或含 Name 的对象）。
fn item_columns(item: &Value) -> Vec<String> {
    let Some(cols) = item
        .get("Columns")
        .or_else(|| item.get("columns"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    cols.iter()
        .filter_map(|c| {
            c.as_str().map(str::to_string).or_else(|| {
                ["Name", "name", "ColumnName", "columnName"]
                    .iter()
                    .find_map(|k| c.get(*k).and_then(Value::as_str))
                    .map(str::to_string)
            })
        })
        .collect()
}

impl SqlSession for NetworkSession {
    fn kind(&self) -> DatabaseKind {
        self.kind
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let (sql, parameters) = translate(sql, params, self.kind);
        let n = self.client.execute(&sql, Some(&parameters))?;
        Ok(n.max(0) as u64)
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let (sql, parameters) = translate(sql, params, self.kind);
        self.client.query_rowset(&sql, Some(&parameters))
    }

    /// 插入并取自增：转发到远端原子接口 `POST Db/InsertAndGetIdentity`（对齐 C# `NetworkSession`）。
    fn insert_and_get_identity(
        &mut self,
        sql: &str,
        params: &[DbValue],
        table: Option<&str>,
    ) -> Result<i64> {
        let _ = table;
        let (sql, parameters) = translate(sql, params, self.kind);
        self.client.insert_and_get_identity(&sql, Some(&parameters))
    }

    fn begin(&mut self) -> Result<()> {
        Err(tx_unsupported())
    }

    fn commit(&mut self) -> Result<()> {
        Err(tx_unsupported())
    }

    fn rollback(&mut self) -> Result<()> {
        Err(tx_unsupported())
    }

    fn last_identity(&mut self) -> Result<i64> {
        Err(Error::Unsupported(
            "network 驱动请使用 insert_and_get_identity（由远端 Db/InsertAndGetIdentity 返回）".into(),
        ))
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let exists = table_items(self.remote_tables()?)
            .iter()
            .any(|item| item_name(item).map(|n| n.eq_ignore_ascii_case(table)).unwrap_or(false));
        Ok(exists)
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        for item in table_items(self.remote_tables()?) {
            if item_name(item)
                .map(|n| n.eq_ignore_ascii_case(table))
                .unwrap_or(false)
            {
                return Ok(item_columns(item));
            }
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::{dal::Dal, dbtable};

    #[test]
    fn translate_rewrites_local_placeholders_to_remote_named() {
        let params = vec![DbValue::Int(1), DbValue::Text("a".into())];

        let (sql, dict) = translate(
            "SELECT * FROM T WHERE A=? AND B=?",
            &params,
            DatabaseKind::Sqlite,
        );
        assert_eq!(sql, "SELECT * FROM T WHERE A=@p0 AND B=@p1");
        assert_eq!(dict.len(), 2);
        assert!(dict.contains_key("@p0") && dict.contains_key("@p1"));

        let (sql, _) = translate("SELECT * FROM T WHERE A=?", &params, DatabaseKind::MySql);
        assert_eq!(sql, "SELECT * FROM T WHERE A=?p0");

        let (sql, _) = translate(
            "SELECT * FROM T WHERE A=$1 AND B=$2",
            &params,
            DatabaseKind::PostgreSql,
        );
        assert_eq!(sql, "SELECT * FROM T WHERE A=@p0 AND B=@p1");

        let (sql, _) = translate("SELECT * FROM T WHERE A=:1", &params, DatabaseKind::Oracle);
        assert_eq!(sql, "SELECT * FROM T WHERE A=:p0");

        // SQL Server 本地风格已是命名占位符，保持原样
        let (sql, dict) = translate(
            "SELECT * FROM T WHERE A=@p0",
            &params,
            DatabaseKind::SqlServer,
        );
        assert_eq!(sql, "SELECT * FROM T WHERE A=@p0");
        assert_eq!(dict.len(), 2);

        // 字符串字面量内的 `?` 不转换
        let (sql, _) = translate("SELECT '?' AS A, B=? FROM T", &params, DatabaseKind::Sqlite);
        assert_eq!(sql, "SELECT '?' AS A, B=@p0 FROM T");
    }

    /// 模拟远端 DbServer：按 5 次请求（Login/Query/Execute/InsertAndGetIdentity/GetTables）依次应答。
    fn spawn_mock_server() -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();

                // 读请求头，按 Content-Length 继续读请求体
                let mut raw = Vec::new();
                let mut buf = [0u8; 2048];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if let Some(pos) = text.find("\r\n\r\n") {
                        let len = text[..pos]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if raw.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&raw).to_string();

                let (content_type, body): (&str, Vec<u8>) = if request.contains("/Db/Login") {
                    (
                        "application/json",
                        br#"{"DbType":"SQLite","Version":"3.45.0"}"#.to_vec(),
                    )
                } else if request.contains("/Db/Query") {
                    let mut set = RowSet::new(vec!["Id".into()]);
                    set.push(vec![DbValue::Int(7)]);
                    ("application/octet-stream", dbtable::encode_rowset(&set))
                } else if request.contains("/Db/GetTables") {
                    (
                        "application/json",
                        br#"[{"Name":"DH_User","Columns":[{"Name":"Id"},{"Name":"Name"}]}]"#
                            .to_vec(),
                    )
                } else if request.contains("/Db/InsertAndGetIdentity") {
                    ("application/json", br#"{"data": 9}"#.to_vec())
                } else {
                    ("application/json", br#"{"data": 3}"#.to_vec())
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        });
        (port, handle)
    }

    #[test]
    fn network_driver_forwards_over_http() {
        let (port, handle) = spawn_mock_server();
        let conn =
            format!("Server=http://127.0.0.1:{port};Database=Demo;Password=tk;provider=network");

        // Dal::open 登录远端并套用远端类型（SQLite）
        let dal = Dal::open(&conn).unwrap();
        assert_eq!(dal.kind(), DatabaseKind::Sqlite);

        let mut session = dal.open_session().unwrap();
        assert_eq!(session.kind(), DatabaseKind::Sqlite);

        // 查询（二进制 DbTable 应答）
        let set = session
            .query("SELECT Id FROM DH_User WHERE Id=?", &[DbValue::Int(7)])
            .unwrap();
        assert_eq!(set.rows[0].get(0), Some(&DbValue::Int(7)));

        // 执行
        let n = session
            .execute("UPDATE DH_User SET Enable=0 WHERE Id=?", &[DbValue::Int(7)])
            .unwrap();
        assert_eq!(n, 3);

        // 插入并取自增（远端原子接口；经连接池会话委派）
        let id = session
            .insert_and_get_identity(
                "INSERT INTO DH_User(Name) VALUES(?)",
                &[DbValue::Text("a".into())],
                None,
            )
            .unwrap();
        assert_eq!(id, 9);

        // 表结构探测：远端 GetTables（仅首次请求，之后走缓存）
        assert!(session.table_exists("dh_user").unwrap(), "表名匹配应忽略大小写");
        assert_eq!(session.table_columns("DH_User").unwrap(), vec!["Id", "Name"]);
        assert!(!session.table_exists("NotExists").unwrap());

        // 事务与裸取自增：明确不支持
        assert!(session.begin().is_err());
        assert!(session.last_identity().is_err());

        let requests = handle.join().unwrap();
        assert!(requests[0].contains("POST /Db/Login"), "{}", requests[0]);
        assert!(requests[0].contains("\"db\":\"Demo\""), "{}", requests[0]);
        assert!(requests[1].contains("POST /Db/Query"), "{}", requests[1]);
        // 占位符与参数键均为远端命名式（与 C# DbClient 一致）
        assert!(requests[1].contains("WHERE Id=@p0"), "{}", requests[1]);
        assert!(requests[1].contains("\\\"@p0\\\""), "{}", requests[1]);
        assert!(requests[2].contains("POST /Db/Execute"), "{}", requests[2]);
        assert!(
            requests[3].contains("POST /Db/InsertAndGetIdentity"),
            "{}",
            requests[3]
        );
        assert!(requests[4].contains("GET /Db/GetTables"), "{}", requests[4]);
    }
}
