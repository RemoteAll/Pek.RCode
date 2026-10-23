//! 树形数据辅助（对应 DH.NCode `Tree` 模块）。
//!
//! 两层能力：
//! 1. **纯逻辑件**（`TreeRow` + `ancestor_ids`/`full_path`/`contains`/`descendant_ids`）：
//!    调用方自己准备行，适合小数据或不落库场景；
//! 2. **表驱动**（`TreeSetting` + `TreeIndex` + `move_node`/`up`/`down`）：
//!    按 C# `BaseType="EntityTree"` 从 `Model.xml` 推导设置，一次加载整表建邻接索引，
//!    语义逐项对齐 `XCode.EntityTree<TKey,TEntity>`（见下）。
//!
//! ### 与 C# 的对应
//!
//! | C# | 本模块 |
//! |---|---|
//! | `EntityTreeSetting.Key/Parent/Sort/Name/Text/BigSort/MaxDeepth` | `TreeSetting`（同样按列存在性自动推导） |
//! | `Childs` / `FindAllByParent` | `TreeIndex::children`（顶级附无头节点） |
//! | `AllChilds` / `FindAllChilds` | `TreeIndex::descendants`（深度优先先序、可排除分支/限深） |
//! | `AllParents` / `FindAllParents` | `TreeIndex::ancestors`（从高到低） |
//! | `Deepth` / `GetFullPath` / `FindByPath` / `Contains` | `depth_of` / `full_path` / `find_by_path` / `contains` |
//! | `TreeNodeName` | `tree_node_name` |
//! | `Valid`（防自己/子孙/超深） | `validate_parent` |
//! | `Up()` / `Down()` | `up` / `down`（交换排序列并落库） |
//! | 整表实体缓存 | `TreeIndex::load` 一次 `SELECT` 建索引 |
//!
//! 暂只支持**整数主键**（Int32/Int64，内部分一按 `i64` 处理）。

use std::collections::{HashMap, HashSet};

use crate::membership::department_and_children;
use crate::model::TableMeta;
use crate::query::Query;
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;
use crate::{Dal, Result};

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

// ——————————————————————————————————————————————————————————————
// 表驱动层：TreeSetting + TreeIndex（对齐 C# EntityTree）
// ——————————————————————————————————————————————————————————————

/// 树形设置（对齐 C# `IEntityTreeSetting` 的推导规则与默认值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSetting {
    /// 主键列（C# `Key`，默认取主键列名）
    pub key: String,
    /// 父键列（C# `Parent`，默认 `"Parent" + 主键列名`；不存在则不是树）
    pub parent: String,
    /// 排序列（C# `Sort`：`Sorting`/`Sort`/`Rank`/`DisplayOrder`/`Order` 中第一个存在的；无则不排序）
    pub sort: Option<String>,
    /// 名称列（C# `Name`：`Name` 或第一个非自增列）
    pub name: Option<String>,
    /// 文本列（C# `Text`：`Text`/`Display`/`DisplayName` 之一）
    pub text: Option<String>,
    /// 是否大排序（C# `BigSort` 默认 **true**：排序值大的在前）
    pub big_sort: bool,
    /// 允许的最大深度（C# `MaxDeepth` 默认 0 = 不限制）
    pub max_depth: i32,
}

impl TreeSetting {
    /// 按 C# 规则从表定义推导；**父键列不存在时返回 `None`（不是树）**。
    pub fn from_table(table: &TableMeta) -> Option<Self> {
        // Key：主键列（C# Factory.Unique.Name）
        let key = table.columns.iter().find(|c| c.primary_key)?.name.clone();

        // Parent：默认 "Parent" + Key（忽略大小写）
        let parent_default = format!("Parent{key}");
        let parent = table
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&parent_default))?
            .name
            .clone();

        // Sort：按 C# 顺序挑第一个存在的列
        let sort = ["Sorting", "Sort", "Rank", "DisplayOrder", "Order"]
            .iter()
            .find(|n| table.columns.iter().any(|c| c.name.eq_ignore_ascii_case(n)))
            .map(|n| n.to_string());

        // Name：Name，否则第一个非自增列
        let name = table
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case("Name"))
            .or_else(|| table.columns.iter().find(|c| !c.identity))
            .map(|c| c.name.clone());

        // Text：Text / Display / DisplayName
        let text = ["Text", "Display", "DisplayName"]
            .iter()
            .find(|n| table.columns.iter().any(|c| c.name.eq_ignore_ascii_case(n)))
            .map(|n| n.to_string());

        Some(TreeSetting { key, parent, sort, name, text, big_sort: true, max_depth: 0 })
    }

    /// 表声明为树（`BaseType` 含 `EntityTree`）**且具备父键列**时返回设置。
    pub fn of(table: &TableMeta) -> Option<Self> {
        let base_type = table.base_type.as_deref().unwrap_or_default();
        if !base_type.to_ascii_lowercase().contains("entitytree") {
            return None;
        }
        Self::from_table(table)
    }
}

impl TableMeta {
    /// 树设置（非树 / 缺父键列时返回 `None`）。
    pub fn tree_setting(&self) -> Option<TreeSetting> {
        TreeSetting::of(self)
    }

    /// 是否树形表（`BaseType="EntityTree"` 且存在父键列）。
    pub fn is_tree(&self) -> bool {
        self.tree_setting().is_some()
    }
}

/// 树节点（加载后的内存快照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// 主键
    pub key: i64,
    /// 父键（`None` 表示顶级；`0` 也按顶级处理，对齐 C# `IsNull(default(TKey))`）
    pub parent: Option<i64>,
    /// 排序值（无排序列时为 0）
    pub sort: i64,
    /// 名称（`Name` 列）
    pub name: String,
    /// 文本（`Text`/`Display`/`DisplayName`，缺省回退名称）
    pub text: String,
}

impl TreeNode {
    /// 节点名（对齐 C# `NodeName`）
    pub fn node_name(&self) -> &str {
        &self.name
    }
}

/// 树索引：一次加载整表，后续全部内存操作（对齐 C# 的整表实体缓存语义）。
#[derive(Debug, Clone)]
pub struct TreeIndex {
    setting: TreeSetting,
    nodes: Vec<TreeNode>,
    by_key: HashMap<i64, usize>,
    /// 父键 → 子节点下标（**已按排序规则排好**）
    children: HashMap<i64, Vec<usize>>,
    /// 顶级节点（父键为空/0）
    roots: Vec<usize>,
    /// 无头节点（父键非空但父不存在；C# `FindAllNoParent`）
    orphans: Vec<usize>,
}

impl TreeIndex {
    /// 从行集构建（行需包含配置的列；缺失列按空处理）。
    pub fn from_rowset(setting: TreeSetting, rows: &RowSet) -> Self {
        let mut nodes: Vec<TreeNode> = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let key = row.get_by_name(&setting.key).and_then(|v| v.as_i64()).unwrap_or(0);
            if key == 0 {
                continue; // 无主键的行不参与树
            }
            let parent = row
                .get_by_name(&setting.parent)
                .and_then(|v| v.as_i64())
                .filter(|v| *v != 0);
            let sort = setting
                .sort
                .as_ref()
                .and_then(|c| row.get_by_name(c))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let name = setting
                .name
                .as_ref()
                .and_then(|c| row.get_by_name(c))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let text = setting
                .text
                .as_ref()
                .and_then(|c| row.get_by_name(c))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&name)
                .to_string();
            nodes.push(TreeNode { key, parent, sort, name, text });
        }

        let mut by_key: HashMap<i64, usize> = HashMap::with_capacity(nodes.len());
        for (i, n) in nodes.iter().enumerate() {
            by_key.insert(n.key, i);
        }

        let mut children: HashMap<i64, Vec<usize>> = HashMap::new();
        let mut roots: Vec<usize> = Vec::new();
        let mut orphans: Vec<usize> = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            match n.parent {
                None => roots.push(i),
                Some(p) => {
                    if by_key.contains_key(&p) {
                        children.entry(p).or_default().push(i);
                    } else {
                        orphans.push(i);
                    }
                }
            }
        }

        // 排序：先排序列、再主键（C#）；BigSort=true 时排序值大的在前
        fn sort_idx(nodes: &[TreeNode], big_sort: bool, a: &usize, b: &usize) -> std::cmp::Ordering {
            let (na, nb) = (&nodes[*a], &nodes[*b]);
            if na.sort == nb.sort {
                na.key.cmp(&nb.key)
            } else if big_sort {
                nb.sort.cmp(&na.sort)
            } else {
                na.sort.cmp(&nb.sort)
            }
        }
        let big = setting.big_sort;
        for v in children.values_mut() {
            v.sort_by(|a, b| sort_idx(&nodes, big, a, b));
        }
        roots.sort_by(|a, b| sort_idx(&nodes, big, a, b));

        Self { setting, nodes, by_key, children, roots, orphans }
    }

    /// 加载整表并建索引（一次 `SELECT`，只取树需要的列）。
    pub fn load(dal: &Dal, session: &mut dyn SqlSession, table_name: &str) -> Result<Self> {
        let model = dal
            .model()
            .ok_or_else(|| crate::Error::Model("模型未加载".to_string()))?;
        let meta = model
            .table(table_name)
            .ok_or_else(|| crate::Error::Model(format!("表 {table_name} 不存在")))?;
        let setting = TreeSetting::from_table(meta).ok_or_else(|| {
            crate::Error::Model(format!("表 {table_name} 不是树形表（缺少 Parent{{主键}} 列）"))
        })?;

        let mut q = Query::new().column(setting.key.as_str()).column(setting.parent.as_str());
        if let Some(c) = setting.sort.as_deref() {
            q = q.column(c);
        }
        if let Some(c) = setting.name.as_deref() {
            q = q.column(c);
        }
        if let Some(c) = setting.text.as_deref()
            && setting.name.as_deref() != Some(c)
        {
            q = q.column(c);
        }
        let rows = dal.table(table_name)?.query(session, &q)?;
        Ok(Self::from_rowset(setting, &rows))
    }

    /// 树设置
    pub fn setting(&self) -> &TreeSetting {
        &self.setting
    }

    /// 节点数
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 全部节点
    pub fn nodes(&self) -> &[TreeNode] {
        &self.nodes
    }

    /// 按主键取节点
    pub fn node(&self, key: i64) -> Option<&TreeNode> {
        self.by_key.get(&key).map(|i| &self.nodes[*i])
    }

    /// 顶级节点（父键为空/0）
    pub fn roots(&self) -> Vec<&TreeNode> {
        self.roots.iter().map(|i| &self.nodes[*i]).collect()
    }

    /// 无头节点（父键非空但父不存在，C# `FindAllNoParent`）
    pub fn orphans(&self) -> Vec<&TreeNode> {
        self.orphans.iter().map(|i| &self.nodes[*i]).collect()
    }

    /// 直接子节点（对齐 C# `Childs` / `FindAllByParent`）。
    ///
    /// `parent = None` 时返回顶级节点，并**附带无头节点**（C# 注释：顶级包含所有无头节点）。
    pub fn children(&self, parent: Option<i64>) -> Vec<&TreeNode> {
        match parent.filter(|p| *p != 0) {
            None => {
                let mut list: Vec<&TreeNode> = self.roots();
                list.extend(self.orphans());
                list
            }
            Some(p) => self
                .children
                .get(&p)
                .map(|v| v.iter().map(|i| &self.nodes[*i]).collect())
                .unwrap_or_default(),
        }
    }

    /// 子孙节点（对齐 C# `FindAllChilds`：深度优先先序、防死循环、可排除分支/限深）。
    ///
    /// - `include_self`：是否包含 `key` 自身；
    /// - `exclude`：排除该节点及其整棵子树；
    /// - `depth`：> 0 时只返回到该绝对深度（C# 用 `Deepth <= depth` 过滤）。
    pub fn descendants(
        &self,
        key: i64,
        include_self: bool,
        exclude: Option<i64>,
        depth: i32,
    ) -> Vec<&TreeNode> {
        let Some(start) = self.by_key.get(&key).copied() else {
            return Vec::new();
        };
        let mut out: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        let mut stack: Vec<usize> = vec![start];

        while let Some(idx) = stack.pop() {
            if !seen.insert(idx) {
                continue;
            }
            let node = &self.nodes[idx];
            if Some(node.key) == exclude {
                continue; // 整棵子树都不进入
            }
            if (include_self || idx != start) && (depth <= 0 || self.depth_of(node.key) <= depth) {
                out.push(idx);
            }
            if let Some(kids) = self.children.get(&node.key) {
                for &c in kids.iter().rev() {
                    if !seen.contains(&c) {
                        stack.push(c);
                    }
                }
            }
        }
        out.into_iter().map(|i| &self.nodes[i]).collect()
    }

    /// 自己 + 子孙（C# `MyAllChilds`）
    pub fn descendants_with_self(&self, key: i64) -> Vec<&TreeNode> {
        self.descendants(key, true, None, -1)
    }

    /// 全部祖先，**从高到低**（对齐 C# `AllParents`：上溯后反转）
    pub fn ancestors(&self, key: i64, include_self: bool) -> Vec<&TreeNode> {
        let mut list: Vec<&TreeNode> = Vec::new();
        let mut cur = if include_self {
            Some(key)
        } else {
            self.node(key).and_then(|n| n.parent)
        };
        let mut guard = 0usize;
        while let Some(k) = cur {
            if guard > self.nodes.len() + 1 {
                break; // 数据成环
            }
            guard += 1;
            let Some(node) = self.node(k) else { break };
            if list.iter().any(|n| n.key == node.key) {
                break; // 成环，就此中断
            }
            list.push(node);
            cur = node.parent;
        }
        list.reverse();
        list
    }

    /// 深度（对齐 C# `Deepth`：顶级 = 1）
    pub fn depth_of(&self, key: i64) -> i32 {
        if key == 0 || self.node(key).is_none() {
            return 0;
        }
        self.ancestors(key, false).len() as i32 + 1
    }

    /// 全路径（对齐 C# `GetFullPath`；C# 默认分隔符 `\`）
    pub fn full_path(&self, key: i64, include_self: bool, sep: &str) -> String {
        self.ancestors(key, include_self)
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>()
            .join(sep)
    }

    /// 按名称路径查找（对齐 C# `FindByPath`，分隔符 `. / \`）
    pub fn find_by_path(&self, path: &str) -> Option<&TreeNode> {
        let parts: Vec<&str> = path
            .split(['.', '/', '\\'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return None;
        }
        let mut level = self.children(None);
        let mut current: Option<&TreeNode> = None;
        for (i, part) in parts.iter().enumerate() {
            let found = *level.iter().find(|n| n.name == *part)?;
            current = Some(found);
            if i + 1 < parts.len() {
                level = self.children(Some(found.key));
            }
        }
        current
    }

    /// `root` 子树是否包含 `key`（对齐 C# `Contains`）
    pub fn contains(&self, root: i64, key: i64) -> bool {
        if key == 0 {
            return false;
        }
        if root == key {
            return true;
        }
        self.descendants(root, false, None, -1).iter().any(|n| n.key == key)
    }

    /// 校验「把 `key` 挂到 `parent` 下」是否合法（对齐 C# `Valid`）：
    /// 上级不能是自己、不能是自己的子孙；`max_depth > 0` 时校验深度上限。
    pub fn validate_parent(
        &self,
        key: i64,
        parent: Option<i64>,
    ) -> std::result::Result<(), String> {
        let Some(p) = parent.filter(|p| *p != 0) else {
            return Ok(()); // 顶级
        };
        if self.node(key).is_some() {
            if p == key {
                return Err("上级不能是当前节点！".to_string());
            }
            if self.contains(key, p) {
                return Err(format!("上级[{p}]是当前节点的子孙节点！"));
            }
        }
        if self.setting.max_depth > 0 {
            let new_depth = self.depth_of(p) + 1;
            if new_depth > self.setting.max_depth {
                return Err(format!("已达到最大深度{}层！", self.setting.max_depth));
            }
        }
        Ok(())
    }

    /// 同级节点（父相同，按排序规则）
    pub fn siblings(&self, key: i64) -> Vec<&TreeNode> {
        match self.node(key) {
            Some(n) => self.children(n.parent),
            None => Vec::new(),
        }
    }

    /// 树形节点名（对齐 C# `TreeNodeName`：每层两个全角空格缩进）
    pub fn tree_node_name(&self, key: i64) -> String {
        let d = self.depth_of(key);
        match self.node(key) {
            Some(n) if d > 0 => format!("{}{}", "　".repeat(((d - 1) * 2) as usize), n.name),
            _ => String::new(),
        }
    }
}

/// 写操作：把节点改挂到新父节点下（校验通过才写库）。返回受影响行数。
pub fn move_node(
    dal: &Dal,
    session: &mut dyn SqlSession,
    table_name: &str,
    index: &TreeIndex,
    key: i64,
    new_parent: Option<i64>,
) -> Result<u64> {
    index
        .validate_parent(key, new_parent)
        .map_err(crate::Error::Validation)?;

    let parent_col = index.setting().parent.clone();
    let values: Vec<(&str, DbValue)> = vec![(
        parent_col.as_str(),
        DbValue::Int(new_parent.unwrap_or(0)),
    )];
    dal.table(table_name)?
        .update_by_pk(session, &values, &[DbValue::Int(key)])
}

/// 写操作：同级上移（与相邻兄弟交换排序列值）。返回是否发生交换。
pub fn up(
    dal: &Dal,
    session: &mut dyn SqlSession,
    table_name: &str,
    index: &TreeIndex,
    key: i64,
) -> Result<bool> {
    swap_with_sibling(dal, session, table_name, index, key, -1)
}

/// 写操作：同级下移（与相邻兄弟交换排序列值）。返回是否发生交换。
pub fn down(
    dal: &Dal,
    session: &mut dyn SqlSession,
    table_name: &str,
    index: &TreeIndex,
    key: i64,
) -> Result<bool> {
    swap_with_sibling(dal, session, table_name, index, key, 1)
}

fn swap_with_sibling(
    dal: &Dal,
    session: &mut dyn SqlSession,
    table_name: &str,
    index: &TreeIndex,
    key: i64,
    dir: i32,
) -> Result<bool> {
    let Some(sort_col) = index.setting().sort.clone() else {
        return Ok(false); // 没有排序列，无法上下移动（同 C#）
    };
    let siblings = index.siblings(key);
    let Some(pos) = siblings.iter().position(|n| n.key == key) else {
        return Ok(false);
    };
    let target = pos as i32 + dir;
    if target < 0 || target as usize >= siblings.len() {
        return Ok(false);
    }
    let (a_sort, b_key, b_sort) = {
        let other = siblings[target as usize];
        (siblings[pos].sort, other.key, other.sort)
    };
    if a_sort == b_sort {
        return Ok(false); // 排序值相同，交换无意义
    }

    let table = dal.table(table_name)?;
    table.update_by_pk(
        session,
        &[(sort_col.as_str(), DbValue::Int(b_sort))],
        &[DbValue::Int(key)],
    )?;
    table.update_by_pk(
        session,
        &[(sort_col.as_str(), DbValue::Int(a_sort))],
        &[DbValue::Int(b_key)],
    )?;
    Ok(true)
}

/// 表是否为树（`BaseType="EntityTree"` 且存在父键列）。
pub fn is_tree_table(dal: &Dal, table_name: &str) -> bool {
    dal.model()
        .and_then(|m| m.table(table_name).map(|t| t.is_tree()))
        .unwrap_or(false)
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

    // ————— 表驱动层（对齐 C# EntityTree） —————

    fn rowset(rows: &[(i64, i64, i64, &str)]) -> RowSet {
        let mut rs = RowSet::new(vec![
            "Id".to_string(),
            "ParentId".to_string(),
            "Sort".to_string(),
            "Name".to_string(),
        ]);
        for (id, parent, sort, name) in rows {
            rs.push(vec![
                DbValue::Int(*id),
                DbValue::Int(*parent),
                DbValue::Int(*sort),
                DbValue::from(*name),
            ]);
        }
        rs
    }

    fn setting() -> TreeSetting {
        TreeSetting {
            key: "Id".into(),
            parent: "ParentId".into(),
            sort: Some("Sort".into()),
            name: Some("Name".into()),
            text: None,
            big_sort: true,
            max_depth: 0,
        }
    }

    /// 3 层树（BigSort=true：排序值大的在前）：
    /// A(1,sort10) → A2(3,sort30)、A1(2,sort20) → A1a(4,sort40)；B(5,sort5)
    fn sample_tree() -> TreeIndex {
        TreeIndex::from_rowset(
            setting(),
            &rowset(&[
                (1, 0, 10, "A"),
                (2, 1, 20, "A1"),
                (3, 1, 30, "A2"),
                (4, 2, 40, "A1a"),
                (5, 0, 5, "B"),
            ]),
        )
    }

    #[test]
    fn tree_roots_follow_big_sort() {
        let t = sample_tree();
        let roots: Vec<i64> = t.roots().iter().map(|n| n.key).collect();
        assert_eq!(roots, vec![1, 5], "BigSort=true：sort 大的在前（A=10 先于 B=5）");
    }

    #[test]
    fn tree_children_sort_by_sort_then_key() {
        let t = sample_tree();
        let kids: Vec<i64> = t.children(Some(1)).iter().map(|n| n.key).collect();
        assert_eq!(kids, vec![3, 2], "sort 30 先于 20");
        assert_eq!(t.children(None).len(), 2);
    }

    #[test]
    fn tree_descendants_are_depth_first_preorder() {
        let t = sample_tree();
        let all: Vec<i64> = t.descendants(1, false, None, -1).iter().map(|n| n.key).collect();
        assert_eq!(all, vec![3, 2, 4], "深度优先先序：A2 → A1 → A1a");

        let with_self: Vec<i64> =
            t.descendants(1, true, None, -1).iter().map(|n| n.key).collect();
        assert_eq!(with_self, vec![1, 3, 2, 4]);

        // 排除 A2 整棵子树
        let no_a2: Vec<i64> =
            t.descendants(1, true, Some(3), -1).iter().map(|n| n.key).collect();
        assert_eq!(no_a2, vec![1, 2, 4]);

        // 限深 2：只到 A2/A1 层，不含 A1a
        let depth2: Vec<i64> = t.descendants(1, true, None, 2).iter().map(|n| n.key).collect();
        assert_eq!(depth2, vec![1, 3, 2]);
    }

    #[test]
    fn tree_ancestors_depth_and_path() {
        let t = sample_tree();
        let up: Vec<i64> = t.ancestors(4, false).iter().map(|n| n.key).collect();
        assert_eq!(up, vec![1, 2], "从高到低");
        assert_eq!(t.depth_of(1), 1);
        assert_eq!(t.depth_of(2), 2);
        assert_eq!(t.depth_of(4), 3);
        assert_eq!(t.full_path(4, true, "\\"), "A\\A1\\A1a");
        assert_eq!(t.full_path(4, false, "/"), "A/A1");
        // C#：new String('　', (Deepth - 1) * 2)；深度 3 → 4 个全角空格
        assert_eq!(t.tree_node_name(4), "　　　　A1a");
    }

    #[test]
    fn tree_find_by_path_and_contains() {
        let t = sample_tree();
        assert_eq!(t.find_by_path("A/A1/A1a").map(|n| n.key), Some(4));
        assert_eq!(t.find_by_path("A.A2").map(|n| n.key), Some(3));
        assert_eq!(t.find_by_path("A/A9"), None);
        assert!(t.contains(1, 4));
        assert!(!t.contains(2, 3));
        assert!(!t.contains(1, 0), "空键视为不包含");
    }

    #[test]
    fn tree_validate_parent_rejects_self_and_descendant() {
        let t = sample_tree();
        assert!(t.validate_parent(2, Some(2)).is_err(), "上级不能是自己");
        assert!(t.validate_parent(2, Some(4)).is_err(), "上级不能是自己的子孙");
        assert!(t.validate_parent(2, Some(5)).is_ok());
        assert!(t.validate_parent(2, None).is_ok());
    }

    #[test]
    fn tree_validate_parent_enforces_max_depth() {
        let mut s = setting();
        s.max_depth = 2;
        let t = TreeIndex::from_rowset(s, &rowset(&[(1, 0, 1, "A"), (2, 1, 1, "A1")]));
        // 挂到第 2 层节点下会成为第 3 层 → 拒绝
        assert!(t.validate_parent(0, Some(2)).is_err());
    }

    #[test]
    fn tree_orphans_listed_under_root() {
        let t = TreeIndex::from_rowset(
            setting(),
            &rowset(&[(1, 0, 1, "A"), (2, 999, 2, "X")]),
        );
        assert_eq!(t.roots().len(), 1);
        assert_eq!(t.orphans().len(), 1);
        assert_eq!(t.children(None).len(), 2, "顶级列表附带无头节点（同 C#）");
    }

    #[test]
    fn tree_setting_derived_from_table_meta() {
        let xml = r#"<EntityModel><Tables>
          <Table Name="DeviceGroup" BaseType="EntityTree">
            <Columns>
              <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
              <Column Name="Name" DataType="String" />
              <Column Name="ParentId" DataType="Int32" />
              <Column Name="Sort" DataType="Int32" />
            </Columns>
          </Table>
          <Table Name="Device">
            <Columns>
              <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
              <Column Name="Name" DataType="String" />
            </Columns>
          </Table>
        </Tables></EntityModel>"#;
        let model = crate::model::EntityModel::parse(xml).unwrap();
        let g = model.table("DeviceGroup").unwrap();
        let s = g.tree_setting().expect("DeviceGroup 应是树");
        assert_eq!(s.key, "Id");
        assert_eq!(s.parent, "ParentId");
        assert_eq!(s.sort.as_deref(), Some("Sort"));
        assert_eq!(s.name.as_deref(), Some("Name"));
        assert!(s.big_sort, "C# BigSort 默认 true");
        assert!(g.is_tree());

        let d = model.table("Device").unwrap();
        assert!(!d.is_tree(), "无 BaseType / 无父键列 → 非树");
    }

    /// 端到端：真实 SQLite 建表 → 加载树 → 移动节点 → 同级上移（验证写操作真的落库）。
    #[test]
    fn tree_end_to_end_on_sqlite() {
        let xml = r#"<EntityModel><Tables>
          <Table Name="DeviceGroup" TableName="DeviceGroup" BaseType="EntityTree">
            <Columns>
              <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
              <Column Name="Name" DataType="String" Nullable="True" />
              <Column Name="ParentId" DataType="Int32" Nullable="True" />
              <Column Name="Sort" DataType="Int32" Nullable="True" />
            </Columns>
          </Table>
        </Tables></EntityModel>"#;
        let model = crate::model::EntityModel::parse(xml).unwrap();

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("pek-tree-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = format!("Data Source={};Provider=SQLite", dir.join("tree.db").display());

        let dal = Dal::open_with_model(&conn, model).unwrap();
        dal.sync_schema().unwrap();
        let mut s = dal.open_session().unwrap();

        for (id, parent, sort, name) in
            [(1i64, 0i64, 10i64, "A"), (2, 1, 20, "A1"), (3, 1, 30, "A2"), (4, 2, 40, "A1a")]
        {
            dal.table("DeviceGroup")
                .unwrap()
                .insert(
                    &mut *s,
                    &[
                        ("Id", DbValue::Int(id)),
                        ("ParentId", DbValue::Int(parent)),
                        ("Sort", DbValue::Int(sort)),
                        ("Name", DbValue::from(name)),
                    ],
                )
                .unwrap();
        }

        // 1) 加载 + 排序（BigSort：sort 大的在前）
        let idx = TreeIndex::load(&dal, &mut *s, "DeviceGroup").unwrap();
        let kids: Vec<i64> = idx.children(Some(1)).iter().map(|n| n.key).collect();
        assert_eq!(kids, vec![3, 2]);
        assert_eq!(idx.full_path(4, true, "\\"), "A\\A1\\A1a");
        assert_eq!(idx.depth_of(4), 3);

        // 2) 防环：把 A 挂到自己的子孙 A1a 下必须被拒绝
        assert!(idx.validate_parent(1, Some(4)).is_err());

        // 3) 正常移动：A1a 改挂到 A2 下（真的写库）
        let affected = move_node(&dal, &mut *s, "DeviceGroup", &idx, 4, Some(3)).unwrap();
        assert_eq!(affected, 1, "应更新 1 行");
        let idx2 = TreeIndex::load(&dal, &mut *s, "DeviceGroup").unwrap();
        assert_eq!(idx2.full_path(4, true, "/"), "A/A2/A1a");

        // 4) 同级上移：BigSort 降序下 A2(3,sort30) 已排首位 → 上移无效；A1(2) 上移与它交换 Sort
        assert!(!up(&dal, &mut *s, "DeviceGroup", &idx2, 3).unwrap(), "已在首位应返回 false");
        assert!(up(&dal, &mut *s, "DeviceGroup", &idx2, 2).unwrap());
        let idx3 = TreeIndex::load(&dal, &mut *s, "DeviceGroup").unwrap();
        let kids3: Vec<i64> = idx3.children(Some(1)).iter().map(|n| n.key).collect();
        assert_eq!(kids3, vec![2, 3], "交换后 A1(sort30) 排在 A2(sort20) 之前");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
