//! 实体对象批量查找器（对应 DH.NCode 的 `BatchFinder<TKey, TEntity>`）。
//!
//! 把 N 次按主键的点查合并为少量 `IN` 批查（默认批大小 500），并缓存结果；
//! 适合“一屏引用多行关联数据（如多张单据的产品/客户）”的场景。
//!
//! 语义（与 C# 版一致）：
//! - 键集合去重且保持顺序；查找时先查缓存，未命中则**从当前位置向前分批** `IN` 查询
//! - 键为 0/空值时直接返回 `None`；键不在集合内时报错
//! - 全部批扫描完仍未命中返回 `None`

use std::collections::HashMap;
use std::sync::Arc;

use crate::dal::Dal;
use crate::error::{Error, Result};
use crate::model::TableMeta;
use crate::query::{Query, Where};
use crate::session::{DbRow, SqlSession};
use crate::value::DbValue;

/// 默认批大小。
pub const DEFAULT_BATCH_SIZE: usize = 500;

/// 实体对象批量查找器。
pub struct BatchFinder {
    /// 表名（模型实体名）
    table: String,
    /// 主键列（实际列名）
    key_column: String,
    /// 去重后的主键集合
    keys: Vec<DbValue>,
    /// 缓存（键文本 → 行）
    cache: HashMap<String, Arc<DbRow>>,
    /// 已扫描位置
    index: usize,
    /// 批大小
    batch_size: usize,
}

impl BatchFinder {
    /// 按表创建（要求单一主键）。
    pub fn new(table: &TableMeta) -> Result<Self> {
        let keys = table.primary_keys();
        let [key] = keys.as_slice() else {
            return Err(Error::Model(format!(
                "批量查找要求单一主键：表 {}",
                table.name
            )));
        };
        Ok(Self {
            table: table.name.clone(),
            key_column: table.effective_column_name(key).to_string(),
            keys: Vec::new(),
            cache: HashMap::new(),
            index: 0,
            batch_size: DEFAULT_BATCH_SIZE,
        })
    }

    /// 按表与键集合创建。
    pub fn with_keys(
        table: &TableMeta,
        keys: impl IntoIterator<Item = DbValue>,
    ) -> Result<Self> {
        let mut finder = Self::new(table)?;
        finder.add(keys);
        Ok(finder)
    }

    /// 添加主键（去重，保持顺序；可多次调用）。
    pub fn add(&mut self, keys: impl IntoIterator<Item = DbValue>) {
        for key in keys {
            let text = key.to_text();
            if !self.keys.iter().any(|existing| existing.to_text() == text) {
                self.keys.push(key);
            }
        }
    }

    /// 设置批大小。
    pub fn set_batch_size(&mut self, size: usize) {
        self.batch_size = size.max(1);
    }

    /// 键集合。
    pub fn keys(&self) -> &[DbValue] {
        &self.keys
    }

    /// 已缓存行数。
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// 查找（先查缓存；未命中按批 `IN` 加载；键不在集合内报错；0/空键返回 `None`）。
    pub fn find(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        key: &DbValue,
    ) -> Result<Option<Arc<DbRow>>> {
        let text = key.to_text();

        // 先查缓存
        if let Some(row) = self.cache.get(&text) {
            return Ok(Some(Arc::clone(row)));
        }
        // 0/空键
        if is_empty_key(key) {
            return Ok(None);
        }
        // 必须在集合内（与 C# 一致：防止误用）
        if !self.keys.iter().any(|existing| existing.to_text() == text) {
            return Err(Error::Model(format!("键 {text} 不在批量查找集合内")));
        }

        let table = dal.table(&self.table)?;
        while self.index < self.keys.len() {
            let end = (self.index + self.batch_size).min(self.keys.len());
            let batch: Vec<DbValue> = self.keys[self.index..end].to_vec();
            self.index = end;

            let filter = Where::new().in_(self.key_column.as_str(), batch);
            let set = table.query(session, &Query::new().filter(filter))?;
            for row in set.rows {
                if let Some(key_value) = row.get_by_name(&self.key_column) {
                    self.cache.insert(key_value.to_text(), Arc::new(row));
                }
            }

            if let Some(row) = self.cache.get(&text) {
                return Ok(Some(Arc::clone(row)));
            }
        }
        Ok(None)
    }
}

/// 空键判断（整数 0 / 空串 / NULL，与 C# 一致）。
fn is_empty_key(key: &DbValue) -> bool {
    match key {
        DbValue::Null => true,
        DbValue::Int(value) => *value == 0,
        DbValue::Text(value) => value.is_empty(),
        _ => false,
    }
}

impl<'a> crate::dal::TableRef<'a> {
    /// 创建批量查找器（对应 `BatchFinder`）。
    pub fn batch_finder(&self) -> Result<BatchFinder> {
        BatchFinder::new(self.meta())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Goods" TableName="DH_Goods">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    fn temp_dal(name: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-batch-{}-{stamp}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("batch.db");
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
    fn batch_find_merges_queries_and_caches() {
        let (dal, dir) = temp_dal("find");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Goods").unwrap();
        let mut ids = Vec::new();
        for index in 1..=6 {
            let code = format!("C{index}");
            ids.push(
                table
                    .insert(session.as_mut(), &[("Code", code.into())])
                    .unwrap(),
            );
        }

        // 只取第 1、3、5 个键；批大小 2 → 查找第 5 个会触发多批
        let mut finder = table.batch_finder().unwrap();
        finder.add([
            DbValue::Int(ids[0]),
            DbValue::Int(ids[2]),
            DbValue::Int(ids[4]),
        ]);
        finder.set_batch_size(2);

        let first = finder
            .find(&dal, session.as_mut(), &DbValue::Int(ids[0]))
            .unwrap()
            .expect("应命中");
        assert_eq!(first.get_by_name("Code").unwrap().as_str(), Some("C1"));

        let third = finder
            .find(&dal, session.as_mut(), &DbValue::Int(ids[2]))
            .unwrap()
            .expect("应命中");
        assert_eq!(third.get_by_name("Code").unwrap().as_str(), Some("C3"));

        let fifth = finder
            .find(&dal, session.as_mut(), &DbValue::Int(ids[4]))
            .unwrap()
            .expect("应命中");
        assert_eq!(fifth.get_by_name("Code").unwrap().as_str(), Some("C5"));

        // 未在集合中的键 → 报错
        let err = finder
            .find(&dal, session.as_mut(), &DbValue::Int(ids[1]))
            .unwrap_err();
        assert!(err.to_string().contains("不在批量查找集合内"), "{err}");

        // 集合内但数据库不存在的键 → None
        finder.add([DbValue::Int(999)]);
        assert!(
            finder
                .find(&dal, session.as_mut(), &DbValue::Int(999))
                .unwrap()
                .is_none()
        );

        // 0 键 → None（不报错）
        assert!(
            finder
                .find(&dal, session.as_mut(), &DbValue::Int(0))
                .unwrap()
                .is_none()
        );

        // 缓存已建立
        assert!(finder.cache_len() >= 3);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
