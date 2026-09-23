//! fq-net 错误类型。

use thiserror::Error;

/// 结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// fq-net 错误。
#[derive(Debug, Error)]
pub enum Error {
    /// 网络 IO 错误。
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 报文/协议层错误(畸形报文、不支持的类型等)。
    #[error("协议错误: {0}")]
    Protocol(String),

    /// 安全层错误(握手失败、解密失败等)。
    #[error("安全层错误: {0}")]
    Crypto(#[from] fq_crypto::Error),

    /// 目标对端尚不可达(未发现、无可用地址、拨号失败)。
    #[error("对端不可达: {0}")]
    PeerUnreachable(String),

    /// 对端身份未通过信任验证(TOFU Changed 且未获用户确认)。
    #[error("对端身份验证失败: {0}")]
    PeerUntrusted(String),

    /// 对端已离线。
    #[error("对端已离线")]
    PeerOffline,

    /// 内部通道已关闭(节点正在关闭)。
    #[error("节点内部通道已关闭")]
    ShuttingDown,
}

impl From<fq_proto::Error> for Error {
    fn from(e: fq_proto::Error) -> Self {
        Self::Protocol(e.to_string())
    }
}
