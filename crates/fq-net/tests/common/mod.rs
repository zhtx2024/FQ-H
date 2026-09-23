//! 集成测试公共助手(被 tests/ 下多个测试二进制复用)。
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use fq_crypto::{Identity, StaticKeys};
use fq_net::{Node, NodeConfig, NodeEvent, PeerInfo};
use fq_proto::NodeId;

/// 不可达"广播"目标(测试不触碰真实网卡)。
pub const DEAD_BROADCAST: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1);

pub struct NodeSpec {
    pub name: &'static str,
    pub discovery_port: u16,
    pub tcp_port: u16,
    pub bootstrap: Vec<SocketAddr>,
    pub download_dir: PathBuf,
}

pub async fn start_node(spec: &NodeSpec) -> Node {
    let mut config = NodeConfig::new(
        Identity::generate().expect("身份生成失败"),
        StaticKeys::generate().expect("静态密钥生成失败"),
        spec.name,
    );
    config.discovery_bind = SocketAddr::from(([127, 0, 0, 1], spec.discovery_port));
    config.listen_addr = SocketAddr::from(([127, 0, 0, 1], spec.tcp_port));
    config.broadcast_addr = DEAD_BROADCAST;
    config.bootstrap = spec.bootstrap.clone();
    config.heartbeat_every = Duration::from_millis(150);
    config.peer_timeout = Duration::from_secs(60);
    config.download_dir = spec.download_dir.clone();
    Node::start(config).await.expect("节点启动失败")
}

/// 启动互指的一对节点并等待双向发现。
pub async fn node_pair(
    tag: &str,
    a_ports: (u16, u16),
    b_ports: (u16, u16),
    b_download: PathBuf,
) -> (Node, Node) {
    let a = start_node(&NodeSpec {
        name: "Alice",
        discovery_port: a_ports.0,
        tcp_port: a_ports.1,
        bootstrap: vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))],
        download_dir: temp_dir(&format!("{tag}-a")),
    })
    .await;
    let b = start_node(&NodeSpec {
        name: "Bob",
        discovery_port: b_ports.0,
        tcp_port: b_ports.1,
        bootstrap: vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
        download_dir: b_download,
    })
    .await;

    wait_peer(&a, b.node_id()).await;
    wait_peer(&b, a.node_id()).await;
    (a, b)
}

pub async fn wait_peer(node: &Node, target: NodeId) -> PeerInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if let Some(peer) = node.peer(&target) {
            return peer;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("8s 内未发现对端 {target}");
}

/// 等待下一条匹配的事件(不匹配的跳过)。
pub async fn next_event(
    rx: &mut tokio::sync::broadcast::Receiver<NodeEvent>,
    pred: impl Fn(&NodeEvent) -> bool,
) -> NodeEvent {
    let deadline = Duration::from_secs(15);
    loop {
        let event = tokio::time::timeout(deadline, rx.recv())
            .await
            .expect("等待事件超时")
            .expect("事件流关闭");
        if pred(&event) {
            return event;
        }
    }
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("fq-net-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("临时目录创建失败");
    dir
}

pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}
