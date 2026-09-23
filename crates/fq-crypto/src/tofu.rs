//! TOFU(首次使用即信任)静态密钥固定存储。
//!
//! 安全语义(强制):
//!
//! * [`TrustDecision::FirstUse`] —— 该 NodeId 首次出现,应把指纹**展示给用户确认**后再固定;
//! * [`TrustDecision::Trusted`] —— 与固定值一致,正常通信;
//! * [`TrustDecision::Changed`] —— **同一 NodeId 换了静态密钥**。这可能是对端
//!   重装系统,也可能是中间人攻击。**绝不静默更新固定值**,必须由用户显式决策;
//!   换绑提供 CAS(比较并交换)接口 [`TofuStore::replace_pin`],防止并发误替换。

use std::collections::BTreeMap;

use fq_proto::NodeId;

use crate::channel::STATIC_KEY_LEN;
use crate::error::{Error, Result};
use crate::fingerprint::static_key_fingerprint;

/// TOFU 校验结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustDecision {
    /// 首次见到该节点:应展示指纹,经用户确认后调用 [`TofuStore::pin`]。
    FirstUse,
    /// 与已固定密钥一致,可放心通信。
    Trusted,
    /// 密钥发生变化:可能是重装,也可能是攻击,**必须交由用户裁决**。
    Changed {
        /// 已固定的旧指纹(展示用)。
        pinned: String,
        /// 本次呈现的新指纹(展示用)。
        presented: String,
    },
}

/// NodeId → 静态公钥 的固定存储。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TofuStore {
    pins: BTreeMap<NodeId, [u8; STATIC_KEY_LEN]>,
}

impl TofuStore {
    /// 空存储。
    pub fn new() -> Self {
        Self::default()
    }

    /// 由键值对集合构造(持久化恢复用)。
    pub fn from_pairs(pairs: impl IntoIterator<Item = (NodeId, [u8; STATIC_KEY_LEN])>) -> Self {
        Self {
            pins: pairs.into_iter().collect(),
        }
    }

    /// 校验一个节点本次呈现的静态公钥。
    pub fn verify(&self, node_id: &NodeId, static_public: &[u8; STATIC_KEY_LEN]) -> TrustDecision {
        match self.pins.get(node_id) {
            None => TrustDecision::FirstUse,
            Some(pinned) if pinned == static_public => TrustDecision::Trusted,
            Some(pinned) => TrustDecision::Changed {
                pinned: static_key_fingerprint(pinned),
                presented: static_key_fingerprint(static_public),
            },
        }
    }

    /// 固定一个节点的静态公钥(首次确认后调用;已存在则覆盖,语义同"用户确认了新密钥")。
    pub fn pin(&mut self, node_id: NodeId, static_public: &[u8; STATIC_KEY_LEN]) {
        self.pins.insert(node_id, *static_public);
    }

    /// 解除固定(用户明确不再信任)。
    pub fn unpin(&mut self, node_id: &NodeId) -> bool {
        self.pins.remove(node_id).is_some()
    }

    /// CAS 换绑:仅当当前固定值等于 `expected_old` 时才替换为 `new`。
    ///
    /// 返回 `Err` 表示固定值与预期不符(并发修改或输入错误),替换未发生。
    pub fn replace_pin(
        &mut self,
        node_id: &NodeId,
        expected_old: &[u8; STATIC_KEY_LEN],
        new: &[u8; STATIC_KEY_LEN],
    ) -> Result<()> {
        match self.pins.get(node_id) {
            Some(current) if current == expected_old => {
                self.pins.insert(*node_id, *new);
                Ok(())
            }
            Some(current) => Err(Error::Crypto(format!(
                "CAS 换绑失败:固定值与预期不符(预期 {},实际 {})",
                static_key_fingerprint(expected_old),
                static_key_fingerprint(current),
            ))),
            None => Err(Error::Crypto("该节点尚未固定,无法换绑".into())),
        }
    }

    /// 是否已固定该节点。
    pub fn contains(&self, node_id: &NodeId) -> bool {
        self.pins.contains_key(node_id)
    }

    /// 固定条目数量。
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    /// 遍历固定条目(持久化保存用)。
    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &[u8; STATIC_KEY_LEN])> {
        self.pins.iter()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn key(byte: u8) -> [u8; STATIC_KEY_LEN] {
        [byte; STATIC_KEY_LEN]
    }

    #[test]
    fn lifecycle_first_use_trusted_changed() {
        let node = NodeId::from_bytes([7u8; 16]);
        let mut store = TofuStore::new();

        // 首次 → FirstUse
        assert_eq!(store.verify(&node, &key(1)), TrustDecision::FirstUse);

        // 固定后 → Trusted
        store.pin(node, &key(1));
        assert_eq!(store.verify(&node, &key(1)), TrustDecision::Trusted);

        // 换密钥 → Changed,且不能被静默接受
        match store.verify(&node, &key(2)) {
            TrustDecision::Changed { pinned, presented } => {
                assert_eq!(pinned, static_key_fingerprint(&key(1)));
                assert_eq!(presented, static_key_fingerprint(&key(2)));
            }
            other => panic!("密钥变化必须报告 Changed,实际 {other:?}"),
        }
        // 校验不改变固定值
        assert_eq!(store.verify(&node, &key(1)), TrustDecision::Trusted);
    }

    #[test]
    fn replace_pin_is_cas() {
        let node = NodeId::from_bytes([8u8; 16]);
        let mut store = TofuStore::new();
        store.pin(node, &key(1));

        // 预期旧值错误 → 拒绝,固定值不变
        assert!(store.replace_pin(&node, &key(9), &key(2)).is_err());
        assert_eq!(store.verify(&node, &key(1)), TrustDecision::Trusted);

        // 预期旧值正确 → 换绑成功
        store.replace_pin(&node, &key(1), &key(2)).unwrap();
        assert_eq!(store.verify(&node, &key(2)), TrustDecision::Trusted);
        assert_eq!(
            store.verify(&node, &key(1)),
            TrustDecision::Changed {
                pinned: static_key_fingerprint(&key(2)),
                presented: static_key_fingerprint(&key(1)),
            }
        );
    }

    #[test]
    fn unpin_removes_entry() {
        let node = NodeId::from_bytes([9u8; 16]);
        let mut store = TofuStore::new();
        store.pin(node, &key(1));
        assert!(store.unpin(&node));
        assert!(!store.contains(&node));
        assert_eq!(store.verify(&node, &key(1)), TrustDecision::FirstUse);
        assert!(!store.unpin(&node), "重复解绑应返回 false");
    }
}
