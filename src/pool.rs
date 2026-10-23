//! 会话连接池（对应 DH.NCode 的 `Common/ConnectionPool.cs`）。
//!
//! 语义对齐 C#：按连接串（每个 [`crate::dal::Dal`]）共享；空闲超时（默认 30 秒）后丢弃；
//! 空闲上限默认 1000；最小连接数默认 CPU 核数（2–8）。
//!
//! 与 C# 的差异（机制差异，非功能缺失）：
//! - C# 由后台定时器清理空闲连接；Rust 版在**借用/归还时顺带清理**，不引入后台线程
//! - C# 的 `Min` 由后台预热；Rust 版**按需创建**（不预建空连接），`min` 仅作配置对齐
//! - 调用出错的会话与未结束事务的会话，**归还时直接关闭**，避免污染池

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::dialect::DatabaseKind;
use crate::error::Result;
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// 取锁（被投毒时恢复内部数据，与 cache 模块约定一致）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 池配置（缺省值对齐 C# `ConnectionPool`：Min=CPU(2–8)、Max=1000、空闲 30s）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolOptions {
    /// 最小连接数（对齐 C# 配置；本实现按需创建，不预建连接）
    pub min: usize,
    /// 最大连接数（空闲超过该值时，归还的连接直接关闭）
    pub max: usize,
    /// 空闲超时（超过后丢弃，下次使用重新创建）
    pub idle_time: Duration,
}

impl Default for PoolOptions {
    /// 缺省配置。
    /// <returns>池配置</returns>
    fn default() -> Self {
        let cpu = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, 8);
        Self {
            min: cpu,
            max: 1000,
            idle_time: Duration::from_secs(30),
        }
    }
}

/// 池统计（用于诊断与测试）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// 累计创建的会话数
    pub created: u64,
    /// 累计复用的会话数
    pub reused: u64,
    /// 累计丢弃的会话数（过期/损坏/未结束事务/超限）
    pub discarded: u64,
}

/// 会话工厂：创建新会话（由 `Dal` 注入，内部逐驱动分发）。
pub(crate) type SessionFactory = Arc<dyn Fn() -> Result<Box<dyn SqlSession>> + Send + Sync>;

/// 空闲会话条目。
struct IdleSession {
    /// 会话
    session: Box<dyn SqlSession>,
    /// 归还时刻（粗时钟毫秒；见 [`coarse_now_ms`]）
    since_ms: u64,
}

/// 系统粗时钟（毫秒）。
///
/// 为什么不用 `Instant::now()`：Windows 上它是 `QueryPerformanceCounter`，实测≈60 ns；
/// 而一次会话借还总共才 150 ns —— 两次取时间就占了 **78%**（实测 118.4 ns）。
/// 空闲超时是秒级判定，1 ms 精度完全够用：
/// - Windows：`GetTickCount64`（系统启动毫秒数，~3 ns，直接链接 kernel32，无需额外依赖）
/// - Unix：进程内固定 epoch 的 `Instant::elapsed()`（vDSO，~20 ns）
#[inline]
fn coarse_now_ms() -> u64 {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            safe fn GetTickCount64() -> u64;
        }
        GetTickCount64()
    }
    #[cfg(unix)]
    {
        use std::sync::OnceLock;
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        EPOCH
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }
    #[cfg(not(any(windows, unix)))]
    {
        use std::sync::OnceLock;
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        EPOCH
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }
}

/// 空闲队列分片数：按 CPU 核数（2–16）。
///
/// 高并发下单一 `Mutex<VecDeque>` 会让所有线程串行（实测 8 线程比单线程慢 4.4 倍）；
/// 按线程分片后，各线程只抢自己的分片，锁争用基本消失。
fn shard_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 16)
}

thread_local! {
    /// 本线程偏好的分片（首次借出时分配，之后固定 → 归还进同一分片，命中率最高）
    static LOCAL_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// 会话连接池。
pub struct SessionPool {
    /// 配置
    options: PoolOptions,
    /// 会话工厂
    factory: SessionFactory,
    /// 空闲会话分片队列（FIFO：队首最旧）
    shards: Box<[Mutex<VecDeque<IdleSession>>]>,
    /// 每分片空闲上限（总量≈`options.max`）
    per_shard_cap: usize,
    /// 空闲超时（毫秒，避免每次比较重新转换）
    idle_time_ms: u64,
    /// 分片分配计数器
    next_shard: AtomicUsize,
    /// 累计创建（每分片独立计数：高并发下各线程只写自己的 cacheline，避免伪共享）
    created: Box<[AtomicU64]>,
    /// 累计复用（分片独立）
    reused: Box<[AtomicU64]>,
    /// 累计丢弃（分片独立）
    discarded: Box<[AtomicU64]>,
}

impl SessionPool {
    /// 创建池。
    /// <param name="options">池配置</param>
    /// <param name="factory">会话工厂</param>
    /// <returns>会话池</returns>
    pub fn new(options: PoolOptions, factory: SessionFactory) -> Self {
        let shards = shard_count();
        let per_shard_cap = if options.max == 0 {
            0
        } else {
            options.max.div_ceil(shards)
        };
        Self {
            options,
            factory,
            shards: (0..shards)
                .map(|_| Mutex::new(VecDeque::new()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            per_shard_cap,
            idle_time_ms: options.idle_time.as_millis() as u64,
            next_shard: AtomicUsize::new(0),
            created: (0..shards).map(|_| AtomicU64::new(0)).collect(),
            reused: (0..shards).map(|_| AtomicU64::new(0)).collect(),
            discarded: (0..shards).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// 池配置。
    pub fn options(&self) -> PoolOptions {
        self.options
    }

    /// 统计快照。
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            created: self.created.iter().map(|c| c.load(Ordering::Relaxed)).sum(),
            reused: self.reused.iter().map(|c| c.load(Ordering::Relaxed)).sum(),
            discarded: self
                .discarded
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .sum(),
        }
    }

    /// 当前空闲会话数。
    pub fn idle_count(&self) -> usize {
        self.shards.iter().map(|shard| lock(shard).len()).sum()
    }

    /// 清空空闲会话（全部关闭；池仍可继续使用）。
    pub fn clear(&self) {
        for shard in self.shards.iter() {
            lock(shard).clear();
        }
    }

    /// 预热：预建 `count` 个会话放入空闲分片（失败即停止，返回成功数）。
    ///
    /// **为什么值得预热**：建连不是免费的——MySQL 一次建连要「TCP 连接 + 认证握手 +
    /// 会话初始化（`SET NAMES`）」，约 **3~4 次往返**（本地回环 ≈ 300~400 µs，远程按 RTT 放大）。
    /// 冷启动时首批并发请求会各自承担这笔开销，预热把这笔成本前置到启动阶段。
    ///
    /// 一般经 [`crate::dal::Dal::warmup_pool`] 调用（连接串 `PoolMinSize=n` 也可自动预热）。
    pub fn prewarm(&self, count: usize) -> usize {
        if self.per_shard_cap == 0 || self.idle_time_ms == 0 {
            return 0;
        }
        let mut created = 0usize;
        for offset in 0..count {
            let Ok(session) = (self.factory)() else {
                break;
            };
            let index = offset % self.shards.len();
            let mut idle = lock(&self.shards[index]);
            if idle.len() >= self.per_shard_cap {
                break;
            }
            idle.push_back(IdleSession {
                session,
                since_ms: coarse_now_ms(),
            });
            self.note_create(index);
            created += 1;
        }
        created
    }

    /// 记一次丢弃（写入本线程分片的计数）。
    fn note_discard(&self, index: usize) {
        self.discarded[index % self.discarded.len()].fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次新建（写入本线程分片的计数）。
    fn note_create(&self, index: usize) {
        self.created[index % self.created.len()].fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次复用（写入本线程分片的计数）。
    fn note_reuse(&self, index: usize) {
        self.reused[index % self.reused.len()].fetch_add(1, Ordering::Relaxed);
    }

    /// 偏好的分片（首次借出时分配并缓存到线程本地）。
    fn shard_index(&self) -> usize {
        let n = self.shards.len();
        if n == 1 {
            return 0;
        }
        let cached = LOCAL_SHARD.with(|cell| cell.get());
        if cached != usize::MAX {
            return cached % n;
        }
        let index = self.next_shard.fetch_add(1, Ordering::Relaxed) % n;
        LOCAL_SHARD.with(|cell| cell.set(index));
        index
    }

    /// 借出一个会话：优先复用本线程分片的空闲会话，无可用时按工厂新建。
    /// <returns>池化会话（归还由 `Drop` 自动完成）</returns>
    pub(crate) fn checkout(self: &Arc<Self>) -> Result<Box<dyn SqlSession>> {
        let n = self.shards.len();
        // 常见情形（本线程上次归还的就在自己分片）：1 次锁即命中
        let start = self.shard_index();
        if let Some(session) = self.take_idle(start) {
            self.note_reuse(start);
            return Ok(Box::new(PooledSession::new(session, self.clone())));
        }
        // 本分片空：尝试其它分片（会话曾被别的线程借走时会落到那边）
        for offset in 1..n {
            let index = (start + offset) % n;
            if let Some(session) = self.take_idle(index) {
                self.note_reuse(index);
                return Ok(Box::new(PooledSession::new(session, self.clone())));
            }
        }
        let session = (self.factory)()?;
        self.note_create(start);
        Ok(Box::new(PooledSession::new(session, self.clone())))
    }

    /// 归还会话（仅供 `PooledSession` 调用）。
    fn checkin(&self, session: Box<dyn SqlSession>) {
        let index = self.shard_index();
        // `max=0` 或 `idle_time=0` 都表示「不缓存空闲会话」（后者对齐原有语义）
        if self.per_shard_cap == 0 || self.idle_time_ms == 0 {
            self.note_discard(index);
            return;
        }
        let since_ms = coarse_now_ms();
        let mut idle = lock(&self.shards[index]);
        self.sweep_expired(&mut idle, index, since_ms);
        if idle.len() >= self.per_shard_cap {
            self.note_discard(index);
            return;
        }
        idle.push_back(IdleSession { session, since_ms });
    }

    /// 从指定分片取出一个未过期的空闲会话（顺带清理过期项）。
    fn take_idle(&self, index: usize) -> Option<Box<dyn SqlSession>> {
        let now_ms = coarse_now_ms();
        let mut idle = lock(&self.shards[index]);
        while let Some(entry) = idle.pop_front() {
            if now_ms.saturating_sub(entry.since_ms) <= self.idle_time_ms {
                return Some(entry.session);
            }
            self.note_discard(index);
        }
        None
    }

    /// 清理队首的过期会话（FIFO 队列，过期者必在队首连续分布）。
    fn sweep_expired(&self, idle: &mut VecDeque<IdleSession>, index: usize, now_ms: u64) {
        while let Some(entry) = idle.front() {
            if now_ms.saturating_sub(entry.since_ms) > self.idle_time_ms {
                idle.pop_front();
                self.note_discard(index);
            } else {
                break;
            }
        }
    }
}

/// 池化会话：代理底层会话，`Drop` 时自动归还。
struct PooledSession {
    /// 底层会话（`Drop` 时取出）
    inner: Option<Box<dyn SqlSession>>,
    /// 所属池
    pool: Arc<SessionPool>,
    /// 是否处于事务中
    in_tx: bool,
    /// 是否已损坏（任一步骤出错即视为损坏，归还时关闭）
    broken: bool,
}

impl PooledSession {
    /// 包装底层会话。
    fn new(inner: Box<dyn SqlSession>, pool: Arc<SessionPool>) -> Self {
        Self {
            inner: Some(inner),
            pool,
            in_tx: false,
            broken: false,
        }
    }

    /// 底层会话引用。
    fn inner_mut(&mut self) -> &mut dyn SqlSession {
        self.inner.as_mut().expect("池化会话不可用").as_mut()
    }

    /// 记录调用结果：出错 → 标记损坏（归还时关闭，避免坏连接污染池）。
    fn note<T>(&mut self, result: &Result<T>) {
        if result.is_err() {
            self.broken = true;
        }
    }
}

impl SqlSession for PooledSession {
    fn kind(&self) -> DatabaseKind {
        self.inner.as_ref().expect("池化会话不可用").kind()
    }

    fn execute(&mut self, sql: &str, params: &[DbValue]) -> Result<u64> {
        let result = self.inner_mut().execute(sql, params);
        self.note(&result);
        result
    }

    fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<RowSet> {
        let result = self.inner_mut().query(sql, params);
        self.note(&result);
        result
    }

    fn begin(&mut self) -> Result<()> {
        let result = self.inner_mut().begin();
        if result.is_ok() {
            self.in_tx = true;
        }
        self.note(&result);
        result
    }

    fn commit(&mut self) -> Result<()> {
        let result = self.inner_mut().commit();
        if result.is_ok() {
            self.in_tx = false;
        }
        self.note(&result);
        result
    }

    fn rollback(&mut self) -> Result<()> {
        let result = self.inner_mut().rollback();
        if result.is_ok() {
            self.in_tx = false;
        }
        self.note(&result);
        result
    }

    fn last_identity(&mut self) -> Result<i64> {
        let result = self.inner_mut().last_identity();
        self.note(&result);
        result
    }

    fn last_identity_of(&mut self, table: &str) -> Result<i64> {
        let result = self.inner_mut().last_identity_of(table);
        self.note(&result);
        result
    }

    fn insert_and_get_identity(
        &mut self,
        sql: &str,
        params: &[DbValue],
        table: Option<&str>,
    ) -> Result<i64> {
        let result = self
            .inner_mut()
            .insert_and_get_identity(sql, params, table);
        self.note(&result);
        result
    }

    fn table_exists(&mut self, table: &str) -> Result<bool> {
        let result = self.inner_mut().table_exists(table);
        self.note(&result);
        result
    }

    fn table_columns(&mut self, table: &str) -> Result<Vec<String>> {
        let result = self.inner_mut().table_columns(table);
        self.note(&result);
        result
    }
}

impl Drop for PooledSession {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        if self.broken || self.in_tx {
            let index = self.pool.shard_index();
            self.pool.note_discard(index);
        } else {
            self.pool.checkin(inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// 测试用会话：不访问任何真实数据库。
    struct MockSession {
        /// 执行是否失败（模拟损坏连接）
        fail: bool,
    }

    impl SqlSession for MockSession {
        fn kind(&self) -> DatabaseKind {
            DatabaseKind::Sqlite
        }

        fn execute(&mut self, _sql: &str, _params: &[DbValue]) -> Result<u64> {
            if self.fail {
                Err(crate::error::Error::Db("模拟故障".into()))
            } else {
                Ok(1)
            }
        }

        fn query(&mut self, _sql: &str, _params: &[DbValue]) -> Result<RowSet> {
            Ok(RowSet::default())
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

        fn table_exists(&mut self, _table: &str) -> Result<bool> {
            Ok(true)
        }

        fn table_columns(&mut self, _table: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// 构建测试池：返回（池，创建计数）。
    fn mock_pool(options: PoolOptions, fail: bool) -> (Arc<SessionPool>, Arc<AtomicUsize>) {
        let created = Arc::new(AtomicUsize::new(0));
        let counter = created.clone();
        let factory: SessionFactory =
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(MockSession { fail }))
            });
        (Arc::new(SessionPool::new(options, factory)), created)
    }

    #[test]
    fn checkout_reuses_returned_session() {
        let (pool, created) = mock_pool(PoolOptions::default(), false);

        let session = pool.checkout().unwrap();
        drop(session);
        let session = pool.checkout().unwrap();
        drop(session);

        assert_eq!(created.load(Ordering::Relaxed), 1, "归还后应复用同一会话");
        let stats = pool.stats();
        assert_eq!(stats.created, 1);
        assert_eq!(stats.reused, 1);
        assert_eq!(pool.idle_count(), 1);
    }

    #[test]
    fn expired_idle_session_is_recreated() {
        let options = PoolOptions {
            idle_time: Duration::ZERO,
            ..PoolOptions::default()
        };
        let (pool, created) = mock_pool(options, false);

        drop(pool.checkout().unwrap());
        let _session = pool.checkout().unwrap();

        assert_eq!(created.load(Ordering::Relaxed), 2, "过期会话应重新创建");
        assert_eq!(pool.stats().discarded, 1);
    }

    #[test]
    fn max_zero_closes_returned_sessions() {
        let options = PoolOptions {
            max: 0,
            ..PoolOptions::default()
        };
        let (pool, created) = mock_pool(options, false);

        drop(pool.checkout().unwrap());
        let _session = pool.checkout().unwrap();

        assert_eq!(created.load(Ordering::Relaxed), 2);
        assert_eq!(pool.idle_count(), 0);
    }

    #[test]
    fn transactional_session_is_not_reused() {
        let (pool, created) = mock_pool(PoolOptions::default(), false);

        let mut session = pool.checkout().unwrap();
        session.begin().unwrap();
        drop(session); // 未提交/回滚 → 直接关闭

        let _session = pool.checkout().unwrap();
        assert_eq!(created.load(Ordering::Relaxed), 2);
        assert_eq!(pool.stats().discarded, 1);
    }

    #[test]
    fn failed_session_is_not_reused() {
        let (pool, created) = mock_pool(PoolOptions::default(), true);

        let mut session = pool.checkout().unwrap();
        assert!(session.execute("boom", &[]).is_err());
        drop(session);

        let _session = pool.checkout().unwrap();
        assert_eq!(created.load(Ordering::Relaxed), 2, "出错的会话不应回池");
        assert_eq!(pool.stats().discarded, 1);
    }

    #[test]
    fn committed_transaction_session_is_reused() {
        let (pool, created) = mock_pool(PoolOptions::default(), false);

        let mut session = pool.checkout().unwrap();
        session.begin().unwrap();
        session.commit().unwrap();
        drop(session);

        let _session = pool.checkout().unwrap();
        assert_eq!(created.load(Ordering::Relaxed), 1, "已提交事务的会话可复用");
    }

    #[test]
    fn clear_pool_closes_idle_sessions() {
        let (pool, created) = mock_pool(PoolOptions::default(), false);

        drop(pool.checkout().unwrap());
        assert_eq!(pool.idle_count(), 1);
        pool.clear();
        assert_eq!(pool.idle_count(), 0);

        let _session = pool.checkout().unwrap();
        assert_eq!(created.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn dal_pool_reuses_sqlite_session_and_can_be_disabled() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rcode-pool-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("pool.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let model = crate::model::EntityModel::parse(
            r#"<EntityModel><Tables><Table Name="Item" TableName="DH_Item"><Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="50" />
            </Columns></Table></Tables></EntityModel>"#,
        )
        .unwrap();
        let dal = crate::dal::Dal::open_with_model(&conn, model).unwrap();
        dal.sync_schema().unwrap();
        let table = dal.table("Item").unwrap();

        {
            let mut session = dal.open_session().unwrap();
            table
                .insert(session.as_mut(), &[("Code", "A1".into())])
                .unwrap();
        } // 归还到池

        // sync_schema 已建立并归还过一路连接：全程只应存在一个连接，后续均为复用
        let stats = dal.pool_stats();
        assert_eq!(stats.created, 1, "全程只应建立一个连接");

        let reused_before = stats.reused;
        {
            let mut session = dal.open_session().unwrap();
            let count = table.count(session.as_mut(), None).unwrap();
            assert_eq!(count, 1);
        }
        assert_eq!(
            dal.pool_stats().reused,
            reused_before + 1,
            "后续会话应复用池内连接"
        );

        // Pooling=false：不再复用（统计保持缺省）
        let conn2 = format!("Data Source={};Provider=SQLite;Pooling=false", db.display());
        let dal2 = crate::dal::Dal::open(&conn2).unwrap();
        assert!(!dal2.pooling_enabled());
        let s1 = dal2.open_session().unwrap();
        let s2 = dal2.open_session().unwrap();
        assert_eq!(dal2.pool_stats(), PoolStats::default());
        drop(s1);
        drop(s2);

        dal.clear_pool();
        drop(dal);
        drop(dal2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
