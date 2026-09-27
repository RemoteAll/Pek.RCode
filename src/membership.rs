//! 成员资格核心（对应 DH.NCode 的 `Membership` 模块）。
//!
//! 覆盖四类可移植核心件：
//! - 密码：MD5 链式哈希与登录校验（对齐 `用户.Biz.cs` 的 `Login` 语义）；
//! - 枚举：`DataScopes`/`SexKinds`/`MenuTypes`/`RoleTypes`/`TenantTypes`/`DepartmentTypes`/`ParameterKinds`；
//! - 权限：`PermissionFlags` 位标志与判断；
//! - 菜单：从数据行构建菜单树（对齐 `菜单.Biz.cs` 的排序、可见性与权限过滤规则）。
//!
//! 反射相关的实体行为（`EntityFactory`、`Meta.Cache` 全局缓存等）在 Rust 中省略，
//! 由 `Dal::table(name)` 与显式传参取代，见迁移文档"机制差异"一节。

use crate::session::DbRow;
use crate::value::DbValue;

/// 计算字符串的 MD5（32 位小写十六进制），对齐 NewLife 的 `MD5()` 扩展。
///
/// 实现已下沉到 DH 基础库（`DH.RustBase` / crate `dhrust` 的 `sign` 模块），
/// 此处转出以兼容既有调用方。
pub use dhrust::sign::md5_hex;

/// 对密码做 `times` 轮 MD5 哈希（对齐 C# 登录时的 `for (i < hashTimes) p = p.MD5()`）。
/// <param name="password">原始密码</param>
/// <param name="times">哈希轮数，1 表示单次 MD5</param>
/// <returns>哈希结果（空密码原样返回）</returns>
pub fn hash_password(password: &str, times: u32) -> String {
    let mut p = password.to_string();
    for _ in 0..times {
        p = md5_hex(&p);
    }
    p
}

/// 登录校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginResult {
    /// 校验通过。
    Ok,
    /// 校验通过（数据库密码字段为空，任何密码均可登录），调用方应回写该密码哈希。
    NeedsHash(String),
    /// 校验失败，携带错误消息（对齐 C# 异常文案）。
    Error(String),
}

/// 校验登录密码（纯逻辑，不访问数据库），对齐 `用户.Biz.cs` 的 `Login`。
///
/// - `hash_times > 0`：对输入密码做多轮 MD5 后与存储值比较（忽略大小写）；
/// - `hash_times == -1`：自动登录分支，对存储值再哈希 `-hash_times` 轮后与输入比较；
/// - `hash_times == 0`：直接比较；
/// - 存储值为空：返回 [`LoginResult::NeedsHash`]，由调用方回写。
/// <param name="account">账号（用于错误消息）</param>
/// <param name="enable">账号是否启用</param>
/// <param name="stored">数据库中的密码字段</param>
/// <param name="input">用户输入的密码</param>
/// <param name="hash_times">哈希轮数</param>
/// <returns>校验结果</returns>
pub fn verify_login(account: &str, enable: bool, stored: &str, input: &str, hash_times: i32) -> LoginResult {
    if !enable {
        return LoginResult::Error(format!("账号{account}被禁用！"));
    }

    if !stored.is_empty() {
        if hash_times > 0 {
            let mut p = input.to_string();
            if !p.is_empty() {
                for _ in 0..hash_times {
                    p = md5_hex(&p);
                }
            }
            if !p.eq_ignore_ascii_case(stored) {
                return LoginResult::Error("密码不正确！".into());
            }
        } else {
            let mut p = stored.to_string();
            let mut i = 0;
            while i > hash_times {
                p = md5_hex(&p);
                i -= 1;
            }
            if !p.eq_ignore_ascii_case(input) {
                return LoginResult::Error("密 码不正确！".into());
            }
        }
        LoginResult::Ok
    } else if hash_times > 0 {
        LoginResult::NeedsHash(hash_password(input, hash_times as u32))
    } else {
        LoginResult::NeedsHash(input.to_string())
    }
}

/// 数据范围（角色的数据权限范围），对齐 DH.NCode `DataScopes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataScope {
    /// 默认。使用角色或上级默认值，仅用于菜单等覆盖场景（-1）。
    #[default]
    Default = -1,
    /// 全部（0）。
    All = 0,
    /// 本部门及下级（1）。
    DepartmentAndBelow = 1,
    /// 本部门（2）。
    Department = 2,
    /// 仅本人（3）。
    SelfOnly = 3,
    /// 自定义（4）。
    Custom = 4,
}

impl DataScope {
    /// 从数值解析数据范围。
    /// <param name="value">数值</param>
    /// <returns>数据范围，未知值返回 None</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            -1 => Some(Self::Default),
            0 => Some(Self::All),
            1 => Some(Self::DepartmentAndBelow),
            2 => Some(Self::Department),
            3 => Some(Self::SelfOnly),
            4 => Some(Self::Custom),
            _ => None,
        }
    }
}

/// 性别，对齐 DH.NCode `SexKinds`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SexKinds {
    /// 未知（0）。
    #[default]
    Unknown = 0,
    /// 男（1）。
    Male = 1,
    /// 女（2）。
    Female = 2,
}

/// 菜单类型，对齐 DH.NCode `MenuTypes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuTypes {
    /// 目录（1）。
    #[default]
    Directory = 1,
    /// 菜单（2）。
    Menu = 2,
    /// 功能（3）。
    Function = 3,
}

/// 角色类型，对齐 DH.NCode `RoleTypes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoleTypes {
    /// 系统（1）。
    System = 1,
    /// 普通（2）。
    #[default]
    Normal = 2,
    /// 租户（3）。
    Tenant = 3,
}

/// 租户类型，对齐 DH.NCode `TenantTypes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TenantTypes {
    /// 免费（1）。
    #[default]
    Free = 1,
    /// 个人（2）。
    Personal = 2,
    /// 企业（3）。
    Enterprise = 3,
    /// 旗舰（4）。
    Flagship = 4,
}

/// 部门类型，对齐 DH.NCode `DepartmentTypes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DepartmentTypes {
    /// 公司（1）。
    #[default]
    Company = 1,
    /// 部门（2）。
    Department = 2,
    /// 小组（3）。
    Group = 3,
    /// 虚拟（4）。
    Virtual = 4,
}

/// 参数种类，对齐 DH.NCode `ParameterKinds`（数值与 `DbType` 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParameterKinds {
    /// 普通（0）。
    #[default]
    Normal = 0,
    /// 布尔（3）。
    Boolean = 3,
    /// 整数（9）。
    Int = 9,
    /// 双精度（14）。
    Double = 14,
    /// 时间（16）。
    DateTime = 16,
    /// 字符串（18）。
    String = 18,
    /// 列表（21）。
    List = 21,
    /// 哈希（22）。
    Hash = 22,
}

impl SexKinds {
    /// 从数值解析（未知值返回 None）。
    /// <param name="value">数值</param>
    /// <returns>枚举值</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Unknown),
            1 => Some(Self::Male),
            2 => Some(Self::Female),
            _ => None,
        }
    }
}
impl MenuTypes {
    /// 从数值解析（未知值返回 None）。
    /// <param name="value">数值</param>
    /// <returns>枚举值</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::Directory),
            2 => Some(Self::Menu),
            3 => Some(Self::Function),
            _ => None,
        }
    }
}
impl RoleTypes {
    /// 从数值解析（未知值返回 None）。
    /// <param name="value">数值</param>
    /// <returns>枚举值</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::System),
            2 => Some(Self::Normal),
            3 => Some(Self::Tenant),
            _ => None,
        }
    }
}
impl TenantTypes {
    /// 从数值解析（未知值返回 None）。
    /// <param name="value">数值</param>
    /// <returns>枚举值</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::Free),
            2 => Some(Self::Personal),
            3 => Some(Self::Enterprise),
            4 => Some(Self::Flagship),
            _ => None,
        }
    }
}
impl DepartmentTypes {
    /// 从数值解析（未知值返回 None）。
    /// <param name="value">数值</param>
    /// <returns>枚举值</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::Company),
            2 => Some(Self::Department),
            3 => Some(Self::Group),
            4 => Some(Self::Virtual),
            _ => None,
        }
    }
}
impl ParameterKinds {
    /// 从数值解析（未知值返回 None）。
    /// <param name="value">数值</param>
    /// <returns>枚举值</returns>
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            3 => Some(Self::Boolean),
            9 => Some(Self::Int),
            14 => Some(Self::Double),
            16 => Some(Self::DateTime),
            18 => Some(Self::String),
            21 => Some(Self::List),
            22 => Some(Self::Hash),
            _ => None,
        }
    }
}

/// 操作权限位标志，对齐 DH.NCode `PermissionFlags`（`UInt32` 位标志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionFlags(pub u32);

impl PermissionFlags {
    /// 无权限。
    pub const NONE: Self = Self(0);
    /// 查看权限。
    pub const DETAIL: Self = Self(1);
    /// 添加权限。
    pub const INSERT: Self = Self(2);
    /// 修改权限。
    pub const UPDATE: Self = Self(4);
    /// 删除权限。
    pub const DELETE: Self = Self(8);
    /// 所有权限。
    pub const ALL: Self = Self(0xFFFF_FFFF);

    /// 是否包含指定权限位（`need` 的每一位都必须存在）。
    /// <param name="need">需要的权限位</param>
    /// <returns>是否包含</returns>
    pub fn contains(self, need: Self) -> bool {
        self.0 & need.0 == need.0
    }
}

impl std::ops::BitOr for PermissionFlags {
    type Output = Self;

    /// 合并权限位。
    /// <param name="rhs">另一组权限位</param>
    /// <returns>合并结果</returns>
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// 菜单节点（树构建结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuNode {
    /// 编号。
    pub id: i64,
    /// 父级编号。
    pub parent_id: i64,
    /// 名称（唯一）。
    pub name: String,
    /// 显示名（缺省时取名称）。
    pub display_name: String,
    /// 链接（`~` 为应用根）。
    pub url: String,
    /// 排序（降序）。
    pub sort: i32,
    /// 是否可见。
    pub visible: bool,
    /// 是否启用。
    pub enable: bool,
    /// 子节点。
    pub children: Vec<MenuNode>,
}

/// 从菜单数据行构建树（对齐 `菜单.Biz.cs` 的 `FindAllByParentID` 排序规则：Sort 降序，同序按 ID 升序）。
///
/// 列名约定与 DH.NCode `Menu` 实体一致：`ID`、`ParentID`、`Name`、`DisplayName`、`Url`、`Sort`、`Visible`、`Enable`；
/// 缺列取默认值（文本空、Sort 0、Visible/Enable 为 true），`ID` 缺失的行跳过。
/// <param name="rows">菜单数据行</param>
/// <param name="parent_id">父级编号（根为 0）</param>
/// <param name="incl_invisible">是否包含不可见菜单</param>
/// <returns>菜单树</returns>
pub fn build_menu_tree(rows: &[DbRow], parent_id: i64, incl_invisible: bool) -> Vec<MenuNode> {
    build_menu_tree_filtered(rows, parent_id, incl_invisible, None)
}

/// 从菜单数据行构建树并按允许的菜单编号过滤（`allowed=None` 表示不过滤）。
///
/// 过滤仅保留编号命中的节点及其子树（父节点被剔除时其子节点一并不可见），
/// 对齐角色权限中 `GetSubMenus(filters, inclInvisible)` 的用法。
/// <param name="rows">菜单数据行</param>
/// <param name="parent_id">父级编号（根为 0）</param>
/// <param name="incl_invisible">是否包含不可见菜单</param>
/// <param name="allowed">允许的菜单编号集合</param>
/// <returns>菜单树</returns>
pub fn build_menu_tree_filtered(
    rows: &[DbRow],
    parent_id: i64,
    incl_invisible: bool,
    allowed: Option<&[i64]>,
) -> Vec<MenuNode> {
    let mut nodes: Vec<MenuNode> = rows
        .iter()
        .filter_map(|row| row_to_menu(row, parent_id, incl_invisible))
        .filter(|node| allowed.is_none_or(|a| a.contains(&node.id)))
        .collect();
    nodes.sort_by(|a, b| b.sort.cmp(&a.sort).then_with(|| a.id.cmp(&b.id)));
    for node in &mut nodes {
        node.children = build_menu_tree_filtered(rows, node.id, incl_invisible, allowed);
    }
    nodes
}

/// 单行转换为菜单节点（编号缺失或父级不匹配时返回 None）。
fn row_to_menu(row: &DbRow, parent_id: i64, incl_invisible: bool) -> Option<MenuNode> {
    let id = row.get_by_name("ID")?.as_i64()?;
    let pid = row.get_by_name("ParentID").and_then(DbValue::as_i64).unwrap_or(0);
    if pid != parent_id {
        return None;
    }
    let visible = row.get_by_name("Visible").and_then(DbValue::as_bool).unwrap_or(true);
    if !incl_invisible && !visible {
        return None;
    }
    let name = text_of(row, "Name");
    let display_name = {
        let d = text_of(row, "DisplayName");
        if d.trim().is_empty() { name.clone() } else { d }
    };
    Some(MenuNode {
        id,
        parent_id: pid,
        name,
        display_name,
        url: text_of(row, "Url"),
        sort: row.get_by_name("Sort").and_then(DbValue::as_i32).unwrap_or(0),
        visible,
        enable: row.get_by_name("Enable").and_then(DbValue::as_bool).unwrap_or(true),
        children: Vec::new(),
    })
}

/// 读取文本列（缺列或空值返回空字符串）。
fn text_of(row: &DbRow, name: &str) -> String {
    row.get_by_name(name).and_then(DbValue::as_str).unwrap_or_default().to_string()
}

/// 角色数据范围（对应 `IRole` 的可移植字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleScope {
    /// 数据权限范围。
    pub data_scope: DataScope,
    /// 是否系统角色（不受限制）。
    pub is_system: bool,
    /// 自定义数据范围的部门编号列表。
    pub data_department_ids: Vec<i32>,
}

/// 计算多角色的有效数据范围（对齐 C# `roles.Min(e => e.DataScope)`：数值越小权限越大）。
/// <param name="roles">角色集合</param>
/// <param name="scope">指定的数据范围，优先使用</param>
/// <returns>有效范围；角色为空且未指定时返回 None</returns>
pub fn effective_scope(roles: &[RoleScope], scope: Option<DataScope>) -> Option<DataScope> {
    if let Some(s) = scope {
        return Some(s);
    }
    let v = roles.iter().map(|r| r.data_scope as i32).min()?;
    DataScope::from_i32(v)
}

/// 数据权限上下文（对齐 C# `DataScopeContext` 的可移植字段）。
///
/// 全局静态 `Current`、缓存与菜单联动在 Rust 中由调用方显式传参取代，
/// 见迁移文档"机制差异"一节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataScopeContext {
    /// 用户编号。
    pub user_id: i32,
    /// 用户所属部门编号。
    pub department_id: i32,
    /// 生效的数据范围。
    pub data_scope: DataScope,
    /// 可访问的部门编号列表；None 表示不限制。
    pub accessible_department_ids: Option<Vec<i32>>,
    /// 是否系统身份（不受限制）。
    pub is_system: bool,
}

impl DataScopeContext {
    /// 应用菜单级数据范围覆盖（对齐 C# `DataScopeContext.SetMenu`）：
    /// 菜单数据范围 `>= 0` 时覆盖角色默认值并重算可访问部门。
    /// <param name="menu_data_scope">菜单的数据范围字段（-1 表示不覆盖）</param>
    /// <param name="roles">用户角色</param>
    /// <param name="dept_and_below">用户所属部门及下级的编号集合</param>
    /// <param name="managed_dept_ids">用户管理的部门及其下级的编号集合</param>
    pub fn apply_menu_scope(
        &mut self,
        menu_data_scope: i32,
        roles: &[RoleScope],
        dept_and_below: &[i32],
        managed_dept_ids: &[i32],
    ) {
        if menu_data_scope < 0 {
            return;
        }
        let Some(scope) = DataScope::from_i32(menu_data_scope) else {
            return;
        };
        self.data_scope = scope;
        self.accessible_department_ids =
            accessible_department_ids(self.department_id, roles, Some(scope), dept_and_below, managed_dept_ids);
    }
}

/// 获取可访问的部门编号列表（对齐 `DataScopeHelper.GetAccessibleDepartmentIds`）。
///
/// - 返回 `None` 表示不限制（系统角色或"全部"范围）；
/// - 返回 `Some(空)` 表示无权限（角色为空或"仅本人"）；
/// - `dept_and_below` 为"用户所属部门及下级"预计算集合（由 [`department_and_children`] 计算）；
/// - `managed_dept_ids` 为"我管理的部门及其下级"预计算集合（对应 C# `GetManagedDepartmentIds` 查询结果）。
/// <param name="user_dept_id">用户所属部门编号</param>
/// <param name="roles">用户角色集合</param>
/// <param name="scope">指定的数据范围，优先使用</param>
/// <param name="dept_and_below">用户所属部门及下级的编号集合</param>
/// <param name="managed_dept_ids">用户管理的部门及其下级的编号集合</param>
/// <returns>可访问的部门编号列表，None 表示不限制</returns>
pub fn accessible_department_ids(
    user_dept_id: i32,
    roles: &[RoleScope],
    scope: Option<DataScope>,
    dept_and_below: &[i32],
    managed_dept_ids: &[i32],
) -> Option<Vec<i32>> {
    if roles.is_empty() {
        return Some(Vec::new());
    }
    // 系统角色不受限制
    if roles.iter().any(|r| r.is_system) {
        return None;
    }
    let effective = effective_scope(roles, scope).unwrap_or(DataScope::Default);
    // 全部权限不限制
    if effective == DataScope::All {
        return None;
    }

    let mut ids: Vec<i32> = Vec::new();
    match effective {
        DataScope::DepartmentAndBelow => {
            for id in dept_and_below {
                push_unique(&mut ids, *id);
            }
        }
        DataScope::Department => {
            if user_dept_id > 0 {
                push_unique(&mut ids, user_dept_id);
            }
        }
        DataScope::SelfOnly => {
            // 仅本人不添加部门，由调用方使用 UserId 过滤
        }
        DataScope::Custom => {
            // 自定义时合并所有角色的自定义部门
            for role in roles {
                if role.data_scope == DataScope::Custom {
                    for id in &role.data_department_ids {
                        push_unique(&mut ids, *id);
                    }
                }
            }
        }
        _ => {}
    }

    // 部门管理者：并入"我管理的部门"及其下级部门；仅本人范围保持最小语义，不并入管理范围
    if effective != DataScope::SelfOnly {
        for id in managed_dept_ids {
            push_unique(&mut ids, *id);
        }
    }

    Some(ids)
}

/// 获取部门及其所有下级部门的编号（对齐 `DataScopeHelper.GetDepartmentAndChildren`）。
///
/// `pairs` 为 `(部门编号, 父级编号)` 集合；部门不存在时返回仅含自身的数组，
/// 下级展开按编号升序深度优先（对齐 C# 中 `Childs` 的 `OrderBy(ID)`）。
/// <param name="root_id">部门编号</param>
/// <param name="pairs">部门层级集合</param>
/// <returns>部门编号列表（包含自身）</returns>
pub fn department_and_children(root_id: i32, pairs: &[(i32, i32)]) -> Vec<i32> {
    if root_id <= 0 {
        return Vec::new();
    }
    let mut ids = vec![root_id];
    // 部门不存在（对齐 C# FindByID 为空时返回 [departmentId]）
    if !pairs.iter().any(|(id, _)| *id == root_id) {
        return ids;
    }
    collect_child_ids(root_id, pairs, &mut ids);
    ids
}

/// 递归收集下级部门编号（对齐 `CollectChildDepartmentIds`，按编号去重防环）。
fn collect_child_ids(parent_id: i32, pairs: &[(i32, i32)], ids: &mut Vec<i32>) {
    let mut children: Vec<i32> = pairs.iter().filter(|(_, p)| *p == parent_id).map(|(c, _)| *c).collect();
    children.sort_unstable();
    for child in children {
        if !ids.contains(&child) {
            ids.push(child);
            collect_child_ids(child, pairs, ids);
        }
    }
}

/// 解析逗号分隔的部门编号字符串（对齐 `ParseDepartmentIds`/`SplitAsInt`）。
/// <param name="text">逗号、分号、竖线或空白分隔的编号字符串</param>
/// <returns>部门编号数组，无法解析的片段跳过</returns>
pub fn parse_department_ids(text: &str) -> Vec<i32> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    text.split([',', ';', '|', ' ', '\t', '\r', '\n'])
        .filter_map(|s| s.trim().parse::<i32>().ok())
        .collect()
}

/// 向集合追加编号（去重）。
fn push_unique(ids: &mut Vec<i32>, id: i32) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

/// 数据权限过滤表达式（对应 C# `Expression` 的可移植子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeFilterExpression {
    /// 无需过滤。
    None,
    /// 列 = 值（`值 = -1` 为恒假条件，对齐 C# 空部门集合）。
    Equal {
        /// 列名。
        column: String,
        /// 值。
        value: i32,
    },
    /// 列 IN (值集合)。
    In {
        /// 列名。
        column: String,
        /// 值集合。
        values: Vec<i32>,
    },
    /// 或组合（如"部门集合 或 本人"）。
    Or(Vec<ScopeFilterExpression>),
}

impl ScopeFilterExpression {
    /// 是否无需过滤。
    /// <returns>是否为空表达式</returns>
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// 渲染为 SQL 条件片段（不含 `WHERE` 关键字；值为整数，无注入风险）。
    /// <returns>SQL 片段，None 表示无需过滤</returns>
    pub fn to_sql(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Equal { column, value } => Some(format!("{column} = {value}")),
            Self::In { column, values } => {
                if values.is_empty() {
                    return None;
                }
                let list = values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
                Some(format!("{column} IN ({list})"))
            }
            Self::Or(parts) => {
                let sql: Vec<String> = parts.iter().filter_map(|p| p.to_sql()).collect();
                match sql.len() {
                    0 => None,
                    1 => sql.into_iter().next(),
                    _ => Some(format!("({})", sql.join(" OR "))),
                }
            }
        }
    }
}

/// 构建数据权限过滤条件（对齐 `DataScopeHelper.BuildFilter`，含 `GetFilter` 的守卫）。
///
/// - `user_column`/`dept_column` 为实体上的用户/部门列名，`None` 表示实体无该列；
/// - 仅本人：优先按用户列过滤，无用户列时退化为按部门列过滤；
/// - 本部门/本部门及下级/自定义：按部门集合过滤，且本人数据始终可见（或组合）。
/// <param name="context">数据权限上下文</param>
/// <param name="user_column">用户列名</param>
/// <param name="dept_column">部门列名</param>
/// <returns>过滤表达式</returns>
pub fn build_scope_filter(
    context: &DataScopeContext,
    user_column: Option<&str>,
    dept_column: Option<&str>,
) -> ScopeFilterExpression {
    if context.is_system || context.data_scope == DataScope::All {
        return ScopeFilterExpression::None;
    }
    match context.data_scope {
        DataScope::SelfOnly => {
            if let Some(column) = user_column {
                return ScopeFilterExpression::Equal { column: column.into(), value: context.user_id };
            }
            if let Some(column) = dept_column {
                return ScopeFilterExpression::Equal { column: column.into(), value: context.department_id };
            }
            ScopeFilterExpression::None
        }
        DataScope::Department | DataScope::DepartmentAndBelow | DataScope::Custom => {
            let dept_filter = build_department_filter(context, dept_column);
            if dept_filter.is_none() {
                return ScopeFilterExpression::None;
            }
            // 本人数据始终可见：无部门归属、调岗或兼任管理部门时，仅按部门集合过滤会把用户自己的数据排除在外
            if let Some(column) = user_column {
                return ScopeFilterExpression::Or(vec![
                    dept_filter,
                    ScopeFilterExpression::Equal { column: column.into(), value: context.user_id },
                ]);
            }
            dept_filter
        }
        _ => ScopeFilterExpression::None,
    }
}

/// 构建部门过滤条件（对齐 `BuildDepartmentFilter`）。
fn build_department_filter(context: &DataScopeContext, dept_column: Option<&str>) -> ScopeFilterExpression {
    let Some(column) = dept_column else {
        return ScopeFilterExpression::None;
    };
    let Some(ids) = &context.accessible_department_ids else {
        return ScopeFilterExpression::None; // None 表示不限制
    };
    match ids.len() {
        0 => ScopeFilterExpression::Equal { column: column.into(), value: -1 }, // 空数组表示无权限，恒假条件
        1 => ScopeFilterExpression::Equal { column: column.into(), value: ids[0] },
        _ => ScopeFilterExpression::In { column: column.into(), values: ids.clone() },
    }
}

/// 构建纯部门实体的过滤条件（对齐 `BuildDepartmentScopeFilter`，适用于"一行一个部门"的表）。
///
/// 仅本人时退化为"当前用户所在部门"（`部门列 = 用户部门编号`），避免恒假条件导致空表。
/// <param name="context">数据权限上下文</param>
/// <param name="dept_column">部门列名</param>
/// <returns>过滤表达式</returns>
pub fn build_department_scope_filter(context: &DataScopeContext, dept_column: Option<&str>) -> ScopeFilterExpression {
    if context.is_system || context.data_scope == DataScope::All {
        return ScopeFilterExpression::None;
    }
    if context.data_scope == DataScope::SelfOnly {
        return match dept_column {
            Some(column) => ScopeFilterExpression::Equal { column: column.into(), value: context.department_id },
            None => ScopeFilterExpression::None,
        };
    }
    build_department_filter(context, dept_column)
}

/// 校验数据行是否在当前数据权限内（对齐 `CanAccess(IDataScope)`，含用户列与部门列）。
/// <param name="context">数据权限上下文</param>
/// <param name="row_user_id">数据行的用户编号</param>
/// <param name="row_department_id">数据行的部门编号</param>
/// <returns>是否有权访问</returns>
pub fn can_access_scope_row(context: &DataScopeContext, row_user_id: i32, row_department_id: i32) -> bool {
    if context.is_system {
        return true;
    }
    match context.data_scope {
        DataScope::All => true,
        DataScope::SelfOnly => row_user_id == context.user_id,
        DataScope::Department | DataScope::DepartmentAndBelow | DataScope::Custom => {
            // 本人数据始终可访问
            if row_user_id == context.user_id {
                return true;
            }
            match &context.accessible_department_ids {
                None => true,
                Some(ids) => ids.contains(&row_department_id),
            }
        }
        _ => true,
    }
}

/// 校验数据行是否在当前数据权限内（对齐 `CanAccess(IUserScope)`，仅用户标识实体）。
/// <param name="context">数据权限上下文</param>
/// <param name="row_user_id">数据行的用户编号</param>
/// <returns>是否有权访问</returns>
pub fn can_access_user_row(context: &DataScopeContext, row_user_id: i32) -> bool {
    if context.is_system || context.data_scope == DataScope::All {
        return true;
    }
    row_user_id == context.user_id
}

/// 校验数据行是否在当前数据权限内（对齐 `CanAccess(IDepartmentScope)`，仅部门标识实体）。
/// <param name="context">数据权限上下文</param>
/// <param name="row_department_id">数据行的部门编号</param>
/// <returns>是否有权访问</returns>
pub fn can_access_department_row(context: &DataScopeContext, row_department_id: i32) -> bool {
    if context.is_system || context.data_scope == DataScope::All {
        return true;
    }
    if context.data_scope == DataScope::SelfOnly {
        return row_department_id == context.department_id;
    }
    match &context.accessible_department_ids {
        None => true,
        Some(ids) => ids.contains(&row_department_id),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// 构造一行测试数据。
    fn menu_row(id: i64, parent_id: i64, name: &str, sort: i64, visible: i64) -> DbRow {
        DbRow::new(
            Arc::new(
                ["ID", "ParentID", "Name", "DisplayName", "Url", "Sort", "Visible", "Enable"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            vec![
                DbValue::Int(id),
                DbValue::Int(parent_id),
                DbValue::Text(name.into()),
                DbValue::Null,
                DbValue::Text(format!("/{name}")),
                DbValue::Int(sort),
                DbValue::Int(visible),
                DbValue::Int(1),
            ],
        )
    }

    #[test]
    fn md5_and_hash_password_chain() {
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        // 两轮哈希等于对单轮结果再哈希
        assert_eq!(hash_password("abc", 2), md5_hex(&md5_hex("abc")));
        assert_eq!(hash_password("abc", 0), "abc");
    }

    #[test]
    fn verify_login_matches_csharp_semantics() {
        let stored = md5_hex("123456");
        // 正常登录
        assert_eq!(verify_login("admin", true, &stored, "123456", 1), LoginResult::Ok);
        // 密码错误
        assert_eq!(
            verify_login("admin", true, &stored, "654321", 1),
            LoginResult::Error("密码不正确！".into())
        );
        // 存储值大小写不敏感
        assert_eq!(verify_login("admin", true, &stored.to_uppercase(), "123456", 1), LoginResult::Ok);
        // 账号被禁用
        assert_eq!(
            verify_login("admin", false, &stored, "123456", 1),
            LoginResult::Error("账号admin被禁用！".into())
        );
        // 空密码字段：任何密码均可登录，回写哈希
        assert_eq!(
            verify_login("admin", true, "", "123456", 1),
            LoginResult::NeedsHash(stored.clone())
        );
        // 自动登录分支（hashTimes=-1）：对存储值再哈希一轮后与输入比较
        assert_eq!(verify_login("admin", true, "abc", &md5_hex("abc"), -1), LoginResult::Ok);
        assert_eq!(
            verify_login("admin", true, "abc", "abc", -1),
            LoginResult::Error("密 码不正确！".into())
        );
    }

    #[test]
    fn enum_values_match_csharp() {
        assert_eq!(DataScope::Default as i32, -1);
        assert_eq!(DataScope::All as i32, 0);
        assert_eq!(DataScope::DepartmentAndBelow as i32, 1);
        assert_eq!(DataScope::Department as i32, 2);
        assert_eq!(DataScope::SelfOnly as i32, 3);
        assert_eq!(DataScope::Custom as i32, 4);
        assert_eq!(DataScope::from_i32(3), Some(DataScope::SelfOnly));
        assert_eq!(DataScope::from_i32(9), None);

        assert_eq!(SexKinds::Male as i32, 1);
        assert_eq!(MenuTypes::Function as i32, 3);
        assert_eq!(RoleTypes::Tenant as i32, 3);
        assert_eq!(TenantTypes::Flagship as i32, 4);
        assert_eq!(DepartmentTypes::Virtual as i32, 4);
        assert_eq!(ParameterKinds::Hash as i32, 22);
    }

    #[test]
    fn permission_flags_combine_and_test() {
        let p = PermissionFlags::DETAIL | PermissionFlags::INSERT;
        assert!(p.contains(PermissionFlags::DETAIL));
        assert!(p.contains(PermissionFlags::DETAIL | PermissionFlags::INSERT));
        assert!(!p.contains(PermissionFlags::DELETE));
        assert!(PermissionFlags::ALL.contains(p));
        assert!(!PermissionFlags::NONE.contains(PermissionFlags::DETAIL));
    }

    #[test]
    fn menu_tree_build_sort_and_visibility() {
        let rows = vec![
            menu_row(1, 0, "Sys", 10, 1),
            menu_row(2, 0, "Biz", 20, 1),
            menu_row(3, 1, "User", 5, 1),
            menu_row(4, 1, "Role", 5, 0),
            menu_row(5, 2, "Order", 1, 1),
        ];
        let tree = build_menu_tree(&rows, 0, false);
        // Sort 降序：Biz(20) 在 Sys(10) 前
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "Biz");
        assert_eq!(tree[1].name, "Sys");
        // 子节点排序：同 Sort 按 ID 升序；不可见节点默认剔除
        let sys = &tree[1];
        assert_eq!(sys.children.len(), 1);
        assert_eq!(sys.children[0].name, "User");
        assert_eq!(sys.children[0].display_name, "User");
        assert_eq!(sys.children[0].url, "/User");
        // 包含不可见节点
        let tree_all = build_menu_tree(&rows, 0, true);
        assert_eq!(tree_all[1].children.len(), 2);
        assert_eq!(tree_all[1].children[1].name, "Role");
        // 权限过滤：仅允许 Sys(1) 与 User(3)
        let filtered = build_menu_tree_filtered(&rows, 0, false, Some(&[1, 3]));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "Sys");
        assert_eq!(filtered[0].children.len(), 1);
    }

    /// 构造角色。
    fn role(scope: DataScope, is_system: bool, custom: &[i32]) -> RoleScope {
        RoleScope {
            data_scope: scope,
            is_system,
            data_department_ids: custom.to_vec(),
        }
    }

    #[test]
    fn accessible_department_ids_rules() {
        // 系统角色不受限制
        assert_eq!(accessible_department_ids(5, &[role(DataScope::SelfOnly, true, &[])], None, &[], &[]), None);
        // 角色为空：无权限
        assert_eq!(accessible_department_ids(5, &[], None, &[], &[]), Some(vec![]));
        // 全部：不限制
        assert_eq!(accessible_department_ids(5, &[role(DataScope::All, false, &[])], None, &[], &[]), None);
        // 多角色取数值最小（权限最大）
        let roles = [role(DataScope::Department, false, &[]), role(DataScope::All, false, &[])];
        assert_eq!(accessible_department_ids(5, &roles, None, &[], &[]), None);
        // 本部门
        assert_eq!(
            accessible_department_ids(5, &[role(DataScope::Department, false, &[])], None, &[], &[]),
            Some(vec![5])
        );
        // 本部门及下级：使用预计算集合
        assert_eq!(
            accessible_department_ids(5, &[role(DataScope::DepartmentAndBelow, false, &[])], None, &[5, 6, 7], &[]),
            Some(vec![5, 6, 7])
        );
        // 仅本人：空集合，且不并入管理范围
        assert_eq!(
            accessible_department_ids(5, &[role(DataScope::SelfOnly, false, &[])], None, &[], &[99]),
            Some(vec![])
        );
        // 自定义：合并自定义部门；管理范围并入（非仅本人）
        assert_eq!(
            accessible_department_ids(
                5,
                &[role(DataScope::Custom, false, &[8, 9])],
                None,
                &[],
                &[10, 8]
            ),
            Some(vec![8, 9, 10])
        );
        // 指定范围优先于角色计算
        assert_eq!(
            accessible_department_ids(5, &[role(DataScope::All, false, &[])], Some(DataScope::Department), &[], &[]),
            Some(vec![5])
        );
    }

    #[test]
    fn department_tree_expansion() {
        let pairs = [(1, 0), (2, 1), (3, 2), (4, 1)];
        // 深度优先、同级按编号升序
        assert_eq!(department_and_children(1, &pairs), vec![1, 2, 3, 4]);
        assert_eq!(department_and_children(2, &pairs), vec![2, 3]);
        // 部门不存在：仅返回自身（对齐 C# FindByID 为空时的行为）
        assert_eq!(department_and_children(9, &pairs), vec![9]);
        // 非法编号
        assert_eq!(department_and_children(0, &pairs), Vec::<i32>::new());
        // 解析字符串
        assert_eq!(parse_department_ids("1,2;3 4|abc"), vec![1, 2, 3, 4]);
        assert_eq!(parse_department_ids("  "), Vec::<i32>::new());
    }

    #[test]
    fn build_scope_filter_and_sql() {
        let ctx = DataScopeContext {
            user_id: 7,
            department_id: 5,
            data_scope: DataScope::Department,
            accessible_department_ids: Some(vec![5]),
            is_system: false,
        };
        // 本部门 + 用户列：或组合（本人数据始终可见）
        let f = build_scope_filter(&ctx, Some("CreateUserID"), Some("DepartmentID"));
        assert_eq!(f.to_sql().as_deref(), Some("(DepartmentID = 5 OR CreateUserID = 7)"));
        // 无用户列：仅部门过滤
        let f = build_scope_filter(&ctx, None, Some("DepartmentID"));
        assert_eq!(f.to_sql().as_deref(), Some("DepartmentID = 5"));
        // 多部门：IN
        let ctx2 = DataScopeContext { accessible_department_ids: Some(vec![5, 6, 7]), ..ctx.clone() };
        let f = build_scope_filter(&ctx2, None, Some("DepartmentID"));
        assert_eq!(f.to_sql().as_deref(), Some("DepartmentID IN (5,6,7)"));
        // 空集合：恒假
        let ctx3 = DataScopeContext { accessible_department_ids: Some(vec![]), ..ctx.clone() };
        let f = build_scope_filter(&ctx3, Some("CreateUserID"), Some("DepartmentID"));
        assert_eq!(f.to_sql().as_deref(), Some("(DepartmentID = -1 OR CreateUserID = 7)"));
        // 仅本人：按用户列
        let ctx4 = DataScopeContext { data_scope: DataScope::SelfOnly, ..ctx.clone() };
        let f = build_scope_filter(&ctx4, Some("CreateUserID"), Some("DepartmentID"));
        assert_eq!(f.to_sql().as_deref(), Some("CreateUserID = 7"));
        // 全部：不过滤
        let ctx5 = DataScopeContext { data_scope: DataScope::All, ..ctx.clone() };
        assert!(build_scope_filter(&ctx5, Some("CreateUserID"), Some("DepartmentID")).is_none());
        // 纯部门实体：仅本人退化为"当前用户所在部门"
        let f = build_department_scope_filter(&ctx4, Some("ID"));
        assert_eq!(f.to_sql().as_deref(), Some("ID = 5"));
    }

    #[test]
    fn can_access_checks() {
        let mut ctx = DataScopeContext {
            user_id: 7,
            department_id: 5,
            data_scope: DataScope::Department,
            accessible_department_ids: Some(vec![5]),
            is_system: false,
        };
        assert!(can_access_scope_row(&ctx, 7, 99)); // 本人始终可见
        assert!(can_access_scope_row(&ctx, 1, 5)); // 部门命中
        assert!(!can_access_scope_row(&ctx, 1, 6)); // 部门未命中
        assert!(can_access_user_row(&ctx, 7));
        assert!(!can_access_user_row(&ctx, 8));
        assert!(can_access_department_row(&ctx, 5));
        assert!(!can_access_department_row(&ctx, 6));

        // 仅本人：部门实体退化为所在部门；用户实体按用户
        ctx.data_scope = DataScope::SelfOnly;
        assert!(can_access_department_row(&ctx, 5));
        assert!(!can_access_department_row(&ctx, 6));
        assert!(can_access_user_row(&ctx, 7));
        assert!(!can_access_user_row(&ctx, 8));

        // 菜单级覆盖：菜单数据范围 >=0 时重算部门集合
        ctx.data_scope = DataScope::All;
        ctx.accessible_department_ids = None;
        let roles = [role(DataScope::Department, false, &[])];
        ctx.apply_menu_scope(1, &roles, &[6, 7], &[]);
        assert_eq!(ctx.data_scope, DataScope::DepartmentAndBelow);
        assert_eq!(ctx.accessible_department_ids, Some(vec![6, 7]));
        // 菜单数据范围 -1：不覆盖
        ctx.apply_menu_scope(-1, &roles, &[], &[]);
        assert_eq!(ctx.data_scope, DataScope::DepartmentAndBelow);
    }
}
