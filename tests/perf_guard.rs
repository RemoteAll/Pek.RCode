//! 性能守护基准（默认忽略，手工执行）：
//!
//! ```text
//! cargo test --release --test perf_guard -- --ignored --nocapture
//! ```
//!
//! 关注点（高并发/海量设备场景下的资源占用）：
//! - 脏追踪的登记开销（每次字段赋值一次）与内存占用；
//! - 脏列名取值开销（每次 UPDATE 一次）；
//! - 实体缓存读路径开销（每次缓存命中一次）。
//!
//! 这些是**纯 CPU 微基准**：不含 IO，因此数字代表框架自身的附加成本。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pek_rcode::cache::{CacheConsistency, EntityCache};
use pek_rcode::dirty::DirtyState;

/// 统计分配次数：用于确认热路径是否发生堆分配。
struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn alloc_since(mark: usize) -> usize {
    ALLOCS.load(Ordering::Relaxed).saturating_sub(mark)
}

fn report(label: &str, n: usize, elapsed: Duration, allocs: usize) {
    let ns = elapsed.as_nanos() as f64 / n as f64;
    println!("{label:<44} {ns:>9.1} ns/op   分配 {allocs} 次（{n} 次调用）");
}

/// 单次字段赋值的脏登记（每次都会新建脏项 → 含一次列名分配）。
#[test]
#[ignore]
fn bench_dirty_mark_single() {
    const N: usize = 1_000_000;
    let state = DirtyState::new();
    // 预热
    for _ in 0..1000 {
        state.mark_static("Status");
        state.clear();
    }
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        state.mark_static("Status");
        state.clear();
    }
    report(
        "脏登记 mark_static（单列）+ 清空",
        N,
        t.elapsed(),
        alloc_since(mark),
    );
}

/// 典型保存周期：登记 5 个字段的脏 + 取值 + 清空。
#[test]
#[ignore]
fn bench_dirty_cycle_five_columns() {
    const N: usize = 500_000;
    let state = DirtyState::new();
    let columns = ["Code", "Status", "Amount", "Remark", "UpdatedAt"];
    for _ in 0..1000 {
        for c in columns {
            state.mark_static(c);
        }
        state.with_columns(|items| items.len());
        state.clear();
    }
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        for c in columns {
            state.mark_static(c);
        }
        // 与 `Entity::update` 真实路径一致：持锁遍历，不分配列名字符串
        state.with_columns(|items| {
            for (name, _) in items.iter() {
                sink += name.len();
            }
        });
        state.clear();
    }
    report(
        "脏周期（5 列 mark_static + 遍历 + 清空）",
        N,
        t.elapsed(),
        alloc_since(mark),
    );
    assert!(sink > 0);
}

/// 只读判断（`has_dirty` / `is_dirty` 这类每次保存前的检查）。
#[test]
#[ignore]
fn bench_dirty_probe() {
    const N: usize = 2_000_000;
    let state = DirtyState::new();
    state.mark("Status");
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        sink += usize::from(!state.is_empty());
        sink += usize::from(state.contains("Status"));
    }
    report("脏状态探查（is_empty + contains）", N * 2, t.elapsed(), 0);
    assert!(sink > 0);
}

/// 实体缓存读路径（命中）：返回 `Arc` 快照，不应有堆分配。
#[test]
#[ignore]
fn bench_cache_hit_is_allocation_free() {
    const N: usize = 1_000_000;
    let cache = EntityCache::new("bench_table");
    cache.set_expire_seconds(3600);
    cache.set_consistency(CacheConsistency::Blocking);
    // 未装载时 `is_valid` 为 false，这里只测读路径的元数据开销
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        sink += usize::from(cache.is_valid());
        sink += cache.expire().as_secs() as usize;
        let _ = cache.consistency();
    }
    report(
        "缓存元数据读（is_valid + expire + mode）",
        N,
        t.elapsed(),
        alloc_since(mark),
    );
    assert!(sink < usize::MAX);
}

/// 缓存元数据读的并发吞吐（模拟多连接线程同时命中）。
#[test]
#[ignore]
fn bench_cache_hit_concurrent() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 500_000;

    let cache = std::sync::Arc::new(EntityCache::new("bench_table"));
    cache.set_expire_seconds(3600);
    let start = Instant::now();
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let cache = cache.clone();
            std::thread::spawn(move || {
                let mut sink = 0usize;
                for _ in 0..PER_THREAD {
                    sink += usize::from(cache.is_valid());
                    sink += cache.expire().as_secs() as usize;
                }
                sink
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap() < usize::MAX);
    }
    report(
        &format!("缓存元数据读（{THREADS} 线程并发）"),
        THREADS * PER_THREAD,
        start.elapsed(),
        0,
    );
}

/// 对照：旧路径（动态列名 + `names()` 取列名列表）。
///
/// 手写实体 / `Entity::set_field` / `enqueue_save` 仍走这条；代码生成的实体走上面的
/// `mark_static` + `with_columns`（零分配）。
#[test]
#[ignore]
fn bench_dirty_legacy_path() {
    const N: usize = 500_000;
    let state = DirtyState::new();
    let columns = ["Code", "Status", "Amount", "Remark", "UpdatedAt"];
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        for c in columns {
            state.mark(c);
        }
        sink += state.names().len();
        state.clear();
    }
    report(
        "对照：旧路径（mark 动态名 + names）",
        N,
        t.elapsed(),
        alloc_since(mark),
    );
    assert_eq!(sink, N * columns.len());
}

/// 结构体尺寸（每实体常驻内存的固定增量）。
#[test]
#[ignore]
fn report_sizes() {
    println!("size_of::<DirtyState>()            = {}", std::mem::size_of::<DirtyState>());
    println!(
        "size_of::<Option<DirtyState>>()    = {}",
        std::mem::size_of::<Option<DirtyState>>()
    );
    println!(
        "size_of::<EntityCache>()           = {}",
        std::mem::size_of::<EntityCache>()
    );
    println!(
        "size_of::<DbRow>()                 = {}",
        std::mem::size_of::<pek_rcode::DbRow>()
    );
    println!(
        "size_of::<DbValue>()               = {}",
        std::mem::size_of::<pek_rcode::DbValue>()
    );
}

/// 模型表查找：线性扫描（旧路径）vs 表名索引（新路径）。
///
/// 热路径（实体 find/insert/update、缓存注册、行数缓存）每次操作都要查表定义；
/// 线性扫描在 176 表模型上是 O(表数×2) 次字符串比较，索引后 O(1)。
#[test]
#[ignore]
fn bench_model_table_lookup() {
    use pek_rcode::model::EntityModel;

    const TABLES: usize = 176;
    const N: usize = 200_000;

    let mut xml = String::from("<EntityModel><Tables>");
    for i in 0..TABLES {
        xml.push_str(&format!(
            r#"<Table Name="T{i}" TableName="DH_T{i}"><Columns><Column Name="Id" DataType="Int32" PrimaryKey="True" /></Columns></Table>"#
        ));
    }
    xml.push_str("</Tables></EntityModel>");
    let model = EntityModel::parse(&xml).unwrap();
    assert_eq!(model.tables.len(), TABLES);

    let conn = format!(
        "Data Source={};Provider=SQLite",
        std::env::temp_dir()
            .join(format!("pek-rcode-lookup-{}.db", std::process::id()))
            .display()
    );
    let dal = pek_rcode::Dal::open_with_model(&conn, EntityModel::parse(&xml).unwrap()).unwrap();

    // 预构造表名（排除 format 分配噪声，只测查找本身）
    let names: Vec<String> = (0..TABLES).map(|i| format!("T{i}")).collect();

    // 旧路径：EntityModel::table 线性扫描
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sink = 0usize;
    for i in 0..N {
        sink += usize::from(model.table(&names[i % TABLES]).is_some());
    }
    report(
        &format!("模型表查找（线性扫描，{TABLES} 表）"),
        N,
        t.elapsed(),
        alloc_since(mark),
    );

    // 新路径：Dal 表名索引
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for i in 0..N {
        sink += usize::from(dal.table(&names[i % TABLES]).is_ok());
    }
    report(
        &format!("模型表查找（表名索引，{TABLES} 表）"),
        N,
        t.elapsed(),
        alloc_since(mark),
    );
    assert!(sink > 0);

    let _ = std::fs::remove_file(
        std::env::temp_dir().join(format!("pek-rcode-lookup-{}.db", std::process::id())),
    );
}
// ---------------------------------------------------------------------------
// 端到端路径基准（第十二轮审计新增）：会话借还 / 行取值 / 写读往返
// ---------------------------------------------------------------------------

/// 宽表模型（20 列）与窄表模型（6 列），用于行取值基准。
const WIDE_MODEL: &str = r#"<EntityModel><Tables><Table Name="Wide" TableName="DH_Wide">
  <Columns>
    <Column Name="C1" DataType="String" Length="50" /><Column Name="C2" DataType="String" Length="50" />
    <Column Name="C3" DataType="String" Length="50" /><Column Name="C4" DataType="String" Length="50" />
    <Column Name="C5" DataType="String" Length="50" /><Column Name="C6" DataType="String" Length="50" />
    <Column Name="C7" DataType="String" Length="50" /><Column Name="C8" DataType="String" Length="50" />
    <Column Name="C9" DataType="String" Length="50" /><Column Name="C10" DataType="String" Length="50" />
    <Column Name="C11" DataType="String" Length="50" /><Column Name="C12" DataType="String" Length="50" />
    <Column Name="C13" DataType="String" Length="50" /><Column Name="C14" DataType="String" Length="50" />
    <Column Name="C15" DataType="String" Length="50" /><Column Name="C16" DataType="String" Length="50" />
    <Column Name="C17" DataType="String" Length="50" /><Column Name="C18" DataType="String" Length="50" />
    <Column Name="C19" DataType="String" Length="50" /><Column Name="C20" DataType="String" Length="50" />
  </Columns>
</Table></Tables></EntityModel>"#;

fn bench_dal(tag: &str) -> pek_rcode::Dal {
    let dir = std::env::temp_dir().join(format!("pek-rcode-perf-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = format!("Data Source={};Provider=SQLite", dir.join("p.db").display());
    let dal = pek_rcode::Dal::open_with_model(
        &conn,
        pek_rcode::model::EntityModel::parse(WIDE_MODEL).unwrap(),
    )
    .unwrap();
    dal.sync_schema().unwrap();
    dal
}

/// 会话借出 + 归还（连接池一次完整周期）；高并发下的登记/复用开销。
#[test]
#[ignore]
fn bench_session_checkout() {
    use std::sync::Arc;
    const N: usize = 200_000;
    let dal = Arc::new(bench_dal("checkout"));

    // 预热：让池里有一个空闲会话
    drop(dal.open_session().unwrap());

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        let s = dal.open_session().unwrap();
        drop(s);
    }
    report("会话借出+归还（连接池，单线程）", N, t.elapsed(), alloc_since(mark));

    const THREADS: usize = 8;
    const PER: usize = 50_000;
    let start = Instant::now();
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let dal = dal.clone();
            std::thread::spawn(move || {
                for _ in 0..PER {
                    let s = dal.open_session().unwrap();
                    drop(s);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    report(
        &format!("会话借出+归还（{THREADS} 线程并发）"),
        THREADS * PER,
        start.elapsed(),
        0,
    );
}

/// 读会话获取（读写分离判定路径）。
#[test]
#[ignore]
fn bench_read_session() {
    const N: usize = 200_000;
    let dal = bench_dal("read-session");
    drop(dal.read_session().unwrap());
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        let s = dal.read_session_for("DH_Wide").unwrap();
        drop(s);
    }
    report("读会话获取（read_session_for）", N, t.elapsed(), alloc_since(mark));
}

/// 行取值：宽表（20 列）按列名取 20 次。
#[test]
#[ignore]
fn bench_row_lookup_wide() {
    use pek_rcode::RowSet;
    const N: usize = 100_000;
    let columns: Vec<String> = (1..=20).map(|i| format!("C{i}")).collect();
    let mut set = RowSet::new(columns.clone());
    for _ in 0..4 {
        set.push(vec![pek_rcode::DbValue::Int(1); 20]);
    }
    let row = set.first().unwrap();

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        for c in &columns {
            sink += usize::from(row.get_by_name(c).is_some());
        }
    }
    report("行取值（20 列，原文列名 × 20）", N * 20, t.elapsed(), alloc_since(mark));

    // 小写列名（模拟手写代码/OR 映射传小写）
    let lower: Vec<String> = columns.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        for c in &lower {
            sink += usize::from(row.get_by_name(c).is_some());
        }
    }
    report("行取值（20 列，小写列名 × 20）", N * 20, t.elapsed(), alloc_since(mark));
    assert!(sink > 0);
}

/// 写一行（10 列文本）：SQL 构建 + 参数绑定 + 执行（事务内，不含 fsync）。
#[test]
#[ignore]
fn bench_insert_row() {
    const N: usize = 20_000;
    let dal = bench_dal("insert");
    let table = dal.table("Wide").unwrap();
    let mut session = dal.open_session().unwrap();
    let sets: Vec<(&'static str, pek_rcode::DbValue)> = (1..=20)
        .map(|i| {
            let name: &'static str = match i {
                1 => "C1", 2 => "C2", 3 => "C3", 4 => "C4", 5 => "C5",
                6 => "C6", 7 => "C7", 8 => "C8", 9 => "C9", 10 => "C10",
                11 => "C11", 12 => "C12", 13 => "C13", 14 => "C14", 15 => "C15",
                16 => "C16", 17 => "C17", 18 => "C18", 19 => "C19", _ => "C20",
            };
            (name, pek_rcode::DbValue::Text("0123456789abcdef".into()))
        })
        .collect();

    session.begin().unwrap();
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        table.insert(session.as_mut(), &sets).unwrap();
    }
    let elapsed = t.elapsed();
    let allocs = alloc_since(mark);
    session.commit().unwrap();
    report("插入一行（20 列文本，事务内）", N, elapsed, allocs);
}

/// 批量插入摊销（5000/批）。
#[test]
#[ignore]
fn bench_insert_batch() {
    const ROWS: usize = 20_000;
    let dal = bench_dal("batch");
    let table = dal.table("Wide").unwrap();
    let mut session = dal.open_session().unwrap();
    let sets: Vec<(&'static str, pek_rcode::DbValue)> = (1..=20)
        .map(|i| {
            let name: &'static str = match i {
                1 => "C1", 2 => "C2", 3 => "C3", 4 => "C4", 5 => "C5",
                6 => "C6", 7 => "C7", 8 => "C8", 9 => "C9", 10 => "C10",
                11 => "C11", 12 => "C12", 13 => "C13", 14 => "C14", 15 => "C15",
                16 => "C16", 17 => "C17", 18 => "C18", 19 => "C19", _ => "C20",
            };
            (name, pek_rcode::DbValue::Text("0123456789abcdef".into()))
        })
        .collect();
    let rows: Vec<Vec<(&str, pek_rcode::DbValue)>> = (0..ROWS).map(|_| sets.clone()).collect();
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    table.write_batch(session.as_mut(), &rows, pek_rcode::sqlbuild::BatchWriteMode::Insert, Some(5000)).unwrap();
    report("批量插入摊销（5000/批，20 列）", ROWS, t.elapsed(), alloc_since(mark));
}

/// 查询一行并取值（20 列）。
#[test]
#[ignore]
fn bench_select_row() {
    const N: usize = 20_000;
    let dal = bench_dal("select");
    let table = dal.table("Wide").unwrap();
    let mut session = dal.open_session().unwrap();
    let sets: Vec<(&'static str, pek_rcode::DbValue)> = (1..=20)
        .map(|i| {
            let name: &'static str = match i {
                1 => "C1", 2 => "C2", 3 => "C3", 4 => "C4", 5 => "C5",
                6 => "C6", 7 => "C7", 8 => "C8", 9 => "C9", 10 => "C10",
                11 => "C11", 12 => "C12", 13 => "C13", 14 => "C14", 15 => "C15",
                16 => "C16", 17 => "C17", 18 => "C18", 19 => "C19", _ => "C20",
            };
            (name, pek_rcode::DbValue::Text("0123456789abcdef".into()))
        })
        .collect();
    table.insert(session.as_mut(), &sets).unwrap();

    let query = pek_rcode::Query::new().take(1);
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        let set = table.query(session.as_mut(), &query).unwrap();
        sink += set.len();
    }
    report("查询一行（20 列，不取值）", N, t.elapsed(), alloc_since(mark));
    assert!(sink >= N);
}

// ---------------------------------------------------------------------------
// 条件更新（CAS）基准（2026-10-23 审计新增）
// ---------------------------------------------------------------------------

/// 带主键的窄表模型（CAS 需要主键定位）。
const CAS_MODEL: &str = r#"<EntityModel><Tables><Table Name="Cas" TableName="DH_Cas">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="Code" DataType="String" Length="50" />
    <Column Name="Amount" DataType="Int32" />
    <Column Name="Version" DataType="Int32" />
  </Columns>
</Table></Tables></EntityModel>"#;

fn bench_cas_dal(tag: &str) -> pek_rcode::Dal {
    let dir = std::env::temp_dir().join(format!("pek-rcode-cas-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = format!("Data Source={};Provider=SQLite", dir.join("c.db").display());
    let dal = pek_rcode::Dal::open_with_model(
        &conn,
        pek_rcode::model::EntityModel::parse(CAS_MODEL).unwrap(),
    )
    .unwrap();
    dal.sync_schema().unwrap();
    dal
}

/// 条件更新（CAS）端到端：成功 / 冲突，与普通 `update_by_pk` 对照。
///
/// 三者都走同一条 SQLite 语句缓存与参数绑定路径，差异只在 `WHERE` 多出的
/// `expected` 条件与冲突时的缓存失效（无缓存时为原子读快路）。
#[test]
#[ignore]
fn bench_update_if() {
    use pek_rcode::DbValue;
    const N: usize = 20_000;
    let dal = bench_cas_dal("update-if");
    let table = dal.table("Cas").unwrap();
    let mut session = dal.open_session().unwrap();
    let id = table
        .insert(
            session.as_mut(),
            &[
                ("Code", DbValue::Text("A".into())),
                ("Amount", DbValue::Int(1)),
                ("Version", DbValue::Int(1)),
            ],
        )
        .unwrap();

    session.begin().unwrap();

    // 预热：让语句缓存与页缓存进入稳态（避免首个阶段独自承担编译成本）
    for _ in 0..2_000 {
        table
            .update_by_pk(session.as_mut(), &[("Amount", DbValue::Int(1))], &[DbValue::Int(id)])
            .unwrap();
        table
            .update_if(
                session.as_mut(),
                &[("Amount", DbValue::Int(1))],
                &[DbValue::Int(id)],
                &[("Version", DbValue::Int(1))],
            )
            .unwrap();
    }

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        table
            .update_by_pk(session.as_mut(), &[("Amount", DbValue::Int(1))], &[DbValue::Int(id)])
            .unwrap();
    }
    let plain = t.elapsed();
    let plain_allocs = alloc_since(mark);

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        let affected = table
            .update_if(
                session.as_mut(),
                &[("Amount", DbValue::Int(1))],
                &[DbValue::Int(id)],
                &[("Version", DbValue::Int(1))],
            )
            .unwrap();
        assert_eq!(affected, 1);
    }
    let cas_ok = t.elapsed();
    let cas_ok_allocs = alloc_since(mark);

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        let affected = table
            .update_if(
                session.as_mut(),
                &[("Amount", DbValue::Int(2))],
                &[DbValue::Int(id)],
                &[("Version", DbValue::Int(999))],
            )
            .unwrap();
        assert_eq!(affected, 0);
    }
    let cas_conflict = t.elapsed();
    let cas_conflict_allocs = alloc_since(mark);

    session.commit().unwrap();

    report("update_by_pk（对照，事务内）", N, plain, plain_allocs);
    report("update_if 成功（CAS，事务内）", N, cas_ok, cas_ok_allocs);
    report(
        "update_if 冲突（0 行 + 缓存失效快路）",
        N,
        cas_conflict,
        cas_conflict_allocs,
    );
}

/// 拆解成本：框架 SQL 构建/拦截器 vs 数据库执行（同一 SQL 预构建后直接 execute）。
///
/// 高并发评估用：框架侧是每次调用的**固定成本**，与并发数无关；
/// 真正的放大项是数据库往返 / 锁 / WAL 提交，它们远大于框架拼接开销。
#[test]
#[ignore]
fn bench_prebuilt_update() {
    use pek_rcode::DbValue;
    const N: usize = 20_000;
    let dal = bench_cas_dal("prebuilt");
    let table = dal.table("Cas").unwrap();
    let mut session = dal.open_session().unwrap();
    let id = table
        .insert(
            session.as_mut(),
            &[
                ("Code", DbValue::Text("A".into())),
                ("Amount", DbValue::Int(1)),
                ("Version", DbValue::Int(1)),
            ],
        )
        .unwrap();

    let filter = pek_rcode::query::Where::new().eq("Id", DbValue::Int(id));
    let (sql, params) = pek_rcode::sqlbuild::update_sql_owned_named(
        dal.kind(),
        table.meta(),
        table.physical_name(),
        vec![("Amount".to_string(), DbValue::Int(1))],
        &filter,
    )
    .unwrap();

    session.begin().unwrap();
    for _ in 0..2_000 {
        table
            .update_by_pk(
                session.as_mut(),
                &[("Amount", DbValue::Int(1))],
                &[DbValue::Int(id)],
            )
            .unwrap();
        session.execute(&sql, &params).unwrap();
    }

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        session.execute(&sql, &params).unwrap();
    }
    let exec_only = t.elapsed();
    let exec_allocs = alloc_since(mark);

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..N {
        table
            .update_by_pk(
                session.as_mut(),
                &[("Amount", DbValue::Int(1))],
                &[DbValue::Int(id)],
            )
            .unwrap();
    }
    let full = t.elapsed();
    let full_allocs = alloc_since(mark);
    session.commit().unwrap();

    report(
        "预构建 UPDATE 直接执行（数据库+参数绑定）",
        N,
        exec_only,
        exec_allocs,
    );
    report("update_by_pk 端到端（框架+数据库）", N, full, full_allocs);
    let framework = full.as_nanos().saturating_sub(exec_only.as_nanos()) as f64 / N as f64;
    println!("框架侧（SQL 构建+拦截器+缓存判定）≈ {framework:>7.1} ns/op");
}

// ---------------------------------------------------------------------------
// 拦截器写入路径基准（2026-10-23 第十六轮新增）：owned 列名 vs Cow 静态列名
// ---------------------------------------------------------------------------

/// 带审计列的窄表模型（拦截器真正会写入 `CreateTime`/`UpdateTime` 等列）。
const AUDIT_MODEL: &str = r#"<EntityModel><Tables><Table Name="IoA" TableName="DH_IoA">
  <Columns>
    <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
    <Column Name="Code" DataType="String" Length="50" Nullable="True" />
    <Column Name="Amount" DataType="Int32" Nullable="True" />
    <Column Name="Status" DataType="Int32" Nullable="True" />
    <Column Name="Remark" DataType="String" Length="100" Nullable="True" />
    <Column Name="CreateTime" DataType="DateTime" Nullable="True" />
    <Column Name="UpdateTime" DataType="DateTime" Nullable="True" />
    <Column Name="CreateUser" DataType="String" Length="50" Nullable="True" />
    <Column Name="UpdateUser" DataType="String" Length="50" Nullable="True" />
    <Column Name="TraceId" DataType="String" Length="64" Nullable="True" />
  </Columns>
</Table></Tables></EntityModel>"#;

fn bench_audit_dal(tag: &str) -> pek_rcode::Dal {
    let dir = std::env::temp_dir().join(format!("pek-rcode-audit-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = format!("Data Source={};Provider=SQLite", dir.join("a.db").display());
    let dal = pek_rcode::Dal::open_with_model(
        &conn,
        pek_rcode::model::EntityModel::parse(AUDIT_MODEL).unwrap(),
    )
    .unwrap();
    dal.sync_schema().unwrap();
    dal
}

/// 一次 `update_by_pk` / `update_by_pk_static` 对照测量（独立库，避免顺序偏差）。
fn measure_updates(
    dal: &pek_rcode::Dal,
    n: usize,
) -> ((Duration, usize), (Duration, usize)) {
    use pek_rcode::DbValue;
    let table = dal.table("IoA").unwrap();
    let mut session = dal.open_session().unwrap();
    let sets: Vec<(&'static str, DbValue)> = vec![
        ("Code", DbValue::Text("A".into())),
        ("Amount", DbValue::Int(1)),
        ("Status", DbValue::Int(1)),
        ("Remark", DbValue::Text(String::new())),
    ];
    let id = table.insert(session.as_mut(), &sets).unwrap();
    let pk = [DbValue::Int(id)];

    session.begin().unwrap();
    for _ in 0..2_000 {
        table.update_by_pk(session.as_mut(), &sets, &pk).unwrap();
        table.update_by_pk_static(session.as_mut(), &sets, &pk).unwrap();
    }

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..n {
        table.update_by_pk(session.as_mut(), &sets, &pk).unwrap();
    }
    let owned = (t.elapsed(), alloc_since(mark));

    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..n {
        table.update_by_pk_static(session.as_mut(), &sets, &pk).unwrap();
    }
    let cow = (t.elapsed(), alloc_since(mark));
    session.commit().unwrap();
    (owned, cow)
}

/// 插入对照测量（每次用独立库，避开页增长/自增带来的顺序偏差）。
fn measure_insert(dal: &pek_rcode::Dal, n: usize, use_static: bool) -> (Duration, usize) {
    use pek_rcode::DbValue;
    let table = dal.table("IoA").unwrap();
    let mut session = dal.open_session().unwrap();
    let sets: Vec<(&'static str, DbValue)> = vec![
        ("Code", DbValue::Text("A".into())),
        ("Amount", DbValue::Int(1)),
        ("Status", DbValue::Int(1)),
        ("Remark", DbValue::Text(String::new())),
    ];
    session.begin().unwrap();
    let mark = ALLOCS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..n {
        if use_static {
            table.insert_static(session.as_mut(), &sets).unwrap();
        } else {
            table.insert(session.as_mut(), &sets).unwrap();
        }
    }
    let elapsed = t.elapsed();
    let allocs = alloc_since(mark);
    session.commit().unwrap();
    (elapsed, allocs)
}

/// 有拦截器（物联网常见配置）时，写入路径的列名开销：
/// `update_by_pk` / `insert`（owned 列名，每列一次 `String` 分配）
/// vs `update_by_pk_static` / `insert_static`（Cow 静态列名，零分配）。
///
/// 分三档注册（无 / 仅 Time / 三件套）以分离「拦截器自身开销」与「列名分配开销」。
#[test]
#[ignore]
fn bench_interceptor_write_paths() {
    use pek_rcode::interceptor;
    const N: usize = 20_000;
    let saved = interceptor::interceptors();

    // ① 无拦截器（借用快路）
    interceptor::clear();
    let none_dal = bench_audit_dal("none");
    let ((u1, a1), (u2, a2)) = measure_updates(&none_dal, N);
    report("update_by_pk（无拦截器 · 借用快路）", N, u1, a1);
    report("update_by_pk_static（无拦截器 · 委托借用）", N, u2, a2);

    // ② 仅 TimeInterceptor（只补 UpdateTime）
    interceptor::clear();
    interceptor::register(std::sync::Arc::new(interceptor::TimeInterceptor));
    let time_dal = bench_audit_dal("time");
    let ((t1, ta1), (t2, ta2)) = measure_updates(&time_dal, N);
    report("update_by_pk（仅 Time · owned 列名）", N, t1, ta1);
    report("update_by_pk_static（仅 Time · Cow 列名）", N, t2, ta2);

    // ③ 内置三件套（Time + User + Trace）
    interceptor::clear();
    interceptor::enable_defaults();
    let all_dal = bench_audit_dal("all");
    let ((w1, wa1), (w2, wa2)) = measure_updates(&all_dal, N);
    report("update_by_pk（三件套 · owned 列名）", N, w1, wa1);
    report("update_by_pk_static（三件套 · Cow 列名）", N, w2, wa2);

    // ④ 插入（独立库各测一次）
    let ins_owned_dal = bench_audit_dal("ins-owned");
    let (i1, ia1) = measure_insert(&ins_owned_dal, N, false);
    let ins_cow_dal = bench_audit_dal("ins-cow");
    let (i2, ia2) = measure_insert(&ins_cow_dal, N, true);
    report("insert（三件套 · owned 列名）", N, i1, ia1);
    report("insert_static（三件套 · Cow 列名）", N, i2, ia2);

    // 还原注册表（不影响同进程其它基准）
    interceptor::clear();
    for item in saved {
        interceptor::register(item);
    }
}
