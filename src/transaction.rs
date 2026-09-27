//! 实体事务区域（对应 DH.NCode 的 `Entity/EntityTransaction.cs`）。
//!
//! RAII 语义（与 C# 的 `using` 等价）：
//! - [`EntityTransaction::new`] 立即开启事务
//! - 显式调用 [`EntityTransaction::commit`] 提交
//! - 未提交即离开作用域（panic/提前 return）时自动**回滚**
//!
//! ```no_run
//! # use pek_rcode::dal::Dal;
//! # use pek_rcode::transaction::EntityTransaction;
//! # fn demo(dal: &Dal) -> pek_rcode::Result<()> {
//! let mut session = dal.open_session()?;
//! let table = dal.table("JiLiYu")?;
//! let mut tx = EntityTransaction::new(session.as_mut())?;
//! table.insert(tx.session(), &[("Content", "A".into())])?;
//! table.insert(tx.session(), &[("Content", "B".into())])?;
//! tx.commit()?; // 不调用则自动回滚
//! # Ok(())
//! # }
//! ```

use crate::error::Result;
use crate::session::SqlSession;

/// 实体事务区域。
pub struct EntityTransaction<'a> {
    /// 数据库会话
    session: &'a mut dyn SqlSession,
    /// 是否已结束（提交或回滚）
    finished: bool,
}

impl<'a> EntityTransaction<'a> {
    /// 开启事务。
    pub fn new(session: &'a mut dyn SqlSession) -> Result<Self> {
        session.begin()?;
        Ok(Self {
            session,
            finished: false,
        })
    }

    /// 事务内的会话（执行实体操作时使用）。
    pub fn session(&mut self) -> &mut dyn SqlSession {
        self.session
    }

    /// 提交事务（消费自身）。
    pub fn commit(mut self) -> Result<()> {
        self.session.commit()?;
        self.finished = true;
        Ok(())
    }

    /// 显式回滚（消费自身）。
    pub fn rollback(mut self) -> Result<()> {
        self.session.rollback()?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for EntityTransaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // 未提交即离开作用域：回滚（忽略错误，与 C# 一致）
            let _ = self.session.rollback();
        }
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
            "rcode-tx-{}-{stamp}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("tx.db");
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
    fn commit_persists() {
        let (dal, dir) = temp_dal("commit");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();

        let mut tx = EntityTransaction::new(session.as_mut()).unwrap();
        table.insert(tx.session(), &[("Code", "A".into())]).unwrap();
        table.insert(tx.session(), &[("Code", "B".into())]).unwrap();
        tx.commit().unwrap();

        assert_eq!(table.count(session.as_mut(), None).unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_without_commit_rolls_back() {
        let (dal, dir) = temp_dal("rollback");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();

        {
            let mut tx = EntityTransaction::new(session.as_mut()).unwrap();
            table.insert(tx.session(), &[("Code", "X".into())]).unwrap();
            // 未 commit 即离开作用域 → 自动回滚
        }
        assert_eq!(table.count(session.as_mut(), None).unwrap(), 0);

        assert!(
            table
                .insert(session.as_mut(), &[("Code", "Y".into())])
                .map(|_| ())
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_rollback() {
        let (dal, dir) = temp_dal("explicit");
        let mut session = dal.open_session().unwrap();
        let table = dal.table("Item").unwrap();

        let mut tx = EntityTransaction::new(session.as_mut()).unwrap();
        table.insert(tx.session(), &[("Code", "Z".into())]).unwrap();
        tx.rollback().unwrap();

        assert_eq!(table.count(session.as_mut(), None).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
