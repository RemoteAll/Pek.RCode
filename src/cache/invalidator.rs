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
}
