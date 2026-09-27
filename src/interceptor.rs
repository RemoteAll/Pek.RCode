//! 实体拦截器（对应 DH.NCode 的 `EntityInterceptor` / `TimeModule` / `UserModule` / `TraceModule`）。
//!
//! 拦截器在**写入前**自动补全审计字段（按字段存在性自动匹配，对应 `OnInit`）：
//! - [`TimeInterceptor`]：插入填 `CreateTime`/`UpdateTime`，更新刷新 `UpdateTime`
//! - [`UserInterceptor`]：插入填 `CreateUser`/`CreateUserID`/`UpdateUser`/`UpdateUserID`，更新刷新 `UpdateUser*`
//! - [`TraceInterceptor`]：填 `TraceId`（可合并多个，对应 `AllowMerge`）
//!
//! 使用方式：调用 [`enable_defaults`] 一键注册默认三件套，或 [`register`] 注册自定义拦截器；
//! 写入路径（[`crate::dal::TableRef`]）会自动应用已注册的拦截器。
//!
//! 取值语义（对齐 C# 的 `SetItem` / `SetNoDirtyItem`）：
//! - 插入（`SetItem`）：仅当显式传入的值为“默认值”（整数 0 / 空串 / Null / 年份 < 2000）时才覆盖；未传入的列补全
//! - 更新（`SetNoDirtyItem`）：调用方显式传入的列**不覆盖**；未传入的列补全
//! - 与 C# 的差异：C# 用实体“脏标记”判断是否覆盖；Rust 以调用方显式传入的列集合近似（语义等价）

use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Datelike, Local};

use crate::model::TableMeta;
use crate::session::DbRow;
use crate::types::DataType;
use crate::value::DbValue;

/// 数据操作方法（对应 `DataMethod`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataMethod {
    /// 插入
    Insert,
    /// 更新
    Update,
    /// 删除
    Delete,
}

/// 实体拦截器。
pub trait EntityInterceptor: Send + Sync {
    /// 是否应用于该表（对应 `OnInit`；默认全部应用）。
    fn on_init(&self, table: &TableMeta) -> bool {
        let _ = table;
        true
    }

    /// 数据验证/填充（对应 `OnValid`；写入前调用，可增改列值）。
    fn on_valid(&self, table: &TableMeta, method: DataMethod, values: &mut Vec<(String, DbValue)>) {
        let _ = (table, method, values);
    }

    /// 是否允许访问该行（对应 `OnFilter`；默认允许）。
    fn on_filter(&self, row: &DbRow) -> bool {
        let _ = row;
        true
    }
}

// ============================ 全局注册表 ============================

/// 全局拦截器注册表。
fn registry() -> &'static Mutex<Vec<Arc<dyn EntityInterceptor>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Arc<dyn EntityInterceptor>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 获取互斥锁（被投毒时恢复内部数据）。
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 注册拦截器（写入路径自动应用）。
pub fn register(interceptor: Arc<dyn EntityInterceptor>) {
    lock(registry()).push(interceptor);
}

/// 注册默认拦截器三件套（Time + User + Trace，与 C# 实体工厂的默认装配一致）。
pub fn enable_defaults() {
    register(Arc::new(TimeInterceptor));
    register(Arc::new(UserInterceptor::new()));
    register(Arc::new(TraceInterceptor::new()));
}

/// 清空注册（测试与动态装配场景使用）。
pub fn clear() {
    lock(registry()).clear();
}

/// 注册表快照。
fn snapshot() -> Vec<Arc<dyn EntityInterceptor>> {
    lock(registry()).clone()
}

/// 应用指定拦截器（公开以便组合/测试，不依赖全局注册表）。
pub fn apply_to(
    interceptors: &[Arc<dyn EntityInterceptor>],
    table: &TableMeta,
    method: DataMethod,
    values: &mut Vec<(String, DbValue)>,
) {
    for interceptor in interceptors {
        if interceptor.on_init(table) {
            interceptor.on_valid(table, method, values);
        }
    }
}

/// 应用全局注册的拦截器（写入路径调用；注册表为空时零开销）。
pub(crate) fn apply_registered(
    table: &TableMeta,
    method: DataMethod,
    values: &mut Vec<(String, DbValue)>,
) {
    let interceptors = snapshot();
    if interceptors.is_empty() {
        return;
    }
    apply_to(&interceptors, table, method, values);
}

/// 把 `&[(&str, DbValue)]` 转为 owned 列集合，并应用全局拦截器（写入路径使用）。
pub(crate) fn prepare(
    table: &TableMeta,
    method: DataMethod,
    fields: &[(&str, DbValue)],
) -> Vec<(String, DbValue)> {
    let mut values: Vec<(String, DbValue)> = fields
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect();
    apply_registered(table, method, &mut values);
    values
}

// ============================ 取值辅助（SetItem / SetNoDirtyItem） ============================

/// 插入场景：值为“默认值”或未传入时覆盖（对应 `SetItem`；表中不存在该列时跳过，与 C# 一致）。
fn set_item(table: &TableMeta, values: &mut Vec<(String, DbValue)>, name: &str, value: DbValue) {
    if table.column(name).is_none() {
        return;
    }
    match values.iter_mut().find(|(key, _)| key.eq_ignore_ascii_case(name)) {
        Some((_, existing)) => {
            let is_default = match existing {
                DbValue::Null => true,
                DbValue::Int(v) => *v == 0,
                DbValue::Text(v) => v.is_empty(),
                DbValue::DateTime(v) => v.year() < 2000,
                _ => false,
            };
            if is_default {
                *existing = value;
            }
        }
        None => values.push((name.to_string(), value)),
    }
}

/// 更新场景：调用方未显式传入时补全（对应 `SetNoDirtyItem`；表中不存在该列时跳过）。
fn set_no_dirty(table: &TableMeta, values: &mut Vec<(String, DbValue)>, name: &str, value: DbValue) {
    if table.column(name).is_none() {
        return;
    }
    if !values.iter().any(|(key, _)| key.eq_ignore_ascii_case(name)) {
        values.push((name.to_string(), value));
    }
}

// ============================ 时间拦截器 ============================

/// 时间拦截器（对应 `TimeInterceptor`）：插入填 `CreateTime`/`UpdateTime`，更新刷新 `UpdateTime`。
pub struct TimeInterceptor;

impl EntityInterceptor for TimeInterceptor {
    fn on_init(&self, table: &TableMeta) -> bool {
        table.columns.iter().any(|col| {
            col.data_type == DataType::DateTime
                && (col.name.eq_ignore_ascii_case("CreateTime")
                    || col.name.eq_ignore_ascii_case("UpdateTime"))
        })
    }

    fn on_valid(&self, table: &TableMeta, method: DataMethod, values: &mut Vec<(String, DbValue)>) {
        let now = Local::now().naive_local();
        match method {
            DataMethod::Insert => {
                set_item(table, values, "CreateTime", DbValue::DateTime(now));
                set_item(table, values, "UpdateTime", DbValue::DateTime(now));
            }
            DataMethod::Update => {
                set_no_dirty(table, values, "UpdateTime", DbValue::DateTime(now));
            }
            DataMethod::Delete => {}
        }
    }
}

// ============================ 用户拦截器 ============================

/// 当前用户提供者（对应 `IManageProvider`；由应用集成登录态）。
pub trait UserProvider: Send + Sync {
    /// 当前用户 ID（未登录返回 0）。
    fn current_id(&self) -> i64 {
        0
    }

    /// 当前用户显示名（未登录返回 `None`）。
    fn current_name(&self) -> Option<String> {
        None
    }
}

/// 用户拦截器（对应 `UserInterceptor`）：自动填充创建人/更新人。
pub struct UserInterceptor {
    /// 当前用户提供者
    provider: Option<Arc<dyn UserProvider>>,
    /// 无当前用户时是否允许清空更新人（对应 `AllowEmpty`）
    allow_empty: bool,
}

impl UserInterceptor {
    /// 创建（使用环境变量兜底当前用户）。
    pub fn new() -> Self {
        Self {
            provider: None,
            allow_empty: false,
        }
    }

    /// 创建并指定用户提供者。
    pub fn with_provider(provider: Arc<dyn UserProvider>) -> Self {
        Self {
            provider: Some(provider),
            allow_empty: false,
        }
    }

    /// 设置用户提供者。
    pub fn set_provider(&mut self, provider: Arc<dyn UserProvider>) {
        self.provider = Some(provider);
    }

    /// 设置“无当前用户时允许清空更新人”。
    pub fn set_allow_empty(&mut self, allow: bool) {
        self.allow_empty = allow;
    }

    /// 解析当前用户：`Provider` 优先，插入时用环境变量兜底（对应 C# `Environment.UserName` → `MachineName`）。
    fn resolve_user(&self, method: DataMethod) -> Option<(i64, String)> {
        if let Some(provider) = &self.provider
            && let Some(name) = provider.current_name()
        {
            return Some((provider.current_id(), name));
        }
        if method != DataMethod::Insert {
            return None;
        }
        let mut name = std::env::var("USERNAME").unwrap_or_default();
        if name.is_empty() || name.eq_ignore_ascii_case("root") || name.eq_ignore_ascii_case("Administrator") {
            name = std::env::var("COMPUTERNAME").unwrap_or_default();
        }
        if name.is_empty() { None } else { Some((0, name)) }
    }
}

impl Default for UserInterceptor {
    fn default() -> Self {
        Self::new()
    }
}

impl EntityInterceptor for UserInterceptor {
    fn on_init(&self, table: &TableMeta) -> bool {
        table.columns.iter().any(|col| {
            (col.data_type == DataType::Int32 || col.data_type == DataType::Int64)
                && (col.name.eq_ignore_ascii_case("CreateUserID")
                    || col.name.eq_ignore_ascii_case("UpdateUserID"))
        }) || table.columns.iter().any(|col| {
            col.data_type == DataType::String
                && (col.name.eq_ignore_ascii_case("CreateUser")
                    || col.name.eq_ignore_ascii_case("UpdateUser"))
        })
    }

    fn on_valid(&self, table: &TableMeta, method: DataMethod, values: &mut Vec<(String, DbValue)>) {
        match self.resolve_user(method) {
            Some((id, name)) => match method {
                DataMethod::Insert => {
                    set_item(table, values, "CreateUserID", DbValue::Int(id));
                    set_item(table, values, "CreateUser", DbValue::Text(name.clone()));
                    set_item(table, values, "UpdateUserID", DbValue::Int(id));
                    set_item(table, values, "UpdateUser", DbValue::Text(name));
                }
                DataMethod::Update => {
                    set_no_dirty(table, values, "UpdateUserID", DbValue::Int(id));
                    set_no_dirty(table, values, "UpdateUser", DbValue::Text(name));
                }
                DataMethod::Delete => {}
            },
            None => {
                if self.allow_empty && method == DataMethod::Update {
                    set_no_dirty(table, values, "UpdateUserID", DbValue::Int(0));
                    set_no_dirty(table, values, "UpdateUser", DbValue::Text(String::new()));
                }
            }
        }
    }
}

// ============================ 追踪拦截器 ============================

/// 链路追踪上下文（替代 C# 的 `DefaultSpan.Current`；由请求入口（如 Web 中间件）设置）。
pub struct TraceContext;

fn trace_slot() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

impl TraceContext {
    /// 设置当前 TraceId（None 表示清除）。
    pub fn set_current(trace_id: Option<String>) {
        *lock(trace_slot()) = trace_id;
    }

    /// 当前 TraceId。
    pub fn current() -> Option<String> {
        lock(trace_slot()).clone()
    }
}

/// 追踪拦截器（对应 `TraceInterceptor`）：自动给 `TraceId` 赋值。
pub struct TraceInterceptor {
    /// 允许合并多个 TraceId（逗号连接，裁剪到列长度）
    allow_merge: bool,
}

impl TraceInterceptor {
    /// 创建（默认不合并）。
    pub fn new() -> Self {
        Self { allow_merge: false }
    }

    /// 设置是否允许合并。
    pub fn set_allow_merge(&mut self, allow: bool) {
        self.allow_merge = allow;
    }
}

impl Default for TraceInterceptor {
    fn default() -> Self {
        Self::new()
    }
}

impl EntityInterceptor for TraceInterceptor {
    fn on_init(&self, table: &TableMeta) -> bool {
        table.columns.iter().any(|col| {
            col.data_type == DataType::String && col.name.eq_ignore_ascii_case("TraceId")
        })
    }

    fn on_valid(&self, table: &TableMeta, method: DataMethod, values: &mut Vec<(String, DbValue)>) {
        if method == DataMethod::Delete {
            return;
        }
        let Some(trace_id) = TraceContext::current() else {
            return;
        };
        // 与 C# 一致：超长（>=50）不处理
        if trace_id.len() >= 50 || trace_id.is_empty() {
            return;
        }

        let max_length = table
            .column("TraceId")
            .map(|col| if col.length > 0 { col.length as usize } else { 50 })
            .unwrap_or(50);

        let value = if self.allow_merge {
            let existing = values
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("TraceId"))
                .and_then(|(_, v)| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let mut parts: Vec<String> = existing
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if !parts.iter().any(|s| s == &trace_id) {
                parts.push(trace_id.clone());
            }
            let mut joined = parts.join(",");
            while joined.len() > max_length && parts.len() > 1 {
                parts.remove(0);
                joined = parts.join(",");
            }
            joined
        } else {
            trace_id
        };

        if self.allow_merge {
            // 合并模式：TraceId 由拦截器全权维护（与传入的旧值合并后强制覆盖）
            match values
                .iter_mut()
                .find(|(key, _)| key.eq_ignore_ascii_case("TraceId"))
            {
                Some((_, existing)) => *existing = DbValue::Text(value),
                None => values.push(("TraceId".to_string(), DbValue::Text(value))),
            }
        } else {
            set_no_dirty(table, values, "TraceId", DbValue::Text(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
        <Column Name="CreateUser" DataType="String" />
        <Column Name="CreateUserID" DataType="Int32" />
        <Column Name="UpdateUser" DataType="String" />
        <Column Name="UpdateUserID" DataType="Int32" />
        <Column Name="CreateTime" DataType="DateTime" />
        <Column Name="UpdateTime" DataType="DateTime" />
        <Column Name="TraceId" DataType="String" Length="20" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    fn table() -> crate::model::TableMeta {
        EntityModel::parse(MODEL).unwrap().tables.remove(0)
    }

    struct FixedUser;

    impl UserProvider for FixedUser {
        fn current_id(&self) -> i64 {
            7
        }

        fn current_name(&self) -> Option<String> {
            Some("tester".to_string())
        }
    }

    #[test]
    fn time_interceptor_fills_and_respects_explicit_values() {
        let table = table();
        let interceptors: Vec<Arc<dyn EntityInterceptor>> = vec![Arc::new(TimeInterceptor)];

        // 插入：未传 → 补全
        let mut values: Vec<(String, DbValue)> = vec![("Code".into(), DbValue::Text("A".into()))];
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        assert!(values.iter().any(|(k, _)| k == "CreateTime"));
        assert!(values.iter().any(|(k, _)| k == "UpdateTime"));

        // 插入：显式传具体时间（>=2000）→ 保留
        let explicit = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let mut values: Vec<(String, DbValue)> =
            vec![("CreateTime".into(), DbValue::DateTime(explicit))];
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        assert_eq!(
            values
                .iter()
                .find(|(k, _)| k == "CreateTime")
                .map(|(_, v)| v.clone()),
            Some(DbValue::DateTime(explicit)),
            "显式时间应保留"
        );

        // 插入：默认值时间（0001）→ 覆盖
        let default_time = chrono::NaiveDate::from_ymd_opt(1, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let mut values: Vec<(String, DbValue)> =
            vec![("CreateTime".into(), DbValue::DateTime(default_time))];
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        assert_ne!(
            values
                .iter()
                .find(|(k, _)| k == "CreateTime")
                .map(|(_, v)| v.clone()),
            Some(DbValue::DateTime(default_time)),
            "默认时间应被覆盖"
        );

        // 更新：刷新 UpdateTime；显式 UpdateTime 不覆盖
        let mut values: Vec<(String, DbValue)> = vec![("Code".into(), DbValue::Text("B".into()))];
        apply_to(&interceptors, &table, DataMethod::Update, &mut values);
        assert!(values.iter().any(|(k, _)| k == "UpdateTime"));

        let explicit_update = chrono::NaiveDate::from_ymd_opt(2026, 5, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let mut values: Vec<(String, DbValue)> =
            vec![("UpdateTime".into(), DbValue::DateTime(explicit_update))];
        apply_to(&interceptors, &table, DataMethod::Update, &mut values);
        assert_eq!(
            values
                .iter()
                .find(|(k, _)| k == "UpdateTime")
                .map(|(_, v)| v.clone()),
            Some(DbValue::DateTime(explicit_update)),
            "显式 UpdateTime 应保留"
        );
    }

    #[test]
    fn user_interceptor_fills_current_user() {
        let table = table();
        let interceptors: Vec<Arc<dyn EntityInterceptor>> =
            vec![Arc::new(UserInterceptor::with_provider(Arc::new(FixedUser)))];

        let mut values: Vec<(String, DbValue)> = Vec::new();
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        let find = |name: &str, values: &Vec<(String, DbValue)>| {
            values
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(find("CreateUserID", &values), Some(DbValue::Int(7)));
        assert_eq!(
            find("CreateUser", &values),
            Some(DbValue::Text("tester".into()))
        );
        assert_eq!(find("UpdateUserID", &values), Some(DbValue::Int(7)));
        assert_eq!(
            find("UpdateUser", &values),
            Some(DbValue::Text("tester".into()))
        );

        // 显式 CreateUser 非空 → 保留
        let mut values: Vec<(String, DbValue)> =
            vec![("CreateUser".into(), DbValue::Text("custom".into()))];
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        assert_eq!(
            find("CreateUser", &values),
            Some(DbValue::Text("custom".into())),
            "显式创建人应保留"
        );

        // 更新：刷新 UpdateUser（显式传入则不覆盖）
        let mut values: Vec<(String, DbValue)> = Vec::new();
        apply_to(&interceptors, &table, DataMethod::Update, &mut values);
        assert_eq!(
            find("UpdateUser", &values),
            Some(DbValue::Text("tester".into()))
        );
        let mut values: Vec<(String, DbValue)> =
            vec![("UpdateUser".into(), DbValue::Text("z".into()))];
        apply_to(&interceptors, &table, DataMethod::Update, &mut values);
        assert_eq!(find("UpdateUser", &values), Some(DbValue::Text("z".into())));
    }

    #[test]
    fn trace_interceptor_sets_and_merges() {
        let table = table();
        TraceContext::set_current(Some("t-001".into()));

        let mut interceptor = TraceInterceptor::new();
        let interceptors: Vec<Arc<dyn EntityInterceptor>> = vec![Arc::new(TraceInterceptor::new())];
        let mut values: Vec<(String, DbValue)> = Vec::new();
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        assert_eq!(
            values
                .iter()
                .find(|(k, _)| k == "TraceId")
                .map(|(_, v)| v.to_text()),
            Some("t-001".to_string())
        );

        // 合并模式：与旧值合并，并裁剪到列长度（20）
        interceptor.set_allow_merge(true);
        let merged: Vec<Arc<dyn EntityInterceptor>> = vec![Arc::new(interceptor)];
        TraceContext::set_current(Some("t-002".into()));
        let mut values: Vec<(String, DbValue)> =
            vec![("TraceId".into(), DbValue::Text("t-001".into()))];
        apply_to(&merged, &table, DataMethod::Update, &mut values);
        let text = values
            .iter()
            .find(|(k, _)| k == "TraceId")
            .map(|(_, v)| v.to_text())
            .unwrap();
        assert_eq!(text, "t-001,t-002");
        assert!(text.len() <= 20);

        TraceContext::set_current(None);
        let mut values: Vec<(String, DbValue)> = Vec::new();
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        assert!(!values.iter().any(|(k, _)| k == "TraceId"), "无 TraceId 时不填");
    }

    #[test]
    fn skips_missing_columns() {
        // 简化表：只有 CreateTime（无 UpdateTime、无用户列、无 TraceId）
        let table = EntityModel::parse(
            r#"<EntityModel><Tables><Table Name="Log" TableName="DH_Log">
              <Columns>
                <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
                <Column Name="CreateTime" DataType="DateTime" />
              </Columns>
            </Table></Tables></EntityModel>"#,
        )
        .unwrap()
        .tables
        .remove(0);

        let interceptors: Vec<Arc<dyn EntityInterceptor>> = vec![
            Arc::new(TimeInterceptor),
            Arc::new(UserInterceptor::with_provider(Arc::new(FixedUser))),
        ];
        let mut values: Vec<(String, DbValue)> = Vec::new();
        apply_to(&interceptors, &table, DataMethod::Insert, &mut values);
        assert!(values.iter().any(|(k, _)| k == "CreateTime"));
        assert!(
            !values.iter().any(|(k, _)| k == "UpdateTime"),
            "缺少的列不应添加：{values:?}"
        );
        assert!(
            !values.iter().any(|(k, _)| k == "CreateUser"),
            "缺少的列不应添加：{values:?}"
        );
    }

    #[test]
    fn on_init_matches_by_columns() {
        let table = table();
        assert!(TimeInterceptor.on_init(&table));
        assert!(UserInterceptor::new().on_init(&table));
        assert!(TraceInterceptor::new().on_init(&table));

        let empty = EntityModel::parse(
            r#"<EntityModel><Tables><Table Name="T" TableName="DH_T">
              <Columns><Column Name="Id" DataType="Int32" PrimaryKey="True" /></Columns>
            </Table></Tables></EntityModel>"#,
        )
        .unwrap()
        .tables
        .remove(0);
        assert!(!TimeInterceptor.on_init(&empty));
        assert!(!UserInterceptor::new().on_init(&empty));
        assert!(!TraceInterceptor::new().on_init(&empty));
    }
}
