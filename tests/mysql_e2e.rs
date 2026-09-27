//! MySQL 端到端测试：需要真实 MySQL 服务器；未配置连接串时自动跳过。
//!
//! 运行方式（连接串请指向**测试库**）：
//!
//! ```powershell
//! $env:RCODE_MYSQL = "Server=127.0.0.1;Port=3306;Database=rcode_test;Uid=root;Pwd=root;provider=mysql;SslMode=None"
//! cargo test --test mysql_e2e
//! ```
//!
//! 安全说明：测试只创建/删除带 `rcode_test_` 前缀的专用表，读写不触碰其它表；
//! 测试结束会清理自己创建的表。

use pek_rcode::{Dal, DbRow, DbValue, Entity, EntityModel, Query, Result, Where};

/// 专用测试模型（表名统一 `rcode_test_` 前缀，避免污染业务库）
const MODEL: &str = r#"<EntityModel><Tables>
  <Table Name="RCodeTestItem" TableName="rcode_test_item" Description="驱动测试明细">
    <Columns>
      <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
      <Column Name="Code" DataType="String" Length="50" Description="编码" />
      <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" Description="金额" />
      <Column Name="Ok" DataType="Boolean" Description="是否有效" />
      <Column Name="SId" DataType="Int64" Nullable="True" Description="外部编号" />
      <Column Name="CreateTime" DataType="DateTime" Description="创建时间" />
      <Column Name="Data" DataType="Binary" Nullable="True" Description="附件" />
    </Columns>
  </Table>
  <Table Name="RCodeTestKey" TableName="rcode_test_key" Description="字符串主键测试">
    <Columns>
      <Column Name="Key" DataType="String" Length="40" PrimaryKey="True" Description="键" />
      <Column Name="Value" DataType="String" Length="100" Nullable="True" Description="值" />
    </Columns>
  </Table>
</Tables></EntityModel>"#;

/// 与 `rcodegen` 生成模式同构的实体（对应表 `rcode_test_item`）。
///
/// 说明：结构体按生成器输出格式手写，用于在集成测试中直接验证
/// “对象实体 + MySQL 驱动”整条链路；生成产物的可编译性另有 gencheck 临时工程验证。
#[derive(Debug, Clone, PartialEq)]
struct RCodeTestItem {
    id: i32,
    code: String,
    amount: rust_decimal::Decimal,
    ok: bool,
    s_id: Option<i64>,
    create_time: chrono::NaiveDateTime,
    data: Option<Vec<u8>>,
}

impl RCodeTestItem {
    /// 数据库表名。
    pub const TABLE_NAME: &'static str = "rcode_test_item";

    /// 按列类型默认值创建新实体。
    pub fn new() -> Self {
        Self {
            id: 0,
            code: String::new(),
            amount: Default::default(),
            ok: false,
            s_id: None,
            create_time: chrono::DateTime::UNIX_EPOCH.naive_utc(),
            data: None,
        }
    }
}

impl Default for RCodeTestItem {
    fn default() -> Self {
        Self::new()
    }
}

impl Entity for RCodeTestItem {
    fn table() -> &'static str {
        Self::TABLE_NAME
    }

    fn columns() -> &'static [&'static str] {
        &["Id", "Code", "Amount", "Ok", "SId", "CreateTime", "Data"]
    }

    fn primary_keys() -> &'static [&'static str] {
        &["Id"]
    }

    fn identity_column() -> Option<&'static str> {
        Some("Id")
    }

    fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
        vec![
            ("Id", self.id.into()),
            ("Code", self.code.clone().into()),
            ("Amount", self.amount.into()),
            ("Ok", self.ok.into()),
            ("SId", self.s_id.into()),
            ("CreateTime", self.create_time.into()),
            ("Data", self.data.clone().into()),
        ]
    }

    fn from_row(row: &DbRow) -> Result<Self> {
        Ok(Self {
            id: row.get_by_name("Id").and_then(DbValue::as_i32).unwrap_or_default(),
            code: row.get_by_name("Code").map(DbValue::to_text).unwrap_or_default(),
            amount: row.get_by_name("Amount").and_then(DbValue::as_decimal).unwrap_or_default(),
            ok: row.get_by_name("Ok").and_then(DbValue::as_bool).unwrap_or_default(),
            s_id: row.get_by_name("SId").and_then(DbValue::as_i64),
            create_time: row
                .get_by_name("CreateTime")
                .and_then(DbValue::as_datetime)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc()),
            data: row.get_by_name("Data").and_then(|v| v.as_blob().map(<[u8]>::to_vec)),
        })
    }

    fn set_identity(&mut self, value: i64) -> Result<()> {
        self.id = value as i32;
        Ok(())
    }
}

/// 与 `rcodegen` 生成模式同构的实体（对应表 `rcode_test_key`，字符串主键）。
#[derive(Debug, Clone, PartialEq)]
struct RCodeTestKey {
    key: String,
    value: Option<String>,
}

impl RCodeTestKey {
    /// 数据库表名。
    pub const TABLE_NAME: &'static str = "rcode_test_key";

    /// 按列类型默认值创建新实体。
    pub fn new() -> Self {
        Self {
            key: String::new(),
            value: None,
        }
    }
}

impl Default for RCodeTestKey {
    fn default() -> Self {
        Self::new()
    }
}

impl Entity for RCodeTestKey {
    fn table() -> &'static str {
        Self::TABLE_NAME
    }

    fn columns() -> &'static [&'static str] {
        &["Key", "Value"]
    }

    fn primary_keys() -> &'static [&'static str] {
        &["Key"]
    }

    fn to_fields(&self) -> Vec<(&'static str, DbValue)> {
        vec![
            ("Key", self.key.clone().into()),
            ("Value", self.value.clone().into()),
        ]
    }

    fn from_row(row: &DbRow) -> Result<Self> {
        Ok(Self {
            key: row.get_by_name("Key").map(DbValue::to_text).unwrap_or_default(),
            value: row.get_by_name("Value").and_then(|v| (!v.is_null()).then(|| v.to_text())),
        })
    }
}

/// 读取环境变量中的连接串（未设置时返回 None，测试跳过）。
fn connection_string() -> Option<String> {
    std::env::var("RCODE_MYSQL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 用专用连接执行清理（无论测试成败都调用）。
fn cleanup(dal: &Dal) {
    if let Ok(mut session) = dal.open_session() {
        let _ = session.execute("DROP TABLE IF EXISTS rcode_test_item", &[]);
        let _ = session.execute("DROP TABLE IF EXISTS rcode_test_key", &[]);
    }
}

#[test]
fn mysql_full_crud_roundtrip() {
    let Some(conn) = connection_string() else {
        eprintln!("未设置 RCODE_MYSQL，跳过 MySQL 端到端测试（示例见文件头注释）");
        return;
    };

    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    assert_eq!(dal.kind(), pek_rcode::DatabaseKind::MySql);

    // 清理历史残留并建表
    cleanup(&dal);
    let report = dal.sync_schema().unwrap();
    assert_eq!(report.created_tables.len(), 2, "{report}");
    assert!(dal.sync_schema().unwrap().is_empty(), "重复同步应无变更");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_roundtrip(&dal);
    }));

    cleanup(&dal);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// 具体用例：全部通过**对象实体（Entity）API**完成——insert/save/find/query/count/delete/事务。
fn run_roundtrip(dal: &Dal) {
    let mut session = dal.open_session().unwrap();

    // 新增（实体 insert）：自增主键回写到对象
    let created = chrono::NaiveDate::from_ymd_opt(2026, 9, 26)
        .unwrap()
        .and_hms_micro_opt(18, 1, 2, 123_456)
        .unwrap();

    let mut item1 = RCodeTestItem::new();
    item1.code = "HLT-0001".into();
    item1.amount = "12.3400".parse().unwrap();
    item1.ok = true;
    item1.s_id = Some(9_000_000_000);
    item1.create_time = created;
    item1.data = Some(vec![0u8, 1, 2, 255]);

    let id1 = item1.insert(dal, session.as_mut()).unwrap();
    assert!(id1 > 0, "应回写自增主键");
    assert_eq!(i64::from(item1.id), id1, "插入后主键应自动回写到实体");

    // 新增（实体 save：Id=0 视为新增）
    let mut item2 = RCodeTestItem::new();
    item2.code = "HLT-0002".into();
    item2.create_time = created;
    assert_eq!(item2.save(dal, session.as_mut()).unwrap(), 1);
    assert_eq!(i64::from(item2.id), id1 + 1, "自增应连续且 save 回写主键");

    // 按主键查询：实体全字段往返（Decimal 精度 / 布尔 TINYINT / Int64 / 微秒时间 / 二进制）
    let found = RCodeTestItem::find(dal, session.as_mut(), &[id1.into()])
        .unwrap()
        .unwrap();
    assert_eq!(found, item1, "实体全字段应原样往返");
    assert_eq!(found.create_time, created, "时间应保持微秒精度");

    // 未提供的可空列应为 NULL（Option 字段为 None）
    let found2 = RCodeTestItem::find(dal, session.as_mut(), &[item2.id.into()])
        .unwrap()
        .unwrap();
    assert_eq!(found2.s_id, None);
    assert_eq!(found2.data, None);

    // 条件统计 + 分页查询（实体 count/query/all）
    let filter = Where::new().like("Code", "HLT-%");
    assert_eq!(RCodeTestItem::count(dal, session.as_mut(), Some(&filter)).unwrap(), 2);

    let page1 = RCodeTestItem::query(
        dal,
        session.as_mut(),
        &Query::new().filter(filter.clone()).order_by("Id", true).page(1, 1),
    )
    .unwrap();
    assert_eq!(page1.len(), 1, "每页 1 条");
    assert_eq!(page1[0].code, "HLT-0002");

    let page2 = RCodeTestItem::query(
        dal,
        session.as_mut(),
        &Query::new().filter(filter).order_by("Id", true).page(2, 1),
    )
    .unwrap();
    assert_eq!(page2[0].code, "HLT-0001");

    assert_eq!(RCodeTestItem::all(dal, session.as_mut()).unwrap().len(), 2);

    // 更新（实体 save：Id != 0 视为更新）与删除（实体 delete）
    let mut edit = found.clone();
    edit.ok = false;
    assert_eq!(edit.save(dal, session.as_mut()).unwrap(), 1);
    let reloaded = RCodeTestItem::find(dal, session.as_mut(), &[id1.into()])
        .unwrap()
        .unwrap();
    assert!(!reloaded.ok, "save 更新应生效");

    assert_eq!(item2.delete(dal, session.as_mut()).unwrap(), 1);
    assert!(
        RCodeTestItem::find(dal, session.as_mut(), &[item2.id.into()])
            .unwrap()
            .is_none()
    );

    // 字符串主键实体：save 新增 → 查询 → save 更新 → 删除（覆盖 save 的两条分支）
    let mut key1 = RCodeTestKey::new();
    key1.key = "k-001".into();
    key1.value = Some("你好，MySQL".into());
    assert_eq!(key1.save(dal, session.as_mut()).unwrap(), 1, "无自增实体 save 应走新增");

    let loaded = RCodeTestKey::find(dal, session.as_mut(), &["k-001".into()])
        .unwrap()
        .unwrap();
    assert_eq!(loaded.value.as_deref(), Some("你好，MySQL"), "utf8mb4 中文应原样往返");

    let mut edit_key = loaded.clone();
    edit_key.value = Some("改过的值".into());
    assert_eq!(edit_key.save(dal, session.as_mut()).unwrap(), 1, "已存在实体 save 应走更新");
    let reloaded_key = RCodeTestKey::find(dal, session.as_mut(), &["k-001".into()])
        .unwrap()
        .unwrap();
    assert_eq!(reloaded_key.value.as_deref(), Some("改过的值"));

    assert_eq!(reloaded_key.delete(dal, session.as_mut()).unwrap(), 1);
    assert!(
        RCodeTestKey::find(dal, session.as_mut(), &["k-001".into()])
            .unwrap()
            .is_none()
    );

    // 事务：回滚后实体插入的数据不应保留
    session.begin().unwrap();
    let mut tx = RCodeTestItem::new();
    tx.code = "HLT-TX".into();
    tx.create_time = created;
    tx.insert(dal, session.as_mut()).unwrap();
    session.rollback().unwrap();

    let after_tx = RCodeTestItem::query(
        dal,
        session.as_mut(),
        &Query::new().filter(Where::new().eq("Code", "HLT-TX")),
    )
    .unwrap();
    assert!(after_tx.is_empty(), "回滚后不应存在事务内插入的数据");

    // 剩余数据核对（id1 仍在）
    assert_eq!(RCodeTestItem::count(dal, session.as_mut(), None).unwrap(), 1);
}

#[test]
fn mysql_connection_failure_is_reported() {
    // 未配置连接串时跳过
    if connection_string().is_none() {
        eprintln!("未设置 RCODE_MYSQL，跳过连接失败用例");
        return;
    }

    // 指向一个必然连不上的端口，应得到清晰错误而非 panic
    let dal = Dal::open("Server=127.0.0.1;Port=1;Database=x;Uid=x;Pwd=x;provider=mysql;timeout=2").unwrap();
    let err = match dal.open_session() {
        Ok(_) => panic!("端口 1 不应连接成功"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("连接 MySQL 失败"), "{err}");
}
