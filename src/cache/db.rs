//! 数据库缓存（对应 DH.NCode 的 `DbCache`）：用一张数据表当 KV 缓存后端（不依赖 Redis）。
//!
//! 约定：目标表必须在模型中，且为**单一字符串主键**（作为缓存键）；
//! 值/过期时间/创建时间三列列名可配置（默认 `Value` / `ExpiredTime` / `CreateTime`）。
//! 值以文本存储（JSON 序列化由调用方自行处理，与 C# 版存 JSON 文本一致）。
//!
//! 与 C# 版的差异：C# 通过 `TimerX` 定时清理过期项；Rust 版提供显式
//! [`DbCache::remove_expired`]（由调用方定时或手动触发）。

use chrono::Local;

use crate::dal::Dal;
use crate::error::{Error, Result};
use crate::session::{DbRow, SqlSession};
use crate::types::DataType;
use crate::value::DbValue;

/// 数据库缓存。
pub struct DbCache {
    /// 表名（模型中的表名或实体名）
    table: String,
    /// 值列
    value_column: String,
    /// 过期时间列
    expire_column: String,
    /// 创建时间列
    create_column: String,
}

impl DbCache {
    /// 按表名创建（默认列名 `Value` / `ExpiredTime` / `CreateTime`）。
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            value_column: "Value".into(),
            expire_column: "ExpiredTime".into(),
            create_column: "CreateTime".into(),
        }
    }

    /// 自定义值/过期/创建三列的列名。
    pub fn with_columns(mut self, value: &str, expire: &str, create: &str) -> Self {
        self.value_column = value.into();
        self.expire_column = expire.into();
        self.create_column = create.into();
        self
    }

    /// 表名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 读取缓存项（不存在或已过期返回 `None`）。
    pub fn get(&self, dal: &Dal, session: &mut dyn SqlSession, key: &str) -> Result<Option<String>> {
        let Some(row) = self.find(dal, session, key)? else {
            return Ok(None);
        };
        if self.is_expired(&row) {
            return Ok(None);
        }
        Ok(row.get_by_name(&self.value_column).map(DbValue::to_text))
    }

    /// 缓存项是否存在且未过期。
    pub fn contains(&self, dal: &Dal, session: &mut dyn SqlSession, key: &str) -> Result<bool> {
        Ok(self.get(dal, session, key)?.is_some())
    }

    /// 写入缓存项（已存在时更新；`expire_seconds` 至少 1 秒）。
    pub fn set(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        key: &str,
        value: &str,
        expire_seconds: i64,
    ) -> Result<()> {
        let key_column = self.key_column(dal)?;
        let table = dal.table(&self.table)?;
        let now = Local::now().naive_local();
        let expires = now + chrono::TimeDelta::seconds(expire_seconds.max(1));

        if table
            .find_by_pk(session, &[DbValue::Text(key.to_string())])?
            .is_some()
        {
            table.update_by_pk(
                session,
                &[
                    (self.value_column.as_str(), DbValue::Text(value.to_string())),
                    (self.expire_column.as_str(), DbValue::DateTime(expires)),
                ],
                &[DbValue::Text(key.to_string())],
            )?;
        } else {
            table.insert(
                session,
                &[
                    (key_column.as_str(), DbValue::Text(key.to_string())),
                    (self.value_column.as_str(), DbValue::Text(value.to_string())),
                    (self.expire_column.as_str(), DbValue::DateTime(expires)),
                    (self.create_column.as_str(), DbValue::DateTime(now)),
                ],
            )?;
        }
        Ok(())
    }

    /// 新增缓存项（已存在时返回 `false`，对应 C# 的 `Add`）。
    pub fn add(
        &self,
        dal: &Dal,
        session: &mut dyn SqlSession,
        key: &str,
        value: &str,
        expire_seconds: i64,
    ) -> Result<bool> {
        if self.find(dal, session, key)?.is_some() {
            return Ok(false);
        }
        self.set(dal, session, key, value, expire_seconds)?;
        Ok(true)
    }

    /// 移除缓存项，返回受影响行数。
    pub fn remove(&self, dal: &Dal, session: &mut dyn SqlSession, key: &str) -> Result<u64> {
        let table = dal.table(&self.table)?;
        table.delete_by_pk(session, &[DbValue::Text(key.to_string())])
    }

    /// 清除全部缓存项（保留表结构），返回删除行数。
    pub fn clear(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<u64> {
        let kind = dal.kind();
        let table = dal.table(&self.table)?.meta().effective_table_name().to_string();
        let sql = format!("DELETE FROM {}", kind.quote(&table));
        dal.log_sql(&sql);
        session.execute(&sql, &[])
    }

    /// 删除已过期缓存项，返回删除行数（替代 C# 的定时清理）。
    pub fn remove_expired(&self, dal: &Dal, session: &mut dyn SqlSession) -> Result<u64> {
        let kind = dal.kind();
        let table = dal.table(&self.table)?.meta().effective_table_name().to_string();
        let sql = format!(
            "DELETE FROM {} WHERE {} < {}",
            kind.quote(&table),
            kind.quote(&self.expire_column),
            kind.placeholder(0)
        );
        dal.log_sql(&sql);
        session.execute(&sql, &[DbValue::DateTime(Local::now().naive_local())])
    }

    /// 按主键查找行。
    fn find(&self, dal: &Dal, session: &mut dyn SqlSession, key: &str) -> Result<Option<DbRow>> {
        let table = dal.table(&self.table)?;
        table.find_by_pk(session, &[DbValue::Text(key.to_string())])
    }

    /// 是否已过期（无过期时间视为不过期）。
    fn is_expired(&self, row: &DbRow) -> bool {
        row.get_by_name(&self.expire_column)
            .and_then(DbValue::as_datetime)
            .is_some_and(|value| value < Local::now().naive_local())
    }

    /// 键列名（要求单一字符串主键）。
    fn key_column(&self, dal: &Dal) -> Result<String> {
        let table = dal.table(&self.table)?;
        let keys = table.meta().primary_keys();
        match keys.as_slice() {
            [key] if key.data_type == DataType::String => Ok(key.name.clone()),
            _ => Err(Error::Model(format!(
                "数据库缓存表 {} 需要单一字符串主键（作为缓存键）",
                self.table
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dal::Dal;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="SysCache" TableName="DH_SysCache">
      <Columns>
        <Column Name="Name" DataType="String" Length="50" PrimaryKey="True" />
        <Column Name="Value" DataType="String" />
        <Column Name="ExpiredTime" DataType="DateTime" />
        <Column Name="CreateTime" DataType="DateTime" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    fn temp_dal(name: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-dbcache-{}-{stamp}-{name}",
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
    fn set_get_add_remove_lifecycle() {
        let (dal, dir) = temp_dal("life");
        let mut session = dal.open_session().unwrap();
        let cache = DbCache::new("SysCache");

        assert!(!cache.contains(&dal, session.as_mut(), "k1").unwrap());
        cache
            .set(&dal, session.as_mut(), "k1", "v1", 60)
            .unwrap();
        assert_eq!(
            cache.get(&dal, session.as_mut(), "k1").unwrap().as_deref(),
            Some("v1")
        );

        // add 语义：已存在返回 false，不覆盖
        assert!(!cache.add(&dal, session.as_mut(), "k1", "v9", 60).unwrap());
        assert_eq!(
            cache.get(&dal, session.as_mut(), "k1").unwrap().as_deref(),
            Some("v1")
        );
        assert!(cache.add(&dal, session.as_mut(), "k2", "v2", 60).unwrap());

        // set 覆盖
        cache
            .set(&dal, session.as_mut(), "k1", "v1b", 60)
            .unwrap();
        assert_eq!(
            cache.get(&dal, session.as_mut(), "k1").unwrap().as_deref(),
            Some("v1b")
        );

        // 移除
        assert_eq!(cache.remove(&dal, session.as_mut(), "k1").unwrap(), 1);
        assert_eq!(cache.get(&dal, session.as_mut(), "k1").unwrap(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expiry_and_cleanup() {
        let (dal, dir) = temp_dal("expire");
        let mut session = dal.open_session().unwrap();
        let cache = DbCache::new("SysCache");

        cache
            .set(&dal, session.as_mut(), "old", "v", 60)
            .unwrap();
        cache
            .set(&dal, session.as_mut(), "live", "v", 60)
            .unwrap();

        // 手工把 old 的过期时间改成过去
        session
            .execute(
                "UPDATE \"DH_SysCache\" SET \"ExpiredTime\" = ? WHERE \"Name\" = ?",
                &[
                    DbValue::DateTime(Local::now().naive_local() - chrono::TimeDelta::hours(1)),
                    DbValue::Text("old".into()),
                ],
            )
            .unwrap();

        assert_eq!(cache.get(&dal, session.as_mut(), "old").unwrap(), None, "过期项不应返回");
        assert!(cache.get(&dal, session.as_mut(), "live").unwrap().is_some());

        // 清理过期项
        assert_eq!(cache.remove_expired(&dal, session.as_mut()).unwrap(), 1);

        // 全清
        assert_eq!(cache.clear(&dal, session.as_mut()).unwrap(), 1);
        assert!(!cache.contains(&dal, session.as_mut(), "live").unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requires_string_primary_key() {
        const BAD_MODEL: &str = r#"<EntityModel><Tables><Table Name="NoKey" TableName="DH_NoKey">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Value" DataType="String" />
          </Columns>
        </Table></Tables></EntityModel>"#;

        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-dbcache-{}-{stamp}-bad",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("bad.db");
        let model = EntityModel::parse(BAD_MODEL).unwrap();
        let dal = Dal::open_with_model(
            &format!("Data Source={};Provider=SQLite", db.display()),
            model,
        )
        .unwrap();
        dal.sync_schema().unwrap();

        let mut session = dal.open_session().unwrap();
        let cache = DbCache::new("NoKey");
        let err = cache
            .set(&dal, session.as_mut(), "k", "v", 60)
            .unwrap_err();
        assert!(err.to_string().contains("单一字符串主键"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
