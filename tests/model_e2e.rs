//! 端到端集成测试：使用**生产模型快照固件**（tests/fixtures/wms_model_sample.xml）做全流程验证。
//!
//! 覆盖点：
//! - 生产模型解析与方言 DDL 生成（SQLite/MySQL/SqlServer/PostgreSQL/Oracle）
//! - 在临时 SQLite 库中按模型真实建表
//! - 以真实表结构执行插入/查询/分页/更新/删除
//! - Rust 实体代码生成
//!
//! 可选全量回归：设置 `RCODE_MODEL` 环境变量指向完整生产 Model.xml 后，
//! `full_model_*` 测试会额外把全部表同步到临时库并验证（默认自动跳过）。

use std::path::PathBuf;

use pek_rcode::{
    Dal, DatabaseKind, DbValue, EntityModel, Query, Where, codegen, dal::ConnectionString,
};

/// 测试固件：生产 WMS 模型快照样本（7 张真实表，覆盖全部 8 种数据类型）
const SAMPLE_MODEL: &str = include_str!("fixtures/wms_model_sample.xml");

/// 临时目录（每次唯一，避免并行冲突）
fn temp_dir(name: &str) -> PathBuf {
    let stamp = chrono::Local::now()
        .format("%H%M%S%.9f")
        .to_string()
        .replace('.', "");
    let dir =
        std::env::temp_dir().join(format!("rcode-e2e-{}-{}-{name}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn model_ddl_for_all_dialects() {
    let model = EntityModel::parse(SAMPLE_MODEL).expect("固件模型应可解析");

    // 抽查：激励语表（Id 自增主键 + 多个字符串/时间列）
    let table = model.table("JiLiYu").expect("应存在 JiLiYu 表");

    let sqlite = DatabaseKind::Sqlite.create_table_sql(table).remove(0);
    assert!(
        sqlite.contains("\"Id\" integer PRIMARY KEY AUTOINCREMENT"),
        "{sqlite}"
    );

    let mysql = DatabaseKind::MySql.create_table_sql(table).remove(0);
    assert!(
        mysql.contains("`Id` int AUTO_INCREMENT NOT NULL"),
        "{mysql}"
    );
    assert!(mysql.contains("PRIMARY KEY (`Id`)"), "{mysql}");

    let sqlserver = DatabaseKind::SqlServer.create_table_sql(table).remove(0);
    assert!(
        sqlserver.contains("[Id] int IDENTITY(1,1) NOT NULL"),
        "{sqlserver}"
    );

    let pg = DatabaseKind::PostgreSql.create_table_sql(table).remove(0);
    // 与 DH.NCode 对齐：PostgreSQL 自增使用 serial/serial8（伪类型，替换原类型）
    assert!(pg.contains("\"Id\" serial NOT NULL"), "{pg}");

    // Oracle：自增列本身无内联属性，依赖独立序列 SEQ_{表名}（随建表脚本导出）
    let oracle_statements = DatabaseKind::Oracle.create_table_sql(table);
    let oracle = &oracle_statements[0];
    assert!(oracle.contains("\"Id\" number(10) NOT NULL"), "{oracle}");
    assert!(
        oracle_statements
            .iter()
            .any(|s| s.contains("CREATE SEQUENCE \"SEQ_DH_JiLiYu\"")),
        "{oracle_statements:?}"
    );
}

#[test]
fn model_end_to_end_on_sqlite() {
    let dir = temp_dir("wms");
    let db = dir.join("DG.db");
    let conn = format!("Data Source={};Provider=SQLite", db.display());

    let model = EntityModel::parse(SAMPLE_MODEL).unwrap();
    // 取几张真实表做全流程（避免在测试里创建 176 张表）
    let subset = model.subset(&["JiLiYu", "VerifyCode", "SingleArticle"]);

    let dal = Dal::open_with_model(&conn, subset).unwrap();
    assert_eq!(dal.kind(), DatabaseKind::Sqlite);

    // 建表
    let report = dal.sync_schema().unwrap();
    assert_eq!(report.created_tables.len(), 3, "{report}");
    // 幂等
    assert!(dal.sync_schema().unwrap().is_empty());

    // 真实表结构验证：DH_SingleArticle 应有唯一索引列 Code
    let mut session = dal.open_session().unwrap();
    let columns = session.table_columns("DH_SingleArticle").unwrap();
    assert!(columns.iter().any(|c| c == "Code"), "{columns:?}");

    // 插入真实表 DH_JiLiYu：审计列（CreateUser/UpdateUser 等）为 NOT NULL，
    // 与 C# 实体基类行为一致，需要显式赋值
    let now = chrono::Local::now().naive_local();
    let jly = dal.table("JiLiYu").unwrap();
    let fields: Vec<(&str, DbValue)> = vec![
        ("Content", "每天进步一点点".into()),
        ("CreateUser", "tester".into()),
        ("CreateUserID", 1.into()),
        ("CreateTime", now.into()),
        ("CreateIP", "127.0.0.1".into()),
        ("UpdateUser", "tester".into()),
        ("UpdateUserID", 1.into()),
        ("UpdateTime", now.into()),
        ("UpdateIP", "127.0.0.1".into()),
    ];
    let id = jly.insert(session.as_mut(), &fields).unwrap();
    assert!(id > 0, "自增主键应回写");

    // 字符串主键表 DH_VerifyCode：插入/条件统计/分页/更新/删除
    let vc = dal.table("VerifyCode").unwrap();
    let end = now + chrono::TimeDelta::minutes(5);
    let inserted = vc
        .insert(
            session.as_mut(),
            &[
                ("Key", "k-001".into()),
                ("Code", "123456".into()),
                ("EndTime", end.into()),
                ("CreateTime", now.into()),
            ],
        )
        .unwrap();
    assert_eq!(inserted, 0, "无自增列时返回 0");

    let filter = Where::new().like("Code", "123%");
    assert_eq!(vc.count(session.as_mut(), Some(&filter)).unwrap(), 1);

    let query = Query::new().order_by("CreateTime", true).page(1, 10);
    let rows = vc.query(session.as_mut(), &query).unwrap();
    assert_eq!(rows.len(), 1);
    let row = rows.first().unwrap();
    assert_eq!(row.get_by_name("code").unwrap().as_str(), Some("123456"));
    assert!(row.get_by_name("EndTime").unwrap().as_datetime().is_some());

    // 主键更新与删除（主键为字符串 Key）
    let affected = vc
        .update_by_pk(
            session.as_mut(),
            &[("Code", "654321".into())],
            &["k-001".into()],
        )
        .unwrap();
    assert_eq!(affected, 1);
    let row = vc
        .find_by_pk(session.as_mut(), &["k-001".into()])
        .unwrap()
        .unwrap();
    assert_eq!(row.get_by_name("Code").unwrap().as_str(), Some("654321"));

    assert_eq!(
        vc.delete_by_pk(session.as_mut(), &["k-001".into()])
            .unwrap(),
        1
    );
    assert!(
        !vc.exists_by_pk(session.as_mut(), &["k-001".into()])
            .unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_codegen_output() {
    let model = EntityModel::parse(SAMPLE_MODEL).unwrap();
    let dev = model.table("HardwareDevices").unwrap();
    let code = codegen::generate(dev);

    assert!(code.contains("pub struct HardwareDevices {"), "{code}");
    assert!(code.contains("    mac: String,"), "字段私有：{code}");
    assert!(code.contains("    h_type: i16,"), "{code}");
    assert!(
        code.contains("pub fn set_mac(&mut self, value: String)"),
        "{code}"
    );
    assert!(code.contains("self.dirty.mark_static(\"Mac\");"), "{code}");
    assert!(
        code.contains("pub const TABLE_NAME: &'static str = \"DH_HardwareDevices\";"),
        "{code}"
    );

    // 全量生成：176 张表各自一个文件
    let files = codegen::generate_all(&model);
    assert_eq!(files.len(), model.tables.len());
    assert!(
        files
            .iter()
            .all(|(name, code)| name.ends_with(".rs") && code.contains("pub struct"))
    );
}

#[test]
fn insert_only_table_parses_roundtrips_and_generates_guard() {
    // `<Table InsertOnly="true">`（对应 C# 的"仅插入表"：日志/记录/事件表）
    let xml = r#"<EntityModel xmlns="http://schemas.newlifex.com/Model202509">
  <Tables>
    <Table Name="DhLog" TableName="DH_Log" Description="日志" InsertOnly="True">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Message" DataType="String" Length="200" />
      </Columns>
    </Table>
  </Tables>
</EntityModel>"#;
    let model = EntityModel::parse(xml).unwrap();
    let table = model.table("DhLog").unwrap();
    assert!(table.insert_only, "InsertOnly=True 应解析为 true");

    // 解析 → 写出 → 再解析：属性不丢失
    let out = model.to_xml();
    assert!(out.contains("InsertOnly=\"true\""), "{out}");
    let back = EntityModel::parse(&out).unwrap();
    assert!(back.table("DhLog").unwrap().insert_only);

    // 生成的实体声明 insert_only()（更新/删除会在运行时被拒绝）
    let code = codegen::generate(table);
    assert!(code.contains("fn insert_only() -> bool {"), "{code}");
}

#[test]
fn unique_index_generates_cached_find_method() {
    // 唯一索引 → 生成 find_by_xxx（内部走 find_cached_by：小表自动使用实体缓存）
    let xml = r#"<EntityModel xmlns="http://schemas.newlifex.com/Model202509">
  <Tables>
    <Table Name="DhUser" TableName="DH_User" Description="用户">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" />
        <Column Name="TenantId" DataType="Int32" />
        <Column Name="Memo" DataType="String" Length="200" Nullable="True" />
      </Columns>
      <Indexes>
        <Index Name="IX_Code" Columns="Code" Unique="True" />
        <Index Name="IX_Tenant_Code" Columns="TenantId,Code" Unique="True" />
        <Index Name="IX_Memo" Columns="Memo" Unique="True" />
      </Indexes>
    </Table>
  </Tables>
</EntityModel>"#;
    let model = EntityModel::parse(xml).unwrap();
    let code = codegen::generate(model.table("DhUser").unwrap());

    // 单列唯一索引
    assert!(code.contains("pub fn find_by_code("), "{code}");
    // 多列唯一索引 → find_by_a_and_b
    assert!(
        code.contains("pub fn find_by_tenant_id_and_code("),
        "{code}"
    );
    // 统一走 find_cached_by（MaxCacheCount 自动判定）
    assert!(
        code.contains("Self::find_cached_by(dal, session, &["),
        "{code}"
    );
    // 可空列的唯一索引不生成（参数无法表达 NULL 语义）
    assert!(!code.contains("find_by_memo"), "{code}");

    // 唯一索引首个字符串列 → 生成字段缓存下拉方法（对应 C# `FieldCache<T>`）
    assert!(code.contains("pub fn code_list("), "{code}");
    assert!(code.contains("field_cache(\"DhUser\", \"Code\")"), "{code}");
    // 可空列同样生成下拉（分组时空值会被过滤，与 C# 一致）
    assert!(code.contains("pub fn memo_list("), "{code}");

    // 结构护栏：`find_by_xxx` / `xxx_list` 是**固有方法**，绝不能落进 `impl Entity for ...`
    // （落进去会变成 trait 方法 → 生成代码无法编译；曾因此漏过一次）
    let entity_start = code
        .find("impl Entity for DhUser")
        .expect("应生成 trait impl");
    let tail = &code[entity_start..];
    let body_end = tail
        .find("\n}\n")
        .map(|i| entity_start + i)
        .unwrap_or(code.len());
    let entity_block = &code[entity_start..body_end];
    for needle in [
        "pub fn find_by_code(",
        "pub fn code_list(",
        "pub fn memo_list(",
    ] {
        assert!(code.contains(needle), "缺少 {needle}");
        assert!(
            !entity_block.contains(needle),
            "{needle} 不能出现在 `impl Entity` 块内（必须是固有 impl）：{code}"
        );
    }
}

#[test]
fn codegen_generates_validate_for_required_columns() {
    // 对齐 C# `EntityBuilder.BuildValid`：非空字符串列（无默认值、非审计列）→ 必填校验
    let xml = r#"<EntityModel xmlns="http://schemas.newlifex.com/Model202509">
  <Tables>
    <Table Name="DhOrder" TableName="DH_Order" Description="订单">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
        <Column Name="Code" DataType="String" Length="50" Description="单号" />
        <Column Name="Memo" DataType="String" Length="200" Nullable="True" Description="备注" />
        <Column Name="Kind" DataType="String" Length="20" DefaultValue="NORMAL" Description="类型" />
        <Column Name="CreateTime" DataType="DateTime" Description="创建时间" />
        <Column Name="CreateUser" DataType="String" Length="50" Description="创建者" />
        <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" Description="金额" />
      </Columns>
    </Table>
  </Tables>
</EntityModel>"#;
    let model = EntityModel::parse(xml).unwrap();
    let code = codegen::generate(model.table("DhOrder").unwrap());

    assert!(
        code.contains("fn validate(&self) -> pek_rcode::Result<()> {"),
        "{code}"
    );
    // 必填列 Code → 校验（提示用 Description）
    assert!(
        code.contains("if self.code.trim().is_empty()"),
        "必填列 Code 应生成校验：{code}"
    );
    assert!(code.contains("单号 不能为空"), "{code}");
    // 可空列 Memo / 有默认值 Kind / 审计列 CreateUser / 非字符串 Amount 都不校验
    assert!(!code.contains("self.memo.trim()"), "{code}");
    assert!(!code.contains("self.kind.trim()"), "{code}");
    assert!(!code.contains("self.create_user.trim()"), "{code}");
    assert!(!code.contains("self.amount.trim()"), "{code}");
}

#[test]
fn max_cache_count_defaults_and_time_scale_disables_cache() {
    // 缺省 1000；含 DataScale=time / timeShard 的表强制 0（对齐 C# EntityBuilder.Prepare）
    let xml = r#"<EntityModel xmlns="http://schemas.newlifex.com/Model202509">
  <Tables>
    <Table Name="A" TableName="T_A">
      <Columns><Column Name="Id" DataType="Int32" PrimaryKey="True" /></Columns>
    </Table>
    <Table Name="B" TableName="T_B" MaxCacheCount="5000">
      <Columns><Column Name="Id" DataType="Int32" PrimaryKey="True" /></Columns>
    </Table>
    <Table Name="C" TableName="T_C" MaxCacheCount="5000">
      <Columns>
        <Column Name="Id" DataType="Int32" PrimaryKey="True" />
        <Column Name="LogTime" DataType="DateTime" DataScale="timeShard:LogTime" />
      </Columns>
    </Table>
    <Table Name="D" TableName="T_D" MaxCacheCount="0">
      <Columns><Column Name="Id" DataType="Int32" PrimaryKey="True" /></Columns>
    </Table>
  </Tables>
</EntityModel>"#;
    let model = EntityModel::parse(xml).unwrap();
    assert_eq!(model.table("A").unwrap().max_cache_count, 1000, "缺省 1000");
    assert_eq!(
        model.table("B").unwrap().max_cache_count,
        5000,
        "显式值生效"
    );
    assert_eq!(
        model.table("C").unwrap().max_cache_count,
        0,
        "时序/分表列 → 强制禁用缓存"
    );
    assert_eq!(model.table("D").unwrap().max_cache_count, 0, "0 = 禁用");

    // 往返：非缺省值写出后仍能解析回来
    let out = model.to_xml();
    assert!(out.contains("MaxCacheCount=\"5000\""), "{out}");
    let back = EntityModel::parse(&out).unwrap();
    assert_eq!(back.table("B").unwrap().max_cache_count, 5000);
}

#[test]
fn connection_string_accepts_wms_configs() {
    let sqlite =
        ConnectionString::parse("Data Source=..\\..\\Data\\DG.db;ShowSql=false;Provider=SQLite");
    assert_eq!(sqlite.kind().unwrap(), DatabaseKind::Sqlite);
    assert!(!sqlite.show_sql());

    let mysql = ConnectionString::parse(
        "Server=db.example.com;Port=3306;Database=demodb;Uid=demo;SslMode=None;provider=mysql",
    );
    assert_eq!(mysql.kind().unwrap(), DatabaseKind::MySql);
    assert_eq!(mysql.data_source(), Some("demodb"));
}

/// 可选全量端到端：`RCODE_MODEL` 指向完整 Model.xml 时，把全部表同步到临时 SQLite 库验证。
#[test]
fn full_model_sync_schema_when_configured() {
    let Ok(path) = std::env::var("RCODE_MODEL") else {
        eprintln!("未设置 RCODE_MODEL，跳过全量建表回归");
        return;
    };

    let model = EntityModel::load(std::path::Path::new(&path)).expect("全量模型应可解析");
    let table_count = model.tables.len();

    let dir = temp_dir("full");
    let db = dir.join("full.db");
    let conn = format!("Data Source={};Provider=SQLite", db.display());
    let dal = Dal::open_with_model(&conn, model).unwrap();

    // 全部表建到临时库（验证所有表的 DDL 均可执行），重复执行应幂等
    let report = dal.sync_schema().unwrap();
    assert_eq!(
        report.created_tables.len(),
        table_count,
        "应建出全部表：{report}"
    );
    assert!(dal.sync_schema().unwrap().is_empty(), "重复同步应无变更");
    eprintln!("全量建表回归通过：{table_count} 张表");

    let _ = std::fs::remove_dir_all(&dir);
}
