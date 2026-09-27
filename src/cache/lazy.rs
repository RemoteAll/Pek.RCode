//! 惰性串行消费者（对应 DH.NCode 的 `LazyConsumer`）。
//!
//! 任务按入队顺序**严格串行**执行；仅在有任务时启动后台线程，队列清空后线程自动退出
//! （空闲时不占线程）。任务内部 panic 会被捕获并忽略，避免中断后续任务。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::lock;

/// 队列中的任务。
type Task = Box<dyn FnOnce() + Send + 'static>;

/// 惰性串行消费者。
pub struct LazyConsumer {
    /// 任务队列
    queue: Arc<Mutex<VecDeque<Task>>>,
    /// 是否有处理线程在运行
    running: Arc<AtomicBool>,
}

impl LazyConsumer {
    /// 创建空消费者。
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 提交任务，由内部线程按入队顺序串行执行。
    pub fn run<F>(&self, task: F)
    where
        F: FnOnce() + Send + 'static,
    {
        lock(&self.queue).push_back(Box::new(task));
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.spawn_worker();
        }
    }

    /// 是否有待处理或正在执行的任务。
    pub fn is_busy(&self) -> bool {
        self.running.load(Ordering::Acquire) || !lock(&self.queue).is_empty()
    }

    /// 等待队列清空（超时返回 `false`）。
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        while self.is_busy() {
            if started.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// 启动处理线程（drain 模式：一次处理完当前队列后退出）。
    fn spawn_worker(&self) {
        let queue = Arc::clone(&self.queue);
        let running = Arc::clone(&self.running);
        let _ = std::thread::Builder::new()
            .name("rcode-lazy".into())
            .spawn(move || loop {
                let task = lock(&queue).pop_front();
                match task {
                    Some(task) => {
                        // 忽略任务内部 panic（与 C# 版一致，不中断队列）
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
                    }
                    None => {
                        running.store(false, Ordering::Release);
                        // 双重检查：清空与退出之间可能有新任务入队
                        if lock(&queue).is_empty()
                            || running
                                .compare_exchange(
                                    false,
                                    true,
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                )
                                .is_err()
                        {
                            break;
                        }
                    }
                }
            });
    }
}

impl Default for LazyConsumer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn executes_in_order_and_drains() {
        let consumer = LazyConsumer::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));

        for index in 0..100 {
            let order = Arc::clone(&order);
            let done = Arc::clone(&done);
            consumer.run(move || {
                lock(&order).push(index);
                done.fetch_add(1, Ordering::SeqCst);
            });
        }

        assert!(consumer.wait_idle(Duration::from_secs(5)), "队列应处理完成");
        assert_eq!(done.load(Ordering::SeqCst), 100);
        assert_eq!(
            *lock(&order),
            (0..100).collect::<Vec<_>>(),
            "任务必须按入队顺序串行执行"
        );
    }

    #[test]
    fn panicking_task_does_not_stop_queue() {
        // 屏蔽 panic 输出噪音
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let consumer = LazyConsumer::new();
        consumer.run(|| panic!("boom"));
        let ok = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ok);
        consumer.run(move || flag.store(true, Ordering::SeqCst));

        let idle = consumer.wait_idle(Duration::from_secs(5));
        std::panic::set_hook(previous);
        assert!(idle, "队列应处理完成");
        assert!(ok.load(Ordering::SeqCst), "任务 panic 不应中断队列");
    }
}
