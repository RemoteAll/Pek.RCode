//! **MySQL 实机验证**：`sync_schema` 是否会把 `datetime` 自动改成 `datetime(3)`
//! 且**保留 `NOT NULL`**（历史实现会把它悄悄变成可空）。
//!
//! 需要本机 MySQL（默认绿色版实例），手动执行：
//!
//! ```powershell
//! # 先建库（示例：本机绿色版 3306→33306）
//! mysql -h 127.0.0.1 -P 33306 -u root --skip-password -e "CREATE DATABASE IF NOT EXISTS hlk_rcode_alter"
//! $env:RCODE_MYSQL_URL = "Server=127.0.0.1;Port=33306;Database=hlk_rcode_alter;Uid=root;Pwd=;Provider=MySQL"
//! cargo test --features driver-mysql --test mysql_alter_live -- --ignored --nocapture
//! ```
//!
//! 为什么要这条实机用例：`datetime` → `datetime(3)` 的判定与 DDL 都用单元测试覆盖了，
//! 但"**真的能执行、且不丢 NOT NULL**"只有在真库上才算证据（用户 2026-10-30：不能猜）。

#![cfg(feature = "driver-mysql")]

use pek_rcode::{Dal, EntityModel, SqlSession};

/// 目标表模型：`LogTime` 声明 `Scale="3"`（毫秒）。
const MODEL: &str = r#"<EntityModel><Tables><Table Name="OpLog" TableName="Hlk_OpLogAlterTest">
  <Columns>
    <Column Name="Id" DataType="Int64" Identity="True" PrimaryKey="True" />
    <Column Name="LogTime" DataType="DateTime" Scale="3" Nullable="False" />
    <Column Name="Note" DataType="String" Length="50" Nullable="True" />
  </Columns>
</Table></Tables></EntityModel>"#;

/// 默认连接串（本机绿色版；可用 `RCODE_MYSQL_URL` 覆盖）。
fn conn() -> String {
    std::env::var("RCODE_MYSQL_URL").unwrap_or_else(|_| {
        "Server=127.0.0.1;Port=33306;Database=hlk_rcode_alter;Uid=root;Pwd=;Provider=MySQL"
            .to_string()
    })
}

/// 取一列文本：`information_schema` 部分列是 binary charset，驱动回来是 `Blob`。
fn cell(row: &pek_rcode::DbRow, name: &str) -> String {
    match row.get_by_name(name) {
        Some(pek_rcode::DbValue::Text(s)) => s.clone(),
        Some(pek_rcode::DbValue::Blob(bytes)) => String::from_utf8_lossy(bytes).to_string(),
        _ => String::new(),
    }
}

#[test]
#[ignore = "需要本机 MySQL 实例（RCODE_MYSQL_URL）；手动执行"]
fn mysql_auto_alters_datetime_scale_and_keeps_not_null() {
    let conn = conn();

    // ① 造一张"旧式"表：`LogTime` 是 0 位小数的 `datetime NOT NULL`（线上老库就是长这样）
    {
        let dal = Dal::open(&conn).expect("连接 MySQL 失败");
        let mut session = dal.open_session().expect("打开会话失败");
        session
            .execute("DROP TABLE IF EXISTS Hlk_OpLogAlterTest", &[])
            .expect("删表失败");
        session
            .execute(
                "CREATE TABLE Hlk_OpLogAlterTest (\
                   Id bigint NOT NULL AUTO_INCREMENT PRIMARY KEY, \
                   LogTime datetime NOT NULL, \
                   Note varchar(50) NULL)",
                &[],
            )
            .expect("建旧式表失败");
        drop(session);
        dal.clear_pool();
    }

    // ② 按模型同步：应检出「精度放宽」并**自动执行** ALTER（默认 On 档）
    let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).expect("打开失败");
    let diff = dal.diff_schema().expect("比对新旧结构失败");
    println!("diff = {diff:?}");
    assert!(
        diff.column_changes.iter().any(|c| c.column == "LogTime"),
        "应检出 LogTime 的精度差异：{diff:?}"
    );
    let report = dal.sync_schema().expect("同步结构失败");
    println!("report = {report}");
    assert!(
        report
            .altered_columns
            .iter()
            .any(|(_, c)| c == "LogTime"),
        "应在默认档执行改列：{report:?}"
    );

    // ③ 验证库端：`datetime(3)` 且**仍是 NOT NULL**（可空性不得被 MODIFY 丢掉）
    let mut session = dal.open_session().expect("打开会话失败");
    let rs = session
        .query(
            "select COLUMN_TYPE, IS_NULLABLE from information_schema.COLUMNS \
             where TABLE_SCHEMA = DATABASE() and TABLE_NAME = 'Hlk_OpLogAlterTest' \
               and COLUMN_NAME = 'LogTime'",
            &[],
        )
        .expect("查列信息失败");
    let row = rs.first().expect("应查到 LogTime 列");
    // `COLUMN_TYPE` 是 binary charset（驱动回来是 Blob），必须走 cell() 兼容读取
    let col_type = cell(row, "COLUMN_TYPE").to_ascii_lowercase();
    let nullable = cell(row, "IS_NULLABLE");
    println!("cols: type={col_type} nullable={nullable}");
    assert_eq!(col_type, "datetime(3)", "应已改成毫秒精度");
    assert_eq!(nullable, "NO", "NOT NULL 不能被改列丢掉");

    // ④ 毫秒真的落得了库（写入 .789 读回来还是 789）
    session
        .execute(
            "insert into Hlk_OpLogAlterTest (LogTime, Note) values ('2026-10-30 12:34:56.789', 'ms')",
            &[],
        )
        .expect("写入毫秒失败");
    let rs = session
        .query(
            "select DATE_FORMAT(LogTime, '%Y-%m-%d %H:%i:%s.%f') as t from Hlk_OpLogAlterTest",
            &[],
        )
        .expect("读回毫秒失败");
    let t = rs.first().map(|r| cell(r, "t")).unwrap_or_default();
    println!("read back = {t}");
    assert!(t.ends_with(".789000"), "毫秒应真的落库：{t}");
}
