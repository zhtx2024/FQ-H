//! fq-store 错误类型。

use thiserror::Error;

/// 结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// fq-store 错误。
#[derive(Debug, Error)]
pub enum Error {
    /// SQLite 错误。
    #[error("数据库错误: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// 数据损坏/不一致。
    #[error("数据不一致: {0}")]
    Corrupted(String),
}
