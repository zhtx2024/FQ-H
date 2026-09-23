//! 报文编解码入口。
//!
//! # 为什么强制使用 map 编码(`to_vec_named`)
//!
//! `rmp_serde::to_vec` 默认把 Rust 结构体编码成 MessagePack **定长数组**。
//! 这在本协议里有三个致命后果:
//!
//! 1. 内部标签枚举(`#[serde(tag = "kind")]`)无法注入 tag,序列化直接报错。
//! 2. 数组是定长的,新增字段即破坏与老版本的兼容 —— 与协议契约冲突。
//! 3. `#[serde(default)]` 永远不会生效(数组长度不匹配就是硬错误)。
//!
//! 因此 [`encode`] 固定使用 `to_vec_named`(结构体 → map)。解码侧
//! [`decode`] 先用 [`crate::msgpack_guard`] 做安全校验,再交给 serde。

use crate::error::{Error, Result};
use crate::frame::{self, MAX_FRAME_BYTES};
use crate::message::Envelope;
use crate::msgpack_guard;

/// 本端支持的协议版本。
pub const PROTOCOL_VERSION: u16 = 1;

/// 编码为 MessagePack 字节(结构体按 map 编码)。
pub fn encode(envelope: &Envelope) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(envelope).map_err(|e| Error::Encode(e.to_string()))
}

/// 解码 MessagePack 字节,**不**校验版本兼容性。
///
/// 用于"能解出来但要自己决定怎么处理新版本"的场景(例如回复一条
/// "请升级"提示,而不是静默丢包)。
pub fn decode(bytes: &[u8]) -> Result<Envelope> {
    msgpack_guard::validate(bytes)?;
    rmp_serde::from_slice(bytes).map_err(|e| Error::Decode(e.to_string()))
}

/// 解码并强制校验版本兼容性,不兼容时返回 [`Error::UnsupportedVersion`]。
pub fn decode_checked(bytes: &[u8]) -> Result<Envelope> {
    let envelope = decode(bytes)?;
    if !envelope.is_protocol_compatible() {
        return Err(Error::UnsupportedVersion {
            got: envelope.v,
            supported: PROTOCOL_VERSION,
        });
    }
    Ok(envelope)
}

/// 编码并加上 4 字节长度前缀,得到可直接写入 TCP 的字节。
pub fn encode_framed(envelope: &Envelope) -> Result<Vec<u8>> {
    let payload = encode(envelope)?;
    frame::encode_frame(&payload, MAX_FRAME_BYTES)
}

/// 解码一帧的载荷(已完成版本校验)。
pub fn decode_framed(payload: &[u8]) -> Result<Envelope> {
    decode_checked(payload)
}

/// 编码并加长度前缀,但失败时返回 `None` 而不报错(适合 `?` 不便的场景)。
pub fn try_encode_framed(envelope: &Envelope) -> Option<Vec<u8>> {
    encode_framed(envelope).ok()
}
