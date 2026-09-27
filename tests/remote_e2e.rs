//! PostgreSQL / SQL Server / Oracle 端到端测试：需要真实服务器；未配置连接串时自动跳过。
//!
//! 运行方式（连接串请指向**测试库**）：
//!
//! ```powershell
//! # PostgreSQL（含瀚高/金仓/海量，同协议）
//! $env:RCODE_POSTGRES = "Server=127.0.0.1;Port=5432;Database=rcode_test;Uid=postgres;Pwd=***;provider=postgresql"
//! cargo test --test remote_e2e
//!
//! # SQL Server
//! $env:RCODE_MSSQL = "Server=127.0.0.1;Port=1433;Database=rcode_test;Uid=sa;Pwd=***;provider=sqlserver"
//!
//! # Oracle（需 Instant Client；ServiceName 指向测试服务）
//! $env:RCODE_ORACLE = "Server=127.0.0.1;Port=1521;ServiceName=xepdb1;Uid=rcode;Pwd=***;provider=oracle"
//!
//! # network（SQL 转发到远端 XCode DbServer；服务端可为 C# `DbServer` 或本仓 examples/dbserver）
//! cargo run --example dbserver -- "Data Source=demo.db;Provider=SQLite" 3305 tk123
//! $env:RCODE_NETWORK = "Server=http://127.0.0.1:3305;Database=Demo;Password=tk123;provider=network"
//! ```
//!
//! 安全说明：测试只创建/删除带 `rcode_test_` 前缀的专用表（Oracle 另含 `SEQ_rcode_test_item` 序列），
//! 读写不触碰其它表；测试前后均会清理自己创建的对象。

use pek_rcode::{Dal, DatabaseKind, DbRow, DbValue, Entity, EntityModel, Query, Result, Where};

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
            amount: row
                .get_by_name("Amount")
                .and_then(DbValue::as_decimal)
                .unwrap_or_default(),
            ok: row.get_by_name("Ok").and_then(DbValue::as_bool).unwrap_or_default(),
            s_id: row.get_by_name("SId").and_then(DbValue::as_i64),
            create_time: row
                .get_by_name("CreateTime")
                .and_then(DbValue::as_datetime)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc()),
            data: row
                .get_by_name("Data")
                .and_then(|v| v.as_blob().map(<[u8]>::to_vec)),
        })
    }

    fn set_identity(&mut self, value: i64) -> Result<()> {
        self.id = value as i32;
        Ok(())
    }
}

/// 字符串主键、无自增的实体（对应表 `rcode_test_key`）。
#[derive(Debug, Clone, PartialEq)]
struct RCodeTestKey {
    key: String,
    value: Option<String>,
}

impl RCodeTestKey {
    /// 数据库表名。
    pub const TABLE_NAME: &'static str = "rcode_test_key";
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
            value: row
                .get_by_name("Value")
                .filter(|v| !v.is_null())
                .map(DbValue::to_text),
        })
    }
}

/// 清理测试对象（不存在时忽略错误）。
fn cleanup(dal: &Dal) {
    let kind = dal.kind();
    let Ok(mut session) = dal.open_session() else {
        return;
    };
    let item = kind.quote(RCodeTestItem::TABLE_NAME);
    let key = kind.quote(RCodeTestKey::TABLE_NAME);
    let statements: Vec<String> = match kind {
        DatabaseKind::Oracle => vec![
            format!("DROP TABLE {item}"),
            format!("DROP TABLE {key}"),
            format!(
                "DROP SEQUENCE {}",
                kind.quote(&pek_rcode::dialect::oracle_identity_sequence(
                    RCodeTestItem::TABLE_NAME
                ))
            ),
        ],
        // SQL Server 2016+ / PostgreSQL 支持 IF EXISTS
        _ => vec![
            format!("DROP TABLE IF EXISTS {item}"),
            format!("DROP TABLE IF EXISTS {key}"),
        ],
    };
    for sql in statements {
        let _ = session.execute(&sql, &[]);
    }
}

/// 全链路回归：建表 → 实体增删改查 → 事务回滚。
fn run_roundtrip(dal: &Dal) {
    cleanup(dal);

    // 1) 建表（含缺列补齐与 Oracle 序列）
    let report = dal.sync_schema().unwrap();
    assert_eq!(report.created_tables.len(), 2, "{report}");
    // 幂等
    assert!(dal.sync_schema().unwrap().is_empty());

    let mut session = dal.open_session().unwrap();

    // 2) 实体插入：自增主键回写
    let base = chrono::NaiveDate::from_ymd_opt(2026, 9, 27)
        .unwrap()
        .and_hms_micro_opt(10, 30, 0, 123_000)
        .unwrap();
    let mut item1 = RCodeTestItem {
        code: "A-001".into(),
        amount: "12.3400".parse().unwrap(),
        ok: true,
        s_id: Some(9_000_000_001),
        create_time: base,
        data: Some(vec![0x01, 0x02, 0xff]),
        ..RCodeTestItem::new()
    };
    let id1 = item1.insert(dal, session.as_mut()).unwrap();
    assert!(id1 > 0, "自增主键应回写，实际 {id1}");
    assert_eq!(i64::from(item1.id), id1);

    // 3) find：全字段往返（含中文、布尔、DECIMAL、Int64、时间、二进制）
    let loaded = RCodeTestItem::find(dal, session.as_mut(), &[id1.into()])
        .unwrap()
        .expect("按主键应能查到");
    assert_eq!(loaded.code, "A-001");
    assert_eq!(loaded.amount, "12.3400".parse().unwrap());
    assert!(loaded.ok);
    assert_eq!(loaded.s_id, Some(9_000_000_001));
    assert_eq!(loaded.data, Some(vec![0x01, 0x02, 0xff]));
    let delta = (loaded.create_time - base).num_microseconds().unwrap().abs();
    assert!(delta < 1_000, "时间应精确到毫秒以内，实际偏差 {delta}us");

    // 4) save：新增分支（Id=0）与更新分支
    let mut item2 = RCodeTestItem {
        code: "你好，数据库".into(),
        s_id: None,
        ..RCodeTestItem::new()
    };
    item2.save(dal, session.as_mut()).unwrap();
    assert!(item2.id > 0, "save 新增应回写主键");

    item2.code = "已更新".into();
    item2.save(dal, session.as_mut()).unwrap();
    let reloaded = RCodeTestItem::find(dal, session.as_mut(), &[i64::from(item2.id).into()])
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.code, "已更新");
    assert_eq!(reloaded.s_id, None, "NULL 字段应往返为 None");

    // 5) count / 条件查询 / 分页 / 全量
    assert_eq!(RCodeTestItem::count(dal, session.as_mut(), None).unwrap(), 2);
    let filtered = RCodeTestItem::query(
        dal,
        session.as_mut(),
        &Query::new().filter(Where::new().like("Code", "A-%")),
    )
    .unwrap();
    assert_eq!(filtered.len(), 1);

    let paged = RCodeTestItem::query(dal, session.as_mut(), &Query::new().page(1, 1)).unwrap();
    assert_eq!(paged.len(), 1);
    assert_eq!(RCodeTestItem::all(dal, session.as_mut()).unwrap().len(), 2);

    // 6) 字符串主键（无自增）：insert + 更新分支 save
    let mut kv = RCodeTestKey {
        key: "k-001".into(),
        value: Some("v1".into()),
    };
    kv.save(dal, session.as_mut()).unwrap();
    kv.value = Some("v2".into());
    kv.save(dal, session.as_mut()).unwrap();
    let loaded_key = RCodeTestKey::find(dal, session.as_mut(), &["k-001".into()])
        .unwrap()
        .unwrap();
    assert_eq!(loaded_key.value.as_deref(), Some("v2"));

    // 7) delete
    assert_eq!(reloaded.delete(dal, session.as_mut()).unwrap(), 1);
    assert!(RCodeTestItem::find(dal, session.as_mut(), &[i64::from(item2.id).into()])
        .unwrap()
        .is_none());

    // 8) 事务回滚：插入后回滚，行数不应变化
    let before = RCodeTestItem::count(dal, session.as_mut(), None).unwrap();
    session.begin().unwrap();
    let mut tx_item = RCodeTestItem {
        code: "回滚".into(),
        ..RCodeTestItem::new()
    };
    tx_item.insert(dal, session.as_mut()).unwrap();
    session.rollback().unwrap();
    assert_eq!(
        RCodeTestItem::count(dal, session.as_mut(), None).unwrap(),
        before,
        "回滚后行数不应变化"
    );

    drop(session);
    cleanup(dal);
}

#[test]
fn postgres_roundtrip_when_configured() {
    let Some(conn) = std::env::var("RCODE_POSTGRES").ok() else {
        eprintln!("跳过：未设置 RCODE_POSTGRES（参见文件头运行说明）");
        return;
    };
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    assert_eq!(dal.kind(), DatabaseKind::PostgreSql);
    run_roundtrip(&dal);
}

#[test]
fn sqlserver_roundtrip_when_configured() {
    let Some(conn) = std::env::var("RCODE_MSSQL").ok() else {
        eprintln!("跳过：未设置 RCODE_MSSQL（参见文件头运行说明）");
        return;
    };
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    assert_eq!(dal.kind(), DatabaseKind::SqlServer);
    run_roundtrip(&dal);
}

#[test]
fn oracle_roundtrip_when_configured() {
    let Some(conn) = std::env::var("RCODE_ORACLE").ok() else {
        eprintln!("跳过：未设置 RCODE_ORACLE（参见文件头运行说明）");
        return;
    };
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    assert_eq!(dal.kind(), DatabaseKind::Oracle);
    run_roundtrip(&dal);
}

/// network 驱动全链路（不包含建表迁移与事务：网络协议未提供，语义对齐 C# `Network.cs`）。
fn run_network_roundtrip(dal: &Dal) {
    let mut session = dal.open_session().unwrap();

    // 1) 插入：自增主键由远端 `Db/InsertAndGetIdentity` 返回（经连接池会话转发）
    let base = chrono::NaiveDate::from_ymd_opt(2026, 9, 27)
        .unwrap()
        .and_hms_micro_opt(10, 30, 0, 123_000)
        .unwrap();
    let mut item1 = RCodeTestItem {
        code: "A-001".into(),
        amount: "12.3400".parse().unwrap(),
        ok: true,
        s_id: Some(9_000_000_001),
        create_time: base,
        data: None, // BLOB 参数经 JSON 文本传递（本端十六进制），跨语言编码可能不同，故跳过
        ..RCodeTestItem::new()
    };
    let id1 = item1.insert(dal, session.as_mut()).unwrap();
    assert!(id1 > 0, "自增主键应回写，实际 {id1}");
    assert_eq!(i64::from(item1.id), id1);

    // 2) find：全字段往返（中文/布尔/DECIMAL/Int64/时间）
    let loaded = RCodeTestItem::find(dal, session.as_mut(), &[id1.into()])
        .unwrap()
        .expect("按主键应能查到");
    assert_eq!(loaded.code, "A-001");
    assert_eq!(loaded.amount, "12.3400".parse().unwrap());
    assert!(loaded.ok);
    assert_eq!(loaded.s_id, Some(9_000_000_001));
    let delta = (loaded.create_time - base).num_microseconds().unwrap().abs();
    assert!(delta < 1_000, "时间应精确到毫秒以内，实际偏差 {delta}us");

    // 3) save：新增与更新分支
    let mut item2 = RCodeTestItem {
        code: "你好，远端".into(),
        ..RCodeTestItem::new()
    };
    item2.save(dal, session.as_mut()).unwrap();
    assert!(item2.id > 0, "save 新增应回写主键");
    item2.code = "已更新".into();
    item2.save(dal, session.as_mut()).unwrap();
    let reloaded = RCodeTestItem::find(dal, session.as_mut(), &[i64::from(item2.id).into()])
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.code, "已更新");
    assert_eq!(reloaded.s_id, None, "NULL 字段应往返为 None");

    // 4) count / 条件查询 / 分页（分页按远端类型套用）
    assert_eq!(RCodeTestItem::count(dal, session.as_mut(), None).unwrap(), 2);
    let filtered = RCodeTestItem::query(
        dal,
        session.as_mut(),
        &Query::new().filter(Where::new().like("Code", "A-%")),
    )
    .unwrap();
    assert_eq!(filtered.len(), 1);
    let paged = RCodeTestItem::query(dal, session.as_mut(), &Query::new().page(1, 1)).unwrap();
    assert_eq!(paged.len(), 1);
    assert_eq!(RCodeTestItem::all(dal, session.as_mut()).unwrap().len(), 2);

    // 5) 字符串主键：insert + 更新分支 save
    let mut kv = RCodeTestKey {
        key: "k-001".into(),
        value: Some("v1".into()),
    };
    kv.save(dal, session.as_mut()).unwrap();
    kv.value = Some("v2".into());
    kv.save(dal, session.as_mut()).unwrap();
    let loaded_key = RCodeTestKey::find(dal, session.as_mut(), &["k-001".into()])
        .unwrap()
        .unwrap();
    assert_eq!(loaded_key.value.as_deref(), Some("v2"));

    // 6) delete
    assert_eq!(reloaded.delete(dal, session.as_mut()).unwrap(), 1);

    // 7) 事务：网络协议未提供，明确报错（对齐 C# `Network` 无法真正生效的事务）
    assert!(session.begin().is_err(), "network 驱动应明确拒绝事务");
}

#[test]
fn network_roundtrip_when_configured() {
    let Some(conn) = std::env::var("RCODE_NETWORK").ok() else {
        eprintln!("跳过：未设置 RCODE_NETWORK（参见文件头运行说明）");
        return;
    };
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
    eprintln!("network 远端数据库类型：{:?}", dal.kind());

    // 网络驱动不在本端做结构迁移（对齐 C# NetworkMetaData 空实现）：显式建表供测试
    cleanup(&dal);
    {
        let model = dal.model().unwrap();
        let mut session = dal.open_session().unwrap();
        for table in &model.tables {
            for sql in dal.kind().create_table_sql(table) {
                session.execute(&sql, &[]).unwrap();
            }
        }
        let exists = session.table_exists(RCodeTestItem::TABLE_NAME).unwrap_or(false);
        eprintln!("远端 table_exists({}) = {exists}", RCodeTestItem::TABLE_NAME);
    }
    assert!(
        dal.sync_schema().unwrap().is_empty(),
        "网络驱动不应执行本地结构迁移"
    );

    run_network_roundtrip(&dal);

    cleanup(&dal);
}
