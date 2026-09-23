//! 协议标识类型:节点 ID 与消息 ID。

use std::fmt;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{Error, Result};

/// 节点唯一标识:`SHA-256(Ed25519 公钥)` 的前 16 字节。
///
/// 之所以用公钥派生而非随机数,是为了让"身份"与"Noise 握手的静态密钥"
/// 天然绑定 —— 攻击者无法在不伪造签名的前提下冒充某个 NodeId。
///
/// 序列化为 32 字符的 hex 字符串(可读性优先,便于抓包排查);
/// 反序列化同时接受 hex 字符串、16 字节二进制与 16 元素数组。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId([u8; 16]);

impl NodeId {
    /// NodeId 的字节长度。
    pub const LEN: usize = 16;

    /// 由原始字节构造。
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 取出原始字节。
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 由 Ed25519 公钥派生节点 ID。
    pub fn from_public_key(public_key: &[u8]) -> Self {
        let digest = Sha256::digest(public_key);
        let mut out = [0u8; Self::LEN];
        out.copy_from_slice(&digest[..Self::LEN]);
        Self(out)
    }

    /// 转 32 字符小写 hex。
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    /// 解析 32 字符 hex。
    pub fn from_hex(raw: &str) -> Result<Self> {
        let bytes = hex::decode(raw).map_err(|e| Error::InvalidNodeId(format!("{raw:?}: {e}")))?;
        let arr: [u8; Self::LEN] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| Error::InvalidNodeId(format!("长度应为 16 字节,实际 {}", v.len())))?;
        Ok(Self(arr))
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 短格式,避免日志刷屏;完整值可用 to_hex()
        write!(f, "NodeId({}…)", &self.to_hex()[..8])
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct NodeIdVisitor;

        impl<'de> Visitor<'de> for NodeIdVisitor {
            type Value = NodeId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("16 字节 NodeId(hex 字符串、二进制或 16 元素数组)")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<NodeId, E> {
                NodeId::from_hex(v).map_err(serde::de::Error::custom)
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> std::result::Result<NodeId, E> {
                let arr: [u8; NodeId::LEN] = v.try_into().map_err(|_| {
                    E::custom(format!("NodeId 字节长度应为 16,实际 {}", v.len()))
                })?;
                Ok(NodeId(arr))
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<NodeId, A::Error> {
                let mut arr = [0u8; NodeId::LEN];
                for (i, slot) in arr.iter_mut().enumerate() {
                    *slot = seq
                        .next_element::<u8>()?
                        .ok_or_else(|| serde::de::Error::custom(format!("NodeId 数组在索引 {i} 处提前结束")))?;
                }
                Ok(NodeId(arr))
            }
        }

        deserializer.deserialize_any(NodeIdVisitor)
    }
}

/// 消息唯一标识(UUIDv7,自带毫秒时间戳,可按时间排序)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MsgId(Uuid);

impl MsgId {
    /// 生成一个新的 UUIDv7 消息 ID。
    pub fn now_v7() -> Self {
        Self(Uuid::now_v7())
    }

    /// 由已有 UUID 构造。
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// 取出内部 UUID。
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// 该消息 ID 内嵌的毫秒时间戳(UUIDv7 特性,可用于延迟观测)。
    pub fn timestamp_ms(self) -> Option<u64> {
        self.0.get_timestamp().map(|ts| ts.to_unix().0 * 1000 + u64::from(ts.to_unix().1) / 1_000_000)
    }

    /// 解析字符串形式的消息 ID(历史库反查用)。
    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        let uuid = Uuid::parse_str(raw).map_err(|e| crate::error::Error::Decode(e.to_string()))?;
        Ok(Self(uuid))
    }
}

impl fmt::Debug for MsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MsgId({self})")
    }
}

impl fmt::Display for MsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for MsgId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MsgId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let uuid = Uuid::parse_str(&raw).map_err(serde::de::Error::custom)?;
        Ok(Self(uuid))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn node_id_hex_roundtrip() {
        let id = NodeId::from_bytes([0xAB; 16]);
        let hex = id.to_hex();
        assert_eq!(hex.len(), 32);
        assert_eq!(NodeId::from_hex(&hex).unwrap(), id);
    }

    #[test]
    fn node_id_rejects_bad_hex() {
        assert!(NodeId::from_hex("zz").is_err());
        assert!(NodeId::from_hex("abcd").is_err());
        assert!(NodeId::from_hex("").is_err());
    }

    #[test]
    fn node_id_is_derived_from_public_key() {
        let a = NodeId::from_public_key(&[1u8; 32]);
        let b = NodeId::from_public_key(&[1u8; 32]);
        let c = NodeId::from_public_key(&[2u8; 32]);
        assert_eq!(a, b, "相同公钥必须得到相同 NodeId");
        assert_ne!(a, c, "不同公钥必须得到不同 NodeId");
    }

    #[test]
    fn msg_id_is_time_ordered() {
        let first = MsgId::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = MsgId::now_v7();
        assert!(first < second, "UUIDv7 必须按时间单调递增");
        assert_ne!(first, second);
    }
}
