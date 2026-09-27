//! `#[derive(Entity)]` 集成测试：手写实体 + 派生宏 + SQLite 全链路。
//!
//! 对应 DH.NCode 中“开发者手写实体类（继承 Entity 基类）”的用法；
//! 与 `rcodegen` 生成的完整实体（含 impl）互为补充。

use chrono::NaiveDateTime;
use pek_rcode::{Dal, Entity, EntityModel, Query, Result, Where};
use pek_rcode_derive::Entity;
use rust_decimal::Decimal;

/// 演示实体：字段名与列名不同大小写（按忽略大小写匹配），个别列显式指定。
#[derive(Debug, Clone, PartialEq, Entity)]
#[entity(table = "DH_Order")]
pub struct DemoOrder {
    /// 主键（自增）
    #[entity(identity, primary_key)]
    pub id: i32,
    /// 编码
    #[entity(column = "Code")]
    pub code: Option<String>,
    /// 状态
    pub status: i32,
    /// 金额
    pub amount: Option<Decimal>,
    /// 标记
    pub ok: bool,
    /// 创建时间
    pub created: NaiveDateTime,
}

const MODEL: &str = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="Code" DataType="String" Length="50" Nullable="True" />
    <Column Name="Status" DataType="Int32" />
    <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" Nullable="True" />
    <Column Name="Ok" DataType="Boolean" />
    <Column Name="Created" DataType="DateTime" />
  </Columns>
  <Indexes><Index Columns="Code" Unique="True" /></Indexes>
</Table></Tables></EntityModel>"#;

/// 建临时库并返回（库路径, Dal）。
fn setup(name: &str) -> Result<(std::path::PathBuf, Dal)> {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.6f")
        .to_string()
        .replace('.', "");
    let dir = std::env::temp_dir().join(format!("rcode-derive-{}-{stamp}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("derive.db");
    let dal = Dal::open_with_model(
        &format!("Data Source={};Provider=SQLite", db.display()),
        EntityModel::parse(MODEL)?,
    )?;
    dal.sync_schema()?;
    Ok((dir, dal))
}

#[test]
fn derive_entity_full_roundtrip() -> Result<()> {
    let (dir, dal) = setup("roundtrip")?;
    let mut session = dal.open_session()?;

    // 新增（自增主键回写）
    let mut order = DemoOrder {
        id: 0,
        code: Some("HLT-001".into()),
        status: 1,
        amount: Some("12.3400".parse::<Decimal>().unwrap()),
        ok: true,
        created: chrono::Local::now().naive_local(),
    };
    let id = order.insert(&dal, session.as_mut())?;
    assert!(id > 0, "应回写自增主键");
    assert_eq!(order.id, id as i32);

    // 按主键查询（字段与列名忽略大小写匹配）
    let found = DemoOrder::find(&dal, session.as_mut(), &[order.id.into()])?.expect("应能查到");
    assert_eq!(found, order);

    // 条件查询与统计
    let list = DemoOrder::query(
        &dal,
        session.as_mut(),
        &Query::new().filter(Where::new().like("Code", "HLT%")),
    )?;
    assert_eq!(list.len(), 1);
    assert_eq!(
        DemoOrder::count(&dal, session.as_mut(), Some(&Where::new().eq("Status", 1)))?,
        1
    );

    // 保存（有自增：Id 非 0 → 更新）
    let mut order = found;
    order.status = 9;
    order.code = None;
    order.save(&dal, session.as_mut())?;
    let again = DemoOrder::find(&dal, session.as_mut(), &[order.id.into()])?.unwrap();
    assert_eq!(again.status, 9);
    assert_eq!(again.code, None);

    // 删除
    assert_eq!(order.delete(&dal, session.as_mut())?, 1);
    assert!(DemoOrder::find(&dal, session.as_mut(), &[order.id.into()])?.is_none());
    assert_eq!(DemoOrder::count(&dal, session.as_mut(), None)?, 0);

    drop(session);
    dal.clear_pool();
    drop(dal);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
