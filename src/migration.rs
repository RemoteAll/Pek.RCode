//! 结构迁移档位：对应 DH.NCode 的 `Migration` 枚举与语义。
//!
//! 与 C# 版逐档对齐（`XCodeSetting.Migration`，默认 `On`）：
//!
//! | 档位 | 行为 |
//! |------|------|
//! | `Off` | 完全跳过结构检查与迁移（`SetTables` 直接返回） |
//! | `ReadOnly` | 只读检查：收集将执行的 DDL（[`SchemaReport::pending_sql`](crate::dal::SchemaReport::pending_sql)），不执行 |
//! | `On`（默认） | 只做创建类：建表、加列、建索引（不修改、不删除；对应 `onlyCreate = mode < Full`） |
//! | `Full` | 新建 + 修改（列类型）+ 删除（多余列/索引；删除类动作仅此档允许） |
//!
//! 配置来源（优先级从高到低）：
//! 1. 连接串：`...;Migration=Full`（与 XCode 的 `DbBase` 从连接串解析一致）
//! 2. 模型级：`<Option><Migration>Full</Migration></Option>`
//! 3. 缺省 `On`
//!
//! 表级可再收紧（**只能更保守、不能更激进**）：`<Table Name="X" Migration="Off">`，
//! 生效档位 = `min(表级, 全局)`——与 DH.NCode 的 `ResolveMigration` 完全一致。

/// 结构迁移档位（对应 DH.NCode 的 `Migration` 枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Migration {
    /// 关闭：不检查、不迁移
    Off = 0,
    /// 只读：只检查差异并收集将执行的 DDL，不执行
    ReadOnly = 1,
    /// 默认：只创建（建表 / 加列 / 建索引）
    #[default]
    On = 2,
    /// 完全：新建、修改、删除（删除类动作仅此档允许）
    Full = 3,
}

impl Migration {
    /// 解析档位名称（大小写不敏感；兼容数字与布尔写法：`Off`/`0`/`false`）。
    /// <param name="text">档位文本</param>
    /// <returns>档位；无法识别时为 `None`</returns>
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" => Some(Self::Off),
            "readonly" | "read_only" | "read-only" | "1" => Some(Self::ReadOnly),
            "on" | "2" | "true" => Some(Self::On),
            "full" | "3" => Some(Self::Full),
            _ => None,
        }
    }

    /// 档位名称（与 C# 枚举名一致）。
    /// <returns>名称</returns>
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::ReadOnly => "ReadOnly",
            Self::On => "On",
            Self::Full => "Full",
        }
    }

    /// 是否只读（`mode <= ReadOnly`；对应 C# 的 `readonly` 判断）。
    /// <returns>是否只读</returns>
    pub fn is_readonly(self) -> bool {
        self <= Self::ReadOnly
    }

    /// 是否允许执行 DDL（`mode > ReadOnly`；对应 C# 的 `mode > Migration.ReadOnly`）。
    /// <returns>是否可执行</returns>
    pub fn can_execute(self) -> bool {
        self > Self::ReadOnly
    }

    /// 是否仅创建（`mode < Full`；对应 C# 的 `onlyCreate`——修改/删除仅 `Full` 允许）。
    /// <returns>是否仅创建</returns>
    pub fn only_create(self) -> bool {
        self < Self::Full
    }

    /// 收紧：表级档位只能更保守（对应 C# `ResolveMigration` 的 `tableMode < mode ? tableMode : mode`）。
    /// <param name="table">表级档位（`None` 表示未配置）</param>
    /// <returns>生效档位</returns>
    pub fn tighten(self, table: Option<Self>) -> Self {
        match table {
            Some(t) if t < self => t,
            _ => self,
        }
    }
}

impl std::fmt::Display for Migration {
    /// 输出档位名称。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_names_numbers_and_booleans() {
        assert_eq!(Migration::parse("off"), Some(Migration::Off));
        assert_eq!(Migration::parse("ReadOnly"), Some(Migration::ReadOnly));
        assert_eq!(Migration::parse("read-only"), Some(Migration::ReadOnly));
        assert_eq!(Migration::parse("on"), Some(Migration::On));
        assert_eq!(Migration::parse(" FULL "), Some(Migration::Full));
        assert_eq!(Migration::parse("0"), Some(Migration::Off));
        assert_eq!(Migration::parse("3"), Some(Migration::Full));
        assert_eq!(Migration::parse("false"), Some(Migration::Off));
        assert_eq!(Migration::parse("bogus"), None);
        assert_eq!(Migration::parse(""), None);
    }

    #[test]
    fn default_is_on_like_xcode() {
        assert_eq!(Migration::default(), Migration::On);
        assert_eq!(Migration::default().name(), "On");
    }

    #[test]
    fn level_helpers_match_xcode_semantics() {
        assert!(Migration::Off.is_readonly());
        assert!(Migration::ReadOnly.is_readonly());
        assert!(!Migration::On.is_readonly());
        assert!(!Migration::Full.is_readonly());

        assert!(!Migration::Off.can_execute());
        assert!(!Migration::ReadOnly.can_execute());
        assert!(Migration::On.can_execute());
        assert!(Migration::Full.can_execute());

        assert!(Migration::On.only_create());
        assert!(!Migration::Full.only_create());
        assert!(Migration::Off.only_create());
    }

    #[test]
    fn tighten_only_restricts() {
        // 表级只能收紧：min(表级, 全局)——与 XCode ResolveMigration 一致
        assert_eq!(Migration::Full.tighten(Some(Migration::On)), Migration::On);
        assert_eq!(Migration::On.tighten(Some(Migration::Full)), Migration::On);
        assert_eq!(Migration::On.tighten(Some(Migration::Off)), Migration::Off);
        assert_eq!(Migration::Full.tighten(None), Migration::Full);
        assert_eq!(Migration::Off.tighten(None), Migration::Off);
    }
}
