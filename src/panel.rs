//! 面板用户与操作审计（管理系统的通用组装线）。
//!
//! 提炼自 Pek.RAgent / HlkProductTool 两处逐字同构实现（2026-10-05）——
//! 使“涉及管理功能的系统都有用户与审计”与 [`crate::db_admin`]（数据库管理）一样成为一条组装线：
//!
//! - **多用户**：`PanelUser` 实体（用户名 / 密码哈希+盐 / 菜单权限 / 启用 / 备注）；
//!   内置管理员（配置文件凭据，超级权限）不落表，由应用层处理；
//! - **审计**：`OperationLog` 实体记录操作（动作 / 路径 / 参数摘要 / 结果 / 耗时），
//!   参数摘要经敏感字段脱敏（`password`/`secret`/`token`/`apikey` → `***`）；
//! - **权限**：以“权限 key 列表”字符串存储；[`PanelAuth`] 持有 key→中文名映射，
//!   负责规范化（去重 / 过滤未知项 / 保持展示顺序）与用户 JSON 视图。
//!
//! 约定：
//! - 实体名固定为 [`TABLE_USER`]（PanelUser）与 [`TABLE_OPLOG`]（OperationLog），
//!   物理表名由应用 `Model.xml` 自定（如 `Agent_PanelUser`、`Hlk_PanelUser`）；
//! - `OperationLog.Category` 列为**可选**：[`AuditEntry::category`] 为 `None` 时不写该列
//!   （兼容不含该列的既有表）；查询时 category 过滤在内存完成，两种表通用；
//! - 数据访问由调用方传入 [`SharedStore`]（`store::get_or_open` 打开的共享库），
//!   本模块不持有全局注册表；
//! - 密码存储：`SHA-256(salt:password)` hex（每用户随机盐，不可逆；对齐“凭据不明文落盘”）。
//!
//! 用法（示例）：
//!
//! ```no_run
//! use pek_rcode::panel::{AuditEntry, PanelAuth};
//! use pek_rcode::store::SharedStore;
//!
//! # fn demo(store: &SharedStore) -> Result<(), String> {
//! let auth = PanelAuth::new(&[("records", "记录查询"), ("settings", "配置修改")]);
//! let user = auth.verify_login(store, "alice", "pw123")?;
//! let view = auth.list_users_json(store)?;
//! pek_rcode::panel::record(
//!     store,
//!     &AuditEntry {
//!         category: Some("panel".into()),
//!         user: "alice".into(),
//!         action: "userSave".into(),
//!         ..Default::default()
//!     },
//! );
//! # let _ = (user, view);
//! # Ok(())
//! # }
//! ```

use chrono::{Local, NaiveDateTime};
use serde_json::{json, Value as Json};

use crate::session::DbRow;
use crate::store::SharedStore;
use crate::value::DbValue;
use crate::{Query, Where};

/// 用户表（实体名，见各应用 `Entity/Model.xml`）。
pub const TABLE_USER: &str = "PanelUser";
/// 操作日志表（实体名）。
pub const TABLE_OPLOG: &str = "OperationLog";

// ————— 权限 —————

/// 面板权限表（key → 中文名；顺序即展示顺序）。
///
/// `users`（用户管理）等超级权限不在授予范围：仅内置管理员可访问，由应用层保证。
#[derive(Clone, Debug)]
pub struct PanelAuth {
    perms: Vec<(String, String)>,
}

impl PanelAuth {
    /// 构建（`perms` 为 `(key, 中文名)` 列表，顺序即展示顺序）。
    pub fn new(perms: &[(&str, &str)]) -> Self {
        Self {
            perms: perms
                .iter()
                .map(|(k, n)| (k.to_string(), n.to_string()))
                .collect(),
        }
    }

    /// 全部权限定义（面板「用户」页渲染用）：`[{key,name}]`。
    pub fn all_permissions_json(&self) -> Vec<Json> {
        self.perms
            .iter()
            .map(|(k, n)| json!({ "key": k, "name": n }))
            .collect()
    }

    /// 权限 key → 中文名（未知回退 key 本身）。
    pub fn permission_name(&self, key: &str) -> String {
        self.perms
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| key.to_string())
    }

    /// 规范化权限列表（去重、去空白、过滤未知项，保持定义顺序）。
    pub fn normalize(&self, list: &[String]) -> Vec<String> {
        self.perms
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| list.iter().any(|p| p.trim() == k))
            .collect()
    }

    /// 列出全部用户（JSON 视图，含权限中文名）+ 可选权限定义。
    pub fn list_users_json(&self, store: &SharedStore) -> Result<Json, String> {
        let users = self.list_raw(store)?;
        let items: Vec<Json> = users
            .iter()
            .map(|u| {
                let mut view = u.to_json();
                let names: Vec<String> = u
                    .permissions
                    .iter()
                    .map(|k| self.permission_name(k))
                    .collect();
                if let Some(obj) = view.as_object_mut() {
                    obj.insert("permissionNames".to_string(), json!(names));
                }
                view
            })
            .collect();
        Ok(json!({
            "users": items,
            "permissions": self.all_permissions_json(),
        }))
    }

    /// 列出全部用户（内部结构，按 Id 升序）。
    fn list_raw(&self, store: &SharedStore) -> Result<Vec<PanelUser>, String> {
        store.with_session(|dal, session| {
            let table = dal.table(TABLE_USER)?;
            let rows = table.query(session, &Query::new().order_by("Id", false))?;
            Ok(rows.rows.iter().filter_map(row_to_user).collect())
        })
    }

    /// 按用户名查找（大小写不敏感：与内置管理员登录判定一致）。
    pub fn find_user(
        &self,
        store: &SharedStore,
        user_name: &str,
    ) -> Result<Option<PanelUser>, String> {
        let name = user_name.trim().to_string();
        if name.is_empty() {
            return Ok(None);
        }
        let all = self.list_raw(store)?;
        Ok(all
            .into_iter()
            .find(|u| u.user_name.eq_ignore_ascii_case(&name)))
    }

    /// 登录校验：返回启用且密码正确的用户。
    pub fn verify_login(
        &self,
        store: &SharedStore,
        user_name: &str,
        password: &str,
    ) -> Result<Option<PanelUser>, String> {
        let Some(user) = self.find_user(store, user_name)? else {
            return Ok(None);
        };
        if !user.enabled {
            return Ok(None);
        }
        if user.verify(password) {
            Ok(Some(user))
        } else {
            Ok(None)
        }
    }

    /// 保存用户（不存在则创建，要求 `password` 非空；存在则更新，`password` 为空表示不改）。
    pub fn save_user(
        &self,
        store: &SharedStore,
        user_name: &str,
        password: Option<&str>,
        permissions: &[String],
        enabled: bool,
        remark: &str,
    ) -> Result<(), String> {
        let name = user_name.trim();
        if name.is_empty() || name.len() > 50 {
            return Err("用户名不能为空且不超过 50 字符".to_string());
        }
        if name.contains(',') || name.contains(char::is_whitespace) {
            return Err("用户名不能包含逗号或空白字符".to_string());
        }
        let perms = self.normalize(permissions);
        let password = password.unwrap_or("").trim().to_string();

        let existing = self.find_user(store, name)?;
        match existing {
            Some(user) => store.with_session(|dal, session| {
                let table = dal.table(TABLE_USER)?;
                let mut fields: Vec<(&str, DbValue)> = vec![
                    ("Permissions", perms.join(",").into()),
                    ("Enabled", enabled.into()),
                    ("Remark", remark.trim().into()),
                ];
                if !password.is_empty() {
                    let salt = new_salt();
                    let hash = hash_password(&salt, &password);
                    fields.push(("Salt", salt.into()));
                    fields.push(("PasswordHash", hash.into()));
                }
                table.update_by_pk(session, &fields, &[user.id.into()])?;
                Ok(())
            }),
            None => {
                if password.is_empty() {
                    return Err("新用户必须设置密码".to_string());
                }
                let salt = new_salt();
                let hash = hash_password(&salt, &password);
                store.with_session(|dal, session| {
                    let table = dal.table(TABLE_USER)?;
                    table.insert(
                        session,
                        &[
                            ("UserName", name.into()),
                            ("PasswordHash", hash.as_str().into()),
                            ("Salt", salt.as_str().into()),
                            ("Permissions", perms.join(",").as_str().into()),
                            ("Enabled", enabled.into()),
                            ("Remark", remark.trim().into()),
                        ],
                    )?;
                    Ok(())
                })
            }
        }
    }

    /// 修改用户密码（校验旧密码；供用户自助改密）。
    pub fn change_user_password(
        &self,
        store: &SharedStore,
        user_name: &str,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), String> {
        if new_password.is_empty() {
            return Err("新密码不能为空".to_string());
        }
        let Some(user) = self.find_user(store, user_name)? else {
            return Err("用户不存在".to_string());
        };
        if !user.verify(old_password) {
            return Err("旧密码不正确".to_string());
        }
        let salt = new_salt();
        let hash = hash_password(&salt, new_password);
        store.with_session(|dal, session| {
            let table = dal.table(TABLE_USER)?;
            table.update_by_pk(
                session,
                &[
                    ("Salt", salt.as_str().into()),
                    ("PasswordHash", hash.as_str().into()),
                ],
                &[user.id.into()],
            )?;
            Ok(())
        })
    }

    /// 删除用户。
    pub fn delete_user(&self, store: &SharedStore, user_name: &str) -> Result<(), String> {
        let Some(user) = self.find_user(store, user_name)? else {
            return Err("用户不存在".to_string());
        };
        store.with_session(|dal, session| {
            let table = dal.table(TABLE_USER)?;
            table.delete_by_pk(session, &[user.id.into()])?;
            Ok(())
        })
    }
}

// ————— 密码 —————

/// 生成随机盐（32 位十六进制）。
fn new_salt() -> String {
    dhrust::random::hex(16)
}

/// 计算密码哈希：`SHA-256(salt:password)` hex（口径统一在 `dhrust::sign::salted_sha256_hex`）。
fn hash_password(salt: &str, password: &str) -> String {
    dhrust::sign::salted_sha256_hex(salt, password)
}

// ————— 用户结构 —————

/// 面板用户（不含哈希细节的对外视图；登录校验用内部结构）。
#[derive(Clone, Debug)]
pub struct PanelUser {
    /// 编号
    pub id: i64,
    /// 用户名
    pub user_name: String,
    /// 密码哈希（hex）
    pub password_hash: String,
    /// 随机盐
    pub salt: String,
    /// 菜单权限 key 列表
    pub permissions: Vec<String>,
    /// 是否启用
    pub enabled: bool,
    /// 备注
    pub remark: String,
}

impl PanelUser {
    /// 用户 JSON 视图（面板用；不含密码字段）。
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "userName": self.user_name,
            "permissions": self.permissions,
            "enabled": self.enabled,
            "remark": self.remark,
        })
    }

    /// 校验密码。
    pub fn verify(&self, password: &str) -> bool {
        !self.password_hash.is_empty() && self.password_hash == hash_password(&self.salt, password)
    }
}

/// 行 → 用户。
fn row_to_user(row: &DbRow) -> Option<PanelUser> {
    let user_name = row.get_by_name("UserName")?.as_str()?.to_string();
    let text = |name: &str| {
        row.get_by_name(name)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    Some(PanelUser {
        id: row
            .get_by_name("Id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default(),
        user_name,
        password_hash: text("PasswordHash"),
        salt: text("Salt"),
        permissions: text("Permissions")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        enabled: row
            .get_by_name("Enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        remark: text("Remark"),
    })
}

// ————— 操作审计 —————

/// 审计记录（写入用）。
#[derive(Default)]
pub struct AuditEntry {
    /// 类别（`Some("panel")` 面板操作 / `Some("api")` 接口调用；`None` = 表无 `Category` 列时不写）
    pub category: Option<String>,
    /// 操作者
    pub user: String,
    /// 来源 IP
    pub ip: String,
    /// 动作（接口名）
    pub action: String,
    /// 动作中文名
    pub title: String,
    /// HTTP 方法
    pub method: String,
    /// 请求路径
    pub path: String,
    /// 参数摘要（已脱敏）
    pub detail: String,
    /// 是否成功
    pub success: bool,
    /// 结果码
    pub code: i32,
    /// 结果消息
    pub message: String,
    /// 耗时毫秒
    pub elapsed_ms: i64,
}

/// 写入一条审计记录（失败只记程序日志，不影响业务）。
pub fn record(store: &SharedStore, entry: &AuditEntry) {
    let time: NaiveDateTime = Local::now().naive_local();
    let result = store.with_session(|dal, session| {
        let table = dal.table(TABLE_OPLOG)?;
        let mut fields: Vec<(&str, DbValue)> = Vec::with_capacity(13);
        if let Some(category) = &entry.category {
            fields.push(("Category", category.as_str().into()));
        }
        fields.push(("LogTime", time.into()));
        fields.push(("UserName", entry.user.as_str().into()));
        fields.push(("Ip", entry.ip.as_str().into()));
        fields.push(("Action", entry.action.as_str().into()));
        fields.push(("Title", entry.title.as_str().into()));
        fields.push(("Method", entry.method.as_str().into()));
        fields.push(("Path", entry.path.as_str().into()));
        fields.push(("Detail", entry.detail.as_str().into()));
        fields.push(("Success", entry.success.into()));
        fields.push(("Code", entry.code.into()));
        fields.push(("Message", entry.message.as_str().into()));
        fields.push(("ElapsedMs", entry.elapsed_ms.into()));
        table.insert(session, &fields)?;
        Ok(())
    });
    if let Err(e) = result {
        dhrust::logs::log().error(&format!("操作日志写入失败：{e}"));
    }
}

/// 分页查询操作日志（按时间倒序）。
///
/// - `category`：按类别过滤（`panel`/`api`；空 = 全部；内存过滤——兼容无 `Category` 列的表）；
/// - `user`：按操作者精确过滤（空 = 全部）；
/// - `keyword`：对 动作/中文名/路径/消息/详情/操作者 做模糊匹配（不区分大小写）；
/// - `success`：`Some(true/false)` 过滤成功/失败。
///
/// 实现：SQL 侧先按 `user`/`success` 过滤（受 `MAX_SCAN` 上限保护），类别/关键词命中与分页
/// 在内存完成——面板日志量级（万级以内）足够；超出上限时以 `truncated` 标记提示。
pub fn query_logs(
    store: &SharedStore,
    page: usize,
    size: usize,
    category: &str,
    user: &str,
    keyword: &str,
    success: Option<bool>,
) -> Result<Json, String> {
    /// 单次扫描上限（防御；面板操作频率低，万级内足够）。
    const MAX_SCAN: usize = 20_000;

    let page = page.max(1);
    let size = size.clamp(1, 200);
    let user = user.trim().to_string();
    let category = category.trim().to_lowercase();
    let keyword = keyword.trim().to_lowercase();

    store.with_session(|dal, session| {
        let table = dal.table(TABLE_OPLOG)?;
        let mut w = Where::new();
        let mut has_filter = false;
        if !user.is_empty() {
            w = w.eq("UserName", user.as_str());
            has_filter = true;
        }
        if let Some(ok) = success {
            w = w.eq("Success", ok);
            has_filter = true;
        }
        let mut query = Query::new()
            .order_by("LogTime", true)
            .order_by("Id", true)
            .take(MAX_SCAN + 1);
        if has_filter {
            query = query.filter(w);
        }
        let rows = table.query(session, &query)?;
        let truncated = rows.len() > MAX_SCAN;

        let mut items: Vec<Json> = Vec::new();
        for row in rows.rows.iter().take(MAX_SCAN) {
            let text = |name: &str| {
                row.get_by_name(name)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let time = row
                .get_by_name("LogTime")
                .and_then(|v| v.as_datetime())
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            let row_category = text("Category");
            let action = text("Action");
            let title = text("Title");
            let path = text("Path");
            let message = text("Message");
            let detail = text("Detail");
            let user_name = text("UserName");
            if !category.is_empty() && row_category.to_lowercase() != category {
                continue;
            }
            if !keyword.is_empty() {
                let haystack =
                    format!("{action} {title} {path} {message} {user_name} {detail}").to_lowercase();
                if !haystack.contains(&keyword) {
                    continue;
                }
            }
            items.push(json!({
                "id": row.get_by_name("Id").and_then(|v| v.as_i64()).unwrap_or_default(),
                "category": row_category,
                "time": time,
                "userName": user_name,
                "ip": text("Ip"),
                "action": action,
                "title": title,
                "method": text("Method"),
                "path": path,
                "detail": detail,
                "success": row.get_by_name("Success").and_then(|v| v.as_bool()).unwrap_or(false),
                "code": row.get_by_name("Code").and_then(|v| v.as_i64()).unwrap_or_default(),
                "message": message,
                "elapsedMs": row.get_by_name("ElapsedMs").and_then(|v| v.as_i64()).unwrap_or_default(),
            }));
        }

        let total = items.len();
        let start = page.saturating_sub(1) * size;
        let page_items: Vec<Json> = items.into_iter().skip(start).take(size).collect();
        Ok(json!({
            "total": total,
            "page": page,
            "size": size,
            "truncated": truncated,
            "items": page_items,
        }))
    })
}

// ————— 审计展示工具（纯函数，供应用层组装摘要/标题） —————

/// 路径最后一段（动作名，如 `/panel/userSave` → `userSave`）。
pub fn action_name_of(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// 动作中文名（`titles` 为应用的动作→中文名映射；未知动作回退为 `操作 {action}`）。
pub fn action_title(action: &str, titles: &[(&str, &str)]) -> String {
    titles
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, title)| (*title).to_string())
        .unwrap_or_else(|| format!("操作 {action}"))
}

/// 请求参数摘要：查询串 + JSON/表单体（敏感字段脱敏；截断至 400 字符）。
///
/// `content_type` 传请求头值（小写匹配 `application/json` / `x-www-form-urlencoded`）；
/// 其他类型只记 `body={n} 字节`（不落二进制内容）。
pub fn summarize_body(query: &str, content_type: &str, body: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !query.is_empty() {
        parts.push(redact_form(query));
    }
    if !body.is_empty() {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.contains("application/json") && body.len() <= 64 * 1024 {
            match serde_json::from_slice::<Json>(body) {
                Ok(mut v) => {
                    redact_json(&mut v);
                    parts.push(v.to_string());
                }
                Err(_) => parts.push(format!("body={} 字节", body.len())),
            }
        } else if content_type.contains("x-www-form-urlencoded") {
            parts.push(redact_form(&String::from_utf8_lossy(body)));
        } else {
            parts.push(format!("body={} 字节", body.len()));
        }
    }
    truncate_text(&parts.join(" | "), 400)
}

/// `k=v&k2=v2` 形式摘要素材的脱敏。
pub fn redact_form(text: &str) -> String {
    text.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_sensitive_key(k) => format!("{k}=***"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// JSON 体递归脱敏（键名含 password/secret/token/apikey 的值替换为 `***`）。
pub fn redact_json(v: &mut Json) {
    match v {
        Json::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_sensitive_key(k) {
                    *val = json!("***");
                } else {
                    redact_json(val);
                }
            }
        }
        Json::Array(items) => {
            for item in items {
                redact_json(item);
            }
        }
        _ => {}
    }
}

/// 是否敏感键（不区分大小写包含匹配；`apikey` 覆盖 AI 接口密钥等）。
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("password") || k.contains("secret") || k.contains("token") || k.contains("apikey")
}

/// 按字符截断（附省略号；不破坏 UTF-8 边界）。
pub fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::model::EntityModel;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// 权限表（测试用）。
    const PERMS: &[(&str, &str)] = &[
        ("records", "记录查询"),
        ("settings", "配置修改"),
        ("database", "数据库管理"),
    ];

    /// 测试模型（含 `Category` 列）。
    const MODEL_WITH_CATEGORY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<EntityModel xmlns:xs="http://www.w3.org/2001/XMLSchema-instance" xs:schemaLocation="https://newlifex.com https://newlifex.com/Model202509.xsd" Version="1.0" Document="https://newlifex.com/xcode/model" ModelVersion="2.0" xmlns="https://newlifex.com/Model202509.xsd">
  <Option>
    <BaseClass>EntityBase</BaseClass>
    <ConnName>Test</ConnName>
    <NameFormat>Default</NameFormat>
    <Nullable>True</Nullable>
    <HasIModel>False</HasIModel>
  </Option>
  <Tables>
    <Table Name="PanelUser" TableName="Test_PanelUser">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="UserName" DataType="String" Length="50" Master="True" Nullable="False" />
        <Column Name="PasswordHash" DataType="String" Length="128" Nullable="False" />
        <Column Name="Salt" DataType="String" Length="64" Nullable="False" />
        <Column Name="Permissions" DataType="String" Length="500" Nullable="False" />
        <Column Name="Enabled" DataType="Boolean" Nullable="False" />
        <Column Name="Remark" DataType="String" Length="200" />
        <Column Name="CreateTime" DataType="DateTime" />
        <Column Name="UpdateTime" DataType="DateTime" />
      </Columns>
    </Table>
    <Table Name="OperationLog" TableName="Test_OperationLog">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Category" DataType="String" Length="20" Nullable="False" />
        <Column Name="LogTime" DataType="DateTime" Nullable="False" />
        <Column Name="UserName" DataType="String" Length="50" Nullable="False" />
        <Column Name="Ip" DataType="String" Length="64" Nullable="False" />
        <Column Name="Action" DataType="String" Length="60" Nullable="False" />
        <Column Name="Title" DataType="String" Length="100" Nullable="False" />
        <Column Name="Method" DataType="String" Length="10" Nullable="False" />
        <Column Name="Path" DataType="String" Length="200" Nullable="False" />
        <Column Name="Detail" DataType="String" Length="500" />
        <Column Name="Success" DataType="Boolean" Nullable="False" />
        <Column Name="Code" DataType="Int32" Nullable="False" />
        <Column Name="Message" DataType="String" Length="300" />
        <Column Name="ElapsedMs" DataType="Int32" Nullable="False" />
      </Columns>
    </Table>
  </Tables>
</EntityModel>"#;

    /// 测试模型（`OperationLog` **无** `Category` 列——Pek.RAgent 形态）。
    const MODEL_WITHOUT_CATEGORY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<EntityModel xmlns:xs="http://www.w3.org/2001/XMLSchema-instance" xs:schemaLocation="https://newlifex.com https://newlifex.com/Model202509.xsd" Version="1.0" Document="https://newlifex.com/xcode/model" ModelVersion="2.0" xmlns="https://newlifex.com/Model202509.xsd">
  <Option>
    <BaseClass>EntityBase</BaseClass>
    <ConnName>Test</ConnName>
    <NameFormat>Default</NameFormat>
    <Nullable>True</Nullable>
    <HasIModel>False</HasIModel>
  </Option>
  <Tables>
    <Table Name="PanelUser" TableName="Test_PanelUser">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="UserName" DataType="String" Length="50" Master="True" Nullable="False" />
        <Column Name="PasswordHash" DataType="String" Length="128" Nullable="False" />
        <Column Name="Salt" DataType="String" Length="64" Nullable="False" />
        <Column Name="Permissions" DataType="String" Length="500" Nullable="False" />
        <Column Name="Enabled" DataType="Boolean" Nullable="False" />
        <Column Name="Remark" DataType="String" Length="200" />
        <Column Name="CreateTime" DataType="DateTime" />
        <Column Name="UpdateTime" DataType="DateTime" />
      </Columns>
    </Table>
    <Table Name="OperationLog" TableName="Test_OperationLog">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="LogTime" DataType="DateTime" Nullable="False" />
        <Column Name="UserName" DataType="String" Length="50" Nullable="False" />
        <Column Name="Ip" DataType="String" Length="64" Nullable="False" />
        <Column Name="Action" DataType="String" Length="60" Nullable="False" />
        <Column Name="Title" DataType="String" Length="100" Nullable="False" />
        <Column Name="Method" DataType="String" Length="10" Nullable="False" />
        <Column Name="Path" DataType="String" Length="200" Nullable="False" />
        <Column Name="Detail" DataType="String" Length="500" />
        <Column Name="Success" DataType="Boolean" Nullable="False" />
        <Column Name="Code" DataType="Int32" Nullable="False" />
        <Column Name="Message" DataType="String" Length="300" />
        <Column Name="ElapsedMs" DataType="Int32" Nullable="False" />
      </Columns>
    </Table>
  </Tables>
</EntityModel>"#;

    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rcode-panel-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// 用指定模型打开临时 SQLite 共享存储。
    fn open_store(base: &Path, model_xml: &str) -> Arc<SharedStore> {
        let model = EntityModel::parse(model_xml).expect("模型解析");
        let db_path = base.join("panel.db");
        let conn = format!(
            "Data Source={};Provider=SQLite;ShowSql=false",
            db_path.display()
        );
        let (store, _) = crate::store::get_or_open(base, move || {
            let dal = Dal::open_with_model(&conn, model).map_err(|e| e.to_string())?;
            dal.sync_schema().map_err(|e| e.to_string())?;
            Ok(dal)
        })
        .expect("打开共享存储");
        store
    }

    fn cleanup(base: &Path) {
        crate::store::drop_for_test(base);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn password_hash_is_salted_and_stable() {
        let salt = new_salt();
        let hash = hash_password(&salt, "secret");
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash_password(&salt, "secret"));
        assert_ne!(hash, hash_password(&salt, "other"));
        assert_ne!(hash, hash_password("other-salt", "secret"));
        assert_ne!(hash, new_salt());
    }

    #[test]
    fn user_crud_and_login_flow() {
        let base = temp_base("users");
        let store = open_store(&base, MODEL_WITH_CATEGORY);
        let auth = PanelAuth::new(PERMS);

        // 创建（权限自动归一化排序：records 在 settings 前）
        auth.save_user(
            &store,
            "alice",
            Some("pw123"),
            &["settings".into(), "records".into()],
            true,
            "测试用户",
        )
        .unwrap();
        let u = auth.find_user(&store, "ALICE").unwrap().unwrap();
        assert!(u.enabled);
        assert_eq!(
            u.permissions,
            vec!["records".to_string(), "settings".to_string()]
        );
        assert!(u.verify("pw123"));

        // 登录
        assert!(auth.verify_login(&store, "alice", "pw123").unwrap().is_some());
        assert!(auth.verify_login(&store, "alice", "bad").unwrap().is_none());

        // 更新（不改密码）：权限/启用/备注更新
        auth.save_user(&store, "alice", None, &["database".into()], false, "停用")
            .unwrap();
        let u = auth.find_user(&store, "alice").unwrap().unwrap();
        assert!(!u.enabled);
        assert!(u.verify("pw123"), "未提供密码时密码不变");
        assert_eq!(u.permissions, vec!["database".to_string()]);
        assert!(
            auth.verify_login(&store, "alice", "pw123").unwrap().is_none(),
            "禁用后不能登录"
        );

        // 新用户需密码；非法用户名拒绝
        assert!(auth.save_user(&store, "bob", None, &[], true, "").is_err());
        assert!(auth
            .save_user(&store, "bad name", Some("x"), &[], true, "")
            .is_err());

        // 列表 JSON（含权限中文名）
        let view = auth.list_users_json(&store).unwrap();
        assert_eq!(view["users"].as_array().unwrap().len(), 1);
        assert_eq!(view["users"][0]["permissionNames"][0], "数据库管理");
        assert_eq!(view["permissions"].as_array().unwrap().len(), 3);

        // 改密码
        auth.change_user_password(&store, "alice", "pw123", "pw456").unwrap();
        assert!(auth
            .change_user_password(&store, "alice", "wrong", "x")
            .is_err());
        auth.save_user(&store, "alice", None, &["database".into()], true, "")
            .unwrap();
        assert!(auth.verify_login(&store, "alice", "pw456").unwrap().is_some());

        // 删除
        auth.delete_user(&store, "alice").unwrap();
        assert!(auth.find_user(&store, "alice").unwrap().is_none());
        assert!(auth.delete_user(&store, "alice").is_err());

        cleanup(&base);
    }

    fn sample_entry(i: usize, user: &str, ok: bool) -> AuditEntry {
        AuditEntry {
            category: Some("panel".to_string()),
            user: user.to_string(),
            ip: "127.0.0.1".to_string(),
            action: "fileDelete".to_string(),
            title: "删除文件".to_string(),
            method: "POST".to_string(),
            path: "/panel/fileDelete".to_string(),
            detail: format!("paths=item-{i}"),
            success: ok,
            code: if ok { 1 } else { 2 },
            message: if ok { "已删除".to_string() } else { "失败".to_string() },
            elapsed_ms: 5,
        }
    }

    #[test]
    fn audit_records_and_queries_with_filters() {
        let base = temp_base("oplog");
        let store = open_store(&base, MODEL_WITH_CATEGORY);

        for i in 0..5 {
            record(&store, &sample_entry(i, "alice", true));
        }
        record(&store, &sample_entry(9, "bob", false));
        let mut api = sample_entry(20, "api", true);
        api.category = Some("api".to_string());
        record(&store, &api);

        // 全部：7 条
        let all = query_logs(&store, 1, 4, "", "", "", None).unwrap();
        assert_eq!(all["total"], 7);
        assert_eq!(all["items"].as_array().unwrap().len(), 4);
        let page2 = query_logs(&store, 2, 4, "", "", "", None).unwrap();
        assert_eq!(page2["items"].as_array().unwrap().len(), 3);

        // 类别过滤（api；内存过滤）
        let only_api = query_logs(&store, 1, 50, "api", "", "", None).unwrap();
        assert_eq!(only_api["total"], 1);
        assert_eq!(only_api["items"][0]["category"], "api");

        // 用户过滤
        let alice = query_logs(&store, 1, 50, "", "alice", "", None).unwrap();
        assert_eq!(alice["total"], 5);

        // 成功/失败过滤
        let failed = query_logs(&store, 1, 50, "", "", "", Some(false)).unwrap();
        assert_eq!(failed["total"], 1);
        assert_eq!(failed["items"][0]["userName"], "bob");

        // 关键词（命中 detail / 中文标题）
        let kw = query_logs(&store, 1, 50, "", "", "item-3", None).unwrap();
        assert_eq!(kw["total"], 1);
        let kw2 = query_logs(&store, 1, 50, "", "", "删除文件", None).unwrap();
        assert_eq!(kw2["total"], 7);

        cleanup(&base);
    }

    #[test]
    fn audit_without_category_column_is_compatible() {
        let base = temp_base("oplog-nocat");
        let store = open_store(&base, MODEL_WITHOUT_CATEGORY);

        // category=None：不写 Category 列（表无该列）
        let mut entry = sample_entry(1, "op1", true);
        entry.category = None;
        record(&store, &entry);
        // 显式 Some 在无列的表会写失败（只记日志、不 panic）——仍应能继续写入
        let mut with_cat = sample_entry(2, "op1", true);
        with_cat.category = Some("panel".to_string());
        record(&store, &with_cat);

        let all = query_logs(&store, 1, 50, "", "", "", None).unwrap();
        assert_eq!(all["total"], 1, "无 Category 列时仅 None 条目落库成功");
        assert_eq!(all["items"][0]["category"], "", "无列读取回退为空串");
        // category 过滤对无列数据（空串）不命中
        let filtered = query_logs(&store, 1, 50, "panel", "", "", None).unwrap();
        assert_eq!(filtered["total"], 0);

        cleanup(&base);
    }

    #[test]
    fn permissions_normalized_filters_unknown_keys() {
        let auth = PanelAuth::new(PERMS);
        let list: Vec<String> = vec![
            "settings".into(),
            " unknown ".into(),
            "records".into(),
            "records".into(),
        ];
        assert_eq!(auth.normalize(&list), vec!["records", "settings"]);
        assert_eq!(auth.permission_name("database"), "数据库管理");
        assert_eq!(auth.permission_name("missing"), "missing");
        assert_eq!(auth.all_permissions_json().len(), 3);
    }

    #[test]
    fn audit_tools_redact_and_summarize() {
        // 表单脱敏
        assert_eq!(redact_form("password=x&a=1"), "password=***&a=1");
        assert_eq!(redact_form("Token=t&b=2"), "Token=***&b=2");
        // JSON 脱敏（递归）
        let mut v: Json =
            serde_json::from_str(r#"{"a":{"apiKey":"k"},"list":[{"secret":"s"}],"n":1}"#).unwrap();
        redact_json(&mut v);
        assert_eq!(v["a"]["apiKey"], "***");
        assert_eq!(v["list"][0]["secret"], "***");
        assert_eq!(v["n"], 1);
        // 摘要（查询串 + JSON 体）
        let s = summarize_body("password=q", "application/json", br#"{"userName":"u","password":"p"}"#);
        assert!(s.contains("password=***"), "{s}");
        assert!(s.contains("\"password\":\"***\""), "{s}");
        assert!(s.contains("\"userName\":\"u\""), "{s}");
        // 二进制体只记字节数
        let s2 = summarize_body("", "application/zip", &[0u8; 10]);
        assert_eq!(s2, "body=10 字节");
        // 截断（中文安全）
        assert_eq!(truncate_text("中文测试", 2), "中文…");
        assert_eq!(truncate_text("ok", 5), "ok");
        // 动作名/标题
        assert_eq!(action_name_of("/panel/userSave/"), "userSave");
        assert_eq!(action_title("userSave", &[("userSave", "保存面板用户")]), "保存面板用户");
        assert_eq!(action_title("unknownAct", &[]), "操作 unknownAct");
    }
}
