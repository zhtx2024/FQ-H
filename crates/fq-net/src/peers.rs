//! 对端表:在线成员的内存视图(发现 → 更新 → 离线)。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fq_proto::{Capabilities, NodeId, PresenceStatus};

/// 一个已发现对端的最新快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// 对端 NodeId。
    pub node_id: NodeId,
    /// 昵称。
    pub display_name: String,
    /// 主机名。
    pub host_name: String,
    /// 在线状态。
    pub status: PresenceStatus,
    /// 分组。
    pub group: Option<String>,
    /// 能力声明。
    pub capabilities: Capabilities,
    /// Ed25519 身份公钥(已通过绑定签名验证)。
    pub ed25519_public_key: [u8; 32],
    /// X25519 Noise 静态公钥(已通过绑定签名验证)。
    pub noise_static: [u8; 32],
    /// 可用 TCP 端点(按优先级)。
    pub endpoints: Vec<SocketAddr>,
    /// 对端软件版本(对端未上报时为 None)。
    pub app_version: Option<String>,
    /// 对端头像内容哈希(对端未设置头像时为 None)。
    pub avatar_sha256: Option<String>,
    /// 最后一次收到通告的时刻(单调时钟,用于超时判定)。
    pub last_seen: Instant,
}

/// 对端表变化类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerChange {
    /// 首次发现。
    Discovered,
    /// 信息发生变化(昵称/状态/端点等)。
    Updated,
}

/// 线程安全的对端表。
///
/// 离线的对端**保留条目**(显示为 Offline、保留端点便于重连),
/// 只有 [`PeerTable::remove`] 才真正删除。
#[derive(Debug, Clone, Default)]
pub struct PeerTable {
    inner: Arc<Mutex<HashMap<NodeId, PeerInfo>>>,
}

impl PeerTable {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 应用一次通告(调用方已完成签名与 TOFU 验证)。
    ///
    /// `endpoint_hint` 是从 UDP 源地址推导的可达端点,会被并入对端端点列表头部。
    ///
    /// 返回 `Some(Discovered)`(首次发现)/ `Some(Updated)`(信息变化)/
    /// `None`(仅心跳刷新,无需产生事件)。
    pub fn apply(&self, mut info: PeerInfo, endpoint_hint: SocketAddr) -> Option<PeerChange> {
        let mut guard = self.lock();
        match guard.get_mut(&info.node_id) {
            None => {
                info.endpoints.insert(0, endpoint_hint);
                guard.insert(info.node_id, info);
                Some(PeerChange::Discovered)
            }
            Some(existing) => {
                // 保留历史端点:新通告的 endpoints 常为空,可达端点由 hint 推导
                let mut endpoints = existing.endpoints.clone();
                if !endpoints.contains(&endpoint_hint) {
                    endpoints.insert(0, endpoint_hint);
                }
                for extra in info.endpoints.drain(..) {
                    if !endpoints.contains(&extra) {
                        endpoints.push(extra);
                    }
                }
                info.endpoints = endpoints;

                let changed = existing.display_name != info.display_name
                    || existing.host_name != info.host_name
                    || existing.status != info.status
                    || existing.group != info.group
                    || existing.noise_static != info.noise_static
                    // 头像/版本/能力变化也要产生"更新"事件,否则上层不会去拉头像或提示更新
                    || existing.avatar_sha256 != info.avatar_sha256
                    || existing.app_version != info.app_version
                    || existing.capabilities != info.capabilities;
                *existing = info;
                changed.then_some(PeerChange::Updated)
            }
        }
    }

    /// 标记对端主动下线。返回 `true` 表示状态发生了变化(之前不在线)。
    pub fn mark_offline(&self, node_id: &NodeId) -> bool {
        let mut guard = self.lock();
        match guard.get_mut(node_id) {
            Some(peer) if peer.status != PresenceStatus::Offline => {
                peer.status = PresenceStatus::Offline;
                true
            }
            _ => false,
        }
    }

    /// 心跳超时清扫:把超过 `timeout` 未见到的对端标记为离线,返回它们的 NodeId。
    pub fn sweep(&self, timeout: Duration) -> Vec<NodeId> {
        let now = Instant::now();
        let mut guard = self.lock();
        guard
            .iter_mut()
            .filter(|(_, peer)| {
                peer.status != PresenceStatus::Offline
                    && now.duration_since(peer.last_seen) > timeout
            })
            .map(|(id, peer)| {
                peer.status = PresenceStatus::Offline;
                *id
            })
            .collect()
    }

    /// 查询对端。
    pub fn get(&self, node_id: &NodeId) -> Option<PeerInfo> {
        self.lock().get(node_id).cloned()
    }

    /// 按 Noise 静态公钥反查(入站握手后确定对端身份用)。
    pub fn find_by_noise_static(&self, key: &[u8; 32]) -> Option<NodeId> {
        self.lock()
            .iter()
            .find(|(_, peer)| &peer.noise_static == key)
            .map(|(id, _)| *id)
    }

    /// 全部对端快照(按昵称排序,便于 UI 展示)。
    pub fn list(&self) -> Vec<PeerInfo> {
        let mut peers: Vec<PeerInfo> = self.lock().values().cloned().collect();
        peers.sort_by(|a, b| {
            a.display_name
                .cmp(&b.display_name)
                .then(a.node_id.cmp(&b.node_id))
        });
        peers
    }

    /// 删除条目(极少用:用户手动移除)。
    pub fn remove(&self, node_id: &NodeId) -> Option<PeerInfo> {
        self.lock().remove(node_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<NodeId, PeerInfo>> {
        // 中毒锁意味着某个持有者 panic;对端表是自洽的简单结构,恢复优于永久卡死
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn peer(name: &str, static_byte: u8) -> PeerInfo {
        PeerInfo {
            node_id: NodeId::from_bytes([static_byte; 16]),
            display_name: name.to_string(),
            host_name: "host".to_string(),
            status: PresenceStatus::Online,
            group: None,
            capabilities: Capabilities::TEXT,
            ed25519_public_key: [static_byte; 32],
            noise_static: [static_byte; 32],
            app_version: Some("0.1.0".into()),
            avatar_sha256: None,
            endpoints: vec![],
            last_seen: Instant::now(),
        }
    }

    fn hint(b: u8) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 40000 + u16::from(b)))
    }

    #[test]
    fn first_presence_discovers() {
        let table = PeerTable::new();
        let info = peer("张三", 1);
        assert_eq!(
            table.apply(info.clone(), hint(1)),
            Some(PeerChange::Discovered)
        );
        assert_eq!(table.get(&info.node_id).unwrap().display_name, "张三");
        // hint 成为端点
        assert_eq!(table.get(&info.node_id).unwrap().endpoints, vec![hint(1)]);
    }

    #[test]
    fn heartbeat_refresh_is_not_an_event() {
        let table = PeerTable::new();
        let info = peer("张三", 2);
        table.apply(info.clone(), hint(2));
        // 同内容再次通告 → None
        assert_eq!(table.apply(info, hint(2)), None);
    }

    #[test]
    fn name_change_is_an_update() {
        let table = PeerTable::new();
        let info = peer("旧名", 3);
        table.apply(info.clone(), hint(3));
        let mut renamed = info.clone();
        renamed.display_name = "新名".to_string();
        assert_eq!(table.apply(renamed, hint(3)), Some(PeerChange::Updated));
        assert_eq!(table.get(&info.node_id).unwrap().display_name, "新名");
    }

    #[test]
    fn endpoints_accumulate_without_duplicates() {
        let table = PeerTable::new();
        let info = peer("张三", 4);
        table.apply(info.clone(), hint(4));
        // 换一个源地址通告 → 端点累积,不重复
        table.apply(info.clone(), hint(44));
        let stored = table.get(&info.node_id).unwrap();
        assert_eq!(stored.endpoints.len(), 2);
        assert!(stored.endpoints.contains(&hint(4)));
        assert!(stored.endpoints.contains(&hint(44)));
    }

    #[test]
    fn timeout_sweep_marks_offline_once() {
        let table = PeerTable::new();
        let info = peer("张三", 5);
        let node_id = info.node_id;
        table.apply(info, hint(5));
        std::thread::sleep(Duration::from_millis(2));
        let timed_out = table.sweep(Duration::from_millis(1));
        assert_eq!(timed_out, vec![node_id], "超时后应被标记离线");
        // 已离线的对端不再重复报告
        assert!(table.sweep(Duration::from_millis(0)).is_empty());
    }

    #[test]
    fn leave_marks_offline() {
        let table = PeerTable::new();
        let info = peer("张三", 6);
        table.apply(info.clone(), hint(6));
        assert!(table.mark_offline(&info.node_id));
        assert_eq!(
            table.get(&info.node_id).unwrap().status,
            PresenceStatus::Offline
        );
        // 重复下线不再变化
        assert!(!table.mark_offline(&info.node_id));
    }

    #[test]
    fn reverse_lookup_by_noise_static() {
        let table = PeerTable::new();
        let info = peer("张三", 7);
        let node_id = info.node_id;
        table.apply(info, hint(7));
        assert_eq!(table.find_by_noise_static(&[7u8; 32]), Some(node_id));
        assert_eq!(table.find_by_noise_static(&[9u8; 32]), None);
    }
}
