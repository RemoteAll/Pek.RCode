//! MongoDB 驱动：把 XCode 生成的关系式 SQL **子集**翻译为集合/文档操作。
//!
//! DH.NCode 的 MongoDB 支持本身不提供 SQL（直接使用 MongoDB.Driver 操作集合）；
//! Pek.RCode 为了让对象实体（`Entity`）与表接口（`TableRef`）直接可用，
//! 实现了本仓库生成 SQL 形态的子集翻译：
//!
//! | SQL 形态 | MongoDB 操作 |
//! |----------|--------------|
//! | `INSERT INTO t (a,b) VALUES (?, ?)` | `insert_one`（`Id=0` 时交给 `_id` 自动生成） |
//! | `SELECT ... FROM t [WHERE ...] [ORDER BY ...] [LIMIT/OFFSET]` | `find`（含 $and/$or、$gt/$gte/$lt/$lte/$ne/$in/$regex） |
//! | `SELECT COUNT(*) FROM t [WHERE ...]` | `count_documents` |
//! | `UPDATE t SET a = ? [WHERE ...]` | `update_many`（$set） |
//! | `DELETE FROM t [WHERE ...]` | `delete_many` |
//!
//! 实现要点：
//! - 表 → collection、行 → document、列 → 字段；`_id` 与模型主键列（`Id`）互映射
//! - 事务：MongoDB 事务依赖会话（session），当前按无事务处理（`begin/commit/rollback` 为空操作）
//! - 无 DDL：collection 首次写入自动创建（`supports_ddl = false`）
//! - 无自增主键：插入返回 0（与 DH.NCode 一致）
//!
//! 线程模型：`mongodb` crate 为异步实现，本驱动通过共享 tokio 运行时 `block_on` 暴露同步接口，
//! **不要在 tokio 异步上下文中调用**。

use mongodb::bson::{Bson, Document, doc};
use mongodb::{Client, Collection};

use crate::dal::ConnectionString;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::rt::runtime;
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// MongoDB 会话。
pub struct MongoSession {
    /// 数据库句柄
    database: mongodb::Database,
}

/// 连接设置。
#[derive(Debug, Clone, PartialEq)]
struct MongoSettings {
    /// mongodb:// 连接串
    uri: String,
    /// 数据库名（必填）
    database: String,
}

impl MongoSession {
    /// 根据 XCode 风格连接串打开连接。
    pub fn open(conn_str: &ConnectionString) -> Result<Self> {
        let settings = parse_settings(conn_str)?;
        let client = runtime()?
            .block_on(async { Client::with_uri_str(&settings.uri).await })
            .map_err(|e| Error::Db(format!("连接 MongoDB 失败：{e}")))?;
        // 触发一次 ping 明确连通性（mongodb 驱动为惰性连接）
        runtime()?
            .block_on(async {
                client
                    .database("admin")
                    .run_command(doc! { "ping": 1 })
                    .await
            })
            .map_err(|e| Error::Db(format!("连接 MongoDB 失败（ping）：{e}")))?;
        Ok(Self {
            database: client.database(&settings.database),
        })
    }

    /// 取集合句柄。
    fn collection(&self, table: &str) -> Collection<Document> {
        self.database.collection(table)
    }
}

/// 解析连接串（与 XCode 键名兼容）。
fn parse_settings(cs: &ConnectionString) -> Result<MongoSettings> {
    // 已给出完整 mongodb:// 时透传
    if let Some(uri) = cs.get("uri").or(cs.get("connectionstring")) {
        let database = cs
            .get("database")
            .or(cs.get("db"))
            .ok_or_else(|| Error::Model("MongoDB 连接串缺少 Database".into()))?;
        return Ok(MongoSettings {
            uri: uri.to_string(),
            database: database.to_string(),
        });
    }

    let host = cs
        .get("server")
        .or(cs.get("host"))
        .or(cs.get("data source"))
        .unwrap_or("127.0.0.1");
    let port = cs.get("port").unwrap_or("27017");
    let database = cs
        .get("database")
        .or(cs.get("db"))
        .ok_or_else(|| Error::Model("MongoDB 连接串缺少 Database".into()))?;
    let user = cs.get("uid").or(cs.get("user")).or(cs.get("username"));
    let password = cs.get("pwd").or(cs.get("password")).unwrap_or("");

    let uri = match user {
        Some(user) => {
            let source = cs.get("authsource").unwrap_or("admin");
            format!("mongodb://{user}:{password}@{host}:{port}/?authSource={source}&appName=Pek.RCode")
        }
        None => format!("mongodb://{host}:{port}/?appName=Pek.RCode"),
    };

    Ok(MongoSettings {
        uri,
        database: database.to_string(),
    })
}

/// 构建单键文档。
fn doc_kv(key: impl Into<String>, value: impl Into<Bson>) -> Document {
    let mut document = Document::new();
    document.insert(key, value);
    document
}

/// `DbValue` → BSON。
fn dbvalue_to_bson(value: &DbValue) -> Bson {
    match value {
        DbValue::Null => Bson::Null,
        DbValue::Bool(v) => Bson::Boolean(*v),
        DbValue::Int(v) => {
            if let Ok(small) = i32::try_from(*v) {
                Bson::Int32(small)
            } else {
                Bson::Int64(*v)
            }
        }
        DbValue::Float(v) => Bson::Double(*v),
        // DECIMAL 降级为双精度（BSON 无十进制类型；需要精确金额时请用字符串列）
        DbValue::Decimal(v) => Bson::Double(v.to_string().parse().unwrap_or(0.0)),
        DbValue::Text(v) => Bson::String(v.clone()),
        DbValue::Blob(v) => Bson::Binary(mongodb::bson::Binary {
            subtype: mongodb::bson::spec::BinarySubtype::Generic,
            bytes: v.clone(),
        }),
        // bson 2.x 在未启用 chrono 特性时不暴露 from_chrono，统一用毫秒时间戳互转
        DbValue::DateTime(v) => Bson::DateTime(mongodb::bson::DateTime::from_millis(
            v.and_utc().timestamp_millis(),
        )),
    }
}

/// BSON → `DbValue`。
fn bson_to_dbvalue(value: &Bson) -> DbValue {
    match value {
        Bson::Null | Bson::Undefined => DbValue::Null,
        Bson::Boolean(v) => DbValue::Bool(*v),
        Bson::Int32(v) => DbValue::Int(i64::from(*v)),
        Bson::Int64(v) => DbValue::Int(*v),
        Bson::Double(v) => DbValue::Float(*v),
        Bson::String(v) => DbValue::Text(v.clone()),
        Bson::Binary(v) => DbValue::Blob(v.bytes.clone()),
        Bson::DateTime(v) => {
            let millis = v.timestamp_millis();
            chrono::DateTime::from_timestamp_millis(millis)
                .map(|dt| DbValue::DateTime(dt.naive_utc()))
                .unwrap_or(DbValue::Null)
        }
        Bson::ObjectId(v) => DbValue::Text(v.to_hex()),
        other => DbValue::Text(other.to_string()),
    }
}

/// SQL 词法单元。
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// 标识符（引号已剥离）
    Ident(String),
    /// 参数占位符 `?`
    Param,
    /// 字符串/数字字面量
    Literal(DbValue),
    /// 运算符 / 关键字（大写）
    Word(String),
}

/// 词法分析（识别引号标识符、字符串、数字、占位符与关键字）。
fn tokenize(sql: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        match ch {
            '(' => {
                tokens.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                tokens.push(Tok::Comma);
                i += 1;
            }
            '?' => {
                tokens.push(Tok::Param);
                i += 1;
            }
            '"' | '`' | '[' => {
                let close = match ch {
                    '[' => ']',
                    other => other,
                };
                let mut text = String::new();
                i += 1;
                while i < chars.len() {
                    let c = chars[i];
                    if c == close {
                        // 双写引号转义
                        if close != ']' && i + 1 < chars.len() && chars[i + 1] == close {
                            text.push(c);
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    text.push(c);
                    i += 1;
                }
                i += 1; // 跳过闭合
                tokens.push(Tok::Ident(text));
            }
            '\'' => {
                let mut text = String::new();
                i += 1;
                while i < chars.len() {
                    let c = chars[i];
                    if c == '\'' {
                        if i + 1 < chars.len() && chars[i + 1] == '\'' {
                            text.push('\'');
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    text.push(c);
                    i += 1;
                }
                i += 1;
                tokens.push(Tok::Literal(DbValue::Text(text)));
            }
            c if c.is_ascii_digit() || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) => {
                let mut text = String::new();
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    text.push(chars[i]);
                    i += 1;
                }
                let value = if text.contains('.') {
                    DbValue::Float(text.parse().map_err(|e| {
                        Error::Db(format!("Mongo 翻译：无法解析数字 {text}：{e}"))
                    })?)
                } else {
                    DbValue::Int(text.parse().map_err(|e| {
                        Error::Db(format!("Mongo 翻译：无法解析数字 {text}：{e}"))
                    })?)
                };
                tokens.push(Tok::Literal(value));
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' => {
                let mut text = String::new();
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    text.push(chars[i]);
                    i += 1;
                }
                // 运算符组合（>= <= <> !=）
                if i < chars.len() && matches!(chars[i], '>' | '<' | '!' | '=') {
                    let mut op = text.clone();
                    op.push(chars[i]);
                    i += 1;
                    if i < chars.len() && chars[i] == '=' {
                        op.push('=');
                        i += 1;
                    }
                    tokens.push(Tok::Word(op.to_uppercase()));
                    continue;
                }
                tokens.push(Tok::Word(text.to_uppercase()));
            }
            '=' | '>' | '<' | '!' => {
                let mut op = String::new();
                op.push(ch);
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    op.push('=');
                    i += 1;
                }
                tokens.push(Tok::Word(op));
            }
            '*' => {
                // COUNT(*) / SELECT *
                tokens.push(Tok::Word("*".into()));
                i += 1;
            }
            other => {
                return Err(Error::Db(format!(
                    "Mongo 翻译：不支持的字符 '{other}'（SQL：{sql}）"
                )));
            }
        }
    }
    Ok(tokens)
}

/// 词法流 + 参数游标。
struct Parser<'a> {
    tokens: Vec<Tok>,
    pos: usize,
    params: &'a [DbValue],
    param_index: usize,
}

impl<'a> Parser<'a> {
    /// 新建解析器。
    fn new(tokens: Vec<Tok>, params: &'a [DbValue]) -> Self {
        Self {
            tokens,
            pos: 0,
            params,
            param_index: 0,
        }
    }

    /// 下一个参数值。
    fn next_param(&mut self) -> Result<DbValue> {
        let value = self
            .params
            .get(self.param_index)
            .ok_or_else(|| Error::Db("Mongo 翻译：参数数量不足".into()))?;
        self.param_index += 1;
        Ok(value.clone())
    }

    /// 当前 token。
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    /// 吃掉当前 token。
    fn bump(&mut self) -> Option<Tok> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    /// 匹配一个关键字并前进。
    fn eat_word(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Word(w)) if w == word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// 期望一个关键字。
    fn expect_word(&mut self, word: &str) -> Result<()> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(Error::Db(format!(
                "Mongo 翻译：期待关键字 {word}，实际 {:?}",
                self.peek()
            )))
        }
    }

    /// 期望一个标识符。
    fn expect_ident(&mut self) -> Result<String> {
        match self.bump() {
            Some(Tok::Ident(name)) => Ok(name),
            other => Err(Error::Db(format!(
                "Mongo 翻译：期待标识符，实际 {other:?}"
            ))),
        }
    }

    /// 期待 `(`。
    fn expect_lparen(&mut self) -> Result<()> {
        if matches!(self.bump(), Some(Tok::LParen)) {
            Ok(())
        } else {
            Err(Error::Db("Mongo 翻译：期待 (".into()))
        }
    }

    /// 期待 `)`。
    fn expect_rparen(&mut self) -> Result<()> {
        if matches!(self.bump(), Some(Tok::RParen)) {
            Ok(())
        } else {
            Err(Error::Db("Mongo 翻译：期待 )".into()))
        }
    }

    /// 参数或字面量。
    fn value(&mut self) -> Result<DbValue> {
        match self.bump() {
            Some(Tok::Param) => self.next_param(),
            Some(Tok::Literal(value)) => Ok(value),
            other => Err(Error::Db(format!(
                "Mongo 翻译：期待参数或字面量，实际 {other:?}"
            ))),
        }
    }

    /// 解析布尔表达式（AND/OR/括号）为查询文档。
    fn filter(&mut self) -> Result<Document> {
        let mut current = self.primary_filter()?;
        loop {
            if self.eat_word("AND") {
                let right = self.primary_filter()?;
                current = merge_filter(current, right, "$and");
            } else if self.eat_word("OR") {
                let right = self.primary_filter()?;
                current = merge_filter(current, right, "$or");
            } else {
                break;
            }
        }
        Ok(current)
    }

    /// 单个比较条件 / 括号表达式。
    fn primary_filter(&mut self) -> Result<Document> {
        if matches!(self.peek(), Some(Tok::LParen)) {
            self.bump();
            let inner = self.filter()?;
            self.expect_rparen()?;
            return Ok(inner);
        }

        let column = self.expect_ident()?;

        // IS [NOT] NULL
        if self.eat_word("IS") {
            let not = self.eat_word("NOT");
            self.expect_word("NULL")?;
            return Ok(if not {
                doc_kv(column, doc_kv("$ne", Bson::Null))
            } else {
                doc_kv(column, Bson::Null)
            });
        }

        // [NOT] IN / [NOT] LIKE / 比较运算
        let mut negate = false;
        if self.eat_word("NOT") {
            negate = true;
        }

        if self.eat_word("IN") {
            self.expect_lparen()?;
            let mut values = Vec::new();
            loop {
                values.push(dbvalue_to_bson(&self.value()?));
                if matches!(self.peek(), Some(Tok::Comma)) {
                    self.bump();
                    continue;
                }
                break;
            }
            self.expect_rparen()?;
            let key = if negate { "$nin" } else { "$in" };
            return Ok(doc_kv(column, doc_kv(key, values)));
        }

        if self.eat_word("BETWEEN") {
            let low = self.value()?;
            self.expect_word("AND")?;
            let high = self.value()?;
            let bounds = doc_kv("$gte", dbvalue_to_bson(&low));
            let mut bounds = bounds;
            bounds.insert("$lte", dbvalue_to_bson(&high));
            return Ok(doc_kv(column, bounds));
        }

        if self.eat_word("LIKE") {
            let pattern = self.value()?.to_text();
            let regex = like_to_regex(&pattern);
            return Ok(if negate {
                doc_kv(column, doc_kv("$not", doc_kv("$regex", regex)))
            } else {
                doc_kv(column, doc_kv("$regex", regex))
            });
        }

        let op = match self.bump() {
            Some(Tok::Word(op)) => op,
            other => {
                return Err(Error::Db(format!(
                    "Mongo 翻译：期待比较运算符，实际 {other:?}"
                )));
            }
        };
        let value = self.value()?;
        let bson = dbvalue_to_bson(&value);

        let mongo_op = match op.as_str() {
            "=" => None,
            "<>" | "!=" => Some("$ne"),
            ">" => Some("$gt"),
            ">=" => Some("$gte"),
            "<" => Some("$lt"),
            "<=" => Some("$lte"),
            other => {
                return Err(Error::Unsupported(format!(
                    "Mongo 翻译暂不支持运算符 {other}"
                )));
            }
        };
        let condition = match mongo_op {
            None => doc_kv(column, bson),
            Some(op) => doc_kv(column, doc_kv(op, bson)),
        };
        Ok(condition)
    }
}
/// 合并两个过滤文档（AND → 平铺字段；OR → $or 数组）。
fn merge_filter(left: Document, right: Document, combinator: &str) -> Document {
    if combinator == "$and" {
        // 字段不冲突时直接平铺，减少嵌套
        let conflict = left.keys().any(|key| right.contains_key(key));
        if !conflict {
            let mut merged = left;
            for (key, value) in right {
                merged.insert(key, value);
            }
            return merged;
        }
    }
    doc_kv(combinator, vec![Bson::Document(left), Bson::Document(right)])
}

/// SQL LIKE 模式 → 正则（`%`→`.*`、`_`→`.`，其余转义）。
fn like_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '%' => regex.push_str(".*"),
            '_' => regex.push('.'),
            other => regex.push_str(&escape_regex_char(other)),
        }
    }
    regex.push('$');
    regex
}

/// 正则元字符转义。
fn escape_regex_char(ch: char) -> String {
    match ch {
        '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
            format!("\\{ch}")
        }
        other => other.to_string(),
    }
}

/// INSERT 解析结果。
struct InsertPlan {
    /// 集合名
    table: String,
    /// 列与值
    fields: Vec<(String, DbValue)>,
}

/// SELECT 解析结果。
struct SelectPlan {
    /// 集合名
    table: String,
    /// 是否是 `COUNT(*)`
    count: bool,
    /// 投影列（空表示全部）
    projection: Vec<String>,
    /// 过滤条件
    filter: Document,
    /// 排序（列名, 是否降序）
    sort: Option<(String, bool)>,
    /// LIMIT
    limit: Option<i64>,
    /// OFFSET
    skip: Option<u64>,
}

/// UPDATE 解析结果。
struct UpdatePlan {
    /// 集合名
    table: String,
    /// 待更新列
    sets: Vec<(String, DbValue)>,
    /// 过滤条件
    filter: Document,
}

/// 解析一条 SQL（仅支持本仓库生成的形态）。
enum Statement {
    /// 插入
    Insert(InsertPlan),
    /// 查询
    Select(SelectPlan),
    /// 更新
    Update(UpdatePlan),
    /// 删除
    Delete {
        /// 集合名
        table: String,
        /// 过滤条件
        filter: Document,
    },
}

/// 解析 SQL。
fn translate(sql: &str, params: &[DbValue]) -> Result<Statement> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser::new(tokens, params);

    // 首个关键字
    let Some(Tok::Word(first)) = parser.peek().cloned() else {
        return Err(Error::Db("Mongo 翻译：空语句".into()));
    };
    match first.as_str() {
        "INSERT" => {
            parser.bump();
            parser.expect_word("INTO")?;
            let table = parser.expect_ident()?;
            parser.expect_lparen()?;
            let mut columns = Vec::new();
            loop {
                columns.push(parser.expect_ident()?);
                if matches!(parser.peek(), Some(Tok::Comma)) {
                    parser.bump();
                    continue;
                }
                break;
            }
            parser.expect_rparen()?;
            parser.expect_word("VALUES")?;
            parser.expect_lparen()?;
            let mut fields = Vec::new();
            let mut index = 0usize;
            loop {
                let value = parser.value()?;
                let name = columns
                    .get(index)
                    .ok_or_else(|| Error::Db("Mongo 翻译：INSERT 列数与值数不匹配".into()))?
                    .clone();
                fields.push((name, value));
                index += 1;
                if matches!(parser.peek(), Some(Tok::Comma)) {
                    parser.bump();
                    continue;
                }
                break;
            }
            parser.expect_rparen()?;
            Ok(Statement::Insert(InsertPlan { table, fields }))
        }
        "SELECT" => {
            parser.bump();
            // COUNT(*) 或列清单
            let mut count = false;
            let mut projection = Vec::new();
            if matches!(parser.peek(), Some(Tok::Word(w)) if w == "COUNT") {
                parser.bump();
                parser.expect_lparen()?;
                parser.bump(); // *
                parser.expect_rparen()?;
                count = true;
            } else if matches!(parser.peek(), Some(Tok::Word(w)) if w == "*") {
                parser.bump();
            } else {
                loop {
                    projection.push(parser.expect_ident()?);
                    if matches!(parser.peek(), Some(Tok::Comma)) {
                        parser.bump();
                        continue;
                    }
                    break;
                }
            }
            parser.expect_word("FROM")?;
            let table = parser.expect_ident()?;

            let mut filter = Document::new();
            if parser.eat_word("WHERE") {
                filter = parser.filter()?;
            }

            let mut sort = None;
            if parser.eat_word("ORDER") {
                parser.expect_word("BY")?;
                let column = parser.expect_ident()?;
                let desc = parser.eat_word("DESC");
                if !desc {
                    parser.eat_word("ASC");
                }
                sort = Some((column, desc));
            }

            let mut limit = None;
            let mut skip = None;
            if parser.eat_word("LIMIT")
                && let Tok::Literal(DbValue::Int(v)) = parser.bump().unwrap_or(Tok::Comma)
            {
                limit = Some(v);
            }
            if parser.eat_word("OFFSET")
                && let Tok::Literal(DbValue::Int(v)) = parser.bump().unwrap_or(Tok::Comma)
            {
                skip = Some(v.max(0) as u64);
            }

            Ok(Statement::Select(SelectPlan {
                table,
                count,
                projection,
                filter,
                sort,
                limit,
                skip,
            }))
        }
        "UPDATE" => {
            parser.bump();
            let table = parser.expect_ident()?;
            parser.expect_word("SET")?;
            let mut sets = Vec::new();
            loop {
                let column = parser.expect_ident()?;
                let op = parser.bump();
                if !matches!(&op, Some(Tok::Word(w)) if w == "=") {
                    return Err(Error::Db(format!("Mongo 翻译：SET 期待 =，实际 {op:?}")));
                }
                let value = parser.value()?;
                sets.push((column, value));
                if matches!(parser.peek(), Some(Tok::Comma)) {
                    parser.bump();
                    continue;
                }
                break;
            }
            let mut filter = Document::new();
            if parser.eat_word("WHERE") {
                filter = parser.filter()?;
            }
            Ok(Statement::Update(UpdatePlan { table, sets, filter }))
        }
        "DELETE" => {
            parser.bump();
            parser.expect_word("FROM")?;
            let table = parser.expect_ident()?;
            let mut filter = Document::new();
            if parser.eat_word("WHERE") {
                filter = parser.filter()?;
            }
            Ok(Statement::Delete { table, filter })
        }
        other => Err(Error::Unsupported(format!(
            "MongoDB 翻译暂不支持语句：{other}（支持 INSERT/SELECT/UPDATE/DELETE/COUNT）"
        ))),
    }
}

/// 文档 → 行（`_id` 映射回 `Id`，值按 BSON 转换）。
fn document_to_row(document: &Document, columns: &[String]) -> Vec<DbValue> {
    columns
        .iter()
        .map(|column| {
            let key = if column == "Id" { "_id" } else { column.as_str() };
            document
                .get(key)
                .map(bson_to_dbvalue)
                .unwrap_or(DbValue::Null)
        })
        .collect()
}

/// 驱动错误 → 统一错误。
fn map_err(e: mongodb::error::Error) -> Error {
    Error::Db(format!("MongoDB 错误：{e}"))
}

impl SqlSession for MongoSession {
    fn kind(&self) -> DatabaseKind {
        DatabaseKind::MongoDb
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let statement = translate(sql, params)?;
        let runtime = runtime()?;
        match statement {
            Statement::Insert(plan) => {
                let mut document = Document::new();
                let mut has_id = false;
                for (column, value) in &plan.fields {
                    if column.eq_ignore_ascii_case("Id") {
                        has_id = true;
                        // Id=0 视为未指定：交给 MongoDB 生成 _id（无自增语义，回写值为 0）
                        if matches!(value, DbValue::Int(0)) {
                            continue;
                        }
                        document.insert("_id", dbvalue_to_bson(value));
                        continue;
                    }
                    document.insert(column, dbvalue_to_bson(value));
                }
                let _ = has_id;
                let collection = self.collection(&plan.table);
                runtime
                    .block_on(async { collection.insert_one(&document).await })
                    .map_err(map_err)?;
                Ok(1)
            }
            Statement::Select(plan) => {
                // SELECT 通过 execute 走不到（DAL 使用 query），这里给出明确提示
                Err(Error::Unsupported(format!(
                    "MongoDB 的 SELECT 请通过 query 接口调用（表：{}）",
                    plan.table
                )))
            }
            Statement::Update(plan) => {
                let mut set = Document::new();
                for (column, value) in &plan.sets {
                    set.insert(column, dbvalue_to_bson(value));
                }
                let collection = self.collection(&plan.table);
                let result = runtime
                    .block_on(async {
                        collection
                            .update_many(plan.filter, doc! { "$set": set })
                            .await
                    })
                    .map_err(map_err)?;
                Ok(result.modified_count)
            }
            Statement::Delete { table, filter } => {
                let collection = self.collection(&table);
                let result = runtime
                    .block_on(async { collection.delete_many(filter).await })
                    .map_err(map_err)?;
                Ok(result.deleted_count)
            }
        }
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let statement = translate(sql, params)?;
        let Statement::Select(plan) = statement else {
            return Err(Error::Db("MongoDB 查询接口仅支持 SELECT".into()));
        };
        let runtime = runtime()?;
        let collection = self.collection(&plan.table);

        if plan.count {
            let total = runtime
                .block_on(async { collection.count_documents(plan.filter).await })
                .map_err(map_err)?;
            let mut set = RowSet::new(vec!["COUNT(*)".to_string()]);
            set.push(vec![DbValue::Int(total as i64)]);
            return Ok(set);
        }

        let mut find = collection.find(plan.filter);
        if !plan.projection.is_empty() {
            let mut projection = Document::new();
            let mut has_id = false;
            for column in &plan.projection {
                if column.eq_ignore_ascii_case("Id") {
                    has_id = true;
                    projection.insert("_id", 1);
                } else {
                    projection.insert(column, 1);
                }
            }
            if !has_id {
                projection.insert("_id", 0);
            }
            find = find.projection(projection);
        }
        if let Some((column, desc)) = &plan.sort {
            let key = if column.eq_ignore_ascii_case("Id") {
                "_id".to_string()
            } else {
                column.clone()
            };
            find = find.sort(doc_kv(key, if *desc { -1 } else { 1 }));
        }
        if let Some(skip) = plan.skip {
            find = find.skip(skip);
        }
        if let Some(limit) = plan.limit {
            find = find.limit(limit);
        }

        let documents: Vec<Document> = runtime
            .block_on(async {
                use futures_util::TryStreamExt as _;
                find.await?.try_collect().await
            })
            .map_err(map_err)?;

        // 列清单：优先显式投影，否则取首文档键序（`_id` → `Id`）
        let columns: Vec<String> = if !plan.projection.is_empty() {
            plan.projection.clone()
        } else if let Some(first) = documents.first() {
            first
                .keys()
                .map(|key| {
                    if key == "_id" {
                        "Id".to_string()
                    } else {
                        key.clone()
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut set = RowSet::new(columns.clone());
        for document in documents {
            set.push(document_to_row(&document, &columns));
        }
        Ok(set)
    }

    fn begin(&mut self) -> Result<()> {
        // MongoDB 事务依赖会话对象，当前按无事务处理
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        Ok(())
    }

    fn last_identity(&mut self) -> Result<i64> {
        // MongoDB 使用 ObjectId，无自增数字（与 DH.NCode 一致返回 0）
        Ok(0)
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let runtime = runtime()?;
        let names = runtime
            .block_on(async { self.database.list_collection_names().await })
            .map_err(map_err)?;
        Ok(names.iter().any(|name| name.eq_ignore_ascii_case(table)))
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let runtime = runtime()?;
        let collection = self.collection(table);
        let sample = runtime
            .block_on(async { collection.find_one(doc! {}).await })
            .map_err(map_err)?;
        Ok(sample
            .map(|document| {
                document
                    .keys()
                    .map(|key| {
                        if key == "_id" {
                            "Id".to_string()
                        } else {
                            key.clone()
                        }
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(values: &[DbValue]) -> Vec<DbValue> {
        values.to_vec()
    }

    #[test]
    fn insert_translation() {
        let sql = "INSERT INTO \"DH_Order\" (\"Code\", \"Amount\") VALUES (?, ?)";
        let values = params(&[DbValue::Text("A-001".into()), DbValue::Int(5)]);
        match translate(sql, &values).unwrap() {
            Statement::Insert(plan) => {
                assert_eq!(plan.table, "DH_Order");
                assert_eq!(plan.fields.len(), 2);
                assert_eq!(plan.fields[0].0, "Code");
                assert_eq!(plan.fields[0].1, DbValue::Text("A-001".into()));
            }
            other => panic!("应为 INSERT：{:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn select_translation_with_filter_sort_paging() {
        let sql = "SELECT * FROM \"DH_Order\" WHERE ((\"Code\" = ?) AND (\"Amount\" > ?)) ORDER BY \"Id\" DESC LIMIT 10 OFFSET 20";
        let values = params(&[
            DbValue::Text("A-001".into()),
            DbValue::Int(3),
        ]);
        match translate(sql, &values).unwrap() {
            Statement::Select(plan) => {
                assert_eq!(plan.table, "DH_Order");
                assert_eq!(plan.filter.len(), 2);
                assert_eq!(
                    plan.filter.get("Code"),
                    Some(&Bson::String("A-001".into()))
                );
                assert_eq!(plan.limit, Some(10));
                assert_eq!(plan.skip, Some(20));
                assert_eq!(plan.sort, Some(("Id".to_string(), true)));
            }
            _ => panic!("应为 SELECT"),
        }
    }

    #[test]
    fn count_like_in_between_null_translation() {
        let sql = "SELECT COUNT(*) FROM \"t\" WHERE (\"Code\" LIKE ?)";
        let values = params(&[DbValue::Text("A-%".into())]);
        match translate(sql, &values).unwrap() {
            Statement::Select(plan) => {
                assert!(plan.count);
                let code = plan.filter.get("Code").unwrap();
                let doc = code.as_document().unwrap();
                assert_eq!(
                    doc.get("$regex"),
                    Some(&Bson::String("^A-.*$".into()))
                );
            }
            _ => panic!("应为 SELECT COUNT"),
        }

        let sql = "SELECT * FROM \"t\" WHERE (\"Status\" IN (?, ?) AND \"Ok\" IS NOT NULL)";
        let values = params(&[DbValue::Int(1), DbValue::Int(2)]);
        assert!(translate(sql, &values).is_ok());

        let sql = "SELECT * FROM \"t\" WHERE (\"Amount\" BETWEEN ? AND ?)";
        let values = params(&[DbValue::Int(1), DbValue::Int(9)]);
        assert!(translate(sql, &values).is_ok());
    }

    #[test]
    fn update_delete_translation() {
        let sql = "UPDATE \"t\" SET \"Code\" = ? WHERE (\"Id\" = ?)";
        let values = params(&[DbValue::Text("B".into()), DbValue::Int(7)]);
        match translate(sql, &values).unwrap() {
            Statement::Update(plan) => {
                assert_eq!(plan.sets.len(), 1);
                assert_eq!(plan.filter.get("Id"), Some(&Bson::Int32(7)));
            }
            _ => panic!("应为 UPDATE"),
        }

        let sql = "DELETE FROM \"t\" WHERE (\"Id\" = ?)";
        let values = params(&[DbValue::Int(7)]);
        assert!(matches!(
            translate(sql, &values).unwrap(),
            Statement::Delete { .. }
        ));
    }

    #[test]
    fn settings_parsing() {
        let cs = ConnectionString::parse(
            "Server=mongo.local;Port=27017;Database=wms;Uid=app;Pwd=***;provider=mongodb",
        );
        let s = parse_settings(&cs).unwrap();
        assert_eq!(s.database, "wms");
        assert!(s.uri.contains("app:***@mongo.local:27017"), "{}", s.uri);

        let cs = ConnectionString::parse("Database=wms;provider=mongo");
        let s = parse_settings(&cs).unwrap();
        assert!(s.uri.starts_with("mongodb://127.0.0.1:27017"), "{}", s.uri);

        assert!(parse_settings(&ConnectionString::parse("Server=x;provider=mongodb")).is_err());
    }
}
