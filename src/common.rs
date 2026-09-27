//! 通用辅助（对应 DH.NCode `Common` 模块的可移植部分）。
//!
//! - [`KeyedLocker`]：基于键的锁定器（对齐 `KeyedLocker`，8 个共享锁分桶）；
//! - [`is_null_key`]：主键空值判断（对齐 `Helper.IsNullKey`）。
//!
//! C# 的类型转换助手（`ValidHelper`）由 [`crate::value::DbValue`] 的 `as_*` 系列与
//! 各扩展方法覆盖，不再单独端口。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

use crate::value::DbValue;

/// 共享锁桶数量（对齐 C# `KeyedLocker` 的 8 个桶）。
pub const LOCKER_BUCKETS: usize = 8;

/// 基于键的锁定器（对齐 `KeyedLocker`）：按键哈希落到固定数量的共享锁上，
/// 相同键必然互斥；不同键可能共享同一把锁（对齐 C# 分桶设计，避免锁对象膨胀）。
pub struct KeyedLocker {
    /// 共享锁桶。
    locks: [Mutex<()>; LOCKER_BUCKETS],
}

impl Default for KeyedLocker {
    /// 使用默认桶数量实例化。
    /// <returns>锁定器</returns>
    fn default() -> Self {
        Self::new()
    }
}

impl KeyedLocker {
    /// 实例化。
    pub fn new() -> Self {
        Self {
            locks: [const { Mutex::new(()) }; LOCKER_BUCKETS],
        }
    }

    /// 取共享锁引用（按键哈希分桶；同键同锁，跨线程可用）。
    /// <param name="key">锁键</param>
    /// <returns>对应的共享锁引用</returns>
    pub fn shared_lock(&self, key: &str) -> &Mutex<()> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        &self.locks[(hasher.finish() % LOCKER_BUCKETS as u64) as usize]
    }

    /// 直接加锁（中毒锁自动恢复内部值，对齐 C# 无中毒概念）。
    /// <param name="key">锁键</param>
    /// <returns>锁守卫，作用域结束自动释放</returns>
    pub fn lock(&self, key: &str) -> MutexGuard<'_, ()> {
        self.shared_lock(key).lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 判断主键是否为空（对齐 `Helper.IsNullKey`）：
/// 空值、整数 0、空文本、空字节数组视为空；其余类型一律非空。
/// <param name="value">键值</param>
/// <returns>是否为空键</returns>
pub fn is_null_key(value: &DbValue) -> bool {
    match value {
        DbValue::Null => true,
        DbValue::Int(v) => *v == 0,
        DbValue::Text(v) => v.is_empty(),
        DbValue::Blob(v) => v.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    #[test]
    fn keyed_locker_shares_by_key() {
        let locker = KeyedLocker::new();
        // 同键同锁
        let a = locker.shared_lock("order-1") as *const Mutex<()>;
        let b = locker.shared_lock("order-1") as *const Mutex<()>;
        assert!(ptr::eq(a, b));
        // 锁可正常获取与释放
        {
            let _guard = locker.lock("order-2");
        }
        let c = locker.shared_lock("order-2") as *const Mutex<()>;
        let d = locker.shared_lock("order-2") as *const Mutex<()>;
        assert!(ptr::eq(c, d));
    }

    #[test]
    fn null_key_rules() {
        assert!(is_null_key(&DbValue::Null));
        assert!(is_null_key(&DbValue::Int(0)));
        assert!(!is_null_key(&DbValue::Int(1)));
        assert!(is_null_key(&DbValue::Text(String::new())));
        assert!(!is_null_key(&DbValue::Text("x".into())));
        assert!(is_null_key(&DbValue::Blob(Vec::new())));
        assert!(!is_null_key(&DbValue::Blob(vec![1])));
        // 布尔与浮点不参与空键判断（对齐 C# switch 未覆盖即 false）
        assert!(!is_null_key(&DbValue::Bool(false)));
        assert!(!is_null_key(&DbValue::Float(0.0)));
    }
}
