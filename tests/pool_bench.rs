//! 连接池开销拆解基准（默认忽略，手工执行）：
//!
//! ```text
//! cargo test --release --test pool_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 目的：把「会话借出+归还 ~152 ns」拆成可归因的几部分（Mutex/VecDeque、Instant::now、Box 分配），
//! 判断哪一块值得优化、哪一块是硬成本。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn report(label: &str, n: usize, elapsed: Duration) {
    let ns = elapsed.as_nanos() as f64 / n as f64;
    println!("{label:<46} {ns:>9.1} ns/op");
}

/// 纯 `Mutex<VecDeque<usize>>` 取放（连接池的最小骨架）。
#[test]
#[ignore]
fn bench_pool_skeleton() {
    const N: usize = 1_000_000;
    let queue = Mutex::new(VecDeque::from(vec![42usize]));
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        let value = {
            let mut q = queue.lock().unwrap();
            q.pop_front().unwrap()
        };
        sink += value;
        {
            let mut q = queue.lock().unwrap();
            q.push_back(value);
        }
    }
    report("池骨架（Mutex + VecDeque 取/放）", N, t.elapsed());
    assert!(sink > 0);
}

/// 加上 `Instant::now()` × 2（借出判过期 + 归还记时刻）。
#[test]
#[ignore]
fn bench_pool_with_instant() {
    const N: usize = 1_000_000;
    let idle_time = Duration::from_secs(30);
    let t = Instant::now();
    let mut sink = 0u128;
    for _ in 0..N {
        let now = Instant::now();
        sink += now.elapsed().as_nanos();
        let _ = now + idle_time;
        let at = Instant::now();
        sink += at.elapsed().as_nanos();
    }
    report("Instant::now ×2（判过期 + 记时刻）", N, t.elapsed());
    assert!(sink > 0);
}

/// 加上 `Box::new`（池化会话包装）。
#[test]
#[ignore]
fn bench_pool_with_box() {
    const N: usize = 1_000_000;
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        let boxed: Box<usize> = Box::new(42);
        sink += *boxed;
    }
    report("Box::new + drop（会话包装）", N, t.elapsed());
    assert!(sink > 0);
}

/// thread_local 读 + 写入成本（用于评估「线程本地会话缓存」方案）。
#[test]
#[ignore]
fn bench_thread_local_read() {
    use std::cell::Cell;
    thread_local! {
        static SLOT: Cell<usize> = const { Cell::new(0) };
    }
    const N: usize = 2_000_000;
    let t = Instant::now();
    let mut sink = 0usize;
    for i in 0..N {
        SLOT.with(|c| c.set(i));
        sink += SLOT.with(|c| c.get());
    }
    report("thread_local 读+写 ×2", N, t.elapsed());
    assert!(sink > 0);
}

/// 原子计数（分片统计）成本：每周期 2 次 `fetch_add`。
#[test]
#[ignore]
fn bench_atomic_counters() {
    const N: usize = 1_000_000;
    let a = AtomicUsize::new(0);
    let b = AtomicUsize::new(0);
    let t = Instant::now();
    for _ in 0..N {
        a.fetch_add(1, Ordering::Relaxed);
        b.fetch_add(1, Ordering::Relaxed);
    }
    report("原子计数 ×2（复用/创建统计）", N, t.elapsed());
}

/// Windows `GetTickCount64` 的并发成本（判断它能否作为连接池粗时钟）。
#[test]
#[ignore]
fn bench_coarse_clock_concurrent() {
    use std::sync::Arc;
    #[inline]
    fn tick() -> u64 {
        #[cfg(windows)]
        {
            unsafe extern "system" {
                safe fn GetTickCount64() -> u64;
            }
            GetTickCount64()
        }
        #[cfg(not(windows))]
        {
            0
        }
    }
    const N: usize = 500_000;

    let t = Instant::now();
    let mut sink = 0u64;
    for _ in 0..N {
        sink = sink.wrapping_add(tick());
    }
    report("GetTickCount64（单线程）", N, t.elapsed());
    assert!(sink < u64::MAX);

    const THREADS: usize = 8;
    let start = Arc::new(Instant::now());
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            std::thread::spawn(move || {
                let mut s = 0u64;
                for _ in 0..N {
                    s = s.wrapping_add(tick());
                }
                s
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap() < u64::MAX);
    }
    report(
        &format!("GetTickCount64（{THREADS} 线程）"),
        THREADS * N,
        start.elapsed(),
    );
}
