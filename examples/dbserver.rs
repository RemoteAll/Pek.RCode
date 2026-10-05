//! 网络数据库服务端示例（对应 C# `DbServer`）：把 [`pek_rcode::db_service::DbService`] 挂到极简 HTTP 宿主。
//!
//! 协议与 C# `DbController` 对齐：
//!
//! | 路由 | 方法 | 说明 |
//! |---|---|---|
//! | `/Db/Login` | POST | 登录（`{db, token}`），返回 `{DbType, Version}` |
//! | `/Db/Query` | POST | 查询，返回 **DbTable v3 二进制**（C# `DbClient.QueryAsync` 可直接解析） |
//! | `/Db/Execute` | POST | 执行，返回 `{data: 行数}` |
//! | `/Db/InsertAndGetIdentity` | POST | 插入并返回自增 ID，返回 `{data: id}` |
//! | `/Db/QueryCount` | GET | 单表记录数（`tableName`/`db`/`token`），返回 `{data: n}` |
//! | `/Db/GetTables` | GET | 表结构（`db`/`token`），返回 `[{Name, Columns:[{Name}]}]` |
//!
//! 运行：
//!
//! ```powershell
//! # 服务端（一个进程服务一个数据库连接串；默认仅回环绑定，供本机驱动宿主拉起）
//! cargo run --release --example dbserver -- "Data Source=demo.db;Provider=SQLite" 3305 tk123
//! # 端口 0 = 自动分配；--host 0.0.0.0 = 对外提供服务（默认 127.0.0.1）
//! cargo run --release --example dbserver -- "Server=db;Database=x;Provider=MySql" 0 tk123
//! # 就绪后额外输出单行 JSON（驱动宿主解析用）：
//! # {"event":"ready","addr":"127.0.0.1:3305","kind":"MySql","protocol":1}
//! # 驱动宿主（`pek_rcode::driver_pack`）会附加 --watch-stdin：调用方退出（stdin EOF）时自动退出
//! # 按驱动裁剪的驱动包构建：见 scripts/pack-drivers.ps1
//! ```
//!
//! 客户端连接串（C# 或 Rust 均可用）：
//!
//! ```text
//! Server=http://127.0.0.1:3305;Database=Demo;Password=tk123;provider=network
//! ```
//!
//! 说明：本示例用标准库手写 HTTP/1.1，无第三方依赖，仅用于演示与联调；
//! 生产环境可将 [`DbService`] 挂到 axum/actix 等框架的路由（服务层与框架无耦合）。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{Value, json};

use pek_rcode::dal::Dal;
use pek_rcode::db_service::DbService;

fn main() {
    // 参数：<连接串> [端口=3305] [令牌] [--host 127.0.0.1] [--watch-stdin]
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut host = "127.0.0.1".to_string();
    if let Some(i) = args.iter().position(|a| a == "--host") {
        if i + 1 < args.len() {
            host = args.remove(i + 1);
        }
        args.remove(i);
    }
    if let Some(i) = args.iter().position(|a| a.starts_with("--host=")) {
        host = args[i]["--host=".len()..].to_string();
        args.remove(i);
    }
    let watch_stdin = if let Some(i) = args.iter().position(|a| a == "--watch-stdin") {
        args.remove(i);
        true
    } else {
        false
    };
    let conn = args.first().cloned().unwrap_or_default();
    if conn.is_empty() {
        eprintln!("用法：dbserver <连接串> [端口=3305] [令牌] [--host 127.0.0.1] [--watch-stdin]");
        eprintln!("示例：dbserver \"Data Source=demo.db;Provider=SQLite\" 3305 tk123");
        eprintln!("说明：端口 0 = 自动分配；--host 默认 127.0.0.1（仅本机）；");
        eprintln!("      --watch-stdin：调用方 stdin 关闭（EOF）时自动退出（驱动宿主防孤儿）。");
        std::process::exit(2);
    }
    let port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3305);
    let token = args.get(2).cloned().unwrap_or_default();

    if watch_stdin {
        // 父进程（驱动宿主调用方，如 `pek_rcode::driver_pack` 的 DriverManager）退出后
        // stdin 读到 EOF → 自动退出，防止强杀场景下的孤儿进程。手动运行时不会触发。
        std::thread::spawn(|| {
            use std::io::Read;
            let stdin = std::io::stdin();
            let mut lock = stdin.lock();
            let mut buf = [0u8; 256];
            loop {
                match lock.read(&mut buf) {
                    Ok(0) => break,    // EOF：调用方已退出
                    Ok(_) => continue, // 忽略输入内容
                    Err(_) => break,
                }
            }
            std::process::exit(0);
        });
    }

    let dal = Arc::new(Dal::open(&conn).expect("连接串无效"));

    let mut service = DbService::new();
    if !token.is_empty() {
        // 空库列表 = 允许访问所有库（对齐 C# `DbService.Tokens`）
        service.set_token(&token, &[]);
    }
    let service = Arc::new(service);

    let listener = TcpListener::bind((host.as_str(), port)).expect("端口占用或权限不足");
    let local = listener.local_addr().expect("读取监听地址失败");
    let kind = dal.kind().name().to_string();
    println!(
        "DbServer 已启动：http://{local}（数据库类型 {kind}，令牌 {}）",
        if token.is_empty() { "<未设置>" } else { "<已设置>" }
    );
    // 就绪行（驱动宿主解析用；稳定单行 JSON 协议 v1）
    println!("{{\"event\":\"ready\",\"addr\":\"{local}\",\"kind\":\"{kind}\",\"protocol\":1}}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let dal = Arc::clone(&dal);
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &dal, &service) {
                eprintln!("请求处理失败：{e}");
            }
        });
    }
}

/// 读取并处理单个 HTTP 请求。
fn handle(mut stream: TcpStream, dal: &Dal, service: &DbService) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();

    // ── 请求行与请求头
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.trim().split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        if header.trim().is_empty() {
            break;
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    // ── 请求体
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    // ── 路由
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.clone(), BTreeMap::new()),
    };

    match (method.as_str(), path.as_str()) {
        ("POST", "/Db/Login") => {
            let args = parse_json_body(&body);
            let db = arg_str(&args, "db");
            let token = arg_str(&args, "token");
            match service.validate_token(&token, &db) {
                Ok(()) => {
                    let info = service.login_info(dal);
                    let body = json!({ "DbType": info.db_type, "Version": info.version });
                    respond_json(&mut stream, 200, &body)
                }
                Err(e) => respond_error(&mut stream, 401, &e.to_string()),
            }
        }
        ("POST", "/Db/Query") => {
            let args = parse_json_body(&body);
            let db = arg_str(&args, "db");
            let token = arg_str(&args, "token");
            if let Err(e) = service.validate_token(&token, &db) {
                return respond_error(&mut stream, 401, &e.to_string());
            }
            let sql = arg_str(&args, "sql");
            let parameters = arg_parameters(&args);
            match service.query_packet(dal, &sql, parameters.as_ref()) {
                Ok(packet) => respond(&mut stream, 200, "application/octet-stream", &packet),
                Err(e) => respond_error(&mut stream, 500, &e.to_string()),
            }
        }
        ("POST", "/Db/Execute") => {
            let args = parse_json_body(&body);
            let db = arg_str(&args, "db");
            let token = arg_str(&args, "token");
            if let Err(e) = service.validate_token(&token, &db) {
                return respond_error(&mut stream, 401, &e.to_string());
            }
            let sql = arg_str(&args, "sql");
            let parameters = arg_parameters(&args);
            match service.execute(dal, &sql, parameters.as_ref()) {
                Ok(n) => respond_json(&mut stream, 200, &json!({ "data": n })),
                Err(e) => respond_error(&mut stream, 500, &e.to_string()),
            }
        }
        ("POST", "/Db/InsertAndGetIdentity") => {
            let args = parse_json_body(&body);
            let db = arg_str(&args, "db");
            let token = arg_str(&args, "token");
            if let Err(e) = service.validate_token(&token, &db) {
                return respond_error(&mut stream, 401, &e.to_string());
            }
            let sql = arg_str(&args, "sql");
            let parameters = arg_parameters(&args);
            match service.insert_and_get_identity(dal, &sql, parameters.as_ref()) {
                Ok(id) => respond_json(&mut stream, 200, &json!({ "data": id })),
                Err(e) => respond_error(&mut stream, 500, &e.to_string()),
            }
        }
        ("GET", "/Db/QueryCount") => {
            let db = query.get("db").cloned().unwrap_or_default();
            let token = query.get("token").cloned().unwrap_or_default();
            if let Err(e) = service.validate_token(&token, &db) {
                return respond_error(&mut stream, 401, &e.to_string());
            }
            let table = query.get("tableName").cloned().unwrap_or_default();
            match service.query_count(dal, &table) {
                Ok(n) => respond_json(&mut stream, 200, &json!({ "data": n })),
                Err(e) => respond_error(&mut stream, 500, &e.to_string()),
            }
        }
        ("GET", "/Db/GetTables") => {
            let db = query.get("db").cloned().unwrap_or_default();
            let token = query.get("token").cloned().unwrap_or_default();
            if let Err(e) = service.validate_token(&token, &db) {
                return respond_error(&mut stream, 401, &e.to_string());
            }
            match service.get_tables(dal) {
                Ok(tables) => {
                    let list: Vec<Value> = tables
                        .iter()
                        .map(|t| {
                            let columns: Vec<Value> = t
                                .columns
                                .iter()
                                .map(|c| json!({ "Name": t.effective_column_name(c) }))
                                .collect();
                            json!({ "Name": t.effective_table_name(), "Columns": columns })
                        })
                        .collect();
                    respond_json(&mut stream, 200, &Value::Array(list))
                }
                Err(e) => respond_error(&mut stream, 500, &e.to_string()),
            }
        }
        _ => respond_error(&mut stream, 404, "未知路由（支持 /Db/Login、/Db/Query、/Db/Execute、/Db/InsertAndGetIdentity、/Db/QueryCount、/Db/GetTables）"),
    }
}

/// 解析 JSON 请求体（非法时返回空对象，交由后续参数校验报错）。
fn parse_json_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Object(serde_json::Map::new()))
}

/// 取字符串参数。
fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 取参数字典：兼容 JSON 字符串（C#/Rust 客户端）与直接对象两种形式。
fn arg_parameters(args: &Value) -> Option<BTreeMap<String, Value>> {
    let value = args.get("parameters")?;
    if value.is_null() {
        return None;
    }
    let parsed: Value = match value {
        Value::String(text) if !text.is_empty() => serde_json::from_str(text).ok()?,
        Value::String(_) => return None,
        other => other.clone(),
    };
    parsed
        .as_object()
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// 解析查询字符串（`a=1&b=2`，值做百分号解码）。
fn parse_query(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

/// 百分号解码（`+` 按空格处理）。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 输出 JSON 应答。
fn respond_json(stream: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    respond(stream, status, "application/json", body.to_string().as_bytes())
}

/// 输出错误应答（NewLife 信封 `{code, msg}`）。
fn respond_error(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let body = json!({ "code": status, "msg": message });
    respond(stream, status, "application/json", body.to_string().as_bytes())
}

/// 输出 HTTP 应答。
fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
