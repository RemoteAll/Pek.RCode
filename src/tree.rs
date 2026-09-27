//! 树形数据辅助（对应 DH.NCode `Tree` 模块的可移植部分）。
//!
//! C# `EntityTree` 依附实体与数据库（`Up`/`Down` 交换排序后 `Save`）；Rust 版提供纯逻辑件：
//! 全路径、祖先链、子树展开与包含判断（对齐 `GetFullPath`/`FindAllParents`/`Contains` 语义），
//! 排序交换由调用方按 Sort 字段处理后落库。

use crate::membership::department_and_children;

/// 树行（编号、父级编号、名称）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// 节点编号。
    pub id: i32,
    /// 父级编号（根为 0）。
    pub parent_id: i32,
    /// 节点名称（用于全路径拼接）。
    pub name: String,
}

impl TreeRow {
    /// 实例化。
    /// <param name="id">节点编号</param>
    /// <param name="parent_id">父级编号</param>
    /// <param name="name">节点名称</param>
    pub fn new(id: i32, parent_id: i32, name: impl Into<String>) -> Self {
        Self {
            id,
            parent_id,
            name: name.into(),
        }
    }
}

/// 获取祖先链（由根到自身；`include_self = false` 时不含自身）。
///
/// 节点不存在时返回空列表；存在环时按已访问链截断（对齐 C# 递归中已含判断）。
/// <param name="rows">树行</param>
/// <param name="id">节点编号</param>
/// <param name="include_self">是否包含自身</param>
/// <returns>节点编号链（根 → 自身或父级）</returns>
pub fn ancestor_ids(rows: &[TreeRow], id: i32, include_self: bool) -> Vec<i32> {
    let mut chain: Vec<i32> = Vec::new();
    let mut current = id;
    while !chain.contains(&current) {
        let Some(row) = rows.iter().find(|r| r.id == current) else {
            break;
        };
        chain.push(row.id);
        if row.parent_id > 0 && row.parent_id != row.id {
            current = row.parent_id;
        } else {
            break;
        }
    }
    chain.reverse(); // 由根到自身
    if !include_self && chain.last() == Some(&id) {
        chain.pop();
    }
    chain
}

/// 获取全路径（对齐 `GetFullPath`）：由根到自身以分隔符连接各节点名称。
/// <param name="rows">树行</param>
/// <param name="id">节点编号</param>
/// <param name="separator">分隔符（C# 默认 `\`）</param>
/// <param name="include_self">是否包含自身</param>
/// <returns>全路径；节点不存在时为 None</returns>
pub fn full_path(rows: &[TreeRow], id: i32, separator: &str, include_self: bool) -> Option<String> {
    let chain = ancestor_ids(rows, id, include_self);
    if chain.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = Vec::with_capacity(chain.len());
    for cid in &chain {
        let name = rows.iter().find(|r| r.id == *cid).map(|r| r.name.as_str())?;
        names.push(name);
    }
    Some(names.join(separator))
}

/// 是否包含指定节点（对齐 `Contains`：自身、直接子级或任意子孙）。
///
/// `key <= 0` 视为空键，返回 false（对齐 C# `IsNull`）。
/// <param name="rows">树行</param>
/// <param name="id">当前节点编号</param>
/// <param name="key">待查找的节点编号</param>
/// <returns>是否包含</returns>
pub fn contains(rows: &[TreeRow], id: i32, key: i32) -> bool {
    if key <= 0 {
        return false;
    }
    if id == key {
        return true;
    }
    descendant_ids(rows, id).contains(&key)
}

/// 子树展开（编号列表，含自身；同级按编号升序深度优先，对齐 C# `Childs` 的 `OrderBy(ID)`）。
/// <param name="rows">树行</param>
/// <param name="id">子树根编号</param>
/// <returns>编号列表（含自身）</returns>
pub fn descendant_ids(rows: &[TreeRow], id: i32) -> Vec<i32> {
    let pairs: Vec<(i32, i32)> = rows.iter().map(|r| (r.id, r.parent_id)).collect();
    department_and_children(id, &pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试树：1(根) -> 2 -> 3；1 -> 4。
    fn sample() -> Vec<TreeRow> {
        vec![
            TreeRow::new(1, 0, "根"),
            TreeRow::new(2, 1, "分支"),
            TreeRow::new(3, 2, "叶子"),
            TreeRow::new(4, 1, "另一分支"),
        ]
    }

    #[test]
    fn ancestor_chain_and_full_path() {
        let rows = sample();
        assert_eq!(ancestor_ids(&rows, 3, true), vec![1, 2, 3]);
        assert_eq!(ancestor_ids(&rows, 3, false), vec![1, 2]);
        assert_eq!(ancestor_ids(&rows, 1, true), vec![1]);
        assert_eq!(ancestor_ids(&rows, 1, false), Vec::<i32>::new());
        // 节点不存在
        assert_eq!(ancestor_ids(&rows, 9, true), Vec::<i32>::new());

        assert_eq!(full_path(&rows, 3, "\\", true).as_deref(), Some("根\\分支\\叶子"));
        assert_eq!(full_path(&rows, 3, "/", false).as_deref(), Some("根/分支"));
        assert_eq!(full_path(&rows, 3, "/", true), Some("根/分支/叶子".into()));
        assert_eq!(full_path(&rows, 9, "/", true), None);
    }

    #[test]
    fn contains_and_descendants() {
        let rows = sample();
        // 自身
        assert!(contains(&rows, 1, 1));
        // 直接子级
        assert!(contains(&rows, 1, 2));
        // 子孙
        assert!(contains(&rows, 1, 3));
        // 不相关节点
        assert!(!contains(&rows, 2, 4));
        // 空键
        assert!(!contains(&rows, 1, 0));

        assert_eq!(descendant_ids(&rows, 1), vec![1, 2, 3, 4]);
        assert_eq!(descendant_ids(&rows, 2), vec![2, 3]);
    }
}
