//! 统计字段缓存（对应 DH.NCode 的 `FieldCache<TEntity>`）。
//!
//! 对指定字段做分组计数（`GROUP BY` + `COUNT(*)`），用于下拉筛选等场景；
//! 输出为 `(字段值, 个数)` 列表，`find_all_name` 可格式化为“显示名 (个数)”字典。
//!
//! 默认取前 50 组（`max_rows`）、缓存 600 秒（`expire`），与 C# 版一致。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::lock;
use crate::dal::Dal;
use crate::error::Result;
use crate::session::SqlSession;
use crate::value::DbValue;

/// 默认最大分组数。
pub const DEFAULT_MAX_ROWS: usize = 50;

/// 默认过期时间（秒）。
pub const DEFAULT_EXPIRE_SECONDS: u64 = 600;

/// 统计字段缓存。
pub struct FieldCache {
    /// 表名
    table: String,
    /// 统计字段
    field: String,
    /// 最大分组数
    max_rows: Mutex<usize>,
    /// 过期时间
    expire: Mutex<Duration>,
    /// 显示名格式（`{0}`=值、`{1}`=个数、`{1:n0}`=千分位个数）
    display_format: Mutex<String>,
    /// 缓存状态
    state: Mutex<FieldCacheState>,
}

/// 缓存状态。
#[derive(Default)]
struct FieldCacheState {
    /// 已加载的分组计数
    items: Option<Arc<Vec<(String, i64)>>>,
    /// 加载完成时刻
    loaded_at: Option<Instant>,
}

impl FieldCache {
    /// 按表与字段创建。
    pub fn new(table: &str, field: &str) -> Self {
        Self {
            table: table.to_string(),
            field: field.to_string(),
            max_rows: Mutex::new(DEFAULT_MAX_ROWS),
            expire: Mutex::new(Duration::from_secs(DEFAULT_EXPIRE_SECONDS)),
            display_format: Mutex::new("{0} ({1})".to_string()),
            state: Mutex::new(FieldCacheState::default()),
        }
    }

    /// 表名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 统计字段。
    pub fn field(&self) -> &str {
        &self.field
    }

    /// 设置最大分组数。
    pub fn set_max_rows(&self, max_rows: usize) {
        *lock(&self.max_rows) = max_rows;
    }

    /// 设置过期时间（秒）。
    pub fn set_expire_seconds(&self, seconds: u64) {
        *lock(&self.expire) = Duration::from_secs(seconds);
    }

    /// 设置显示名格式（默认 `"{0} ({1})"`）。
    pub fn set_display_format(&self, format: &str) {
        *lock(&self.display_format) = format.to_string();
    }

    /// 分组计数（带缓存；首次或过期时重新统计）。
    ///
    /// 字段值为 NULL 的行按空串归组（与 C# 版 `entity[field] + ""` 一致）。
    pub fn counts(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
    ) -> Result<Arc<Vec<(String, i64)>>> {
        {
            let expire = *lock(&self.expire);
            let state = lock(&self.state);
            if let (Some(items), Some(at)) = (&state.items, state.loaded_at)
                && at.elapsed() < expire
            {
                return Ok(items.clone());
            }
        }
        self.reload(dal, session)
    }

    /// 强制重新统计。
    pub fn reload(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<Arc<Vec<(String, i64)>>> {
        let kind = dal.kind();
        let field = kind.quote(&self.field);
        let table = kind.quote(&self.table);
        let value_alias = kind.quote("fc_value");
        let count_alias = kind.quote("group_count");
        let sql = format!(
            "SELECT {field} AS {value_alias}, COUNT(*) AS {count_alias} \
             FROM {table} GROUP BY {field}"
        );
        let order_sql = format!("ORDER BY {count_alias} DESC");
        let max_rows = *lock(&self.max_rows);
        let sql = kind.apply_paging(&sql, &order_sql, 0, max_rows);

        dal.log_sql(&sql);
        let set = session.query(&sql, &[])?;
        let mut items = Vec::with_capacity(set.len());
        for row in &set.rows {
            let value = row
                .get(0)
                .filter(|v| !v.is_null())
                .map(DbValue::to_text)
                .unwrap_or_default();
            let count = row.get(1).and_then(DbValue::as_i64).unwrap_or(0);
            items.push((value, count));
        }

        let items = Arc::new(items);
        let mut state = lock(&self.state);
        state.items = Some(items.clone());
        state.loaded_at = Some(Instant::now());
        Ok(items)
    }

    /// 下拉数据：字段值 → “显示名 (个数)”（对应 C# 的 `FindAllName()`）。
    pub fn find_all_name(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
    ) -> Result<BTreeMap<String, String>> {
        let items = self.counts(dal, session)?;
        let format = lock(&self.display_format).clone();
        let mut map = BTreeMap::new();
        for (value, count) in items.iter() {
            let display = format
                .replace("{1:n0}", &thousands(*count))
                .replace("{0}", value)
                .replace("{1}", &count.to_string());
            map.insert(value.clone(), display);
        }
        Ok(map)
    }

    /// 使缓存失效（下次访问重新统计）。
    pub fn invalidate(&self) {
        let mut state = lock(&self.state);
        state.items = None;
        state.loaded_at = None;
    }
}

/// 千分位格式化：`1234567` → `"1,234,567"`。
fn thousands(value: i64) -> String {
    let text = value.to_string();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.as_str()),
    };
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    format!("{sign}{out}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Goods" TableName="DH_Goods">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Category" DataType="String" Length="50" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    fn temp_dal(name: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-fieldcache-{}-{stamp}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("field.db");
        let model = EntityModel::parse(MODEL).expect("测试模型应可解析");
        let dal = Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db.display()),
            model,
        )
        .unwrap();
        dal.sync_schema().unwrap();
        (dal, dir)
    }

    fn insert_category(dal: &Dal, session: &mut dyn SqlSession, category: &str) {
        let table = dal.table("Goods").unwrap();
        table
            .insert(session, &[("Category", category.into())])
            .unwrap();
    }

    #[test]
    fn groups_and_caches() {
        let (dal, dir) = temp_dal("groups");
        let mut session = dal.open_session().unwrap();
        for category in ["A", "A", "A", "B", "B", "C"] {
            insert_category(&dal, session.as_mut(), category);
        }

        let cache = FieldCache::new("DH_Goods", "Category");
        let items = cache.counts(&dal, session.as_mut()).unwrap();
        assert_eq!(*items, vec![("A".to_string(), 3), ("B".to_string(), 2), ("C".to_string(), 1)]);

        // 命中缓存：绕过统计直接写库，结果不变
        insert_category(&dal, session.as_mut(), "D");
        let cached = cache.counts(&dal, session.as_mut()).unwrap();
        assert_eq!(cached.len(), 3, "未失效时应命中缓存");

        // 失效后重算
        cache.invalidate();
        let fresh = cache.counts(&dal, session.as_mut()).unwrap();
        assert_eq!(fresh.len(), 4);
        assert_eq!(fresh[0], ("A".to_string(), 3));

        // 下拉格式
        let names = cache.find_all_name(&dal, session.as_mut()).unwrap();
        assert_eq!(names.get("D").map(String::as_str), Some("D (1)"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limits_and_formats() {
        let (dal, dir) = temp_dal("limits");
        let mut session = dal.open_session().unwrap();
        for index in 0..5 {
            let category = format!("C{index}");
            for _ in 0..=index {
                insert_category(&dal, session.as_mut(), &category);
            }
        }

        let cache = FieldCache::new("DH_Goods", "Category");
        cache.set_max_rows(2);
        cache.set_display_format("{0} - {1:n0}");
        let names = cache.find_all_name(&dal, session.as_mut()).unwrap();
        assert_eq!(names.len(), 2, "max_rows 应限制分组数");
        assert_eq!(names.get("C4").map(String::as_str), Some("C4 - 5"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thousands_format() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(-12_345), "-12,345");
    }
}
