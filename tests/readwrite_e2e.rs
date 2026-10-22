//! 读写分离与只读连接的端到端测试（真实 SQLite 副本）。
//!
//! 用独立库模拟"主库 + 只读副本"，验证：
//! - 只读连接拒绝写、放行读（运行期可解除）；
//! - `read_session` 轮询到不同副本、无副本/副本不可用时回退主库；
//! - `read_session_for` 命中忽略表 / 全局切主库时回主库。

use std::sync::Arc;

use pek_rcode::dal::Dal;
use pek_rcode::session::SqlSession;
use pek_rcode::value::DbValue;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pek-rcode-readwrite-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 建一个只含 `mark` 表的库，写入一行标识值，返回连接串。
fn seed(path: &std::path::Path, tag: &str) -> String {
    let conn = format!("Data Source={};Provider=SQLite", path.display());
    let dal = Dal::open(&conn).unwrap();
    let mut s = dal.open_session().unwrap();
    s.execute(
        "create table mark (id integer primary key, tag text not null)",
        &[],
    )
    .unwrap();
    s.execute(
        "insert into mark (id, tag) values (1, ?)",
        &[DbValue::Text(tag.into())],
    )
    .unwrap();
    conn
}

/// 用给定会话读标记值。
fn tag_of(session: &mut dyn SqlSession) -> String {
    let rows = session
        .query("select tag from mark where id = 1", &[])
        .unwrap();
    rows.first()
        .and_then(|r| r.get_by_name("tag"))
        .map(DbValue::to_text)
        .unwrap_or_default()
}

#[test]
fn readonly_connection_rejects_writes_but_allows_reads() {
    let dir = temp_dir("readonly");
    let mut conn = seed(&dir.join("main.db"), "main");
    conn.push_str(";Readonly=true");

    let dal = Dal::open(&conn).unwrap();
    assert!(dal.is_readonly());

    let mut session = dal.open_session().unwrap();
    assert_eq!(tag_of(session.as_mut()), "main", "只读连接允许查询");

    // 写被拒（不发 SQL，错误信息明确）
    let err = session
        .execute("update mark set tag = ?", &[DbValue::Text("x".into())])
        .unwrap_err()
        .to_string();
    assert!(err.contains("只读"), "{err}");
    // 事务同样被拒（事务意味着写入意图）
    let err = session.begin().unwrap_err().to_string();
    assert!(err.contains("只读"), "{err}");

    // 运行期解除只读后新会话可写（对应 C# SuspendReadOnly）
    dal.set_readonly(false);
    let mut writable = dal.open_session().unwrap();
    assert_eq!(
        writable
            .execute("update mark set tag = ?", &[DbValue::Text("y".into())])
            .unwrap(),
        1
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_session_round_robins_replicas_and_falls_back() {
    let dir = temp_dir("roundrobin");
    let main = Dal::open(&seed(&dir.join("main.db"), "main")).unwrap();
    let r1 = Arc::new(Dal::open(&seed(&dir.join("r1.db"), "r1")).unwrap());
    let r2 = Arc::new(Dal::open(&seed(&dir.join("r2.db"), "r2")).unwrap());

    // 未挂载副本：回退主库
    assert!(!main.has_read_replicas());
    assert_eq!(tag_of(main.read_session().unwrap().as_mut()), "main");

    main.attach_read_replicas(vec![r1, r2]);
    assert!(main.has_read_replicas());
    assert_eq!(main.read_replicas().len(), 2);

    // 轮询：4 次读分别落到两个副本各 2 次
    let mut seen: Vec<String> = (0..4)
        .map(|_| tag_of(main.read_session().unwrap().as_mut()))
        .collect();
    seen.sort();
    assert_eq!(seen, vec!["r1", "r1", "r2", "r2"], "两个副本各分到一半流量");

    // 副本不可用时自动回退主库（可用性优先）
    let broken = Arc::new(
        Dal::open(&format!(
            "Data Source={};Provider=SQLite",
            dir.join("missing-dir").join("r.db").display()
        ))
        .unwrap(),
    );
    main.attach_read_replicas(vec![broken]);
    assert_eq!(tag_of(main.read_session().unwrap().as_mut()), "main");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ignore_table_and_primary_only_force_main() {
    let dir = temp_dir("strategy");
    let main = Dal::open(&seed(&dir.join("main.db"), "main")).unwrap();
    main.attach_read_replicas(vec![Arc::new(
        Dal::open(&seed(&dir.join("r1.db"), "r1")).unwrap(),
    )]);

    // 默认策略：读副本
    assert_eq!(
        tag_of(main.read_session_for("Other").unwrap().as_mut()),
        "r1"
    );

    // 忽略表：读主库
    main.set_ignore_tables(&["Mark"]);
    assert_eq!(main.ignored_tables(), vec!["mark".to_string()]);
    assert_eq!(
        tag_of(main.read_session_for("Mark").unwrap().as_mut()),
        "main"
    );
    // 未命中的表仍读副本
    assert_eq!(
        tag_of(main.read_session_for("Other").unwrap().as_mut()),
        "r1"
    );

    // 全局切主库（对应"主从延迟不可容忍"的临时止血）
    main.set_primary_only(true);
    assert!(main.is_primary_only());
    assert_eq!(
        tag_of(main.read_session_for("Other").unwrap().as_mut()),
        "main"
    );
    main.set_primary_only(false);
    assert_eq!(
        tag_of(main.read_session_for("Other").unwrap().as_mut()),
        "r1"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
