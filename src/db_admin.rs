//! 数据库管理服务层（面板通用）：信息 / 只读 SQL / 备份 / 还原 / 备份档案。
//!
//! 提炼自 Pek.RAgent Web 面板（2026-10-05），供各管理系统（平台 / DHDeploy / …）复用，
//! 使“涉及管理功能的系统都有数据库管理”成为一条组装线：
//!
//! - [`DbAdmin::database_info`]：库类型（Provider）、文件路径与大小（SQLite）、各业务表行数；
//! - [`DbAdmin::query_readonly`]：只读查询（[`validate_readonly_sql`] 安全闸门；最多 500 行）；
//! - [`DbAdmin::backup_zip`] / [`DbAdmin::restore_zip`]：DbTable zip 包（`backup_all`/`restore_all`，
//!   与 C# 生态互通；还原前全量解码校验，不合格不动现有数据；自适应旧备份仅还原包内实际存在的表）；
//! - 备份档案：`create_backup` / `list_backups` / `read_backup` / `restore_backup` / `delete_backup`
//!   （文件名安全闸门 [`validate_backup_name`]；同秒重名自动 `-2/-3` 后缀）。
//!
//! 用法（示例）：
//!
//! ```no_run
//! use pek_rcode::db_admin::DbAdmin;
//! use pek_rcode::store::SharedStore;
//!
//! # fn demo(store: std::sync::Arc<SharedStore>, base: &std::path::Path) -> Result<(), String> {
//! let admin = DbAdmin::new(
//!     store,
//!     vec!["Order".to_string()],                       // 业务表（实体名）
//!     base.join("Data").join("Backup"),                // 备份档案目录
//!     "demo",                                          // 备份文件名前缀
//! ).with_sqlite_file(Some(base.join("Data").join("demo.db")));
//! let info = admin.database_info();
//! let result = admin.query_readonly("SELECT 1")?;
//! let zip = admin.backup_zip()?;
//! # let _ = (info, result, zip);
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value as Json};

use crate::model::EntityModel;
use crate::store::SharedStore;
use crate::value::DbValue;

/// 只读查询返回的最大行数（超出截断并附标记）。
pub const MAX_QUERY_ROWS: usize = 500;

/// 备份档案存储（仅依赖目录，不依赖数据库；数据库不可用时仍可列出/下载/删除档案）。
pub struct BackupStore {
    dir: PathBuf,
    prefix: String,
}

impl BackupStore {
    /// 构建（`prefix` 用于档案文件名 `{prefix}-{时间}.zip`）。
    pub fn new(dir: PathBuf, prefix: impl Into<String>) -> Self {
        Self {
            dir,
            prefix: prefix.into(),
        }
    }

    /// 档案目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 文件名前缀。
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// 档案列表（按创建时间倒序；`dir` + `items[{name,sizeBytes,created}]`）。
    pub fn list(&self) -> Json {
        let mut items: Vec<Json> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || validate_backup_name(&name).is_err() {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let created = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                items.push(json!({ "name": name, "sizeBytes": meta.len(), "created": created }));
            }
        }
        items.sort_by_key(|item| std::cmp::Reverse(item["created"].as_u64().unwrap_or(0)));
        json!({ "dir": self.dir.display().to_string(), "items": items })
    }

    /// 读取档案（下载）。
    pub fn read(&self, name: &str) -> Result<Vec<u8>, String> {
        validate_backup_name(name)?;
        std::fs::read(self.dir.join(name)).map_err(|e| format!("读取备份文件失败：{e}"))
    }

    /// 删除档案。
    pub fn delete(&self, name: &str) -> Result<(), String> {
        validate_backup_name(name)?;
        std::fs::remove_file(self.dir.join(name)).map_err(|e| format!("删除备份文件失败：{e}"))
    }

    /// 将备份字节落为档案（`{prefix}-{时间}.zip`；同秒重名自动 `-2/-3` 后缀）。
    pub fn save(&self, bytes: &[u8]) -> Result<Json, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("创建备份目录失败：{e}"))?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut name = format!("{}-{stamp}.zip", self.prefix);
        let mut path = self.dir.join(&name);
        let mut n = 1;
        while path.exists() {
            n += 1;
            name = format!("{}-{stamp}-{n}.zip", self.prefix);
            path = self.dir.join(&name);
        }
        std::fs::write(&path, bytes).map_err(|e| format!("写入备份文件失败：{e}"))?;
        Ok(json!({ "name": name, "sizeBytes": bytes.len() }))
    }
}

/// 数据库管理服务（一个实例服务一个库）。
pub struct DbAdmin {
    store: Arc<SharedStore>,
    /// 业务表实体名清单（信息/备份/还原范围；物理表名按模型 `TableName` 解析）。
    tables: Vec<String>,
    /// 备份档案存储。
    backups: BackupStore,
    /// SQLite 数据文件（信息卡大小展示用）。
    sqlite_file: Option<PathBuf>,
}

impl DbAdmin {
    /// 构建（`prefix` 用于备份档案文件名 `{prefix}-{时间}.zip`）。
    pub fn new(
        store: Arc<SharedStore>,
        tables: Vec<String>,
        backup_dir: PathBuf,
        prefix: impl Into<String>,
    ) -> Self {
        Self {
            store,
            tables,
            backups: BackupStore::new(backup_dir, prefix),
            sqlite_file: None,
        }
    }

    /// 设置 SQLite 数据文件路径（信息卡展示大小；非 SQLite 可不设）。
    pub fn with_sqlite_file(mut self, path: Option<PathBuf>) -> Self {
        self.sqlite_file = path;
        self
    }

    /// 业务表清单（实体名）。
    pub fn tables(&self) -> &[String] {
        &self.tables
    }

    // ————— 信息与查询 —————

    /// 数据库概况（面板“数据库”页）：Provider、文件路径与大小（SQLite）、各业务表行数。
    pub fn database_info(&self) -> Json {
        let size = self
            .sqlite_file
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        let path_text = self
            .sqlite_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut tables: Vec<Json> = Vec::new();
        for entity in &self.tables {
            let rows = self
                .store
                .dal()
                .open_session()
                .ok()
                .and_then(|mut session| {
                    self.store
                        .dal()
                        .table(entity)
                        .ok()
                        .and_then(|t| t.count(session.as_mut(), None).ok())
                });
            let name = self.physical_name(entity).unwrap_or_else(|| entity.clone());
            tables.push(json!({ "name": name, "entity": entity, "rows": rows }));
        }
        json!({
            "path": path_text,
            "sizeBytes": size,
            "provider": self.store.dal().kind().name(),
            "tables": tables,
        })
    }

    /// 实体名 → 物理表名（按模型解析；未知返回 `None`）。
    pub fn physical_name(&self, entity: &str) -> Option<String> {
        self.store
            .dal()
            .table(entity)
            .ok()
            .map(|t| t.meta().effective_table_name().to_string())
    }

    /// 只读执行 SQL（安全前置 [`validate_readonly_sql`]；结果最多 [`MAX_QUERY_ROWS`] 行）。
    pub fn query_readonly(&self, sql: &str) -> Result<Json, String> {
        validate_readonly_sql(sql)?;
        let started = std::time::Instant::now();
        let mut session = self
            .store
            .dal()
            .open_session()
            .map_err(|e| e.to_string())?;
        let set = session.query(sql, &[]).map_err(|e| e.to_string())?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let truncated = set.rows.len() > MAX_QUERY_ROWS;
        let rows: Vec<Json> = set
            .rows
            .iter()
            .take(MAX_QUERY_ROWS)
            .map(|row| {
                Json::Array(
                    (0..set.columns.len())
                        .map(|i| row.get(i).map(db_value_json).unwrap_or(Json::Null))
                        .collect(),
                )
            })
            .collect();
        Ok(json!({
            "columns": set.columns.as_ref(),
            "rows": rows,
            "rowCount": rows.len(),
            "truncated": truncated,
            "elapsedMs": elapsed_ms,
        }))
    }

    // ————— 备份与还原 —————

    /// 备份全部业务表为 DbTable zip 包（`backup_schema=true` 带模型 XML；与 C# 生态互通）。
    pub fn backup_zip(&self) -> Result<Vec<u8>, String> {
        let _guard = self.store.lock();
        let dir = self.backups.dir().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录失败：{e}"))?;
        let tmp = dir.join(format!(
            ".tmp-{}-{}.zip",
            self.backups.prefix(),
            chrono::Local::now().format("%Y%m%d%H%M%S%3f")
        ));
        let tables = &self.tables;
        let table_refs: Vec<&str> = tables.iter().map(String::as_str).collect();
        let expected = tables.len();
        let result = self
            .store
            .dal()
            .backup_all(&table_refs, &tmp, true)
            .map_err(|e| format!("备份失败：{e}"))
            .and_then(|count| {
                if count < expected {
                    Err(format!("备份失败：仅成功 {count}/{expected} 张表"))
                } else {
                    std::fs::read(&tmp).map_err(|e| format!("读取备份文件失败：{e}"))
                }
            });
        let _ = std::fs::remove_file(&tmp);
        result
    }

    /// 从 DbTable zip 包还原业务表：先全量解码校验（不合格不动现有数据），再清空导入。
    ///
    /// **自适应旧备份**：仅处理包内实际存在的表（旧版缺表的备份不会清空其余表）；
    /// 至少需包含一张已知表。返回各表恢复行数；完成后失效实体缓存。
    pub fn restore_zip(&self, bytes: &[u8]) -> Result<Json, String> {
        // 1) 结构预校验：包内存在的已知表其 DbTable 流必须完整可解码
        let mut present: Vec<String> = Vec::new();
        {
            use std::io::Read;
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
                .map_err(|e| format!("不是有效的备份包（zip）：{e}"))?;
            let names: Vec<String> = (0..zip.len())
                .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
                .collect();
            for entity in &self.tables {
                let entry_name = format!("{entity}.table");
                if !names.iter().any(|n| n == &entry_name) {
                    continue;
                }
                let mut entry = zip
                    .by_name(&entry_name)
                    .map_err(|_| format!("备份包缺少数据项 {entry_name}"))?;
                let mut data = Vec::new();
                entry
                    .read_to_end(&mut data)
                    .map_err(|e| format!("读取 {entry_name} 失败：{e}"))?;
                crate::dbtable::decode_rowset(&data)
                    .map_err(|e| format!("{entry_name} 解码校验失败：{e}"))?;
                present.push(entity.clone());
            }
            if present.is_empty() {
                return Err(format!(
                    "备份包不包含任何已知数据表（{}）",
                    self.tables.join("/")
                ));
            }
        }

        // 2) 落临时文件
        let tmp_dir = self.backups.dir().to_path_buf();
        std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("创建备份目录失败：{e}"))?;
        let tmp = tmp_dir.join(format!(".tmp-{}-restore.zip", self.backups.prefix()));
        std::fs::write(&tmp, bytes).map_err(|e| format!("写入临时文件失败：{e}"))?;

        // 3) 清空 + 导入（与写路径共用串行锁；完成后失效实体缓存）
        let expected = present.len();
        let outcome = (|| -> Result<(Vec<String>, Json), String> {
            let _guard = self.store.lock();
            let dal = self.store.dal();
            let mut session = dal.open_session().map_err(|e| e.to_string())?;
            for entity in &present {
                let physical = self.physical_name(entity).ok_or_else(|| {
                    format!("未知实体：{entity}（不在模型内）")
                })?;
                // 标识符按目标方言引用：SQLite `"x"`、MySQL `` `x` ``、SqlServer `[x]`。
                // 硬编码双引号在 MySQL 下会被当作字符串字面量，导致语法错误。
                let quoted = dal.kind().quote(&physical);
                session
                    .execute(&format!("DELETE FROM {quoted}"), &[])
                    .map_err(|e| format!("清空 {physical} 失败：{e}"))?;
            }
            drop(session);
            let present_refs: Vec<&str> = present.iter().map(String::as_str).collect();
            let done = self
                .store
                .dal()
                .restore_all(&tmp, Some(&present_refs), false)
                .map_err(|e| format!("导入失败：{e}"))?;
            if done.len() < expected {
                return Err(format!(
                    "导入不完整：成功 {}/{expected} 张表（请重新还原）",
                    done.len()
                ));
            }
            let mut rows = serde_json::Map::new();
            for entity in &present {
                let mut session = self
                    .store
                    .dal()
                    .open_session()
                    .map_err(|e| e.to_string())?;
                let n = self
                    .store
                    .dal()
                    .table(entity)
                    .map_err(|e| e.to_string())?
                    .count(session.as_mut(), None)
                    .map_err(|e| e.to_string())?;
                let name = self.physical_name(entity).unwrap_or_else(|| entity.clone());
                rows.insert(name, json!(n));
            }
            for entity in &present {
                self.store.dal().invalidate_cache(entity);
            }
            Ok((done, Json::Object(rows)))
        })();

        let _ = std::fs::remove_file(&tmp);
        let (done, rows) = outcome?;
        Ok(json!({ "tables": done, "rows": rows }))
    }

    // ————— 备份档案（服务器端文件；仅依赖目录，数据库不可用时也可用） —————

    /// 备份档案存储。
    pub fn backups(&self) -> &BackupStore {
        &self.backups
    }

    /// 备份档案目录。
    pub fn backup_dir(&self) -> &Path {
        self.backups.dir()
    }

    /// 创建服务器端备份文件（`{prefix}-{时间}.zip`；同秒重名自动 `-2/-3` 后缀）。
    pub fn create_backup(&self) -> Result<Json, String> {
        let bytes = self.backup_zip()?;
        self.backups.save(&bytes)
    }

    /// 备份文件列表（按创建时间倒序；`dir` + `items[{name,sizeBytes,created}]`）。
    pub fn list_backups(&self) -> Json {
        self.backups.list()
    }

    /// 读取服务器端备份文件（下载）。
    pub fn read_backup(&self, name: &str) -> Result<Vec<u8>, String> {
        self.backups.read(name)
    }

    /// 从服务器端备份文件还原（覆盖式；预校验不合格不动数据）。
    pub fn restore_backup(&self, name: &str) -> Result<Json, String> {
        let bytes = self.backups.read(name)?;
        self.restore_zip(&bytes)
    }

    /// 删除服务器端备份文件。
    pub fn delete_backup(&self, name: &str) -> Result<(), String> {
        self.backups.delete(name)
    }
}

/// 从模型推导业务表清单（实体名；与 C# DbPackage 实体名互通）。
pub fn tables_from_model(model: &EntityModel) -> Vec<String> {
    model.tables.iter().map(|t| t.name.clone()).collect()
}

/// 只读查询校验（安全闸门）：仅放行单条 `SELECT` 文本。
///
/// 规则（宁可误杀不可放过）：
/// - 非空、长度 ≤ 4000 字符；
/// - 不允许包含 `;`（阻断多语句注入；字符串字面量内的分号一并拒绝）；
/// - 忽略前导空白后必须以 `SELECT` 关键字开头且后跟空白（挡住 `WITH`/`PRAGMA`/`SELECTX` 等路径）。
pub fn validate_readonly_sql(sql: &str) -> Result<(), String> {
    let s = sql.trim();
    if s.is_empty() {
        return Err("SQL 不能为空".into());
    }
    if s.len() > 4000 {
        return Err("SQL 过长（上限 4000 字符）".into());
    }
    if s.contains(';') {
        return Err("仅支持单条查询：SQL 中不允许出现分号（字符串内的分号也会被拒绝）".into());
    }
    let is_select = match s.get(..6) {
        Some(head) if head.eq_ignore_ascii_case("SELECT") => s
            .as_bytes()
            .get(6)
            .is_none_or(|b| b.is_ascii_whitespace()),
        _ => false,
    };
    if !is_select {
        return Err("仅允许 SELECT 只读查询（当前版本不开放增删改与建表操作）".into());
    }
    Ok(())
}

/// 备份文件名校验（安全闸门）：仅允许简单文件名（防路径穿越；`[A-Za-z0-9._-]` 且以 `.zip` 结尾）。
pub fn validate_backup_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.ends_with(".zip")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err("备份文件名无效".into())
    }
}

/// `DbValue` → JSON（面板展示；BLOB 仅显示尺寸避免大对象）。
fn db_value_json(value: &DbValue) -> Json {
    match value {
        DbValue::Null => Json::Null,
        DbValue::Bool(b) => json!(b),
        DbValue::Int(i) => json!(i),
        DbValue::Float(f) => json!(f),
        DbValue::Decimal(d) => json!(d.to_string()),
        DbValue::Text(s) => json!(s),
        DbValue::Blob(b) => json!(format!("<BLOB {} B>", b.len())),
        DbValue::DateTime(dt) => json!(dt.format("%Y-%m-%d %H:%M:%S").to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::store;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pek-dbadmin-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const MINI_MODEL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<EntityModel Version="1.0" xmlns="https://newlifex.com/Model202509.xsd">
  <Option><ConnName>Demo</ConnName><HasIModel>False</HasIModel></Option>
  <Tables>
    <Table Name="Demo" TableName="Demo_Item">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Name" DataType="String" Length="50" />
      </Columns>
    </Table>
  </Tables>
</EntityModel>"#;

    fn admin_for(dir: &Path, tag: &str) -> (Arc<SharedStore>, DbAdmin) {
        let db_path = dir.join(format!("{tag}.db"));
        let conn = format!("Data Source={};Provider=SQLite", db_path.display());
        let model = EntityModel::parse(MINI_MODEL).unwrap();
        let (store, _) = store::get_or_open(dir, || {
            let dal = Dal::open_with_model(&conn, model).map_err(|e| e.to_string())?;
            dal.sync_schema().map_err(|e| e.to_string())?;
            Ok(dal)
        })
        .unwrap();
        let admin = DbAdmin::new(
            store.clone(),
            vec!["Demo".to_string()],
            dir.join("Backup"),
            format!("demo-{tag}"),
        )
        .with_sqlite_file(Some(db_path));
        (store, admin)
    }

    #[test]
    fn readonly_sql_rules() {
        assert!(validate_readonly_sql("SELECT 1").is_ok());
        assert!(validate_readonly_sql("  select * from t  ").is_ok());
        assert!(validate_readonly_sql("").is_err());
        assert!(validate_readonly_sql("SELECTX 1").is_err());
        assert!(validate_readonly_sql("SELECT 1; SELECT 2").is_err());
        assert!(validate_readonly_sql("INSERT INTO t VALUES (1)").is_err());
        assert!(validate_readonly_sql("WITH x AS (SELECT 1) SELECT * FROM x").is_err());
        assert!(validate_readonly_sql(&format!("SELECT '{}'", "a".repeat(4000))).is_err());
    }

    #[test]
    fn backup_name_rules() {
        assert!(validate_backup_name("demo-20260101-120000.zip").is_ok());
        assert!(validate_backup_name("a.zip").is_ok());
        for bad in ["", "a.txt", "../a.zip", "a/b.zip", "a\\b.zip", "a..b.zip"] {
            assert!(validate_backup_name(bad).is_err(), "{bad} 应被拒绝");
        }
    }

    #[test]
    fn info_query_backup_restore_roundtrip() {
        let dir = temp_dir("rt");
        let (store, admin) = admin_for(&dir, "rt");
        // 写入几行
        store
            .with_session(|_dal, session| {
                session.execute("INSERT INTO \"Demo_Item\" (\"Name\") VALUES ('a')", &[])?;
                session.execute("INSERT INTO \"Demo_Item\" (\"Name\") VALUES ('b')", &[])?;
                Ok(())
            })
            .unwrap();
        // 信息
        let info = admin.database_info();
        assert_eq!(info["provider"], "SQLite");
        assert!(info["sizeBytes"].as_u64().unwrap() > 0);
        assert_eq!(info["tables"][0]["name"], "Demo_Item");
        assert_eq!(info["tables"][0]["rows"], 2);
        // 只读查询
        let q = admin.query_readonly("SELECT * FROM \"Demo_Item\" ORDER BY \"Id\"").unwrap();
        assert_eq!(q["rowCount"], 2);
        assert_eq!(q["columns"][1], "Name");
        assert!(admin.query_readonly("DELETE FROM \"Demo_Item\"").is_err());
        // 备份 → 破坏数据 → 还原
        let zip = admin.backup_zip().unwrap();
        assert!(zip.len() > 100);
        store
            .with_session(|_dal, session| {
                session.execute("DELETE FROM \"Demo_Item\"", &[])?;
                Ok(())
            })
            .unwrap();
        let result = admin.restore_zip(&zip).unwrap();
        assert_eq!(result["rows"]["Demo_Item"], 2);
        // 坏包被拒：不破坏现有数据
        assert!(admin.restore_zip(b"not a zip").is_err());
        let after = admin.query_readonly("SELECT COUNT(*) AS C FROM \"Demo_Item\"").unwrap();
        assert_eq!(after["rows"][0][0], 2);
        // 档案：创建 → 列表 → 读取 → 还原 → 删除
        let created = admin.create_backup().unwrap();
        let name = created["name"].as_str().unwrap().to_string();
        assert!(name.starts_with("demo-rt-"));
        let list = admin.list_backups();
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        assert!(admin.read_backup(&name).unwrap().len() > 100);
        assert!(admin.restore_backup(&name).is_ok());
        admin.delete_backup(&name).unwrap();
        assert_eq!(admin.list_backups()["items"].as_array().unwrap().len(), 0);
        assert!(admin.read_backup("../x.zip").is_err());
        store::drop_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
