//! 协议层错误类型。
//!
//! 设计约束:这些错误会被不可信的网络输入触发,因此必须**只**通过 `Err` 返回,
//! 不允许在解码路径上出现 panic / abort。

use thiserror::Error;

/// 协议层结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// 协议层错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// 序列化报文失败(本端问题,通常不会发生)。
    #[error("报文编码失败: {0}")]
    Encode(String),

    /// 反序列化报文失败(可能是对端版本不匹配或恶意输入)。
    #[error("报文解码失败: {0}")]
    Decode(String),

    /// 帧长度超过本端允许上限。
    #[error("帧长度 {size} 字节超出上限 {max} 字节")]
    FrameTooLarge {
        /// 对端声明的帧长度。
        size: usize,
        /// 本端允许的最大帧长度。
        max: usize,
    },

    /// 帧长度前缀不完整。
    #[error("帧长度前缀不足: 需要 4 字节,实际 {0} 字节")]
    FrameHeaderTruncated(usize),

    /// 协议版本超出本端支持范围。
    #[error("协议版本不兼容: 收到 v{got},本端支持 v{supported}")]
    UnsupportedVersion {
        /// 对端声明的版本。
        got: u16,
        /// 本端支持的最高版本。
        supported: u16,
    },

    /// MessagePack 结构非法(长度越界、类型标记非法、嵌套过深等)。
    #[error("MessagePack 结构非法: {0}")]
    MalformedMsgpack(String),

    /// 节点 ID 解析失败。
    #[error("非法节点 ID: {0}")]
    InvalidNodeId(String),
}
