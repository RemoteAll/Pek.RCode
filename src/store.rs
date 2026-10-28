//! 按数据目录共享的“单飞”数据访问层（进程级注册表）。
//!
//! 背景：多个线程（采集器 / 任务 / 面板）可能同时**首次**打开同一数据库，
//! 并发执行 `sync_schema` 建表会撞 “table already exists”
//! （2026-10-02 Pek.RAgent 服务器实测事故；DHDeploy / Pek.RPanlServer 同款修复）。
//! 本模块把“注册表锁内串行打开 + 连接复用 + 串行会话 + 默认拦截器装配”收敛为一处：
//!
//! - [`get_or_open`]：单飞打开（同一目录只执行一次 `open` 闭包，含建表；
//!   失败不注册、下次重试，便于调用方做“已恢复可用”日志节流）；
//! - [`SharedStore::with_session`] / [`SharedStore::lock`]：同一目录的读写串行；
//! - [`lookup`] / [`drop_for_test`]：查询与测试清理。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once};

use crate::dal::Dal;
use crate::session::SqlSession;
use crate::DatabaseKind;

/// 共享存储：一个数据目录一个数据访问层 + 一把串行锁。
pub struct SharedStore {
    base: PathBuf,
    dal: Dal,
    lock: Mutex<()>,
    /// 只读会话池（**仅 SQLite 文件库**：WAL 天然 1 写 N 读）。
    /// `None` = 读写分离不适用（内存库 / DuckDB / 网络库）→ 退回串行会话。
    reads: Option<ReadPool>,
    /// 只读池不可用而退回串行会话的次数。
    read_fallbacks: AtomicU64,
    /// 是否已发生「只读池降级」（宿主可据此告警一次，而不必自己去数次数）。
    read_degraded: AtomicBool,
}

impl SharedStore {
    /// 数据目录（规范化后的绝对路径）。
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// 数据访问层。
    pub fn dal(&self) -> &Dal {
        &self.dal
    }

    /// 串行会话：持锁打开一次会话执行 `f`（同一存储的读写与清理互斥）。
    pub fn with_session<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(&Dal, &mut dyn SqlSession) -> crate::Result<R>,
    {
        let _guard = self.lock();
        let mut session = self
            .dal
            .open_session()
            .map_err(|e| format!("数据会话创建失败：{e}"))?;
        f(&self.dal, session.as_mut()).map_err(|e| e.to_string())
    }

    /// 串行锁守卫（需要跨多段手写会话代码时使用；与 [`Self::with_session`] 共用同一把锁）。
    pub fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 并发执行一次会话操作（**不取进程内串行锁**，每次从连接池独立取会话）。
    ///
    /// 适用：MySQL / PostgreSQL / SQL Server 等支持多写者的网络数据库——
    /// 它们由服务端保证并发安全，进程内再串行化只会把吞吐压成单线程。
    ///
    /// **不适用**：SQLite / DuckDB 等进程内单写者数据库，并发写会撞 `database is locked`，
    /// 这类驱动请继续用 [`SharedStore::with_session`]。
    ///
    /// # 参数
    /// - `f`：会话回调。
    ///
    /// # 返回值
    /// 回调结果（错误转字符串）。
    pub fn with_session_concurrent<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(&Dal, &mut dyn SqlSession) -> crate::Result<R>,
    {
        let mut session = self
            .dal
            .open_session()
            .map_err(|e| format!("数据会话创建失败：{e}"))?;
        f(&self.dal, session.as_mut()).map_err(|e| e.to_string())
    }

    /// **只读**会话：与写路径互斥锁无关（SQLite/WAL 允许 1 写 N 读）。
    ///
    /// 面板查询 / 设备认证这类纯读请求走这里，不再被一次上万行的批量写堵住；
    /// 写路径仍然单写者串行（SQLite 只允许一个写者，这一点不能变）。
    ///
    /// 安全约束：
    /// - 只读连接带 `PRAGMA query_only=ON`，**写 SQL 会直接报错**（不会偷偷写进库）；
    /// - 只对 **SQLite 文件库** 生效：`Data Source=:memory:` / `mode=memory` 下每个连接是
    ///   独立数据库，不能开池（否则读到空库）；DuckDB / 网络库保持原行为；
    /// - 池不可用（如 `-shm` 无法创建、权限不足）时**自动退回串行会话**，
    ///   即“优化失效但功能不受影响”，并计入 [`SharedStore::read_fallback_count`]。
    ///
    /// # 参数
    /// - `f`：会话回调（只能发查询；写入会报错）。
    ///
    /// # 返回值
    /// 回调结果（错误转字符串）。
    pub fn with_read_session<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(&Dal, &mut dyn SqlSession) -> crate::Result<R>,
    {
        let Some(pool) = &self.reads else {
            // 不适用读写分离：行为与旧版完全一致
            return if matches!(self.dal.kind(), DatabaseKind::Sqlite | DatabaseKind::DuckDb) {
                self.with_session(f)
            } else {
                self.with_session_concurrent(f)
            };
        };
        let mut session = match pool.checkout(&self.dal) {
            Ok(session) => session,
            Err(_) => {
                // 只读连接建不起来：退回串行会话（宁可慢，不能因为“优化”让读请求失败）
                self.read_fallbacks.fetch_add(1, Ordering::Relaxed);
                self.read_degraded.store(true, Ordering::Relaxed);
                return if matches!(self.dal.kind(), DatabaseKind::Sqlite | DatabaseKind::DuckDb) {
                    self.with_session(f)
                } else {
                    self.with_session_concurrent(f)
                };
            }
        };
        let out = f(&self.dal, session.as_mut());
        pool.checkin(session);
        out.map_err(|e| e.to_string())
    }

    /// 只读池失效退回串行会话的次数（运维观察用；持续增长说明只读连接打不开）。
    ///
    /// # 返回值
    /// 累计次数。
    pub fn read_fallback_count(&self) -> u64 {
        self.read_fallbacks.load(Ordering::Relaxed)
    }

    /// 是否发生过「只读池降级」（只读连接建不起来，已退回串行会话）。
    ///
    /// 宿主可在面板/日志里拿它做一次性告警，不必自己计数。
    ///
    /// # 返回值
    /// 降级过返回 `true`。
    pub fn read_degraded(&self) -> bool {
        self.read_degraded.load(Ordering::Relaxed)
    }
}

/// 只读会话池（SQLite/WAL）。
///
/// 设计取舍：
/// - 空闲会话待在池里复用（建连 + 编译语句都有成本），上限 [`default_read_pool_size`]；
/// - 池满时**阻塞等**空位（`Condvar`）——这里不丢读请求，只排队；
/// - 会话建连失败时立即释放名额并把错误交给调用方（调用方会退回串行会话）。
struct ReadPool {
    state: Mutex<ReadPoolState>,
    wait: Condvar,
}

struct ReadPoolState {
    idle: Vec<Box<dyn SqlSession>>,
    /// 已创建（含借出）的会话数。
    created: usize,
    /// 上限。
    max: usize,
}

impl ReadPool {
    fn new(max: usize) -> Self {
        Self {
            state: Mutex::new(ReadPoolState {
                idle: Vec::new(),
                created: 0,
                max: max.max(1),
            }),
            wait: Condvar::new(),
        }
    }

    fn checkout(&self, dal: &Dal) -> Result<Box<dyn SqlSession>, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(session) = state.idle.pop() {
                return Ok(session);
            }
            if state.created < state.max {
                state.created += 1;
                drop(state);
                match dal.open_read_session() {
                    Ok(session) => return Ok(session),
                    Err(e) => {
                        // 建连失败：把名额退回去，再通知可能在等的人
                        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.created = state.created.saturating_sub(1);
                        self.wait.notify_one();
                        return Err(format!("创建只读会话失败：{e}"));
                    }
                }
            }
            state = self.wait.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn checkin(&self, session: Box<dyn SqlSession>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.idle.push(session);
        drop(state);
        self.wait.notify_one();
    }
}

/// 默认只读池大小：CPU 核数（2~8）。读请求通常很短，不需要与核数同量级的连接。
fn default_read_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 8)
}

/// 该库是否适合开只读池（仅 SQLite 文件库：内存库每个连接是**独立数据库**，开池会读到空表）。
fn read_pool_for(dal: &Dal) -> Option<ReadPool> {
    if dal.kind() != DatabaseKind::Sqlite || dal.is_readonly() {
        return None;
    }
    let source = dal.connection_string().data_source()?.trim().to_string();
    if source.is_empty() {
        return None;
    }
    let lower = source.to_ascii_lowercase();
    if lower == ":memory:" || lower.contains("mode=memory") {
        return None;
    }
    Some(ReadPool::new(default_read_pool_size()))
}

/// 注册表：（数据目录 + 作用域名）→ 共享存储（同一目录可承载多个逻辑库，如多数据源连接的多个文件）。
static REGISTRY: Mutex<BTreeMap<(PathBuf, String), Arc<SharedStore>>> = Mutex::new(BTreeMap::new());
/// 默认拦截器装配（每进程一次；TimeInterceptor 等由本模块统一保证）。
static INTERCEPTORS: Once = Once::new();

fn key_of(base: &Path) -> PathBuf {
    std::path::absolute(base).unwrap_or_else(|_| base.to_path_buf())
}

/// 取或“单飞”打开共享存储。
///
/// 已注册直接返回；否则**在注册表锁内**执行 `open`（首开含建表，只会成功一次），
/// 返回 `(存储, 是否本次打开)`——`true` 可用于一次性初始化与“已恢复可用”日志。
/// 打开失败不注册，调用方可在下次调用时自动重试。
pub fn get_or_open<F>(base: &Path, open: F) -> Result<(Arc<SharedStore>, bool), String>
where
    F: FnOnce() -> Result<Dal, String>,
{
    get_or_open_scoped(base, "", open)
}

/// 取或“单飞”打开共享存储（带作用域名）：同一数据目录可注册多个逻辑库
/// （如 `Config/Database.toml` 多数据源的多个连接）；其余语义同 [`get_or_open`]。
pub fn get_or_open_scoped<F>(
    base: &Path,
    scope: &str,
    open: F,
) -> Result<(Arc<SharedStore>, bool), String>
where
    F: FnOnce() -> Result<Dal, String>,
{
    let key = (key_of(base), scope.to_string());
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&key) {
        return Ok((existing.clone(), false));
    }
    INTERCEPTORS.call_once(crate::interceptor::enable_defaults);
    let dal = open()?;
    let store = Arc::new(SharedStore {
        base: key.0.clone(),
        reads: read_pool_for(&dal),
        dal,
        lock: Mutex::new(()),
        read_fallbacks: AtomicU64::new(0),
        read_degraded: AtomicBool::new(false),
    });
    registry.insert(key, store.clone());
    Ok((store, true))
}

/// 查找已注册的共享存储（未注册返回 `None`，不触发打开）。
pub fn lookup(base: &Path) -> Option<Arc<SharedStore>> {
    lookup_scoped(base, "")
}

/// 查找已注册的共享存储（带作用域名；未注册返回 `None`，不触发打开）。
pub fn lookup_scoped(base: &Path, scope: &str) -> Option<Arc<SharedStore>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(key_of(base), scope.to_string()))
        .cloned()
}

/// 移除注册（测试用：Windows 下不释放连接无法删除临时目录；生产代码勿用）。
pub fn drop_for_test(base: &Path) {
    drop_for_test_scoped(base, "")
}

/// 移除注册（带作用域名；测试用）。
pub fn drop_for_test_scoped(base: &Path, scope: &str) {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(key_of(base), scope.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pek-rcode-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_dal(dir: &Path) -> Result<Dal, String> {
        Dal::open(&format!(
            "Data Source={};Provider=SQLite",
            dir.join("t.db").display()
        ))
        .map_err(|e| e.to_string())
    }

    #[test]
    fn opens_once_and_reuses() {
        let dir = temp_dir("once");
        let calls = AtomicUsize::new(0);
        let (s1, created1) = get_or_open(&dir, || {
            calls.fetch_add(1, Ordering::SeqCst);
            open_dal(&dir)
        })
        .unwrap();
        let (s2, created2) = get_or_open(&dir, || {
            calls.fetch_add(1, Ordering::SeqCst);
            open_dal(&dir)
        })
        .unwrap();
        assert!(created1 && !created2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&s1, &s2));

        // with_session 可拿到 dal 与会话
        let got = s1.with_session(|_dal, _session| Ok(7)).unwrap();
        assert_eq!(got, 7);

        drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_first_open_runs_once() {
        let dir = temp_dir("race");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let dir = dir.clone();
            let calls = calls.clone();
            handles.push(std::thread::spawn(move || {
                let (store, _) = get_or_open(&dir, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    open_dal(&dir)
                })
                .unwrap();
                Arc::as_ptr(&store) as usize
            }));
        }
        let ptrs: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "首开闭包只能执行一次");
        assert!(
            ptrs.windows(2).all(|w| w[0] == w[1]),
            "全部线程应拿到同一实例"
        );

        drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只读会话：**读得到已提交的写**，但写 SQL 必须被数据库层拒绝（`query_only`）。
    #[test]
    fn read_session_sees_committed_rows_but_cannot_write() {
        let dir = temp_dir("read");
        let (store, _) = get_or_open(&dir, || open_dal(&dir)).unwrap();

        store
            .with_session(|_dal, session| {
                session.execute("create table t (id integer primary key, v text)", &[])?;
                session.execute("insert into t (id, v) values (1, 'a')", &[])?;
                Ok(())
            })
            .unwrap();

        // 只读会话能看到写连接已提交的数据
        let n = store
            .with_read_session(|_dal, session| {
                let rs = session.query("select count(*) as n from t", &[])?;
                Ok(rs
                    .first()
                    .and_then(|r| r.get_by_name("n"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0))
            })
            .unwrap();
        assert_eq!(n, 1);

        // 只读会话写库必须报错（PRAGMA query_only=ON）
        let write = store.with_read_session(|_dal, session| {
            session.execute("insert into t (id, v) values (2, 'b')", &[])
        });
        assert!(write.is_err(), "只读连接上写库必须失败，实际：{write:?}");

        // 内存库不适合只读池（每个连接是独立数据库）
        let mem = Dal::open("Data Source=:memory:;Provider=SQLite").unwrap();
        assert!(read_pool_for(&mem).is_none(), "内存库不得开只读池");

        drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写会话被长时间占用时，只读会话**不该被阻塞**（这就是读写分离的目的）。
    #[test]
    fn read_session_is_not_blocked_by_long_write() {
        let dir = temp_dir("rwsplit");
        let (store, _) = get_or_open(&dir, || open_dal(&dir)).unwrap();
        store
            .with_session(|_dal, session| {
                session.execute("create table t (id integer primary key, v text)", &[])?;
                Ok(())
            })
            .unwrap();

        // 后台占住写会话 300ms（模拟一次大事务）
        let holder = store.clone();
        let started = Arc::new(AtomicUsize::new(0));
        let flag = started.clone();
        let handle = std::thread::spawn(move || {
            holder
                .with_session(|_dal, session| {
                    flag.store(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    session.execute("insert into t (id, v) values (1, 'a')", &[])
                })
                .unwrap();
        });
        // 等写会话真的被占住
        while started.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let t0 = std::time::Instant::now();
        store
            .with_read_session(|_dal, session| {
                session.query("select count(*) as n from t", &[])?;
                Ok(())
            })
            .unwrap();
        let elapsed = t0.elapsed();
        handle.join().unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "只读会话被写会话阻塞了（{elapsed:?}），读写分离没生效"
        );

        drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_after_drop_runs_open_again() {        let dir = temp_dir("reopen");
        let (_, created1) = get_or_open(&dir, || open_dal(&dir)).unwrap();
        assert!(created1);
        drop_for_test(&dir);
        let (_, created2) = get_or_open(&dir, || open_dal(&dir)).unwrap();
        assert!(created2);
        drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_error_is_not_cached() {
        let dir = temp_dir("err");
        let r = get_or_open(&dir, || Err("boom".to_string()));
        assert!(r.is_err());
        // 失败不注册：再次调用仍会执行 open
        let (_, created) = get_or_open(&dir, || open_dal(&dir)).unwrap();
        assert!(created);
        drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_stores_are_independent() {
        let dir = temp_dir("scoped");
        fn open_at(dir: &Path, name: &str) -> Result<Dal, String> {
            Dal::open(&format!(
                "Data Source={};Provider=SQLite",
                dir.join(name).display()
            ))
            .map_err(|e| e.to_string())
        }
        let (a, ca) = get_or_open_scoped(&dir, "a", || open_at(&dir, "a.db")).unwrap();
        let (b, cb) = get_or_open_scoped(&dir, "b", || open_at(&dir, "b.db")).unwrap();
        assert!(ca && cb, "两个作用域应各自新开");
        assert!(!Arc::ptr_eq(&a, &b));
        // 复用：再次按同名取不重复打开
        let (a2, ca2) = get_or_open_scoped(&dir, "a", || panic!("不应再次打开")).unwrap();
        assert!(!ca2);
        assert!(Arc::ptr_eq(&a, &a2));
        assert!(lookup_scoped(&dir, "a").is_some());
        assert!(lookup_scoped(&dir, "none").is_none());
        // 默认作用域独立于具名作用域
        let (d, cd) = get_or_open(&dir, || open_at(&dir, "d.db")).unwrap();
        assert!(cd);
        assert!(!Arc::ptr_eq(&d, &a));
        drop_for_test_scoped(&dir, "a");
        drop_for_test_scoped(&dir, "b");
        drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
