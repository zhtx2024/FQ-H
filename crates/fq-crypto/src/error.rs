//! fq-crypto 错误类型。所有由不可信输入触发的失败都必须落到这里,不允许 panic。

use thiserror::Error;

/// 结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// fq-crypto 错误。
#[derive(Debug, Error)]
pub enum Error {
    /// 系统熵源不可用(极少见,但必须处理)。
    #[error("熵源失败: {0}")]
    Entropy(String),

    /// 底层密码学库错误(密钥长度、签名验证失败等)。
    #[error("密码学操作失败: {0}")]
    Crypto(String),

    /// 密钥绑定签名验证失败 —— 声称的 NodeId 与其 Noise 静态密钥没有合法绑定。
    #[error("密钥绑定验证失败: {0}")]
    BindingInvalid(String),

    /// Noise 握手失败(对端静态密钥不匹配、报文被篡改、状态错乱)。
    #[error("握手失败: {0}")]
    Handshake(String),

    /// 传输模式解密失败(密文被篡改、乱序或重放)。
    #[error("解密失败(密文被篡改或乱序)")]
    Decrypt,

    /// 身份/信任存储文件损坏。
    #[error("存储文件损坏: {0}")]
    CorruptedStore(String),

    /// 文件 IO 错误。
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}
