//! 实体缓存：对应 DH.NCode 的 `XCode.Cache`（`Meta.Cache` / `Meta.SingleCache`）。
//!
//! 覆盖 DH.NCode 缓存架构中的两层（见其 `Cache/readme.md`）：
//! - **实体缓存** [`EntityCache`]：整表缓存“读多写少”的数据（系统参数、分类表等）。
//!   首次访问执行一次整表查询，之后在内存列表上执行查找；默认过期 **60 秒**
//! - **单对象缓存** [`SingleCache`]：以主键为键逐行缓存（用户表等点查场景）。
//!   未命中时查库并回填；默认过期 **60 秒**、最大 **10000** 项（超出即整体清空）
//!
//! 一致性策略（与 DH.NCode 语义对齐）：
//! - 任何写入（[`crate::dal::TableRef`] 的 `insert` / `update_by_pk` / `delete_by_pk`，
//!   实体层的写入同源）都会使对应表的缓存**立即失效**，下次访问自动重新加载
//! - 与 C# 版的差异：C# 版过期后“立即返回旧数据 + 异步更新（LazyConsumer）”；
//!   Rust 版首版采用“过期后同步重载”，语义更直观且不引入后台线程，后续可按需演进
//!
//! 使用入口：按表获取共享缓存实例——
//! ```no_run
//! # use pek_rcode::dal::Dal;
//! # fn demo(dal: &Dal) -> pek_rcode::Result<()> {
//! let mut session = dal.open_session()?;
//! // 对应 C# 的 Meta.Cache.Entities（整表缓存）
//! let cache = dal.entity_cache("JiLiYu")?;
//! let rows = cache.entities(dal, session.as_mut())?;
//! println!("整表 {} 行", rows.len());
//!
//! // 对应 C# 的 Meta.SingleCache[key]（单对象缓存）
//! let single = dal.single_cache("VerifyCode")?;
//! let item = single.get(dal, session.as_mut(), &["k-001".into()])?;
//! println!("{item:?}");
//! # Ok(())
//! # }
//! ```

pub mod data;
pub mod db;
pub mod field;
pub mod invalidator;
pub mod lazy;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use crate::dal::{Dal, TableRef};
use crate::dialect::DatabaseKind;
use crate::error::Result;
use crate::model::TableMeta;
use crate::session::{DbRow, SqlSession};
use crate::value::DbValue;

/// 默认过期时间（秒，与 DH.NCode 一致）。
pub const DEFAULT_EXPIRE_SECONDS: u64 = 60;

/// 1 秒的纳秒数（避免浮点与常数魔法值分散）。
const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// 一致性模式：过期后同步重载（默认）。
const CONSISTENCY_BLOCKING: u8 = 0;
/// 一致性模式：过期先返旧值再刷新。
const CONSISTENCY_SWR: u8 = 1;

/// 实体缓存一致性模式（按业务场景选择）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheConsistency {
    /// 过期后同步重载（**默认**；强一致优先，读会阻塞一次查询）。
    /// 适合权限、余额、库存等不能接受旧值的场景。
    #[default]
    Blocking,
    /// 过期后先返回旧数据、下次访问刷新（读不阻塞；允许最长 `max_stale`）。
    /// 适合参数、字典、分类等读多写少且可容忍短暂旧值的场景。
    StaleWhileRevalidate {
        /// 过期后允许继续返回旧数据的最长时间（超过则同步刷新）
        max_stale: Duration,
    },
}

/// 单对象缓存默认最大实体数（与 DH.NCode 一致）。
pub const DEFAULT_MAX_ENTITY: usize = 10_000;

/// 获取互斥锁；被投毒（持有线程 panic）时恢复内部数据，避免级联失败。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 获取读锁（被投毒时恢复内部数据）。
fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// 获取写锁（被投毒时恢复内部数据）。
fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// 主键缓存键：**单列整数主键零分配**（自增/雪花，物联网主键口径），其余走拼接字符串。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PkKey {
    /// 单列整数主键
    Int(i64),
    /// 其它（文本/时间/多列）—— 按原规则拼接
    Other(String),
}

/// 主键值 → 缓存键（多列主键按顺序拼接，分隔符为不可见字符）。
fn pk_key(pk: &[DbValue]) -> PkKey {
    if pk.len() == 1
        && let DbValue::Int(value) = &pk[0]
    {
        return PkKey::Int(*value);
    }
    let mut key = String::new();
    for (index, value) in pk.iter().enumerate() {
        if index > 0 {
            key.push('\u{1f}');
        }
        key.push_str(&value.to_text());
    }
    PkKey::Other(key)
}

/// 实体缓存（整表缓存，对应 DH.NCode 的 `Meta.Cache`）。
///
/// 建议只对**读取很多、修改极少**的小表使用（DH.NCode 建议单表 1000 行以内）。
pub struct EntityCache {
    /// 表名（注册键，小写）
    table: String,
    /// 过期时间（纳秒；原子读写，避免高并发命中时抢锁）
    expire_nanos: AtomicU64,
    /// 一致性模式（0=Blocking，1=StaleWhileRevalidate）
    consistency_mode: AtomicU8,
    /// SWR 模式下允许的陈旧窗口（纳秒）
    stale_max_nanos: AtomicU64,
    /// 缓存状态（写锁用于刷新，保证并发过期时只有一个线程查库）
    state: RwLock<EntityCacheState>,
    /// 已向调用方返回过“过期旧值”（SWR 模式）
    stale_served: AtomicBool,
}

/// 实体缓存内部状态。
#[derive(Default)]
struct EntityCacheState {
    /// 已缓存的整表数据
    data: Option<Arc<Vec<DbRow>>>,
    /// 加载完成时刻
    loaded_at: Option<Instant>,
}

impl EntityCache {
    /// 按表名创建（一般经 [`Dal::entity_cache`] 获取共享实例）。
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            expire_nanos: AtomicU64::new(DEFAULT_EXPIRE_SECONDS * NANOS_PER_SECOND),
            consistency_mode: AtomicU8::new(CONSISTENCY_BLOCKING),
            stale_max_nanos: AtomicU64::new(0),
            state: RwLock::new(EntityCacheState::default()),
            stale_served: AtomicBool::new(false),
        }
    }

    /// 读锁（被投毒时恢复内部数据）。
    fn read_state(&self) -> RwLockReadGuard<'_, EntityCacheState> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    /// 写锁（被投毒时恢复内部数据）。
    fn write_state(&self) -> RwLockWriteGuard<'_, EntityCacheState> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// 表名（小写）。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 当前过期时间。
    pub fn expire(&self) -> Duration {
        Duration::from_nanos(self.expire_nanos.load(Ordering::Relaxed))
    }

    /// 设置过期时间（秒）。
    pub fn set_expire_seconds(&self, seconds: u64) {
        self.expire_nanos
            .store(seconds.saturating_mul(NANOS_PER_SECOND), Ordering::Relaxed);
    }

    /// 当前一致性模式。
    pub fn consistency(&self) -> CacheConsistency {
        if self.consistency_mode.load(Ordering::Relaxed) == CONSISTENCY_SWR {
            CacheConsistency::StaleWhileRevalidate {
                max_stale: Duration::from_nanos(self.stale_max_nanos.load(Ordering::Relaxed)),
            }
        } else {
            CacheConsistency::Blocking
        }
    }

    /// 设置一致性模式（强一致用 [`CacheConsistency::Blocking`]；读多写少、可容忍短暂旧值的表
    /// 可开 [`CacheConsistency::StaleWhileRevalidate`]）。
    pub fn set_consistency(&self, mode: CacheConsistency) {
        match mode {
            CacheConsistency::Blocking => {
                self.consistency_mode
                    .store(CONSISTENCY_BLOCKING, Ordering::Relaxed);
                self.stale_max_nanos.store(0, Ordering::Relaxed);
            }
            CacheConsistency::StaleWhileRevalidate { max_stale } => {
                self.stale_max_nanos.store(
                    u64::try_from(max_stale.as_nanos()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                self.consistency_mode
                    .store(CONSISTENCY_SWR, Ordering::Relaxed);
            }
        }
    }

    /// 缓存是否可用（已加载且未过期）。
    pub fn is_valid(&self) -> bool {
        let expire = self.expire();
        let state = self.read_state();
        state.loaded_at.is_some_and(|at| at.elapsed() < expire) && state.data.is_some()
    }

    /// 取得整表数据（按一致性模式处理过期，对应 C# `Meta.Cache.Entities`）。
    ///
    /// - 未过期：直接返回缓存（读锁）；
    /// - 过期 + [`CacheConsistency::Blocking`]（默认）：同步重载（强一致）；
    /// - 过期 + [`CacheConsistency::StaleWhileRevalidate`]：在 `max_stale` 内先返回旧值
    ///   （不阻塞），下次访问刷新；超过窗口则同步重载。
    ///
    /// 重载在**写锁内**执行：并发过期时只有一个线程查库，其余线程等它完成后直接拿新数据，
    /// 避免缓存击穿（N 个线程同时整表查询）。
    pub fn entities(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<Arc<Vec<DbRow>>> {
        let expire = self.expire();
        let mode = self.consistency();

        // 快路径：读锁
        {
            let state = self.read_state();
            if let (Some(data), Some(at)) = (state.data.clone(), state.loaded_at) {
                let elapsed = at.elapsed();
                if elapsed < expire {
                    return Ok(data);
                }
                if let CacheConsistency::StaleWhileRevalidate { max_stale } = mode
                    && elapsed < expire.saturating_add(max_stale)
                    && !self.stale_served.load(Ordering::Relaxed)
                {
                    self.stale_served.store(true, Ordering::Relaxed);
                    return Ok(data);
                }
            }
        }

        // 慢路径：写锁内双重检查 + 查询（单飞）
        let mut state = self.write_state();
        if let (Some(data), Some(at)) = (state.data.clone(), state.loaded_at)
            && at.elapsed() < expire
        {
            return Ok(data);
        }
        let rows = dal.select_all_rows(&self.table, session)?;
        let data = Arc::new(rows);
        state.data = Some(data.clone());
        state.loaded_at = Some(Instant::now());
        self.stale_served.store(false, Ordering::Relaxed);
        Ok(data)
    }

    /// 强制重新加载整表。
    pub fn reload(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<Arc<Vec<DbRow>>> {
        let mut state = self.write_state();
        let rows = dal.select_all_rows(&self.table, session)?;
        let data = Arc::new(rows);
        state.data = Some(data.clone());
        state.loaded_at = Some(Instant::now());
        self.stale_served.store(false, Ordering::Relaxed);
        Ok(data)
    }

    /// 使缓存失效（写入后由 DAL 自动调用；下次访问重新加载）。
    pub fn invalidate(&self) {
        let mut state = self.write_state();
        state.data = None;
        state.loaded_at = None;
        self.stale_served.store(false, Ordering::Relaxed);
    }

    /// 在已缓存的整表数据上做过滤查找（未加载时返回空列表）。
    pub fn find_all<P>(&self, predicate: P) -> Vec<DbRow>
    where
        P: Fn(&DbRow) -> bool,
    {
        let state = self.read_state();
        match &state.data {
            Some(data) => data.iter().filter(|row| predicate(row)).cloned().collect(),
            None => Vec::new(),
        }
    }

    /// 在已缓存的整表数据上按列值查找首个匹配行。
    pub fn find_first<P>(&self, predicate: P) -> Option<DbRow>
    where
        P: Fn(&DbRow) -> bool,
    {
        let state = self.read_state();
        state
            .data
            .as_ref()
            .and_then(|data| data.iter().find(|row| predicate(row)).cloned())
    }

    /// 在已缓存的整表数据上按主键列查找（对应 C# 的 `Meta.Cache.Entities.Find(__.ID, id)`）。
    pub fn find_by_pk(&self, column: &str, value: &DbValue) -> Option<DbRow> {
        self.find_first(|row| row.get_by_name(column) == Some(value))
    }
}

/// 单对象缓存（以主键为键逐行缓存，对应 DH.NCode 的 `Meta.SingleCache`）。
pub struct SingleCache {
    /// 表名（注册键，小写）
    table: String,
    /// 过期时间（纳秒；原子读写）
    expire_nanos: AtomicU64,
    /// 最大实体数（0 表示不限制）
    max_entity: Mutex<usize>,
    /// 缓存项（键为归一化主键文本）
    state: Mutex<SingleCacheState>,
}

/// 单对象缓存内部状态。
#[derive(Default)]
struct SingleCacheState {
    /// 主键 → (行数据, 写入时刻)
    items: HashMap<PkKey, (Arc<DbRow>, Instant)>,
}

impl SingleCache {
    /// 按表名创建（一般经 [`Dal::single_cache`] 获取共享实例）。
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            expire_nanos: AtomicU64::new(DEFAULT_EXPIRE_SECONDS * NANOS_PER_SECOND),
            max_entity: Mutex::new(DEFAULT_MAX_ENTITY),
            state: Mutex::new(SingleCacheState::default()),
        }
    }

    /// 表名（小写）。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 当前过期时间。
    pub fn expire(&self) -> Duration {
        Duration::from_nanos(self.expire_nanos.load(Ordering::Relaxed))
    }

    /// 设置过期时间（秒）。
    pub fn set_expire_seconds(&self, seconds: u64) {
        self.expire_nanos
            .store(seconds.saturating_mul(NANOS_PER_SECOND), Ordering::Relaxed);
    }

    /// 最大实体数。
    pub fn max_entity(&self) -> usize {
        *lock(&self.max_entity)
    }

    /// 设置最大实体数（0 表示不限制；超出时整体清空后重填）。
    pub fn set_max_entity(&self, max: usize) {
        *lock(&self.max_entity) = max;
    }

    /// 当前缓存项数量。
    pub fn count(&self) -> usize {
        lock(&self.state).items.len()
    }

    /// 是否包含指定主键（含已过期项；过期项会在下次 [`Self::get`] 时重载）。
    pub fn contains(&self, pk: &[DbValue]) -> bool {
        lock(&self.state).items.contains_key(&pk_key(pk))
    }

    /// 取得实体（未命中或过期时查库并回填；查不到时不缓存空值）。
    pub fn get(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
    ) -> Result<Option<Arc<DbRow>>> {
        let key = pk_key(pk);
        {
            let expire = self.expire();
            let state = lock(&self.state);
            if let Some((row, at)) = state.items.get(&key)
                && at.elapsed() < expire
            {
                return Ok(Some(row.clone()));
            }
        }

        let table = dal.table(&self.table)?;
        let row = table.find_by_pk(session, pk)?;
        if let Some(found) = &row {
            self.store(key, Arc::new(found.clone()));
        }
        Ok(row.map(Arc::new))
    }

    /// 写入缓存项（超出最大实体数时先整体清空）。
    fn store(&self, key: PkKey, row: Arc<DbRow>) {
        let max = *lock(&self.max_entity);
        let mut state = lock(&self.state);
        if max > 0 && state.items.len() >= max {
            state.items.clear();
        }
        state.items.insert(key, (row, Instant::now()));
    }

    /// 按主键移除缓存项（对应 C# 的 `SingleCache.Remove(entity)`）。
    pub fn remove(&self, pk: &[DbValue]) {
        lock(&self.state).items.remove(&pk_key(pk));
    }

    /// 清空全部缓存项（对应 C# 的 `SingleCache.Clear(reason)`）。
    pub fn clear(&self) {
        lock(&self.state).items.clear();
    }
}

/// 字段缓存的默认过期时间（秒；对齐 C# `FieldCache` 的 10 分钟）。
pub const DEFAULT_FIELD_CACHE_EXPIRE_SECONDS: u64 = 600;
/// 字段缓存默认保留的分组数（对齐 C# 生成代码的 `limit 20`）。
pub const DEFAULT_FIELD_CACHE_TOP: usize = 20;

/// 字段缓存状态：分组统计（值, 计数）+ 采样时刻。
type FieldState = Option<(Arc<Vec<(String, i64)>>, Instant)>;

/// 字段缓存（对应 DH.NCode 的 `FieldCache<T>`）：按某列分组统计，取出现次数最多的前 N 种。
///
/// 典型用途：分类/字典的**下拉选项**——避免每次请求都做 `GROUP BY`。
/// 默认缓存 10 分钟；该表任何添删改都会使其失效。
pub struct FieldCache {
    /// 物理表名
    table: String,
    /// 物理列名
    column: String,
    /// 过期时间（纳秒）
    expire_nanos: AtomicU64,
    /// 保留的分组数
    top: AtomicUsize,
    /// 缓存项（值 + 计数，按计数降序）
    state: RwLock<FieldState>,
}

impl FieldCache {
    /// 创建（表名/列名均为**物理名**；一般经 [`Dal::field_cache`] 获取共享实例）。
    pub fn new(table: &str, column: &str) -> Self {
        Self {
            table: table.to_string(),
            column: column.to_string(),
            expire_nanos: AtomicU64::new(DEFAULT_FIELD_CACHE_EXPIRE_SECONDS * NANOS_PER_SECOND),
            top: AtomicUsize::new(DEFAULT_FIELD_CACHE_TOP),
            state: RwLock::new(None),
        }
    }

    /// 列名（物理）。
    pub fn column(&self) -> &str {
        &self.column
    }

    /// 当前过期时间。
    pub fn expire(&self) -> Duration {
        Duration::from_nanos(self.expire_nanos.load(Ordering::Relaxed))
    }

    /// 设置过期时间（秒；0 = 每次重新统计）。
    pub fn set_expire_seconds(&self, seconds: u64) {
        self.expire_nanos
            .store(seconds.saturating_mul(NANOS_PER_SECOND), Ordering::Relaxed);
    }

    /// 保留的分组数。
    pub fn top(&self) -> usize {
        self.top.load(Ordering::Relaxed)
    }

    /// 设置保留的分组数（默认 20）。
    pub fn set_top(&self, top: usize) {
        self.top.store(top.max(1), Ordering::Relaxed);
    }

    /// 使缓存失效（写入路径自动调用）。
    pub fn invalidate(&self) {
        *self.write_state() = None;
    }

    fn read_state(&self) -> RwLockReadGuard<'_, FieldState> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_state(&self) -> RwLockWriteGuard<'_, FieldState> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// 分组统计结果（值 + 计数，按计数降序），命中缓存时不查库。
    pub fn items(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
    ) -> Result<Arc<Vec<(String, i64)>>> {
        let expire = self.expire();
        {
            let state = self.read_state();
            if let Some((items, at)) = state.as_ref()
                && at.elapsed() < expire
            {
                return Ok(items.clone());
            }
        }
        let mut guard = self.write_state();
        // 双重检查：并发过期时只有一个线程查库
        if let Some((items, at)) = guard.as_ref()
            && at.elapsed() < expire
        {
            return Ok(items.clone());
        }

        let kind = dal.kind();
        let top = self.top();
        let mut sql = format!(
            "SELECT COUNT(*) AS c, {} AS v FROM {} GROUP BY {} ORDER BY c DESC",
            kind.quote(&self.column),
            kind.quote(&self.table),
            kind.quote(&self.column),
        );
        // 方言不支持 LIMIT 时取全量、内存截断（时序/文档库的兼容退让）
        if let Some(suffix) = limit_suffix(kind, top) {
            sql.push_str(&suffix);
        }
        dal.log_sql(&sql);
        let rows = session.query(&sql, &[])?;
        let mut items: Vec<(String, i64)> = rows
            .rows
            .iter()
            .filter_map(|row| {
                let value = row.get_by_name("v")?;
                if value.is_null() {
                    return None;
                }
                let count = row
                    .get_by_name("c")
                    .and_then(DbValue::as_i64)
                    .unwrap_or_default();
                Some((value.to_text(), count))
            })
            .collect();
        if items.len() > top {
            items.truncate(top);
        }
        let items = Arc::new(items);
        *guard = Some((items.clone(), Instant::now()));
        Ok(items)
    }

    /// 下拉选项（按计数降序的值列表）。
    pub fn names(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<Vec<String>> {
        Ok(self
            .items(dal, session)?
            .iter()
            .map(|(value, _)| value.clone())
            .collect())
    }

    /// 值 → 值映射（对应 C# `FieldCache.FindAllName()` 的 `IDictionary<String,String>`）。
    pub fn name_map(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        Ok(self
            .items(dal, session)?
            .iter()
            .map(|(value, _)| (value.clone(), value.clone()))
            .collect())
    }
}

/// `GROUP BY` 统计的 LIMIT 子句（方言差异；不支持时返回 `None`）。
fn limit_suffix(kind: DatabaseKind, top: usize) -> Option<String> {
    let suffix = match kind {
        DatabaseKind::Sqlite
        | DatabaseKind::MySql
        | DatabaseKind::PostgreSql
        | DatabaseKind::DuckDb
        | DatabaseKind::ClickHouse
        | DatabaseKind::Hana
        | DatabaseKind::DaMeng
        | DatabaseKind::Iris => format!(" LIMIT {top}"),
        DatabaseKind::SqlServer => format!(" OFFSET 0 ROWS FETCH NEXT {top} ROWS ONLY"),
        DatabaseKind::Oracle | DatabaseKind::Db2 => format!(" FETCH FIRST {top} ROWS ONLY"),
        DatabaseKind::Firebird => format!(" ROWS {top}"),
        // 时序/文档库的 SQL 子集不支持该句式：不限制，由调用方内存截断
        DatabaseKind::TDengine
        | DatabaseKind::InfluxDb
        | DatabaseKind::MongoDb
        | DatabaseKind::Access => {
            return None;
        }
    };
    Some(suffix)
}

impl Dal {
    /// 获取字段缓存（按「表 + 列」共享，惰性创建；对应 DH.NCode 的 `FieldCache<T>`）。
    ///
    /// `table` 可传实体名或物理表名，`column` 可传属性名或列名。
    pub fn field_cache(&self, table: &str, column: &str) -> Result<Arc<FieldCache>> {
        self.has_caches
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let meta = self.table_meta(table)?;
        let physical_table = meta.effective_table_name();
        let col = meta.column(column).ok_or_else(|| {
            crate::error::Error::Model(format!("表 {} 不存在列 {column}", meta.name))
        })?;
        let physical_column = meta.effective_column_name(col);

        if let Some(cache) = read_lock(&self.field_caches)
            .get(physical_table)
            .and_then(|columns| columns.get(physical_column))
        {
            return Ok(cache.clone());
        }
        let mut field_maps = write_lock(&self.field_caches);
        let columns = field_maps.entry(physical_table.to_string()).or_default();
        Ok(columns
            .entry(physical_column.to_string())
            .or_insert_with(|| Arc::new(FieldCache::new(physical_table, physical_column)))
            .clone())
    }

    /// 获取实体缓存（按表共享，惰性创建；对应 DH.NCode 的 `Meta.Cache`）。
    pub fn entity_cache(&self, table: &str) -> Result<Arc<EntityCache>> {
        self.has_caches
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 叫名归一（实体名/物理名、大小写均可）→ 注册键统一用物理表名原文
        let name = self.table_meta(table)?.effective_table_name();
        if let Some(cache) = read_lock(&self.entity_caches).get(name) {
            return Ok(cache.clone());
        }
        let mut map = write_lock(&self.entity_caches);
        if let Some(cache) = map.get(name) {
            return Ok(cache.clone());
        }
        let cache = Arc::new(EntityCache::new(name));
        map.insert(name.to_string(), cache.clone());
        Ok(cache)
    }

    /// 获取单对象缓存（按表共享，惰性创建；对应 DH.NCode 的 `Meta.SingleCache`）。
    pub fn single_cache(&self, table: &str) -> Result<Arc<SingleCache>> {
        self.has_caches
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let name = self.table_meta(table)?.effective_table_name();
        if let Some(cache) = read_lock(&self.single_caches).get(name) {
            return Ok(cache.clone());
        }
        let mut map = write_lock(&self.single_caches);
        if let Some(cache) = map.get(name) {
            return Ok(cache.clone());
        }
        let cache = Arc::new(SingleCache::new(name));
        map.insert(name.to_string(), cache.clone());
        Ok(cache)
    }

    /// 使指定表的两类缓存全部失效（写入路径自动调用，也可手动触发）。
    ///
    /// 对应 DH.NCode：任何添删改操作都让缓存马上过期。
    /// `table` 可传实体名或实际表名（忽略大小写）。
    pub fn invalidate_cache(&self, table: &str) {
        // 快路径：从未创建过任何缓存（写多读少的应用常见）→ 直接返回，不取锁、不分配
        if !self.has_caches.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let owned;
        let key = match self.table_meta(table) {
            Ok(meta) => meta.effective_table_name(),
            Err(_) => {
                owned = table.to_string();
                owned.as_str()
            }
        };
        if let Some(cache) = read_lock(&self.entity_caches).get(key) {
            cache.invalidate();
        }
        if let Some(cache) = read_lock(&self.single_caches).get(key) {
            cache.clear();
        }
        // 字段缓存（先按表取一次，避免全注册表扫描）
        {
            let mut field_maps = write_lock(&self.field_caches);
            if let Some(columns) = field_maps.remove(key) {
                for cache in columns.into_values() {
                    cache.invalidate();
                }
                // 保留注册项：清空数据但避免下次重新分配（重新插回空表）
                field_maps.insert(key.to_string(), HashMap::new());
            }
        }
        // 行数缓存同步失效（下次按 MAX(id)/COUNT(*) 重新采样）
        write_lock(&self.count_cache).remove(key);
    }

    /// 表行数（带 60 秒缓存；对应 C# `EntitySession.Count` 的阀值判定用途）。
    ///
    /// 用于"小表走整表缓存"的判定（`Model.xml` 的 `MaxCacheCount`），
    /// **不应作为业务精确计数**——需要精确值直接 `SELECT COUNT(*)`。
    ///
    /// 采样策略（对齐 C# `GetCount`）：SQLite 且有自增列时用 `MAX(自增列)`（`COUNT(*)` 在 SQLite 上很慢），
    /// 其余数据库用 `COUNT(*)`；任何添删改都使缓存失效。
    pub fn cached_count(&self, table: &str, session: &mut dyn SqlSession) -> Result<i64> {
        let meta = self.table_meta(table)?;
        let key = meta.effective_table_name();

        const TTL: Duration = Duration::from_secs(60);
        {
            let cache = read_lock(&self.count_cache);
            if let Some((count, at)) = cache.get(key)
                && at.elapsed() < TTL
            {
                return Ok(*count);
            }
        }

        let sql = match (self.kind(), meta.identity()) {
            // SQLite：自增列最大值即近似行数（空表为 NULL → 0）
            (crate::dialect::DatabaseKind::Sqlite, Some(column)) => format!(
                "SELECT MAX({}) AS c FROM {}",
                self.kind().quote(meta.effective_column_name(column)),
                self.kind().quote(meta.effective_table_name()),
            ),
            _ => format!(
                "SELECT COUNT(*) AS c FROM {}",
                self.kind().quote(meta.effective_table_name())
            ),
        };
        self.log_sql(&sql);
        let rows = session.query(&sql, &[])?;
        let count = rows
            .first()
            .and_then(|r| r.get_by_name("c"))
            .and_then(DbValue::as_i64)
            .unwrap_or(0);
        write_lock(&self.count_cache).insert(key.to_string(), (count, std::time::Instant::now()));
        Ok(count)
    }

    /// 读取整表（实体缓存加载用；列引用按方言转义）。
    pub(crate) fn select_all_rows(
        &self,
        table: &str,
        session: &mut dyn SqlSession,
    ) -> Result<Vec<DbRow>> {
        let meta = self.table_meta(table)?;
        let (sql, params) =
            crate::sqlbuild::select_sql(self.kind(), meta, &crate::query::Query::new());
        self.log_sql(&sql);
        Ok(session.query(&sql, &params)?.rows)
    }

    /// 按名取表定义（表名或实体名，忽略大小写）。
    pub(crate) fn table_meta(&self, table: &str) -> Result<&TableMeta> {
        self.dal_table_meta(table)
            .ok_or_else(|| crate::error::Error::Model(format!("模型中不存在表/实体：{table}")))
    }
}

impl<'a> TableRef<'a> {
    /// 该表的实体缓存（对应 DH.NCode 的 `Meta.Cache`）。
    pub fn entity_cache(&self) -> Result<Arc<EntityCache>> {
        self.dal().entity_cache(self.meta().name.as_str())
    }

    /// 该表的单对象缓存（对应 DH.NCode 的 `Meta.SingleCache`）。
    pub fn single_cache(&self) -> Result<Arc<SingleCache>> {
        self.dal().single_cache(self.meta().name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Item" TableName="DH_Item">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    /// 建一个临时 SQLite 库并建表。
    fn temp_dal(name: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir =
            std::env::temp_dir().join(format!("rcode-cache-{}-{stamp}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("cache.db");
        let model = EntityModel::parse(MODEL).expect("测试模型应可解析");
        let dal = Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db.display()),
            model,
        )
        .unwrap();
        dal.sync_schema().unwrap();
        (dal, dir)
    }

    #[test]
    fn entity_cache_hits_until_invalidated() {
        let (dal, dir) = temp_dal("entity");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        let id = table
            .insert(session.as_mut(), &[("Code", "A".into())])
            .unwrap();
        assert!(id > 0);

        let cache = dal.entity_cache("Item").unwrap();
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 1);

        // 绕过表句柄直接写库（模拟外部写入）：缓存未失效时仍读旧数据（命中证明）
        session
            .execute(
                "INSERT INTO \"DH_Item\" (\"Code\") VALUES (?)",
                &[DbValue::Text("B".into())],
            )
            .unwrap();
        assert_eq!(
            cache.entities(&dal, session.as_mut()).unwrap().len(),
            1,
            "未失效时应命中缓存"
        );

        // 手动失效后重新加载
        dal.invalidate_cache("Item");
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 2);

        // 经表句柄写入自动失效
        table
            .insert(session.as_mut(), &[("Code", "C".into())])
            .unwrap();
        assert_eq!(
            cache.entities(&dal, session.as_mut()).unwrap().len(),
            3,
            "写入应自动使缓存失效"
        );

        // 内存内按主键查找（不再回库）
        let found = cache.find_by_pk("Id", &DbValue::Int(1)).unwrap();
        assert_eq!(found.get_by_name("Code").unwrap().as_str(), Some("A"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_cache_queries_once_then_hits() {
        let (dal, dir) = temp_dal("single");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        let id = table
            .insert(session.as_mut(), &[("Code", "X".into())])
            .unwrap();

        let cache = dal.single_cache("Item").unwrap();
        let first = cache
            .get(&dal, session.as_mut(), &[DbValue::Int(id)])
            .unwrap()
            .unwrap();
        assert_eq!(first.get_by_name("Code").unwrap().as_str(), Some("X"));
        assert!(cache.contains(&[DbValue::Int(id)]));

        // 绕过句柄改库：缓存命中返回旧值（命中证明）
        session
            .execute(
                "UPDATE \"DH_Item\" SET \"Code\" = ? WHERE \"Id\" = ?",
                &[DbValue::Text("Y".into()), DbValue::Int(id)],
            )
            .unwrap();
        let cached = cache
            .get(&dal, session.as_mut(), &[DbValue::Int(id)])
            .unwrap()
            .unwrap();
        assert_eq!(
            cached.get_by_name("Code").unwrap().as_str(),
            Some("X"),
            "未失效时应命中缓存"
        );

        // 表句柄更新 → 自动清空，重新查库拿到新值
        table
            .update_by_pk(
                session.as_mut(),
                &[("Code", "Z".into())],
                &[DbValue::Int(id)],
            )
            .unwrap();
        let fresh = cache
            .get(&dal, session.as_mut(), &[DbValue::Int(id)])
            .unwrap()
            .unwrap();
        assert_eq!(
            fresh.get_by_name("Code").unwrap().as_str(),
            Some("Z"),
            "写入应自动使单对象缓存失效"
        );

        // 删除后查询：不缓存空值
        table
            .delete_by_pk(session.as_mut(), &[DbValue::Int(id)])
            .unwrap();
        assert!(
            cache
                .get(&dal, session.as_mut(), &[DbValue::Int(id)])
                .unwrap()
                .is_none()
        );
        assert_eq!(cache.count(), 0, "删除使缓存清空");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expire_reloads_when_elapsed() {
        let (dal, dir) = temp_dal("expire");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        table
            .insert(session.as_mut(), &[("Code", "A".into())])
            .unwrap();

        let cache = dal.entity_cache("Item").unwrap();
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 1);
        assert!(cache.is_valid());

        // 外部写入（绕过句柄）
        session
            .execute(
                "INSERT INTO \"DH_Item\" (\"Code\") VALUES (?)",
                &[DbValue::Text("B".into())],
            )
            .unwrap();

        // 默认 Blocking：过期后第一次访问即同步重载（强一致）
        cache.set_expire_seconds(0);
        assert!(!cache.is_valid());
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_while_revalidate_serves_old_then_refreshes() {
        let (dal, dir) = temp_dal("stale");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        table
            .insert(session.as_mut(), &[("Code", "A".into())])
            .unwrap();

        let cache = dal.entity_cache("Item").unwrap();
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 1);

        session
            .execute(
                "INSERT INTO \"DH_Item\" (\"Code\") VALUES (?)",
                &[DbValue::Text("B".into())],
            )
            .unwrap();

        // 开启 SWR 并立即过期：首次访问返回旧值，第二次访问刷新
        cache.set_consistency(CacheConsistency::StaleWhileRevalidate {
            max_stale: Duration::from_secs(60),
        });
        cache.set_expire_seconds(0);
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 1);
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发过期：写锁内单飞刷新，所有线程都能拿到数据（不会击穿成多次整表查询）。
    #[test]
    fn concurrent_expiry_single_flight() {
        let (dal, dir) = temp_dal("single-flight");
        {
            let mut session = dal.open_session().unwrap();
            let table = dal.table("Item").unwrap();
            table
                .insert(session.as_mut(), &[("Code", "A".into())])
                .unwrap();
            let cache = dal.entity_cache("Item").unwrap();
            assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 1);
            cache.set_expire_seconds(0);
        }

        let dal = std::sync::Arc::new(dal);
        let cache = dal.entity_cache("Item").unwrap();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let dal = dal.clone();
            let cache = cache.clone();
            handles.push(std::thread::spawn(move || {
                let mut session = dal.open_session().unwrap();
                cache.entities(&dal, session.as_mut()).unwrap().len()
            }));
        }
        for handle in handles {
            assert_eq!(handle.join().unwrap(), 1, "并发过期应都能拿到数据");
        }

        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
