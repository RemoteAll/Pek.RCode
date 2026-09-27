//! 统一错误类型。
//!
//! 对应 DH.NCode 中的 `XCodeException` 等异常体系，按来源分类便于上层处理。

use thiserror::Error;

/// 库统一错误。
#[derive(Debug, Error)]
pub enum Error {
    /// 文件/IO 错误
    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),

    /// XML 解析错误
    #[error("XML 解析失败：{0}")]
    Xml(String),

    /// 数据模型错误（Model.xml 内容不合法）
    #[error("数据模型错误：{0}")]
    Model(String),

    /// 数据库访问错误
    #[error("数据库错误：{0}")]
    Db(String),

    /// 功能尚不支持（例如驱动未实现）
    #[error("暂不支持：{0}")]
    Unsupported(String),

    /// 参数错误（对应 ArgumentException 系列）
    #[error("参数错误：{0}")]
    Argument(String),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}

/// 库统一结果类型。
pub type Result<T> = std::result::Result<T, Error>;
