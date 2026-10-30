//! 读写分离与只读连接（对应 DH.NCode 的 `ReadWriteStrategy` + `DbBase.Readonly`）。
//!
//! ## 与 C# 的对应关系
//!
//! | C# / DH.NCode | 本模块 |
//! |---------------|--------|
//! | `DAL.Reads`（只读连接集合，来自连接名约定 `X.readonly` / `X.readonly.rdN`） | [`crate::dal::Dal::attach_read_replicas`] + [`crate::dal::Dal::read_session`] |
//! | `ReadWriteStrategy.TryGet`（轮询 + 策略） | [`ReadWriteStrategy`] + `Dal` 内部轮询计数器 |
//! | `ReadWriteStrategy.IgnoreTables` | [`ReadWriteStrategy::ignore_tables`] |
//! | `ReadWriteStrategy.IgnoreTimes` | **不实现**（见下） |
//! | `DbBase.Readonly` / `DAL.SuspendReadOnly` / `ResumeReadOnly` | `Dal::is_readonly` / `set_readonly` |
//!
//! ## 为什么没有"忽略时间段"
//!
//! C# 的 `IgnoreTimes` 用于"主从延迟高峰时段强制读主库"。Rust 侧不提供：
//! 时段判定依赖本地时区，会引入时间库依赖；且主从延迟是**持续**问题而不是时段问题，
//! 用 [`ReadWriteStrategy::set_primary_only`]（全量切主库）能覆盖"立即止血"的场景，
//! 精度更高、语义更明确。
//!
//! ## 只读连接的价值（澄清一个常见误解）
//!
//! DH.NCode 里**没有"表只读"**，只有三类"只读"：
//!
//! 1. **连接只读**（连接串 `Readonly=true`，`DbBase.Readonly`）——写操作**直接抛异常**，
//!    不发 SQL（`DAL_DbOperate` 的 `if (Db.Readonly) throw`）。价值是**防误写**（故障暴露在
//!    调用点而非等数据库拒绝，省一次网络往返）与允许驱动开只读优化（如 SQLite 不设 WAL）。
//! 2. **字段只读**（`FieldItem.ReadOnly`）——仅用于代码生成/界面（无 setter），**不影响 SQL 列数**。
//! 3. **迁移档位只读**（`Migration.ReadOnly`）——只生成 DDL 不执行，属于结构迁移。
//!
//! 所以"把表配成只读会更快"并不成立；真正的性能收益来自**读写分离**（把 SELECT 分流到从库，
//! 主库只承担写入）。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// 读写分离策略（对应 C# `ReadWriteStrategy`，忽略时间区间那一部分见模块文档）。
#[derive(Debug, Default)]
pub struct ReadWriteStrategy {
    /// 强制读主库的表名（小写；如刚写入就要求读到最新值的表）
    ignore_tables: HashSet<String>,
    /// 全局开关：为 `true` 时所有读都走主库（对应"主从延迟不可容忍"的临时止血）
    primary_only: AtomicBool,
}

impl ReadWriteStrategy {
    /// 创建默认策略（无忽略表、允许读从库）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置忽略表（这些表的读操作强制走主库）。
    pub fn ignore_tables(mut self, tables: &[&str]) -> Self {
        self.ignore_tables = tables.iter().map(|t| t.to_ascii_lowercase()).collect();
        self
    }

    /// 运行期追加忽略表。
    pub fn add_ignore_table(&mut self, table: &str) {
        self.ignore_tables.insert(table.to_ascii_lowercase());
    }

    /// 忽略表列表（小写）。
    pub fn ignored_tables(&self) -> Vec<&str> {
        self.ignore_tables.iter().map(String::as_str).collect()
    }

    /// 是否强制读主库（对应"临时关闭读写分离"，如迁移/对账期间）。
    pub fn is_primary_only(&self) -> bool {
        self.primary_only.load(Ordering::Relaxed)
    }

    /// 设置是否强制读主库。
    pub fn set_primary_only(&self, value: bool) {
        self.primary_only.store(value, Ordering::Relaxed);
    }

    /// 指定表的读操作是否应走从库。
    pub fn allows_replica(&self, table: &str) -> bool {
        if self.is_primary_only() {
            return false;
        }
        // 无忽略表是最常见情形：直接放行，不做小写化与哈希查找
        if self.ignore_tables.is_empty() {
            return true;
        }
        !self.ignore_tables.contains(&table.to_ascii_lowercase())
    }

    /// 读路径是否需要做策略判定（有忽略表或全局切主库）。
    ///
    /// 为 `false` 时可完全跳过策略锁与小写化（`Dal::read_session_for` 的快路径）。
    pub fn needs_check(&self) -> bool {
        self.is_primary_only() || !self.ignore_tables.is_empty()
    }

    /// 复制一份策略（`Dal` 在锁内改配置时用；原子开关一并复制）。
    pub fn snapshot(&self) -> Self {
        Self {
            ignore_tables: self.ignore_tables.clone(),
            primary_only: AtomicBool::new(self.is_primary_only()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_tables_and_primary_only() {
        let strategy = ReadWriteStrategy::new().ignore_tables(&["DH_Order", "DH_Stock"]);
        assert!(strategy.allows_replica("DH_User"));
        assert!(!strategy.allows_replica("dh_order"), "忽略表大小写不敏感");
        assert!(!strategy.allows_replica("DH_Stock"));

        strategy.set_primary_only(true);
        assert!(
            !strategy.allows_replica("DH_User"),
            "全局切主库后一律走主库"
        );
        strategy.set_primary_only(false);
        assert!(strategy.allows_replica("DH_User"));
    }

    #[test]
    fn strategy_can_add_ignore_table_at_runtime() {
        let mut strategy = ReadWriteStrategy::new();
        strategy.add_ignore_table("DH_Account");
        assert!(!strategy.allows_replica("dh_account"));
        assert_eq!(strategy.ignored_tables().len(), 1);
    }
}
