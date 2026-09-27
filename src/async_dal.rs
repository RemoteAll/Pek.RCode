//! 异步门面（对应 DH.NCode 的 `IAsyncDbSession` 与 `*Async` 方法面）。
//!
//! 设计：**统一以 `tokio::task::spawn_blocking` 包装同步核心**——14 个驱动一次性获得
//! 一致的异步 API（与 ADO.NET 异步实现大多为同步内核 + 线程池调度的方式一致；
//! 不改变驱动内部的同步本质，机制差异已在迁移台账记录）。
//!
//! 调用方需处于 tokio 运行时上下文（与扫码枪网关等 tokio 服务天然契合）：
//!
//! ```no_run
//! use pek_rcode::async_dal::AsyncDal;
//!
//! # async fn demo(model: pek_rcode::EntityModel) -> pek_rcode::Result<()> {
//! let dal = AsyncDal::open_with_model("Data Source=demo.db;Provider=SQLite", model)?;
//! let report = dal.sync_schema().await?;
//! println!("{report}");
//!
//! let mut session = dal.open_session().await?;
//! session
//!     .execute("DELETE FROM \"DH_Order\"".into(), vec![])
//!     .await?;
//! # Ok(()) }
//! ```

use std::sync::Arc;

use tokio::task::JoinError;

use crate::dal::{Dal, SchemaDiff, SchemaReport};
use crate::error::{Error, Result};
use crate::model::EntityModel;
use crate::session::{RowSet, SqlSession};
use crate::value::DbValue;

/// 异步数据访问层（同步 [`Dal`] 的 tokio 门面）。
pub struct AsyncDal {
    /// 同步内核（Arc 共享，可跨任务克隆使用）
    inner: Arc<Dal>,
}

impl AsyncDal {
    /// 包装同步 [`Dal`]。
    /// <param name="dal">同步数据访问层</param>
    /// <returns>异步门面</returns>
    pub fn new(dal: Dal) -> Self {
        Self {
            inner: Arc::new(dal),
        }
    }

    /// 按连接串打开（不加载模型）。
    /// <param name="conn_str">XCode 风格连接串</param>
    /// <returns>异步门面</returns>
    pub fn open(conn_str: &str) -> Result<Self> {
        Ok(Self::new(Dal::open(conn_str)?))
    }

    /// 按连接串打开并绑定模型。
    /// <param name="conn_str">XCode 风格连接串</param>
    /// <param name="model">实体模型</param>
    /// <returns>异步门面</returns>
    pub fn open_with_model(conn_str: &str, model: EntityModel) -> Result<Self> {
        Ok(Self::new(Dal::open_with_model(conn_str, model)?))
    }

    /// 同步内核引用（仅用于不跨越 `await` 的场景）。
    pub fn sync_dal(&self) -> &Dal {
        &self.inner
    }

    /// 通用异步执行：把任意同步操作（表操作/实体 CRUD/查询等）搬到阻塞线程池。
    ///
    /// 闭包在**阻塞线程**中执行，允许直接使用完整的同步 API：
    /// ```no_run
    /// # async fn demo(dal: &pek_rcode::async_dal::AsyncDal) -> pek_rcode::Result<()> {
    /// let count = dal
    ///     .run(|d| {
    ///         let mut session = d.open_session()?;
    ///         d.table("Order")?.count(session.as_mut(), None)
    ///     })
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Dal) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let dal = self.inner.clone();
        join(tokio::task::spawn_blocking(move || f(&dal)).await)
    }

    /// 打开一个异步会话（内部走连接池，`AsyncSession` 析构即归还）。
    pub async fn open_session(&self) -> Result<AsyncSession> {
        let session = self.run(|dal| dal.open_session()).await?;
        Ok(AsyncSession {
            inner: Some(session),
        })
    }

    /// 结构同步（建表 / 补列 / 补索引）。
    pub async fn sync_schema(&self) -> Result<SchemaReport> {
        self.run(|dal| dal.sync_schema()).await
    }

    /// 结构差异报告与 ALTER 脚本导出（dry-run）。
    pub async fn diff_schema(&self) -> Result<SchemaDiff> {
        self.run(|dal| dal.diff_schema()).await
    }

    /// 反向工程：数据库结构 → 模型。
    pub async fn read_model(&self) -> Result<EntityModel> {
        self.run(|dal| dal.read_model()).await
    }

    /// 便捷：在阻塞线程内完成“开会话 → 操作 → 归还”的完整流程。
    ///
    /// ```no_run
    /// # async fn demo(dal: &pek_rcode::async_dal::AsyncDal) -> pek_rcode::Result<()> {
    /// let count = dal
    ///     .with_session(|d, s| d.table("Order")?.count(s, None))
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub async fn with_session<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Dal, &mut dyn SqlSession) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.run(|dal| {
            let mut session = dal.open_session()?;
            f(dal, session.as_mut())
        })
        .await
    }
}

impl std::fmt::Debug for AsyncDal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncDal")
            .field("kind", &self.inner.kind())
            .finish()
    }
}

/// 异步会话：内部持有同步会话，逐条调用搬到阻塞线程池执行。
pub struct AsyncSession {
    /// 同步会话（同一时刻仅有本对象持有；执行期间短暂移出）
    inner: Option<Box<dyn SqlSession>>,
}

impl AsyncSession {
    /// 取出会话（若已关闭则报错）。
    fn take(&mut self) -> Result<Box<dyn SqlSession>> {
        self.inner
            .take()
            .ok_or_else(|| Error::Model("异步会话已关闭".into()))
    }

    /// 执行语句（INSERT/UPDATE/DELETE/DDL）。参数需携带所有权进入阻塞线程。
    /// <param name="sql">SQL 语句</param>
    /// <param name="params">参数</param>
    /// <returns>受影响行数</returns>
    pub async fn execute(&mut self, sql: String, params: Vec<DbValue>) -> Result<u64> {
        let session = self.take()?;
        let (session, value) = run_session(session, move |s| s.execute(&sql, &params)).await?;
        self.inner = Some(session);
        Ok(value)
    }

    /// 执行查询。
    /// <param name="sql">SQL 语句</param>
    /// <param name="params">参数</param>
    /// <returns>结果集</returns>
    pub async fn query(&mut self, sql: String, params: Vec<DbValue>) -> Result<RowSet> {
        let session = self.take()?;
        let (session, value) = run_session(session, move |s| s.query(&sql, &params)).await?;
        self.inner = Some(session);
        Ok(value)
    }

    /// 开启事务。
    pub async fn begin(&mut self) -> Result<()> {
        let session = self.take()?;
        let (session, value) = run_session(session, |s| s.begin()).await?;
        self.inner = Some(session);
        Ok(value)
    }

    /// 提交事务。
    pub async fn commit(&mut self) -> Result<()> {
        let session = self.take()?;
        let (session, value) = run_session(session, |s| s.commit()).await?;
        self.inner = Some(session);
        Ok(value)
    }

    /// 回滚事务。
    pub async fn rollback(&mut self) -> Result<()> {
        let session = self.take()?;
        let (session, value) = run_session(session, |s| s.rollback()).await?;
        self.inner = Some(session);
        Ok(value)
    }

    /// 最近一次自增主键值（按表名推导，Oracle/DB2/Firebird 使用）。
    /// <param name="table">表名</param>
    /// <returns>自增值</returns>
    pub async fn last_identity_of(&mut self, table: String) -> Result<i64> {
        let session = self.take()?;
        let (session, value) = run_session(session, move |s| s.last_identity_of(&table)).await?;
        self.inner = Some(session);
        Ok(value)
    }

    /// 表是否存在。
    /// <param name="table">表名</param>
    /// <returns>是否存在</returns>
    pub async fn table_exists(&mut self, table: String) -> Result<bool> {
        let session = self.take()?;
        let (session, value) = run_session(session, move |s| s.table_exists(&table)).await?;
        self.inner = Some(session);
        Ok(value)
    }

    /// 现有表的列名列表。
    /// <param name="table">表名</param>
    /// <returns>列名</returns>
    pub async fn table_columns(&mut self, table: String) -> Result<Vec<String>> {
        let session = self.take()?;
        let (session, value) = run_session(session, move |s| s.table_columns(&table)).await?;
        self.inner = Some(session);
        Ok(value)
    }
}

impl std::fmt::Debug for AsyncSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncSession").finish()
    }
}

/// 在阻塞线程执行会话操作，并把会话带回。
async fn run_session<T, F>(
    mut session: Box<dyn SqlSession>,
    f: F,
) -> Result<(Box<dyn SqlSession>, T)>
where
    F: FnOnce(&mut dyn SqlSession) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::task::spawn_blocking(move || {
        let value = f(session.as_mut());
        (session, value)
    });
    match handle.await {
        Ok((session, value)) => Ok((session, value?)),
        Err(e) => Err(Error::Db(format!("异步任务执行失败：{e}"))),
    }
}

/// 统一处理 `JoinError`（任务 panic / 被取消）。
fn join<T>(result: std::result::Result<Result<T>, JoinError>) -> Result<T> {
    match result {
        Ok(value) => value,
        Err(e) => Err(Error::Db(format!("异步任务执行失败：{e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order" Description="订单">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
        <Column Name="Status" DataType="Int32" />
        <Column Name="CreateTime" DataType="DateTime" />
      </Columns>
      <Indexes><Index Columns="Code" Unique="True" /></Indexes>
    </Table></Tables></EntityModel>"#;

    #[test]
    fn async_sqlite_roundtrip() {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-async-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("async.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());

        let dal = AsyncDal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // 结构同步 + 差异报告（异步）
            let report = dal.sync_schema().await.unwrap();
            assert_eq!(report.created_tables, vec!["DH_Order"]);
            let diff = dal.diff_schema().await.unwrap();
            assert!(diff.is_empty(), "{diff:?}");

            // 会话级异步 CRUD
            let mut session = dal.open_session().await.unwrap();
            let affected = session
                .execute(
                    "INSERT INTO \"DH_Order\" (\"Code\", \"Status\", \"CreateTime\") VALUES (?, ?, ?)"
                        .into(),
                    vec![
                        "A1".into(),
                        1.into(),
                        chrono::Local::now().naive_local().into(),
                    ],
                )
                .await
                .unwrap();
            assert_eq!(affected, 1);

            let set = session
                .query("SELECT COUNT(*) AS C FROM \"DH_Order\"".into(), vec![])
                .await
                .unwrap();
            assert_eq!(set.rows[0].get(0).and_then(DbValue::as_i64), Some(1));

            // 事务回滚
            session.begin().await.unwrap();
            session
                .execute("DELETE FROM \"DH_Order\"".into(), vec![])
                .await
                .unwrap();
            session.rollback().await.unwrap();
            let set = session
                .query("SELECT COUNT(*) AS C FROM \"DH_Order\"".into(), vec![])
                .await
                .unwrap();
            assert_eq!(set.rows[0].get(0).and_then(DbValue::as_i64), Some(1));
            drop(session);

            // 表操作：run 闭包（完整同步 API 可用）
            let identity_column = dal
                .run(|d| {
                    let table = d.table("Order")?;
                    Ok(table.meta().identity().map(|c| c.name.clone()))
                })
                .await
                .unwrap();
            assert_eq!(identity_column.as_deref(), Some("Id"));

            // with_session 便捷通道
            let count = dal
                .with_session(|d, s| d.table("Order")?.count(s, None))
                .await
                .unwrap();
            assert_eq!(count, 1);

            // 反向工程（异步）
            let model = dal.read_model().await.unwrap();
            assert_eq!(model.tables.len(), 1);
            assert_eq!(model.tables[0].indexes.len(), 1);
        });

        dal.sync_dal().clear_pool();
        drop(rt);
        drop(dal);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
