//! 消息 ID 去重窗口。
//!
//! 局域网 + 重传 + 重连场景下,同一条消息可能被投递多次(至少一次语义)。
//! 接收侧用 MsgId 做去重,把"至少一次"还原成"恰好一次"。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use fq_proto::MsgId;

/// 基于 TTL + 容量上限的去重窗口。
#[derive(Debug)]
pub struct DedupWindow {
    seen: HashMap<MsgId, Instant>,
    ttl: Duration,
    cap: usize,
}

impl DedupWindow {
    /// 创建窗口:`ttl` 内见过的 ID 视为重复;最多缓存 `cap` 条。
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            seen: HashMap::new(),
            ttl,
            cap: cap.max(1),
        }
    }

    /// 记录一个消息 ID。
    ///
    /// 返回 `true` 表示**首次**见到(应当投递);`false` 表示重复(应当丢弃)。
    pub fn insert(&mut self, id: MsgId) -> bool {
        // 先清理再查重:否则已过期条目仍会把新消息误判为重复
        let now = Instant::now();
        self.prune(now);
        if self.seen.contains_key(&id) {
            return false;
        }
        // 容量保护:仍超限则逐出最旧条目
        while self.seen.len() >= self.cap {
            let Some(oldest) = self.seen.iter().min_by_key(|(_, t)| **t).map(|(k, _)| *k) else {
                break;
            };
            self.seen.remove(&oldest);
        }
        self.seen.insert(id, now);
        true
    }

    /// 清理过期条目。
    pub fn prune(&mut self, now: Instant) {
        self.seen.retain(|_, seen_at| now.duration_since(*seen_at) < self.ttl);
    }

    /// 当前缓存量。
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_ids_are_rejected() {
        let mut window = DedupWindow::new(Duration::from_secs(60), 1024);
        let id = MsgId::now_v7();
        assert!(window.insert(id), "首次应放行");
        assert!(!window.insert(id), "重复应拦截");
        assert!(!window.insert(id), "继续重复应拦截");
    }

    #[test]
    fn capacity_evicts_oldest() {
        let mut window = DedupWindow::new(Duration::from_secs(60), 3);
        let first = MsgId::now_v7();
        assert!(window.insert(first));
        assert!(window.insert(MsgId::now_v7()));
        assert!(window.insert(MsgId::now_v7()));
        // 超容量 → 最旧的 first 被逐出,它再次出现会被当成新消息
        assert!(window.insert(MsgId::now_v7()));
        assert!(window.insert(first), "被逐出后再次出现视为新消息");
    }

    #[test]
    fn entries_expire_after_ttl() {
        let mut window = DedupWindow::new(Duration::from_millis(0), 1024);
        let id = MsgId::now_v7();
        assert!(window.insert(id));
        // ttl = 0 → 立即过期;同一 ID 再次出现视为新消息
        assert!(window.insert(id));
    }
}
