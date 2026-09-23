//! 能力协商位集。
//!
//! 前向兼容要求(强制):解码时必须保留**未知位**,因此 serde 实现刻意不走
//! `bitflags` 自带的 `from_bits`(它会在遇到未知位时报错)。这样做的代价是
//! `Capabilities` 的 serde 需要手写,收益是新版本节点发送的新能力位不会让
//! 老版本节点解码失败。

use bitflags::bitflags;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

bitflags! {
    /// 节点能力位集。位 32..63 预留给实验特性,任何实现都不得占用。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Capabilities: u64 {
        /// 纯文本消息。
        const TEXT            = 1 << 0;
        /// Markdown 富文本消息。
        const MARKDOWN        = 1 << 1;
        /// 单文件传输。
        const FILE_TRANSFER   = 1 << 2;
        /// 文件断点续传(支持 FileRequest.offset)。
        const FILE_RESUME     = 1 << 3;
        /// 目录(递归)传输。
        const DIRECTORY       = 1 << 4;
        /// 正在输入提示。
        const TYPING          = 1 << 5;
        /// 送达/已读回执。
        const READ_RECEIPT    = 1 << 6;
        /// 群聊。
        const GROUP_CHAT      = 1 << 7;
        /// 头像同步。
        const AVATAR          = 1 << 8;
        /// 在线状态(离开/忙碌/勿扰)。
        const PRESENCE_STATUS = 1 << 9;
        /// Noise IK 安全通道(本协议 v1 的必选能力)。
        const NOISE_IK        = 1 << 10;
        /// IPv6 组播发现。
        const IPV6            = 1 << 11;
    }
}

impl Capabilities {
    /// 本协议 v1 要求的最小能力集。
    pub const REQUIRED_V1: Self = Self::TEXT.union(Self::NOISE_IK);

    /// 本端已定义的所有能力位(用于识别"未知位")。
    pub const KNOWN: Self = Self::TEXT
        .union(Self::MARKDOWN)
        .union(Self::FILE_TRANSFER)
        .union(Self::FILE_RESUME)
        .union(Self::DIRECTORY)
        .union(Self::TYPING)
        .union(Self::READ_RECEIPT)
        .union(Self::GROUP_CHAT)
        .union(Self::AVATAR)
        .union(Self::PRESENCE_STATUS)
        .union(Self::NOISE_IK)
        .union(Self::IPV6);

    /// 对端声明了但本端尚未定义的能力位(诊断用,不影响解码)。
    pub fn unknown_bits(self) -> u64 {
        self.bits() & !Self::KNOWN.bits()
    }

    /// 判断是否满足本协议 v1 的最低要求。
    pub fn satisfies_v1(self) -> bool {
        self.contains(Self::REQUIRED_V1)
    }

    /// 双方共同支持的能力(用于协商实际启用哪些特性)。
    pub fn negotiated(self, peer: Self) -> Self {
        self.intersection(peer)
    }
}

impl Default for Capabilities {
    /// 默认能力集为空 —— 能力必须由节点显式声明,不做乐观假设。
    fn default() -> Self {
        Self::empty()
    }
}

impl Serialize for Capabilities {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.bits())
    }
}

impl<'de> Deserialize<'de> for Capabilities {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = u64::deserialize(deserializer)?;
        Ok(Self::from_bits_retain(raw))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn known_bits_roundtrip() {
        let caps = Capabilities::TEXT | Capabilities::FILE_TRANSFER | Capabilities::READ_RECEIPT;
        let raw = caps.bits();
        assert_eq!(Capabilities::from_bits_retain(raw), caps);
    }

    #[test]
    fn unknown_bits_are_preserved_not_rejected() {
        // 模拟未来版本节点发送了一个本端不认识的能力位(位 40)
        let future_bit = 1u64 << 40;
        let caps = Capabilities::from_bits_retain(future_bit | Capabilities::TEXT.bits());
        assert!(
            caps.contains(Capabilities::from_bits_retain(future_bit)),
            "未知能力位必须被保留,否则无法原样转发/协商"
        );
        assert!((caps & Capabilities::TEXT).contains(Capabilities::TEXT));
    }

    #[test]
    fn negotiation_is_intersection() {
        let mine = Capabilities::TEXT | Capabilities::MARKDOWN;
        let peer = Capabilities::TEXT | Capabilities::FILE_TRANSFER;
        let agreed = mine.negotiated(peer);
        assert!(agreed.contains(Capabilities::TEXT));
        assert!(!agreed.contains(Capabilities::MARKDOWN));
        assert!(!agreed.contains(Capabilities::FILE_TRANSFER));
    }

    #[test]
    fn v1_requirement_check() {
        assert!(!Capabilities::TEXT.satisfies_v1());
        assert!((Capabilities::TEXT | Capabilities::NOISE_IK).satisfies_v1());
    }
}
