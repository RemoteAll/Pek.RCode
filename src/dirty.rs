//! 实体脏数据与扩展属性（对应 DH.NCode 的 `Entity/DirtyCollection.cs` 与 `Entity/EntityExtend.cs`）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::value::DbValue;

/// 获取互斥锁（被投毒时恢复内部数据）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 脏数据集合：记录被修改的列及其**旧值**（对应 `DirtyCollection`）。
///
/// 实体在保存前把自己的修改登记进来，保存时只写脏列；`Save` 成功后清空。
#[derive(Debug, Default, Clone)]
pub struct DirtyCollection {
    /// 列名 → 旧值（保留最早一次的旧值）
    items: Vec<(String, DbValue)>,
}

impl DirtyCollection {
    /// 创建空集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 脏列个数。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 是否包含脏列（忽略大小写）。
    pub fn contains(&self, name: &str) -> bool {
        self.items
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    }

    /// 登记脏列并记录旧值；重复登记**保留最早旧值**。新登记返回 `true`。
    pub fn set(&mut self, name: &str, old_value: DbValue) -> bool {
        if self.contains(name) {
            return false;
        }
        self.items.push((name.to_string(), old_value));
        true
    }

    /// 登记脏列（无旧值，记录为 NULL）。
    pub fn add(&mut self, name: &str) -> bool {
        self.set(name, DbValue::Null)
    }

    /// 取旧值（未登记返回 `None`）。
    pub fn old_value(&self, name: &str) -> Option<&DbValue> {
        self.items
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// 脏列名列表（按登记顺序）。
    pub fn names(&self) -> Vec<&str> {
        self.items.iter().map(|(key, _)| key.as_str()).collect()
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// 带过期的实体扩展属性（对应 `EntityExtend`）。
///
/// 挂在实体实例上的键值缓存：`get_or_add` 不存在时经委托生成并缓存（线程安全）。
#[derive(Debug, Default)]
pub struct EntityExtend {
    /// 过期时间（秒；0 表示不过期）
    expire: Mutex<u64>,
    /// 键 → (值, 写入时刻)
    items: Mutex<HashMap<String, (DbValue, Instant)>>,
}

impl EntityExtend {
    /// 创建（默认不过期）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置过期时间（秒；0 表示不过期）。
    pub fn set_expire_seconds(&self, seconds: u64) {
        *lock(&self.expire) = seconds;
    }

    /// 是否过期。
    fn is_expired(&self, at: Instant) -> bool {
        let expire = *lock(&self.expire);
        expire > 0 && at.elapsed() >= Duration::from_secs(expire)
    }

    /// 读取（过期自动移除）。
    pub fn get(&self, key: &str) -> Option<DbValue> {
        let mut items = lock(&self.items);
        match items.get(key) {
            Some((value, at)) if !self.is_expired(*at) => Some(value.clone()),
            Some(_) => {
                items.remove(key);
                None
            }
            None => None,
        }
    }

    /// 获取或经委托生成（对应 `EntityExtend.GetOrAdd`；线程安全，工厂在锁外执行）。
    pub fn get_or_add<F>(&self, key: &str, factory: F) -> DbValue
    where
        F: FnOnce() -> DbValue,
    {
        if let Some(value) = self.get(key) {
            return value;
        }
        let value = factory();
        self.set(key, value.clone());
        value
    }

    /// 写入。
    pub fn set(&self, key: &str, value: DbValue) {
        lock(&self.items).insert(key.to_string(), (value, Instant::now()));
    }

    /// 移除。
    pub fn remove(&self, key: &str) -> bool {
        lock(&self.items).remove(key).is_some()
    }

    /// 清空。
    pub fn clear(&self) {
        lock(&self.items).clear();
    }

    /// 缓存项个数。
    pub fn count(&self) -> usize {
        lock(&self.items).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn dirty_collection_tracks_keys_and_old_values() {
        let mut dirty = DirtyCollection::new();
        assert!(dirty.is_empty());

        assert!(dirty.set("Code", DbValue::Text("old".into())));
        assert!(!dirty.set("code", DbValue::Text("其它".into())), "重复登记应保留最早旧值");
        assert!(dirty.add("Status"));

        assert_eq!(dirty.len(), 2);
        assert!(dirty.contains("CODE"));
        assert_eq!(
            dirty.old_value("Code"),
            Some(&DbValue::Text("old".into()))
        );
        assert_eq!(dirty.old_value("Status"), Some(&DbValue::Null));
        assert_eq!(dirty.names(), vec!["Code", "Status"]);

        dirty.clear();
        assert!(dirty.is_empty());
    }

    #[test]
    fn entity_extend_caches_and_expires() {
        let extend = EntityExtend::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let calls1 = Arc::clone(&calls);
        let value = extend.get_or_add("k", move || {
            calls1.fetch_add(1, Ordering::SeqCst);
            DbValue::Int(42)
        });
        assert_eq!(value, DbValue::Int(42));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 命中缓存：工厂不再调用
        let calls2 = Arc::clone(&calls);
        let value = extend.get_or_add("k", move || {
            calls2.fetch_add(1, Ordering::SeqCst);
            DbValue::Int(99)
        });
        assert_eq!(value, DbValue::Int(42));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "命中缓存时不应再调用工厂");

        // 过期后重新生成
        extend.set_expire_seconds(0);
        extend.set("k", DbValue::Int(7));
        extend.set_expire_seconds(1);
        assert_eq!(extend.get("k"), Some(DbValue::Int(7)));
        assert_eq!(extend.count(), 1);
        assert!(extend.remove("k"));
        assert!(!extend.remove("k"));
        assert_eq!(extend.get("k"), None);
    }
}
