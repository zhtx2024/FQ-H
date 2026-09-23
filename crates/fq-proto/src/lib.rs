//! # fq-proto
//!
//! feiqiu-r 的局域网通信协议 v1:**纯逻辑、零 IO**。
//!
//! 把协议逻辑独立成 IO-free crate,是为了让"协议正确性"这件事可以被
//! 单元测试和模糊测试彻底锁死 —— 网络层、存储层、UI 层的 bug 不会污染
//! 协议层的判断。
//!
//! ## 分层
//!
//! ```text
//! Envelope / Kind        ← 报文语义(本 crate)
//! MessagePack 编解码      ← codec(强制 map 编码,保证可扩展)
//! MessagePack 结构预校验  ← msgpack_guard(防超大长度分配 / 栈溢出)
//! 4 字节长度前缀分帧      ← frame(防粘包/半包/超长帧)
//! ```
//!
//! ## 安全契约(强制)
//!
//! 所有解码路径只允许返回 [`Error`],**不允许 panic / abort**。不可信输入
//! 包括:截断的帧、随机字节、超大长度声明、超深嵌套、未知枚举取值。
//!
//! ## 快速上手
//!
//! ```
//! use fq_proto::{Envelope, Kind, NodeId, PingBody, codec};
//!
//! let me = NodeId::from_bytes([7u8; 16]);
//! let envelope = Envelope::broadcast(me, Kind::Ping(PingBody { nonce: 42 }));
//!
//! let wire = codec::encode_framed(&envelope).expect("编码失败");
//! // …通过 TCP 发送 wire…
//!
//! let mut decoder = fq_proto::FrameDecoder::with_default_limit();
//! decoder.feed(&wire).expect("分帧失败");
//! let payload = decoder.next_frame().expect("分帧失败").expect("帧不完整");
//! let decoded = codec::decode_framed(&payload).expect("解码失败");
//! assert_eq!(decoded, envelope);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod capability;
pub mod codec;
pub mod error;
pub mod frame;
pub mod ids;
pub mod message;
pub mod msgpack_guard;

pub use capability::Capabilities;
pub use codec::PROTOCOL_VERSION;
pub use error::{Error, Result};
pub use frame::{FrameDecoder, MAX_FRAME_BYTES, encode_frame};
pub use ids::{MsgId, NodeId};
pub use message::{
    AckBody, AckStatus, AvatarPayload, AvatarReply, AvatarRequest, DEFAULT_PORT, Envelope,
    FileAbort, FileChunk, FileDone, FileEntry, FileKind, FileManifest, FileOffer, FileRequest,
    Kind, PingBody, PresenceEvent, PresenceInfo, PresenceStatus, TextBody, TextFormat, TypingBody,
    TypingState, UpdateOffer, UpdateRequest, now_ms,
};
