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
use std::sync::{Arc, Mutex, MutexGuard, Once};

use crate::dal::Dal;
use crate::session::SqlSession;

/// 共享存储：一个数据目录一个数据访问层 + 一把串行锁。
pub struct SharedStore {
    base: PathBuf,
    dal: Dal,
    lock: Mutex<()>,
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
        dal,
        lock: Mutex::new(()),
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

    #[test]
    fn reopen_after_drop_runs_open_again() {
        let dir = temp_dir("reopen");
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
