//! 二级缓存失效协调器（对应 DH.NCode 的 `CacheInvalidator`）。
//!
//! 基于分布式缓存的版本号机制：每次清除缓存时递增版本号，其它进程比对版本变化后
//! 主动过期本地缓存（跨进程协调）。内置内存实现（单进程默认）；
//! 接入 Redis 等分布式缓存时实现 [`VersionStore`] 并注册。
//!
//! 说明：与 C# 版一致，版本号读写非原子（先读后写）；作为“失效通知”语义已足够，
//! 真的需要强一致自增时可让 `VersionStore` 实现方提供原子操作。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "redis")]
use crate::error::{Error, Result};

use super::lock;

/// 版本号存储抽象（由分布式缓存实现，如 Redis）。
pub trait VersionStore: Send + Sync {
    /// 读取版本号（不存在返回 0）。
    fn get(&self, key: &str) -> i64;

    /// 写入版本号。
    fn set(&self, key: &str, value: i64);
}

/// 内存版本号存储（单进程默认实现）。
#[derive(Default)]
pub struct MemoryVersionStore {
    /// 键 → 版本号
    items: Mutex<HashMap<String, i64>>,
}

impl MemoryVersionStore {
    /// 创建空存储。
    pub fn new() -> Self {
        Self::default()
    }
}

impl VersionStore for MemoryVersionStore {
    fn get(&self, key: &str) -> i64 {
        lock(&self.items).get(key).copied().unwrap_or(0)
    }

    fn set(&self, key: &str, value: i64) {
        lock(&self.items).insert(key.to_string(), value);
    }
}

/// 全局提供者（进程生命周期内仅可注册一次）。
static PROVIDER: OnceLock<Arc<dyn VersionStore>> = OnceLock::new();

/// 版本号键前缀（与 C# 版一致）。
const KEY_PREFIX: &str = "XCode:Cache:Ver";

/// 二级缓存失效协调器。
pub struct CacheInvalidator;

impl CacheInvalidator {
    /// 注册分布式缓存提供者（进程内仅首次生效；返回是否注册成功）。
    pub fn set_provider(store: Arc<dyn VersionStore>) -> bool {
        PROVIDER.set(store).is_ok()
    }

    /// 当前提供者。
    pub fn provider() -> Option<Arc<dyn VersionStore>> {
        PROVIDER.get().cloned()
    }

    /// 通知所有进程清除指定实体的缓存（递增分布式版本号）。
    ///
    /// 未注册提供者时为空操作（与 C# 版一致）。
    pub fn invalidate(entity: &str) {
        if let Some(store) = Self::provider() {
            let key = version_key(entity);
            let version = store.get(&key);
            store.set(&key, version + 1);
        }
    }

    /// 读取当前版本号（未注册提供者时返回 0）。
    pub fn version(entity: &str) -> i64 {
        Self::provider()
            .map(|store| store.get(&version_key(entity)))
            .unwrap_or(0)
    }
}

/// 组装版本号键。
fn version_key(entity: &str) -> String {
    format!("{KEY_PREFIX}:{entity}")
}

/// Redis 版本号存储（对接分布式缓存，对齐 C# 的 Redis 版本号机制）。
///
/// 基于 **Pek.RRedis**（DH.NRedis 的 Rust 实现，与 C# 端同一字节格式）：
/// - 键名与 C# 一致（`XCode:Cache:Ver:{实体名}`，C# 写、Rust 读可互相感知）；
/// - 连接串格式与 C# 相同（`server=127.0.0.1:6379;password=123456;db=0`）；
/// - 读写共享 Pek.RRedis 内部连接池（长连接 + 失败重试，优于每次新建连接）；
/// - 连接/命令失败时静默退化为“无版本号”（下次访问自然重载，不阻断业务）。
///
/// ```ignore
/// use std::sync::Arc;
/// use pek_rcode::cache::invalidator::{CacheInvalidator, RedisVersionStore};
///
/// let store = RedisVersionStore::connect("server=127.0.0.1:6379;db=0")?;
/// CacheInvalidator::set_provider(Arc::new(store));
/// # Ok::<(), pek_rcode::Error>(())
/// ```
#[cfg(feature = "redis")]
pub struct RedisVersionStore {
    /// Pek.RRedis 客户端（内含连接池）
    redis: pek_rredis::FullRedis,
}

#[cfg(feature = "redis")]
impl RedisVersionStore {
    /// 按连接串连接（与 C# `new FullRedis(config)` 格式一致）。
    /// <param name="config">Redis 连接串（`server=...;password=...;db=...`）</param>
    /// <returns>版本号存储</returns>
    pub fn connect(config: &str) -> Result<Self> {
        let redis = pek_rredis::FullRedis::from_config(config)
            .map_err(|e| Error::Db(format!("连接 Redis 失败（{config}）：{e}")))?;
        Ok(Self { redis })
    }
}

#[cfg(feature = "redis")]
impl VersionStore for RedisVersionStore {
    fn get(&self, key: &str) -> i64 {
        // 文本形式存储（与 C# `cache.Get<Int64>` 的字节格式一致）；解析失败视为 0
        self.redis
            .redis()
            .get::<i64>(key)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    fn set(&self, key: &str, value: i64) {
        let _ = self.redis.redis().set(key, value, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 说明：全局提供者只能注册一次，本用例覆盖“未注册 → 注册 → 递增”的完整生命周期，
    // 请勿再新增会注册提供者的测试（避免顺序依赖）。
    #[test]
    fn version_store_lifecycle() {
        // 未注册时：版本号 0，invalidate 为空操作
        assert_eq!(CacheInvalidator::version("User"), 0);
        CacheInvalidator::invalidate("User");
        assert_eq!(CacheInvalidator::version("User"), 0);

        // 注册内存提供者（仅一次生效）
        assert!(
            CacheInvalidator::set_provider(Arc::new(MemoryVersionStore::new())),
            "首次注册应成功"
        );
        assert!(
            !CacheInvalidator::set_provider(Arc::new(MemoryVersionStore::new())),
            "重复注册应失败"
        );

        // 递增与隔离
        CacheInvalidator::invalidate("User");
        CacheInvalidator::invalidate("User");
        assert_eq!(CacheInvalidator::version("User"), 2);
        assert_eq!(CacheInvalidator::version("Order"), 0, "不同实体版本号隔离");
        assert!(CacheInvalidator::provider().is_some());
    }

    /// 有真实 Redis 时（环境变量 `RCODE_REDIS`）验证版本号读写。
    #[cfg(feature = "redis")]
    #[test]
    fn redis_version_store_roundtrip_when_configured() {
        let Ok(url) = std::env::var("RCODE_REDIS") else {
            return;
        };
        let store = RedisVersionStore::connect(&url).expect("连接 Redis");
        store.set("rcode:test:cache:ver", 7);
        assert_eq!(store.get("rcode:test:cache:ver"), 7);
    }
}
