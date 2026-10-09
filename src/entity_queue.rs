//! 实体写队列（对应 DH.NCode 的 `Entity/EntityQueue.cs`）：凑批延迟写入。
//!
//! 把逐行写入请求先入队，达到批大小（默认 1000）或显式 [`EntityQueue::flush`] 时批量执行；
//! 每项按 Insert / Update / Delete / Upsert 分发，走标准表句柄（拦截器与缓存失效自动生效）；
//! **刷入时连续 Insert 段合并为多行批量插入、连续 Delete 段（单一主键）合并为主键 `IN` 批量删除**
//! （对应 C# `EntityQueue.OnProcess` 的 `batch.Insert` / `batch.Delete`）。
//!
//! 与 C# 版的差异：C# 由后台线程按周期（默认 1000ms）自动持久化；Rust 版由调用方驱动
//! `flush()`（入队时达到批大小也会自动 flush），需要异步时可在 `LazyConsumer` 任务中调用。

use crate::dal::Dal;
use crate::error::{Error, Result};
use crate::session::SqlSession;
use crate::value::DbValue;

/// 默认批大小。
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// 写入方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMethod {
    /// 插入
    Insert,
    /// 按主键更新
    Update,
    /// 按主键删除
    Delete,
    /// 存在即更新，否则插入
    Upsert,
}

/// 队列中的一项。
#[derive(Debug, Clone)]
struct QueueItem {
    /// 写入方法
    method: QueueMethod,
    /// 主键值（Update/Delete/Upsert 使用）
    pk: Vec<DbValue>,
    /// 列与值
    fields: Vec<(String, DbValue)>,
}

/// 队列执行统计。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    /// 插入数
    pub inserted: u64,
    /// 更新数
    pub updated: u64,
    /// 删除数
    pub deleted: u64,
}

impl QueueStats {
    /// 总执行数。
    pub fn total(&self) -> u64 {
        self.inserted + self.updated + self.deleted
    }
}

/// 实体写队列。
pub struct EntityQueue {
    /// 表名（模型实体名）
    table: String,
    /// 批大小
    batch_size: usize,
    /// 待执行项
    items: Vec<QueueItem>,
    /// 累计统计
    stats: QueueStats,
}

impl EntityQueue {
    /// 按表创建。
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            batch_size: DEFAULT_BATCH_SIZE,
            items: Vec::new(),
            stats: QueueStats::default(),
        }
    }

    /// 设置批大小（达到该大小时入队自动 flush）。
    pub fn set_batch_size(&mut self, size: usize) {
        self.batch_size = size.max(1);
    }

    /// 待执行项数。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 累计统计。
    pub fn stats(&self) -> QueueStats {
        self.stats
    }

    /// 入队插入。
    pub fn insert(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        fields: &[(&str, DbValue)],
    ) -> Result<()> {
        self.push(QueueMethod::Insert, Vec::new(), fields);
        self.auto_flush(dal, session)
    }

    /// 入队更新（按主键）。
    pub fn update(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
        fields: &[(&str, DbValue)],
    ) -> Result<()> {
        self.push(QueueMethod::Update, pk.to_vec(), fields);
        self.auto_flush(dal, session)
    }

    /// 入队删除（按主键）。
    pub fn delete(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
    ) -> Result<()> {
        self.push(QueueMethod::Delete, pk.to_vec(), &[]);
        self.auto_flush(dal, session)
    }

    /// 入队 Upsert（存在即更新，否则插入）。
    pub fn upsert(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        pk: &[DbValue],
        fields: &[(&str, DbValue)],
    ) -> Result<()> {
        self.push(QueueMethod::Upsert, pk.to_vec(), fields);
        self.auto_flush(dal, session)
    }

    /// 入队一项。
    fn push(&mut self, method: QueueMethod, pk: Vec<DbValue>, fields: &[(&str, DbValue)]) {
        self.items.push(QueueItem {
            method,
            pk,
            fields: fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        });
    }

    /// 达到批大小时自动 flush。
    fn auto_flush(&mut self, dal: &Dal, session: &mut dyn SqlSession) -> Result<()> {
        if self.items.len() >= self.batch_size {
            self.flush(dal, session)?;
        }
        Ok(())
    }

    /// 立即执行全部待办项（对应 C# `EntityQueue.OnProcess` 的批处理）：
    ///
    /// - **连续 Insert 段**合并为多行批量插入（对应 C# `batch.Insert` → `EntityExtension.Insert(list)`）；
    /// - **连续 Delete 段**（单一主键）合并为主键 `IN` 分批删除（对应 C# `batch.Delete`）；
    /// - Update / Upsert 逐条执行（Upsert 需逐条存在性判定，与 C# `BatchSave` 拆分前的语义一致）；
    /// - 段内成批执行 = 单条语句原子（与逐条相比不产生更多锁冲突）；事务仍由调用方按需包裹
    ///   （与 C# 一致，SQLite 下不要在外层无条件包事务）。
    pub fn flush(
        &mut self,
        dal: &Dal,
        session: &mut dyn SqlSession,
    ) -> Result<QueueStats> {
        let table = dal.table(&self.table)?;
        let mut stats = QueueStats::default();
        let items = std::mem::take(&mut self.items);
        let mut index = 0usize;
        while index < items.len() {
            match items[index].method {
                QueueMethod::Insert => {
                    // 连续 Insert 段：合并为多行批量插入
                    let start = index;
                    while index < items.len() && items[index].method == QueueMethod::Insert {
                        index += 1;
                    }
                    let rows: Vec<Vec<(&str, DbValue)>> = items[start..index]
                        .iter()
                        .map(|item| {
                            item.fields
                                .iter()
                                .map(|(name, value)| (name.as_str(), value.clone()))
                                .collect()
                        })
                        .collect();
                    stats.inserted += table.insert_batch(session, &rows, None)?;
                }
                QueueMethod::Delete => {
                    // 连续 Delete 段：单一主键时合并为主键 IN 批量删除
                    let start = index;
                    while index < items.len() && items[index].method == QueueMethod::Delete {
                        index += 1;
                    }
                    let run = &items[start..index];
                    let pks = table.meta().primary_keys();
                    if pks.len() == 1 && run.iter().all(|item| item.pk.len() == 1) {
                        let pk = pks[0].name.clone();
                        let values: Vec<DbValue> =
                            run.iter().map(|item| item.pk[0].clone()).collect();
                        stats.deleted += table.delete_by_pk_values(session, &pk, &values, None)?;
                    } else {
                        for item in run {
                            stats.deleted += table.delete_by_pk(session, &item.pk)?;
                        }
                    }
                }
                QueueMethod::Update => {
                    let item = &items[index];
                    let fields: Vec<(&str, DbValue)> = item
                        .fields
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.clone()))
                        .collect();
                    stats.updated += table.update_by_pk(session, &fields, &item.pk)?;
                    index += 1;
                }
                QueueMethod::Upsert => {
                    let item = &items[index];
                    let fields: Vec<(&str, DbValue)> = item
                        .fields
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.clone()))
                        .collect();
                    if table.find_by_pk(session, &item.pk)?.is_some() {
                        let affected = table.update_by_pk(session, &fields, &item.pk)?;
                        stats.updated += affected;
                    } else {
                        // 主键不在 fields 时补入
                        let mut with_pk = fields.clone();
                        let key_names: Vec<String> = table
                            .meta()
                            .primary_keys()
                            .iter()
                            .map(|c| c.name.clone())
                            .collect();
                        if key_names.len() != item.pk.len() {
                            return Err(Error::Model(format!(
                                "表 {} 主键数 {} 与提供值 {} 不匹配",
                                self.table,
                                key_names.len(),
                                item.pk.len()
                            )));
                        }
                        for (name, value) in key_names.iter().zip(item.pk.iter()) {
                            if !with_pk.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
                                with_pk.push((name.as_str(), value.clone()));
                            }
                        }
                        table.insert(session, &with_pk)?;
                        stats.inserted += 1;
                    }
                    index += 1;
                }
            }
        }
        self.stats.inserted += stats.inserted;
        self.stats.updated += stats.updated;
        self.stats.deleted += stats.deleted;
        Ok(stats)
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

    fn temp_dal(name: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-queue-{}-{stamp}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("queue.db");
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
    fn batches_and_flushes() {
        let (dal, dir) = temp_dal("batch");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();

        let mut queue = EntityQueue::new("Item");
        queue.set_batch_size(2);

        queue.insert(&dal, session.as_mut(), &[("Code", "A".into())]).unwrap();
        assert_eq!(queue.len(), 1, "未达批大小不执行");
        queue.insert(&dal, session.as_mut(), &[("Code", "B".into())]).unwrap();
        assert_eq!(queue.len(), 0, "达到批大小自动 flush");
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 2);

        // 手动 flush 剩余
        queue.insert(&dal, session.as_mut(), &[("Code", "C".into())]).unwrap();
        let stats = queue.flush(&dal, session.as_mut()).unwrap();
        assert_eq!(stats.inserted, 1);
        assert_eq!(queue.stats().total(), 3);
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_and_delete() {
        let (dal, dir) = temp_dal("upsert");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();
        let id = table
            .insert(session.as_mut(), &[("Code", "A".into())])
            .unwrap();

        let mut queue = EntityQueue::new("Item");
        // 已存在 → 更新
        queue
            .upsert(&dal, session.as_mut(), &[DbValue::Int(id)], &[("Code", "A2".into())])
            .unwrap();
        // 不存在 → 插入（自动补主键）
        queue
            .upsert(&dal, session.as_mut(), &[DbValue::Int(id + 100)], &[("Code", "NEW".into())])
            .unwrap();
        let stats = queue.flush(&dal, session.as_mut()).unwrap();
        assert_eq!(stats.updated, 1);
        assert_eq!(stats.inserted, 1);

        let row = table.find_by_pk(session.as_mut(), &[DbValue::Int(id)]).unwrap().unwrap();
        assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("A2"));

        // 删除
        queue.delete(&dal, session.as_mut(), &[DbValue::Int(id)]).unwrap();
        let stats = queue.flush(&dal, session.as_mut()).unwrap();
        assert_eq!(stats.deleted, 1);
        assert!(!table.exists_by_pk(session.as_mut(), &[DbValue::Int(id)]).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
