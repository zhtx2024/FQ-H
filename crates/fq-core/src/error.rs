//! fq-core 错误类型。

use thiserror::Error;

/// 结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// fq-core 错误。
#[derive(Debug, Error)]
pub enum Error {
    /// 启动失败。
    #[error("启动失败: {0}")]
    Start(String),
    /// 安全层错误(身份/密钥/信任)。
    #[error("安全层错误: {0}")]
    Crypto(#[from] fq_crypto::Error),
    /// 协议层错误。
    #[error("协议层错误: {0}")]
    Proto(#[from] fq_proto::Error),
    /// 网络层错误。
    #[error("网络层错误: {0}")]
    Net(#[from] fq_net::Error),
    /// 存储层错误。
    #[error("存储层错误: {0}")]
    Store(#[from] fq_store::Error),
}
