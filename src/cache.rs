//! 实体缓存：对应 DH.NCode 的 `XCode.Cache`（`Meta.Cache` / `Meta.SingleCache`）。
//!
//! 覆盖 DH.NCode 缓存架构中的两层（见其 `Cache/readme.md`）：
//! - **实体缓存** [`EntityCache`]：整表缓存“读多写少”的数据（系统参数、分类表等）。
//!   首次访问执行一次整表查询，之后在内存列表上执行查找；默认过期 **60 秒**
//! - **单对象缓存** [`SingleCache`]：以主键为键逐行缓存（用户表等点查场景）。
//!   未命中时查库并回填；默认过期 **60 秒**、最大 **10000** 项（超出即整体清空）
//!
//! 一致性策略（与 DH.NCode 语义对齐）：
//! - 任何写入（[`crate::dal::TableRef`] 的 `insert` / `update_by_pk` / `delete_by_pk`，
//!   实体层的写入同源）都会使对应表的缓存**立即失效**，下次访问自动重新加载
//! - 与 C# 版的差异：C# 版过期后“立即返回旧数据 + 异步更新（LazyConsumer）”；
//!   Rust 版首版采用“过期后同步重载”，语义更直观且不引入后台线程，后续可按需演进
//!
//! 使用入口：按表获取共享缓存实例——
//! ```no_run
//! # use pek_rcode::dal::Dal;
//! # fn demo(dal: &Dal) -> pek_rcode::Result<()> {
//! let mut session = dal.open_session()?;
//! // 对应 C# 的 Meta.Cache.Entities（整表缓存）
//! let cache = dal.entity_cache("JiLiYu")?;
//! let rows = cache.entities(dal, session.as_mut())?;
//! println!("整表 {} 行", rows.len());
//!
//! // 对应 C# 的 Meta.SingleCache[key]（单对象缓存）
//! let single = dal.single_cache("VerifyCode")?;
//! let item = single.get(dal, session.as_mut(), &["k-001".into()])?;
//! println!("{item:?}");
//! # Ok(())
//! # }
//! ```

pub mod data;
pub mod db;
pub mod field;
pub mod invalidator;
pub mod lazy;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::dal::{Dal, TableRef};
use crate::error::Result;
use crate::model::TableMeta;
use crate::session::{DbRow, SqlSession};
use crate::value::DbValue;

/// 默认过期时间（秒，与 DH.NCode 一致）。
pub const DEFAULT_EXPIRE_SECONDS: u64 = 60;

/// 单对象缓存默认最大实体数（与 DH.NCode 一致）。
pub const DEFAULT_MAX_ENTITY: usize = 10_000;

/// 获取互斥锁；被投毒（持有线程 panic）时恢复内部数据，避免级联失败。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 主键值 → 缓存键（多列主键按顺序拼接，分隔符为不可见字符）。
fn pk_key(pk: &[DbValue]) -> String {
    let mut key = String::new();
    for (index, value) in pk.iter().enumerate() {
        if index > 0 {
            key.push('\u{1f}');
        }
        key.push_str(&value.to_text());
    }
    key
}

/// 实体缓存（整表缓存，对应 DH.NCode 的 `Meta.Cache`）。
///
/// 建议只对**读取很多、修改极少**的小表使用（DH.NCode 建议单表 1000 行以内）。
pub struct EntityCache {
    /// 表名（注册键，小写）
    table: String,
    /// 过期时间
    expire: Mutex<Duration>,
    /// 缓存状态
    state: Mutex<EntityCacheState>,
}

/// 实体缓存内部状态。
#[derive(Default)]
struct EntityCacheState {
    /// 已缓存的整表数据
    data: Option<Arc<Vec<DbRow>>>,
    /// 加载完成时刻
    loaded_at: Option<Instant>,
}

impl EntityCache {
    /// 按表名创建（一般经 [`Dal::entity_cache`] 获取共享实例）。
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            expire: Mutex::new(Duration::from_secs(DEFAULT_EXPIRE_SECONDS)),
            state: Mutex::new(EntityCacheState::default()),
        }
    }

    /// 表名（小写）。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 当前过期时间。
    pub fn expire(&self) -> Duration {
        *lock(&self.expire)
    }

    /// 设置过期时间（秒）。
    pub fn set_expire_seconds(&self, seconds: u64) {
        *lock(&self.expire) = Duration::from_secs(seconds);
    }

    /// 缓存是否可用（已加载且未过期）。
    pub fn is_valid(&self) -> bool {
        let expire = *lock(&self.expire);
        let state = lock(&self.state);
        state
            .loaded_at
            .is_some_and(|at| at.elapsed() < expire)
            && state.data.is_some()
    }

    /// 取得整表数据（首次访问或过期时同步重载）。
    pub fn entities(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<Arc<Vec<DbRow>>> {
        {
            let expire = *lock(&self.expire);
            let state = lock(&self.state);
            if let (Some(data), Some(at)) = (&state.data, state.loaded_at)
                && at.elapsed() < expire
            {
                return Ok(data.clone());
            }
        }
        self.reload(dal, session)
    }

    /// 强制重新加载整表。
    pub fn reload(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<Arc<Vec<DbRow>>> {
        let rows = dal.select_all_rows(&self.table, session)?;
        let data = Arc::new(rows);
        let mut state = lock(&self.state);
        state.data = Some(data.clone());
        state.loaded_at = Some(Instant::now());
        Ok(data)
    }

    /// 使缓存失效（写入后由 DAL 自动调用；下次访问重新加载）。
    pub fn invalidate(&self) {
        let mut state = lock(&self.state);
        state.data = None;
        state.loaded_at = None;
    }

    /// 在已缓存的整表数据上做过滤查找（未加载时返回空列表）。
    pub fn find_all<P>(&self, predicate: P) -> Vec<DbRow>
    where
        P: Fn(&DbRow) -> bool,
    {
        let state = lock(&self.state);
        match &state.data {
            Some(data) => data.iter().filter(|row| predicate(row)).cloned().collect(),
            None => Vec::new(),
        }
    }

    /// 在已缓存的整表数据上按列值查找首个匹配行。
    pub fn find_first<P>(&self, predicate: P) -> Option<DbRow>
    where
        P: Fn(&DbRow) -> bool,
    {
        let state = lock(&self.state);
        state
            .data
            .as_ref()
            .and_then(|data| data.iter().find(|row| predicate(row)).cloned())
    }

    /// 在已缓存的整表数据上按主键列查找（对应 C# 的 `Meta.Cache.Entities.Find(__.ID, id)`）。
    pub fn find_by_pk(&self, column: &str, value: &DbValue) -> Option<DbRow> {
        self.find_first(|row| row.get_by_name(column) == Some(value))
    }
}

/// 单对象缓存（以主键为键逐行缓存，对应 DH.NCode 的 `Meta.SingleCache`）。
pub struct SingleCache {
    /// 表名（注册键，小写）
    table: String,
    /// 过期时间
    expire: Mutex<Duration>,
    /// 最大实体数（0 表示不限制）
    max_entity: Mutex<usize>,
    /// 缓存项（键为归一化主键文本）
    state: Mutex<SingleCacheState>,
}

/// 单对象缓存内部状态。
#[derive(Default)]
struct SingleCacheState {
    /// 主键 → (行数据, 写入时刻)
    items: HashMap<String, (Arc<DbRow>, Instant)>,
}

impl SingleCache {
    /// 按表名创建（一般经 [`Dal::single_cache`] 获取共享实例）。
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            expire: Mutex::new(Duration::from_secs(DEFAULT_EXPIRE_SECONDS)),
            max_entity: Mutex::new(DEFAULT_MAX_ENTITY),
            state: Mutex::new(SingleCacheState::default()),
        }
    }

    /// 表名（小写）。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 当前过期时间。
    pub fn expire(&self) -> Duration {
        *lock(&self.expire)
    }

    /// 设置过期时间（秒）。
    pub fn set_expire_seconds(&self, seconds: u64) {
        *lock(&self.expire) = Duration::from_secs(seconds);
    }

    /// 最大实体数。
    pub fn max_entity(&self) -> usize {
        *lock(&self.max_entity)
    }

    /// 设置最大实体数（0 表示不限制；超出时整体清空后重填）。
    pub fn set_max_entity(&self, max: usize) {
        *lock(&self.max_entity) = max;
    }

    /// 当前缓存项数量。
    pub fn count(&self) -> usize {
        lock(&self.state).items.len()
    }

    /// 是否包含指定主键（含已过期项；过期项会在下次 [`Self::get`] 时重载）。
    pub fn contains(&self, pk: &[DbValue]) -> bool {
        lock(&self.state).items.contains_key(&pk_key(pk))
    }

    /// 取得实体（未命中或过期时查库并回填；查不到时不缓存空值）。
    pub fn get(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
    ) -> Result<Option<Arc<DbRow>>> {
        let key = pk_key(pk);
        {
            let expire = *lock(&self.expire);
            let state = lock(&self.state);
            if let Some((row, at)) = state.items.get(&key)
                && at.elapsed() < expire
            {
                return Ok(Some(row.clone()));
            }
        }

        let table = dal.table(&self.table)?;
        let row = table.find_by_pk(session, pk)?;
        if let Some(found) = &row {
            self.store(key, Arc::new(found.clone()));
        }
        Ok(row.map(Arc::new))
    }

    /// 写入缓存项（超出最大实体数时先整体清空）。
    fn store(&self, key: String, row: Arc<DbRow>) {
        let max = *lock(&self.max_entity);
        let mut state = lock(&self.state);
        if max > 0 && state.items.len() >= max {
            state.items.clear();
        }
        state.items.insert(key, (row, Instant::now()));
    }

    /// 按主键移除缓存项（对应 C# 的 `SingleCache.Remove(entity)`）。
    pub fn remove(&self, pk: &[DbValue]) {
        lock(&self.state).items.remove(&pk_key(pk));
    }

    /// 清空全部缓存项（对应 C# 的 `SingleCache.Clear(reason)`）。
    pub fn clear(&self) {
        lock(&self.state).items.clear();
    }
}

impl Dal {
    /// 获取实体缓存（按表共享，惰性创建；对应 DH.NCode 的 `Meta.Cache`）。
    pub fn entity_cache(&self, table: &str) -> Result<Arc<EntityCache>> {
        let name = self.table_meta(table)?.effective_table_name().to_string();
        let key = name.to_ascii_lowercase();
        let mut map = lock(&self.entity_caches);
        Ok(map
            .entry(key)
            .or_insert_with(|| Arc::new(EntityCache::new(&name)))
            .clone())
    }

    /// 获取单对象缓存（按表共享，惰性创建；对应 DH.NCode 的 `Meta.SingleCache`）。
    pub fn single_cache(&self, table: &str) -> Result<Arc<SingleCache>> {
        let name = self.table_meta(table)?.effective_table_name().to_string();
        let key = name.to_ascii_lowercase();
        let mut map = lock(&self.single_caches);
        Ok(map
            .entry(key)
            .or_insert_with(|| Arc::new(SingleCache::new(&name)))
            .clone())
    }

    /// 使指定表的两类缓存全部失效（写入路径自动调用，也可手动触发）。
    ///
    /// 对应 DH.NCode：任何添删改操作都让缓存马上过期。
    /// `table` 可传实体名或实际表名（忽略大小写）。
    pub fn invalidate_cache(&self, table: &str) {
        let key = match self.table_meta(table) {
            Ok(meta) => meta.effective_table_name().to_ascii_lowercase(),
            Err(_) => table.to_ascii_lowercase(),
        };
        if let Some(cache) = lock(&self.entity_caches).get(&key) {
            cache.invalidate();
        }
        if let Some(cache) = lock(&self.single_caches).get(&key) {
            cache.clear();
        }
    }

    /// 读取整表（实体缓存加载用；列引用按方言转义）。
    pub(crate) fn select_all_rows(
        &self,
        table: &str,
        session: &mut dyn SqlSession,
    ) -> Result<Vec<DbRow>> {
        let meta = self.table_meta(table)?;
        let (sql, params) =
            crate::sqlbuild::select_sql(self.kind(), meta, &crate::query::Query::new());
        self.log_sql(&sql);
        Ok(session.query(&sql, &params)?.rows)
    }

    /// 按名取表定义（表名或实体名，忽略大小写）。
    pub(crate) fn table_meta(&self, table: &str) -> Result<&TableMeta> {
        self.model()
            .and_then(|model| model.table(table))
            .ok_or_else(|| crate::error::Error::Model(format!("模型中不存在表/实体：{table}")))
    }
}

impl<'a> TableRef<'a> {
    /// 该表的实体缓存（对应 DH.NCode 的 `Meta.Cache`）。
    pub fn entity_cache(&self) -> Result<Arc<EntityCache>> {
        self.dal().entity_cache(self.meta().name.as_str())
    }

    /// 该表的单对象缓存（对应 DH.NCode 的 `Meta.SingleCache`）。
    pub fn single_cache(&self) -> Result<Arc<SingleCache>> {
        self.dal().single_cache(self.meta().name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Item" TableName="DH_Item">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    /// 建一个临时 SQLite 库并建表。
    fn temp_dal(name: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-cache-{}-{stamp}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("cache.db");
        let model = EntityModel::parse(MODEL).expect("测试模型应可解析");
        let dal = Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db.display()),
            model,
        )
        .unwrap();
        dal.sync_schema().unwrap();
        (dal, dir)
    }

    #[test]
    fn entity_cache_hits_until_invalidated() {
        let (dal, dir) = temp_dal("entity");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        let id = table
            .insert(session.as_mut(), &[("Code", "A".into())])
            .unwrap();
        assert!(id > 0);

        let cache = dal.entity_cache("Item").unwrap();
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 1);

        // 绕过表句柄直接写库（模拟外部写入）：缓存未失效时仍读旧数据（命中证明）
        session
            .execute(
                "INSERT INTO \"DH_Item\" (\"Code\") VALUES (?)",
                &[DbValue::Text("B".into())],
            )
            .unwrap();
        assert_eq!(
            cache.entities(&dal, session.as_mut()).unwrap().len(),
            1,
            "未失效时应命中缓存"
        );

        // 手动失效后重新加载
        dal.invalidate_cache("Item");
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 2);

        // 经表句柄写入自动失效
        table
            .insert(session.as_mut(), &[("Code", "C".into())])
            .unwrap();
        assert_eq!(
            cache.entities(&dal, session.as_mut()).unwrap().len(),
            3,
            "写入应自动使缓存失效"
        );

        // 内存内按主键查找（不再回库）
        let found = cache.find_by_pk("Id", &DbValue::Int(1)).unwrap();
        assert_eq!(found.get_by_name("Code").unwrap().as_str(), Some("A"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_cache_queries_once_then_hits() {
        let (dal, dir) = temp_dal("single");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        let id = table
            .insert(session.as_mut(), &[("Code", "X".into())])
            .unwrap();

        let cache = dal.single_cache("Item").unwrap();
        let first = cache
            .get(&dal, session.as_mut(), &[DbValue::Int(id)])
            .unwrap()
            .unwrap();
        assert_eq!(first.get_by_name("Code").unwrap().as_str(), Some("X"));
        assert!(cache.contains(&[DbValue::Int(id)]));

        // 绕过句柄改库：缓存命中返回旧值（命中证明）
        session
            .execute(
                "UPDATE \"DH_Item\" SET \"Code\" = ? WHERE \"Id\" = ?",
                &[DbValue::Text("Y".into()), DbValue::Int(id)],
            )
            .unwrap();
        let cached = cache
            .get(&dal, session.as_mut(), &[DbValue::Int(id)])
            .unwrap()
            .unwrap();
        assert_eq!(
            cached.get_by_name("Code").unwrap().as_str(),
            Some("X"),
            "未失效时应命中缓存"
        );

        // 表句柄更新 → 自动清空，重新查库拿到新值
        table
            .update_by_pk(session.as_mut(), &[("Code", "Z".into())], &[DbValue::Int(id)])
            .unwrap();
        let fresh = cache
            .get(&dal, session.as_mut(), &[DbValue::Int(id)])
            .unwrap()
            .unwrap();
        assert_eq!(
            fresh.get_by_name("Code").unwrap().as_str(),
            Some("Z"),
            "写入应自动使单对象缓存失效"
        );

        // 删除后查询：不缓存空值
        table
            .delete_by_pk(session.as_mut(), &[DbValue::Int(id)])
            .unwrap();
        assert!(
            cache
                .get(&dal, session.as_mut(), &[DbValue::Int(id)])
                .unwrap()
                .is_none()
        );
        assert_eq!(cache.count(), 0, "删除使缓存清空");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expire_reloads_when_elapsed() {
        let (dal, dir) = temp_dal("expire");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        table
            .insert(session.as_mut(), &[("Code", "A".into())])
            .unwrap();

        let cache = dal.entity_cache("Item").unwrap();
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 1);
        assert!(cache.is_valid());

        // 外部写入（绕过句柄）
        session
            .execute(
                "INSERT INTO \"DH_Item\" (\"Code\") VALUES (?)",
                &[DbValue::Text("B".into())],
            )
            .unwrap();

        // 过期时间置 0：任何访问都视为过期 → 同步重载
        cache.set_expire_seconds(0);
        assert!(!cache.is_valid());
        assert_eq!(cache.entities(&dal, session.as_mut()).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
