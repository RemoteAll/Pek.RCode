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