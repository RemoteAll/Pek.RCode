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
//! ## 事务（2026-10-29 新增，C# 无对应路由）
//!
//! `POST Db/Begin` → 返回事务令牌；`POST Db/Commit` / `POST Db/Rollback` 携带令牌结束事务。
//! 带令牌的 `Db/Query` / `Db/Execute` / `Db/InsertAndGetIdentity` 在**开事务时固定的那条会话**上执行，
//! 所以是**真事务**（可回滚、可读自己的写、可拿未提交的自增值）。
//!
//! - **不破坏 C#**：C# `DbServer` 没这三个路由，C# 客户端也不会调；Rust 客户端连 C# 时拿到 404
//!   → 降级为 `Unsupported`（与旧行为一致）。
//! - **防泄漏**：客户端会话 `Drop` 尽力回滚 + 服务端空闲超时（默认 60 秒）自动回滚回收。
//!
//! 与 C# 的协议互通：
//! - `POST Db/Query` 的应答为 **DbTable v3 二进制**（[`crate::dbtable`]），与 C#
//!   `DbController.Query` 的 `rs.ToPacket()` 一致：C# `DbClient.QueryAsync` 可直接解析，
//!   Rust 侧用 [`DbClient::query_rowset`] 解析；
//! - 其它接口（Login/Execute/InsertAndGetIdentity/QueryCount/GetTables）为 JSON 信封
//!   `{code, data, msg}`（与 C# `ApiHelper.ProcessResponse` 兼容，无 `code` 时按原样返回）；
//! - C# 的 `DbServer`/`DbController` 绑定 NewLife.Http/MVC；Rust 版提供不绑定框架的
//!   [`DbService`]，路由参数到方法的映射由宿主完成；
//! - 参数绑定对齐 C#：命名占位符（`@p0`/`:p0`/`?p0`）**按名绑定**；无命名占位符时
//!   按字典键序（字母序）兼容绑定（见 `bind_forwarded`）。

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::dal::{ConnectionString, Dal};
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::http;
use crate::model::TableMeta;
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// 默认请求超时。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// 数据库请求参数模型（对齐 `DbRequest`）。
#[derive(Debug, Clone, Default)]
pub struct DbRequest {
    /// SQL 语句。
    pub sql: Option<String>,
    /// SQL 参数字典（命名占位符 `p0`/`p1`… 按名绑定；否则按字典键序绑定）。
    pub parameters: BTreeMap<String, Value>,
}

/// 登录信息（对齐 `LoginInfo`；服务端返回给客户端的数据库信息）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoginInfo {
    /// 数据库类型名（C# `DatabaseType` 规范枚举名，如 `SQLite`/`MySql`/`SqlServer`）。
    ///
    /// 必须与 C# 枚举名逐字一致：NewLife 反序列化枚举区分大小写，`Sqlite` 等 Rust
    /// 调试名会导致 C# `DbClient` 登录失败（实测）；Rust 客户端解析不区分大小写。
    pub db_type: String,
    /// 服务端数据库版本（Rust 版不主动连接探测，通常为空）。
    pub version: String,
}

/// 事务槽：一条被「固定住」的会话。
///
/// 为什么需要固定：`provider=network` 是**无状态 HTTP**，而事务要求「BEGIN 之后的语句必须落在
/// 同一条数据库连接上」。服务端因此在 `Db/Begin` 时从连接池取一条会话并 `begin()`，
/// 之后的语句凭令牌路由到这条会话，直到 `Db/Commit` / `Db/Rollback`。
struct TxSlot {
    /// 被固定的会话（已 `begin()`）。
    ///
    /// 加锁是为了**串行化**：同一事务内的语句不允许并发混插（否则语义就乱了）。
    session: Mutex<Box<dyn SqlSession>>,
    /// 最近一次使用时刻（用于空闲超时回收）。
    last_used: Mutex<Instant>,
    /// 归属的数据源名（防止拿 A 库的事务令牌去操作 B 库）。
    db: String,
}

/// 事务注册表（[`DbService`] 内部状态）。
///
/// **防泄漏设计**（这是 `provider=network` 事务最容易出事的地方）：
/// - 调用方崩溃 / 忘了 commit / `?` 提前返回 → 客户端 `Drop` 会尽力回滚；
/// - 客户端连回滚都发不出去（进程被杀）→ 服务端惰性清扫：任何事务相关请求最多每 1 秒扫一次，
///   空闲超过 `timeout`（默认 60 秒）的事务自动 `rollback` 并释放连接；
/// - 清扫只 `try_lock` 会话：正在跑语句的不动，下一轮再收。
#[derive(Default)]
pub struct TxRegistry {
    map: Mutex<HashMap<String, Arc<TxSlot>>>,
    /// 最近一次清扫时刻（惰性清扫，不需要后台线程，也不用要求宿主定时调用）。
    last_sweep: Mutex<Option<Instant>>,
    /// 空闲超时秒数（<=0 视为 1）。
    timeout_secs: AtomicU64,
}

impl fmt::Debug for TxRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let active = self.map.lock().map(|m| m.len()).unwrap_or(0);
        write!(
            f,
            "TxRegistry(active={active}, timeout={}s)",
            self.timeout().as_secs()
        )
    }
}

impl TxRegistry {
    /// 默认空闲超时（秒）。
    pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
    /// 清扫最小间隔。
    const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

    /// 当前空闲超时。
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.load(Ordering::Relaxed).max(1))
    }

    /// 设置空闲超时（秒）。
    /// <param name="secs">秒数（<=0 视为 1）</param>
    pub fn set_timeout(&self, secs: u64) {
        self.timeout_secs.store(secs.max(1), Ordering::Relaxed);
    }

    /// 活跃事务数。
    pub fn len(&self) -> usize {
        self.map.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// 是否没有活跃事务。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 回收空闲超时的事务（回滚后释放连接），返回回收数量。
    pub fn sweep(&self) -> usize {
        let timeout = self.timeout();
        let now = Instant::now();
        let expired: Vec<(String, Arc<TxSlot>)> = {
            let Ok(map) = self.map.lock() else { return 0 };
            map.iter()
                .filter(|(_, slot)| {
                    slot.last_used
                        .lock()
                        .map(|t| now.duration_since(*t) >= timeout)
                        .unwrap_or(false)
                })
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };
        let mut n = 0;
        for (id, slot) in expired {
            // 正在执行语句（锁被占）→ 说明它还活着，下一轮再收
            let Ok(mut session) = slot.session.try_lock() else {
                continue;
            };
            let _ = session.rollback();
            drop(session);
            if let Ok(mut map) = self.map.lock() {
                map.remove(&id);
            }
            n += 1;
        }
        n
    }

    /// 至多每 [`Self::SWEEP_INTERVAL`] 扫一次。
    fn lazy_sweep(&self) {
        let now = Instant::now();
        {
            let Ok(mut last) = self.last_sweep.lock() else {
                return;
            };
            if let Some(t) = *last
                && now.duration_since(t) < Self::SWEEP_INTERVAL
            {
                return;
            }
            *last = Some(now);
        }
        self.sweep();
    }

    /// 取事务槽。
    fn get(&self, id: &str) -> Result<Arc<TxSlot>> {
        let map = self
            .map
            .lock()
            .map_err(|_| Error::Db("事务表锁已中毒（上一次操作 panic）".into()))?;
        map.get(id).cloned().ok_or_else(|| {
            Error::Db(format!(
                "事务[{id}]不存在或已空闲超时回收（默认 {} 秒）",
                self.timeout().as_secs()
            ))
        })
    }
}

/// 数据库服务层（对齐 `DbService`）：令牌校验与 SQL 操作，可被任意 HTTP 宿主复用。
#[derive(Debug, Default)]
pub struct DbService {
    /// 令牌字典：令牌 → 允许访问的数据库连接名列表。
    ///
    /// 空字典表示不校验；某令牌的列表为空表示允许所有数据库（对齐 C# 语义）。
    /// 令牌查找与库名比较均忽略大小写。
    pub tokens: BTreeMap<String, Vec<String>>,
    /// 事务注册表（`Db/Begin` → `Db/Commit` / `Db/Rollback` 之间的固定会话）。
    txs: TxRegistry,
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
        self.tokens.insert(
            token.to_string(),
            databases.iter().map(|s| s.to_string()).collect(),
        );
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
            // 用 C# `DatabaseType` 规范名（而非 Rust 调试名），保证 C# 客户端可反序列化
            db_type: dal.kind().name().to_string(),
            version: String::new(),
        }
    }

    /// 事务空闲超时（秒；默认 60）。超时的事务会被自动回滚并释放连接。
    /// <param name="secs">秒数（<=0 视为 1）</param>
    pub fn set_tx_timeout(&self, secs: u64) {
        self.txs.set_timeout(secs);
    }

    /// 活跃事务数（运维观测用）。
    pub fn tx_count(&self) -> usize {
        self.txs.len()
    }

    /// 主动回滚并回收空闲超时的事务，返回回收数量。
    ///
    /// 宿**不必**定时调（事务相关请求会自动惰性清扫）；如需在面板展示健康度，可自行定时调。
    pub fn sweep_txs(&self) -> usize {
        self.txs.sweep()
    }

    /// 开启事务：固定一条会话、`begin()` 后返回事务令牌（对齐 `POST Db/Begin`）。
    ///
    /// 与 C# 的关系：C# `DbServer` **没有**该路由，所以 C# 客户端不会调，零破坏；
    /// Rust 客户端连接 C# 服务端时拿到 404 → 客户端降级为 `Unsupported`（见 `DbClient::begin_tx`）。
    /// <param name="dal">数据访问层</param>
    /// <param name="db">数据源名（仅用于归属校验）</param>
    /// <returns>事务令牌</returns>
    pub fn begin_tx(&self, dal: &Dal, db: &str) -> Result<String> {
        self.txs.lazy_sweep();
        let mut session = dal.open_session()?;
        session.begin()?;
        let id = dhrust::random::hex(16);
        let slot = Arc::new(TxSlot {
            session: Mutex::new(session),
            last_used: Mutex::new(Instant::now()),
            db: db.to_string(),
        });
        self.txs
            .map
            .lock()
            .map_err(|_| Error::Db("事务表锁已中毒（上一次操作 panic）".into()))?
            .insert(id.clone(), slot);
        Ok(id)
    }

    /// 提交事务（对齐 `POST Db/Commit`）。
    /// <param name="db">数据源名</param>
    /// <param name="tx">事务令牌</param>
    pub fn commit_tx(&self, db: &str, tx: &str) -> Result<()> {
        self.finish_tx(db, tx, true)
    }

    /// 回滚事务（对齐 `POST Db/Rollback`）。
    /// <param name="db">数据源名</param>
    /// <param name="tx">事务令牌</param>
    pub fn rollback_tx(&self, db: &str, tx: &str) -> Result<()> {
        self.finish_tx(db, tx, false)
    }

    /// 收取事务并提交/回滚。
    ///
    /// **先从表里摘除再执行**：即使 commit 报错，令牌也不会再被用（避免“半死不活的事务”）。
    fn finish_tx(&self, db: &str, tx: &str, commit: bool) -> Result<()> {
        let id = tx.trim();
        if id.is_empty() {
            return Err(Error::Argument("事务令牌不能为空".into()));
        }
        let slot = self.txs.get(id)?;
        if !db.trim().is_empty()
            && !slot.db.trim().is_empty()
            && !slot.db.eq_ignore_ascii_case(db.trim())
        {
            return Err(Error::Db(format!(
                "事务[{id}]属于数据源[{}]，不能用于[{db}]",
                slot.db
            )));
        }
        if let Ok(mut map) = self.txs.map.lock() {
            map.remove(id);
        }
        let mut session = slot
            .session
            .lock()
            .map_err(|_| Error::Db(format!("事务[{id}]的会话锁已中毒（上一次执行 panic）")))?;
        if commit {
            session.commit()
        } else {
            session.rollback()
        }
    }

    /// 在「事务固定会话」或「新建会话」上执行闭包。
    fn with_session_tx<R>(
        &self,
        dal: &Dal,
        tx: Option<&str>,
        f: impl FnOnce(&mut dyn SqlSession) -> Result<R>,
    ) -> Result<R> {
        match tx.map(str::trim).filter(|s| !s.is_empty()) {
            None => {
                let mut session = dal.open_session()?;
                f(session.as_mut())
            }
            Some(id) => {
                self.txs.lazy_sweep();
                let slot = self.txs.get(id)?;
                let mut guard = slot.session.lock().map_err(|_| {
                    Error::Db(format!("事务[{id}]的会话锁已中毒（上一次执行 panic）"))
                })?;
                let r = f(guard.as_mut());
                drop(guard);
                // 用“完成时刻”续期：长语句不应被空闲超时误杀
                if let Ok(mut last) = slot.last_used.lock() {
                    *last = Instant::now();
                }
                r
            }
        }
    }

    /// 执行 SQL 查询，返回结果集（对齐 `Query`）。
    ///
    /// 参数绑定：命名占位符（`@p0`/`:p0`/`?p0`）按名绑定；否则按字典键序兼容绑定。
    /// <param name="dal">数据访问层</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>结果集</returns>
    pub fn query(
        &self,
        dal: &Dal,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<RowSet> {
        self.query_in(dal, None, sql, parameters)
    }

    /// 同 [`Self::query`]，但可指定事务令牌（`Some` = 在该事务的固定会话上执行）。
    /// <param name="dal">数据访问层</param>
    /// <param name="tx">事务令牌</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>结果集</returns>
    pub fn query_in(
        &self,
        dal: &Dal,
        tx: Option<&str>,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<RowSet> {
        if sql.is_empty() {
            return Err(Error::Argument("SQL不能为空".into()));
        }
        let (sql, params) = bind_forwarded(dal.kind(), sql, parameters);
        self.with_session_tx(dal, tx, |session| session.query(&sql, &params))
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
        self.query_packet_in(dal, None, sql, parameters)
    }

    /// 同 [`Self::query_packet`]，但可指定事务令牌。
    /// <param name="dal">数据访问层</param>
    /// <param name="tx">事务令牌</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>DbTable 二进制报文</returns>
    pub fn query_packet_in(
        &self,
        dal: &Dal,
        tx: Option<&str>,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<Vec<u8>> {
        let set = self.query_in(dal, tx, sql, parameters)?;
        Ok(crate::dbtable::encode_rowset(&set))
    }

    /// 执行 SQL 语句，返回受影响行数（对齐 `Execute`）。
    /// <param name="dal">数据访问层</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>受影响行数</returns>
    pub fn execute(
        &self,
        dal: &Dal,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<u64> {
        self.execute_in(dal, None, sql, parameters)
    }

    /// 同 [`Self::execute`]，但可指定事务令牌。
    /// <param name="dal">数据访问层</param>
    /// <param name="tx">事务令牌</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>受影响行数</returns>
    pub fn execute_in(
        &self,
        dal: &Dal,
        tx: Option<&str>,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<u64> {
        if sql.is_empty() {
            return Err(Error::Argument("SQL不能为空".into()));
        }
        let (sql, params) = bind_forwarded(dal.kind(), sql, parameters);
        self.with_session_tx(dal, tx, |session| session.execute(&sql, &params))
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
        self.insert_and_get_identity_in(dal, None, sql, parameters)
    }

    /// 同 [`Self::insert_and_get_identity`]，但可指定事务令牌。
    ///
    /// 事务内取自增**必须走这条**：远程另开会话拿不到未提交事务的自增值。
    /// <param name="dal">数据访问层</param>
    /// <param name="tx">事务令牌</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>自增主键值</returns>
    pub fn insert_and_get_identity_in(
        &self,
        dal: &Dal,
        tx: Option<&str>,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<i64> {
        if sql.is_empty() {
            return Err(Error::Argument("SQL不能为空".into()));
        }
        let (sql, params) = bind_forwarded(dal.kind(), sql, parameters);
        self.with_session_tx(dal, tx, |session| {
            session.insert_and_get_identity(&sql, &params, None)
        })
    }

    /// 快速查询单表记录数（对齐 `QueryCount`）。
    ///
    /// 有模型时走 [`crate::dal::TableRef::count`]；无模型/模型缺表时退化为
    /// 原始 `SELECT COUNT(*)`——Rust `DbServer` 宿主可不加载 Model.xml
    /// 直接服务 C# `Network` 驱动的 `QueryCountFast`。
    /// <param name="dal">数据访问层</param>
    /// <param name="table">表名</param>
    /// <returns>记录数</returns>
    pub fn query_count(&self, dal: &Dal, table: &str) -> Result<i64> {
        if table.is_empty() {
            return Err(Error::Argument("表名不能为空".into()));
        }
        let mut session = dal.open_session()?;
        match dal.table(table) {
            Ok(t) => t.count(session.as_mut(), None),
            Err(_) => {
                let sql = format!("SELECT COUNT(*) FROM {}", dal.kind().quote(table));
                let set = session.query(&sql, &[])?;
                Ok(set
                    .rows
                    .first()
                    .and_then(|row| row.get(0))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0))
            }
        }
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
    /// 出站 `User-Agent`（`None` = 不发送）。
    ///
    /// 部分服务端 WAF 的 `block_empty_ua` 开关会拒绝无 UA 的机器请求，
    /// 工具/Agent 类消费方建议显式设置（如 `pek-rdbproxy-tool/0.1.0`）。
    pub user_agent: Option<String>,
    /// 用户令牌（`Authorization: Bearer`）；`None` = 不发送。
    ///
    /// 代理按此识别“谁在调数据”，并据此做按用户/按数据源的权限控制；
    /// 不传时保持旧行为（仅数据源令牌）。
    pub bearer: Option<String>,
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
            user_agent: None,
            bearer: None,
        }
    }

    /// 设置出站 `User-Agent`（建造器风格，便于链式调用）。
    /// <param name="user_agent">UA 字符串</param>
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// 设置用户令牌（`Authorization: Bearer`）。
    /// <param name="token">登录拿到的 JWT</param>
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }

    /// 从 XCode 风格连接串构造（`Server`/`Database`/`Password` 三键；供 `provider=network` 驱动）。
    /// <param name="conn_str">连接串</param>
    /// <returns>HTTP 客户端</returns>
    pub fn from_connection_string(conn_str: &ConnectionString) -> Result<Self> {
        let server = conn_str
            .get("server")
            .ok_or_else(|| Error::Model("network 连接串缺少 Server 地址".into()))?;
        let database = conn_str
            .get("database")
            .ok_or_else(|| Error::Model("network 连接串缺少 Database 名称".into()))?;
        let token = conn_str.get("password").or(conn_str.get("token"));
        Ok(Self::new(server, database, token))
    }

    /// 登录到远端数据库服务（对齐 `LoginAsync`）。
    /// <returns>登录信息</returns>
    pub fn login(&self) -> Result<LoginInfo> {
        let body = json!({ "db": self.db, "token": self.token }).to_string();
        let text = http::post_json_auth(
            &self.url("Db/Login"),
            &body,
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
        )?;
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
    pub fn query_rowset(
        &self,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<RowSet> {
        self.query_rowset_in(None, sql, parameters)
    }

    /// 同 [`Self::query_rowset`]，但可带事务令牌（`Some` = 在远端已开启的事务里执行）。
    /// <param name="tx">事务令牌</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>结果集</returns>
    pub fn query_rowset_in(
        &self,
        tx: Option<&str>,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<RowSet> {
        let args = self.build_args_in(sql, parameters, tx);
        let bytes = http::post_bytes_auth(
            &self.url("Db/Query"),
            &args.to_string(),
            true,
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
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
        self.execute_in(None, sql, parameters)
    }

    /// 同 [`Self::execute`]，但可带事务令牌。
    /// <param name="tx">事务令牌</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>受影响行数</returns>
    pub fn execute_in(
        &self,
        tx: Option<&str>,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<i64> {
        let args = self.build_args_in(sql, parameters, tx);
        let text = http::post_json_auth(
            &self.url("Db/Execute"),
            &args.to_string(),
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
        )?;
        parse_envelope_i64(&text)
    }

    /// 执行插入语句并返回自增标识（对齐 `InsertAndGetIdentityAsync`）。
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>自增主键值</returns>
    pub fn insert_and_get_identity(
        &self,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<i64> {
        self.insert_and_get_identity_in(None, sql, parameters)
    }

    /// 同 [`Self::insert_and_get_identity`]，但可带事务令牌。
    /// <param name="tx">事务令牌</param>
    /// <param name="sql">SQL 语句</param>
    /// <param name="parameters">参数字典</param>
    /// <returns>自增主键值</returns>
    pub fn insert_and_get_identity_in(
        &self,
        tx: Option<&str>,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
    ) -> Result<i64> {
        let args = self.build_args_in(sql, parameters, tx);
        let text = http::post_json_auth(
            &self.url("Db/InsertAndGetIdentity"),
            &args.to_string(),
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
        )?;
        parse_envelope_i64(&text)
    }

    /// 开启远端事务（`POST Db/Begin`），返回事务令牌。
    ///
    /// **与 C# 的兼容**：C# `DbServer` 没有该路由（HTTP 404/405）→ 本方法返回
    /// [`Error::Unsupported`]，调用方据此回退成「无事务」而不报错（与旧行为一致）。
    /// <returns>事务令牌</returns>
    pub fn begin_tx(&self) -> Result<String> {
        let body = json!({ "db": self.db, "token": self.token }).to_string();
        let text = http::post_json_auth(
            &self.url("Db/Begin"),
            &body,
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
        )
        .map_err(route_missing_to_unsupported)?;
        let data = parse_envelope(&text)?;
        let id = match &data {
            Value::String(s) => s.trim().to_string(),
            other => get_str_field(other, &["tx", "Tx"]),
        };
        if id.is_empty() {
            return Err(Error::Db("远端未返回事务令牌".into()));
        }
        Ok(id)
    }

    /// 提交远端事务（`POST Db/Commit`）。
    /// <param name="tx">事务令牌</param>
    pub fn commit_tx(&self, tx: &str) -> Result<()> {
        self.finish_tx("Db/Commit", tx)
    }

    /// 回滚远端事务（`POST Db/Rollback`）。
    /// <param name="tx">事务令牌</param>
    pub fn rollback_tx(&self, tx: &str) -> Result<()> {
        self.finish_tx("Db/Rollback", tx)
    }

    /// 提交/回滚的公共实现。
    fn finish_tx(&self, route: &str, tx: &str) -> Result<()> {
        let body = json!({ "db": self.db, "token": self.token, "tx": tx }).to_string();
        let text = http::post_json_auth(
            &self.url(route),
            &body,
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
        )
        .map_err(route_missing_to_unsupported)?;
        parse_envelope(&text).map(|_| ())
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
        let text = http::get_text_auth(
            &url,
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
        )?;
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
        let text = http::get_text_auth(
            &url,
            None,
            self.bearer.as_deref(),
            self.timeout,
            self.user_agent.as_deref(),
        )?;
        parse_envelope(&text)
    }

    /// 拼接路由地址。
    fn url(&self, route: &str) -> String {
        format!("{}/{route}", self.server)
    }

    /// 构建请求参数（sql/parameters/db/token；parameters 为 JSON 字符串，对齐 C# `BuildArgs`）。
    ///
    /// 带 `tx` 时额外附上事务令牌；不带 `tx` 时请求体与旧版**逐字节一致**
    /// （C# 服务端与既有联调不受影响）。
    fn build_args_in(
        &self,
        sql: &str,
        parameters: Option<&BTreeMap<String, Value>>,
        tx: Option<&str>,
    ) -> Value {
        let mut args = match parameters {
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
        };
        if let Some(id) = tx.map(str::trim).filter(|s| !s.is_empty())
            && let Some(obj) = args.as_object_mut()
        {
            obj.insert("tx".to_string(), Value::String(id.to_string()));
        }
        args
    }
}

/// 远端是 C# `DbServer`（没有事务路由）时，把 HTTP 404/405/501 归一化为 `Unsupported`。
///
/// 这样调用方可以「有事务就用、没有就回退」，而不是把“对端不支持”当成故障。
fn route_missing_to_unsupported(e: Error) -> Error {
    let missing = match &e {
        Error::Db(m) => ["HTTP 404", "HTTP 405", "HTTP 501"]
            .iter()
            .any(|code| m.contains(code)),
        _ => false,
    };
    if missing {
        Error::Unsupported(format!("远端 DbServer 未提供事务接口（{e}）"))
    } else {
        e
    }
}

/// 参数绑定（对齐 C# `DbService.ConvertToDictionary` + `CreateParameters` 的**按名绑定**）。
///
/// - 请求 SQL 含命名占位符（`@p0`/`:p0`/`?p0`，由 `provider=network` 驱动或
///   C# `DbClient` 转发产生）时：按出现顺序替换为本地方言占位符并取对应值；
///   字典键兼容带前缀形式（C# `ConvertParameters` 用 `IDataParameter.ParameterName` 作键，
///   如 `@p0`；本端发送时使用同名键）；
/// - SQL 不含命名占位符时（JSON 客户端的位置占位符 `?`）：保持兼容，按字典键序（字母序）绑定。
/// <param name="kind">本地数据库类型（决定替换后的占位符风格）</param>
/// <param name="sql">SQL 语句</param>
/// <param name="parameters">参数字典</param>
/// <returns>重写后的 SQL 与按序参数</returns>
fn bind_forwarded(
    kind: DatabaseKind,
    sql: &str,
    parameters: Option<&BTreeMap<String, Value>>,
) -> (String, Vec<DbValue>) {
    let Some(ps) = parameters.filter(|p| !p.is_empty()) else {
        return (sql.to_string(), Vec::new());
    };

    // 占位符名 → 值（键兼容带前缀：@p0/:p0/?p0）
    let mut by_name: BTreeMap<&str, &Value> = BTreeMap::new();
    for (k, v) in ps {
        let stripped = k.trim_start_matches(['@', ':', '?']);
        by_name.entry(stripped).or_insert(v);
    }

    // 扫描命名占位符（跳过字符串字面量与 `::` 类型转换）
    let bytes = sql.as_bytes();
    let mut hits: Vec<(usize, usize, Value)> = Vec::new();
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            // 字符串内的 '' 转义
            if in_string && bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }
        if b == b':' && bytes.get(i + 1) == Some(&b':') {
            i += 2;
            continue;
        }
        if matches!(b, b'@' | b':' | b'?') {
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let name = &sql[i + 1..j];
                if let Some(value) = by_name.get(name) {
                    hits.push((i, j, (*value).clone()));
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }

    if hits.is_empty() {
        // 兼容路径：无命名占位符时按字典键序绑定（既有 JSON 客户端行为）
        return (sql.to_string(), ps.values().map(json_to_db_value).collect());
    }

    let mut out = String::with_capacity(sql.len() + hits.len() * 2);
    let mut values = Vec::with_capacity(hits.len());
    let mut last = 0usize;
    for (index, (start, end, value)) in hits.iter().enumerate() {
        out.push_str(&sql[last..*start]);
        out.push_str(&kind.placeholder(index));
        values.push(json_to_db_value(value));
        last = *end;
    }
    out.push_str(&sql[last..]);
    (out, values)
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
    let value: Value =
        serde_json::from_str(text).map_err(|e| Error::Db(format!("响应解析失败：{e}")))?;
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

/// 读取字段文本（兼容多种大小写写法；数字枚举转为十进制文本）。
///
/// NewLife JSON 会把枚举序列化为**数字**（如 `DatabaseType.SQLite` → `"DbType":6`），
/// 因此这里同时接受字符串与数字两种形式。
/// <param name="data">JSON 对象</param>
/// <param name="keys">候选键</param>
/// <returns>字段值（缺失时为空字符串）</returns>
fn get_str_field(data: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| {
            data.get(*k).and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// 对查询串参数做百分号编码（unreserved 字符不编码）。
/// <param name="text">原文</param>
/// <returns>编码文本</returns>
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
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
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-dbsvc-{}-{}-{name}",
            std::process::id(),
            stamp
        ));
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

        // 登录信息（C# 规范枚举名）
        let info = svc.login_info(&dal);
        assert_eq!(info.db_type, "SQLite");
        assert!(info.version.is_empty());

        // 执行插入（无参数）
        assert_eq!(
            svc.execute(
                &dal,
                "INSERT INTO DH_User(Name, Enable) VALUES('a', 1)",
                None
            )
            .unwrap(),
            1
        );
        // 带参数插入（按字母序绑定：Enable → ?1，Name → ?2）
        let mut ps = BTreeMap::new();
        ps.insert("Enable".to_string(), json!(1));
        ps.insert("Name".to_string(), json!("b"));
        assert_eq!(
            svc.execute(
                &dal,
                "INSERT INTO DH_User(Enable, Name) VALUES(?, ?)",
                Some(&ps)
            )
            .unwrap(),
            1
        );

        // 查询
        let set = svc
            .query(&dal, "SELECT Name FROM DH_User ORDER BY Id", None)
            .unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set.first().unwrap().get(0).unwrap().as_str(), Some("a"));

        // 自增标识
        let id = svc
            .insert_and_get_identity(
                &dal,
                "INSERT INTO DH_User(Name, Enable) VALUES('c', 1)",
                None,
            )
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
    fn query_count_falls_back_without_model() {
        // 无模型宿主（Rust DbServer 常见形态）也要能服务 C# 的 QueryCountFast
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!("rcode-qc-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("raw.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        let dal = Dal::open(&conn).unwrap();
        {
            let mut session = dal.open_session().unwrap();
            session
                .execute(
                    "CREATE TABLE raw_t(id integer primary key autoincrement, name text)",
                    &[],
                )
                .unwrap();
            session
                .execute("INSERT INTO raw_t(name) VALUES('a')", &[])
                .unwrap();
            session
                .execute("INSERT INTO raw_t(name) VALUES('b')", &[])
                .unwrap();
        }

        let svc = DbService::new();
        assert_eq!(svc.query_count(&dal, "raw_t").unwrap(), 2);
        assert!(svc.query_count(&dal, "").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_field_accepts_numeric_enum() {
        // C# NewLife JSON：DatabaseType.SQLite → 数字 6
        let data = json!({ "DbType": 6, "Version": "3.53.3" });
        let db_type = get_str_field(&data, &["dbType", "DbType"]);
        assert_eq!(db_type, "6");
        assert_eq!(
            crate::dialect::DatabaseKind::from_remote_name(&db_type).unwrap(),
            crate::dialect::DatabaseKind::Sqlite
        );
        assert_eq!(get_str_field(&data, &["version", "Version"]), "3.53.3");

        // Rust 服务端：枚举调试名（字符串）
        let data = json!({ "DbType": "MySql" });
        assert_eq!(get_str_field(&data, &["dbType", "DbType"]), "MySql");
    }

    #[test]
    fn forwarded_parameters_bind_by_name() {
        // 命名占位符 + 不带前缀键（Rust network 客户端）
        let mut ps = BTreeMap::new();
        ps.insert("p0".to_string(), json!(7));
        ps.insert("p1".to_string(), json!("a"));
        let (sql, values) = bind_forwarded(
            DatabaseKind::Sqlite,
            "SELECT * FROM T WHERE A=@p0 AND B=:p1",
            Some(&ps),
        );
        assert_eq!(sql, "SELECT * FROM T WHERE A=? AND B=?");
        assert_eq!(values, vec![DbValue::Int(7), DbValue::Text("a".into())]);

        // 命名占位符 + 带前缀键（C# DbClient `ConvertParameters` 风格）
        let mut ps = BTreeMap::new();
        ps.insert("@p0".to_string(), json!(7));
        ps.insert(":p1".to_string(), json!("a"));
        let (sql, values) = bind_forwarded(
            DatabaseKind::PostgreSql,
            "SELECT * FROM T WHERE A=@p0 AND B=:p1",
            Some(&ps),
        );
        assert_eq!(sql, "SELECT * FROM T WHERE A=$1 AND B=$2");
        assert_eq!(values, vec![DbValue::Int(7), DbValue::Text("a".into())]);

        // 顺序按占位符出现次序（而非键序）
        let mut ps = BTreeMap::new();
        ps.insert("p2".to_string(), json!("x"));
        ps.insert("p0".to_string(), json!(1));
        let (sql, values) = bind_forwarded(
            DatabaseKind::SqlServer,
            "SELECT * FROM T WHERE A=@p2 AND B=@p0",
            Some(&ps),
        );
        assert_eq!(sql, "SELECT * FROM T WHERE A=@p0 AND B=@p1");
        assert_eq!(values, vec![DbValue::Text("x".into()), DbValue::Int(1)]);

        // 字符串字面量中的占位符不替换
        let mut ps = BTreeMap::new();
        ps.insert("p0".to_string(), json!(1));
        let (sql, values) = bind_forwarded(
            DatabaseKind::Sqlite,
            "SELECT '@p0', * FROM T WHERE A=@p0",
            Some(&ps),
        );
        assert_eq!(sql, "SELECT '@p0', * FROM T WHERE A=?");
        assert_eq!(values, vec![DbValue::Int(1)]);

        // 无命名占位符：按字典键序兼容绑定
        let mut ps = BTreeMap::new();
        ps.insert("Name".to_string(), json!("n"));
        let (sql, values) =
            bind_forwarded(DatabaseKind::Sqlite, "SELECT * FROM T WHERE A=?", Some(&ps));
        assert_eq!(sql, "SELECT * FROM T WHERE A=?");
        assert_eq!(values, vec![DbValue::Text("n".into())]);
    }

    #[test]
    fn rowset_json_roundtrip() {
        let mut ps = RowSet::new(vec!["Id".into(), "Name".into(), "Score".into()]);
        ps.push(vec![
            DbValue::Int(1),
            DbValue::Text("张三".into()),
            DbValue::Null,
        ]);
        ps.push(vec![
            DbValue::Int(2),
            DbValue::Text("李四".into()),
            DbValue::Float(9.5),
        ]);

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
        spawn_mock_status_response("200 OK", content_type, body, times)
    }

    /// 同 [`spawn_mock_response`]，但可指定**状态行**（如 `404 Not Found`）。
    fn spawn_mock_status_response(
        status: &'static str,
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
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        let back = client
            .query_rowset("SELECT Id,Name,Score FROM T", None)
            .unwrap();
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

    // ── 事务（`Db/Begin` → `Db/Commit` / `Db/Rollback`）───────────────────────────

    fn probe_table(svc: &DbService, dal: &Dal) {
        svc.execute(
            dal,
            "CREATE TABLE TxProbe (Id INTEGER PRIMARY KEY AUTOINCREMENT, Name TEXT)",
            None,
        )
        .unwrap();
    }

    fn probe_count(svc: &DbService, dal: &Dal, tx: Option<&str>) -> i64 {
        let set = svc
            .query_in(dal, tx, "SELECT COUNT(*) FROM TxProbe", None)
            .unwrap();
        set.rows
            .first()
            .and_then(|r| r.get(0))
            .and_then(|v| v.as_i64())
            .unwrap_or(-1)
    }

    /// 提交：事务内的写要落库；且事务内能读到自己的写。
    #[test]
    fn tx_commit_persists_and_reads_own_writes() {
        let (dal, dir) = temp_dal("tx-commit");
        let svc = DbService::new();
        probe_table(&svc, &dal);

        let tx = svc.begin_tx(&dal, "Demo").unwrap();
        assert_eq!(svc.tx_count(), 1, "开启后应有一个活跃事务");
        svc.execute_in(
            &dal,
            Some(&tx),
            "INSERT INTO TxProbe (Name) VALUES ('a')",
            None,
        )
        .unwrap();
        // 事务内可见自己的未提交写（说明语句确实落在同一条连接上）
        assert_eq!(
            probe_count(&svc, &dal, Some(&tx)),
            1,
            "事务内应读到自己的写"
        );

        svc.commit_tx("Demo", &tx).unwrap();
        assert_eq!(svc.tx_count(), 0, "提交后事务应被收取");
        assert_eq!(probe_count(&svc, &dal, None), 1, "提交后数据应持久化");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 回滚：事务内的写必须消失。
    #[test]
    fn tx_rollback_discards_writes() {
        let (dal, dir) = temp_dal("tx-rollback");
        let svc = DbService::new();
        probe_table(&svc, &dal);

        let tx = svc.begin_tx(&dal, "Demo").unwrap();
        svc.execute_in(
            &dal,
            Some(&tx),
            "INSERT INTO TxProbe (Name) VALUES ('b')",
            None,
        )
        .unwrap();
        assert_eq!(probe_count(&svc, &dal, Some(&tx)), 1);

        svc.rollback_tx("Demo", &tx).unwrap();
        assert_eq!(probe_count(&svc, &dal, None), 0, "回滚后不应留下数据");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 事务内的自增：必须在同一会话上取（远端另开会话拿不到未提交的自增值）。
    #[test]
    fn tx_identity_is_visible_inside_transaction() {
        let (dal, dir) = temp_dal("tx-identity");
        let svc = DbService::new();
        probe_table(&svc, &dal);

        let tx = svc.begin_tx(&dal, "Demo").unwrap();
        let id1 = svc
            .insert_and_get_identity_in(
                &dal,
                Some(&tx),
                "INSERT INTO TxProbe (Name) VALUES ('x')",
                None,
            )
            .unwrap();
        let id2 = svc
            .insert_and_get_identity_in(
                &dal,
                Some(&tx),
                "INSERT INTO TxProbe (Name) VALUES ('y')",
                None,
            )
            .unwrap();
        assert!(id1 >= 1 && id2 == id1 + 1, "自增应连续（{id1} → {id2}）");
        svc.commit_tx("Demo", &tx).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 令牌不存在 / 归属不符 / 空令牌：都要给可定位的错误，且不能误伤仍有效的事务。
    #[test]
    fn tx_bad_token_is_rejected() {
        let (dal, dir) = temp_dal("tx-bad");
        let svc = DbService::new();
        probe_table(&svc, &dal);

        assert!(
            svc.execute_in(&dal, Some("deadbeef"), "SELECT 1", None)
                .is_err()
        );
        assert!(svc.commit_tx("Demo", "").is_err(), "空令牌应报参数错误");
        assert!(
            svc.rollback_tx("Demo", "deadbeef").is_err(),
            "不存在的令牌应报错"
        );

        let tx = svc.begin_tx(&dal, "Demo").unwrap();
        let wrong_db = svc.commit_tx("Other", &tx);
        assert!(wrong_db.is_err(), "跨数据源用同一令牌应被拒");
        // 归属校验失败**不消耗**令牌：用对的库名仍能正常提交
        svc.commit_tx("Demo", &tx).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 空闲超时：自动回滚并回收（防止调用方崩溃后把连接占死）。
    #[test]
    fn tx_expires_and_releases_connection() {
        let (dal, dir) = temp_dal("tx-expire");
        let svc = DbService::new();
        probe_table(&svc, &dal);

        svc.set_tx_timeout(1);
        let tx = svc.begin_tx(&dal, "Demo").unwrap();
        svc.execute_in(
            &dal,
            Some(&tx),
            "INSERT INTO TxProbe (Name) VALUES ('z')",
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(1200));

        assert_eq!(svc.sweep_txs(), 1, "空闲超时的事务应被回收");
        assert_eq!(svc.tx_count(), 0);
        assert!(
            svc.execute_in(&dal, Some(&tx), "SELECT 1", None).is_err(),
            "回收后旧令牌必须失效"
        );
        assert_eq!(probe_count(&svc, &dal, None), 0, "回收时应已回滚");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 远端是 C# `DbServer`（没有 `Db/Begin`）→ 客户端必须降级为 `Unsupported`，
    /// 而不是当成故障。这条守着「与 C# 互不破坏」的底线。
    #[test]
    fn db_client_begin_tx_degrades_when_route_missing() {
        // 模拟 C# DbServer：/Db/Begin 不存在 → 404
        let (port, handle) = spawn_mock_status_response(
            "404 Not Found",
            "text/plain",
            b"404 page not found".to_vec(),
            1,
        );
        let client = DbClient::new(format!("http://127.0.0.1:{port}"), "Demo", None);
        let err = client
            .begin_tx()
            .map(|_| ())
            .expect_err("404 应转为 Unsupported");
        assert!(
            matches!(err, Error::Unsupported(_)),
            "应降级为 Unsupported（调用方据此回退无事务），实际：{err}"
        );
        handle.join().ok();
    }
}
