//! 同步驱动共用的 tokio 运行时：为异步实现（tiberius / mongodb）提供 `block_on` 桥接。
//!
//! 约定：**不要在 tokio 异步上下文中调用**同步驱动（会直接返回可读错误，避免死锁）。

use std::sync::OnceLock;

use tokio::runtime::Runtime;

use crate::error::{Error, Result};

/// 获取共享运行时（首次调用时创建，双工作线程）。
pub(crate) fn runtime() -> Result<&'static Runtime> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(Error::Unsupported(
            "该驱动为同步接口，不能在 tokio 异步上下文中调用（会死锁）；请在独立线程或同步代码中调用"
                .into(),
        ));
    }

    static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("pek-rcode-async")
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| Error::Db(format!("初始化 tokio 运行时失败：{e}")))
}
