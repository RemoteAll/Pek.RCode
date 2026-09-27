//! 运行数据缓存（对应 DH.NCode 的 `DataCache`）。
//!
//! 把字段缓存（FieldCache）等“启动加速”数据持久化到 JSON 文件，
//! 进程重启后可立即命中，避免冷启动时的集中查询。
//!
//! 与 C# 版的差异：C# 用 `TimerX` 做保存防抖；Rust 版提供显式 [`DataCache::save`]
//! 与基于 [`LazyConsumer`] 的串行异步保存 [`DataCache::save_async`]。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::lazy::LazyConsumer;
use super::lock;
use crate::error::Result;

/// 运行数据缓存。
pub struct DataCache {
    /// 持久化文件路径
    path: PathBuf,
    /// 字段缓存集合（键 = `{实体}_{字段}`，值 = 值 → 显示名）
    field_cache: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    /// 异步保存器（串行执行）
    saver: Arc<LazyConsumer>,
}

impl DataCache {
    /// 从文件加载（不存在或损坏时返回空缓存）。
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut field_cache = BTreeMap::new();

        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(map) = json.get("FieldCache").and_then(|v| v.as_object())
        {
            for (key, value) in map {
                if let Some(inner) = value.as_object() {
                    field_cache.insert(
                        key.clone(),
                        inner
                            .iter()
                            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                            .collect(),
                    );
                }
            }
        }

        Self {
            path,
            field_cache: Mutex::new(field_cache),
            saver: Arc::new(LazyConsumer::new()),
        }
    }

    /// 持久化文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取某实体的字段缓存（如 `FieldCache::find_all_name` 的结果）。
    pub fn field_cache(&self, key: &str) -> Option<BTreeMap<String, String>> {
        lock(&self.field_cache).get(key).cloned()
    }

    /// 写入某实体的字段缓存。
    pub fn set_field_cache(&self, key: &str, values: BTreeMap<String, String>) {
        lock(&self.field_cache).insert(key.to_string(), values);
    }

    /// 同步保存到文件（先写临时文件再原子替换，避免半写损坏）。
    pub fn save(&self) -> Result<()> {
        let json = self.to_json();
        write_atomic(&self.path, &json)
    }

    /// 异步保存（由内部串行消费者执行，不阻塞调用方）。
    pub fn save_async(&self) {
        let path = self.path.clone();
        let json = self.to_json();
        self.saver.run(move || {
            let _ = write_atomic(&path, &json);
        });
    }

    /// 等待异步保存完成（测试等场景使用）。
    pub fn wait_save_idle(&self, timeout: std::time::Duration) -> bool {
        self.saver.wait_idle(timeout)
    }

    /// 序列化为 JSON 文本。
    fn to_json(&self) -> String {
        let field_cache = lock(&self.field_cache);
        serde_json::json!({
            "Name": "Pek.RCode 运行数据缓存",
            "FieldCache": &*field_cache,
        })
        .to_string()
    }
}

/// 原子写文件（临时文件 + 重命名）。
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_reload_roundtrip() {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-datacache-{}-{stamp}",
            std::process::id()
        ));
        let file = dir.join("DataCache.json");

        // 不存在时为空
        let cache = DataCache::load(&file);
        assert!(cache.field_cache("X_Field").is_none());

        let mut values = BTreeMap::new();
        values.insert("A".to_string(), "A (3)".to_string());
        values.insert("B".to_string(), "B (2)".to_string());
        cache.set_field_cache("X_Field", values.clone());
        cache.save().unwrap();
        assert!(file.exists());

        // 重新加载
        let reloaded = DataCache::load(&file);
        assert_eq!(reloaded.field_cache("X_Field"), Some(values));

        // 异步保存
        reloaded.set_field_cache("Y_Field", BTreeMap::new());
        reloaded.save_async();
        assert!(
            reloaded.wait_save_idle(std::time::Duration::from_secs(5)),
            "异步保存应完成"
        );
        let again = DataCache::load(&file);
        assert!(again.field_cache("Y_Field").is_some());

        // 损坏的文件不应 panic
        std::fs::write(&file, "{not-json").unwrap();
        let broken = DataCache::load(&file);
        assert!(broken.field_cache("X_Field").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
