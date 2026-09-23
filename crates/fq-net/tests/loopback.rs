//! 回环集成测试:真实 UDP/TCP 套接字上的双节点全链路验收。
//!
//! 全部走 `127.0.0.1` + 固定端口段(251xx),不触碰真实网卡与防火墙;
//! 真实局域网广播冒烟留待 P6 双机验收。
//!
//! 发现路径:测试不依赖广播回环的平台行为,用 bootstrap 单播互指 ——
//! 这正是企业网禁用广播时的正式退路,被测的就是生产代码路径。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use fq_crypto::{Identity, StaticKeys};
use fq_net::{Node, NodeConfig, NodeEvent};
use fq_proto::{
    Capabilities, Envelope, Kind, NodeId, PresenceEvent, PresenceInfo, PresenceStatus, TextBody,
    TextFormat, codec,
};

/// 不可达的"广播"目标(避免测试触碰真实网卡;发送失败仅告警)。
const DEAD_BROADCAST: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1);

async fn node_named(name: &str, disc_port: u16, tcp_port: u16, bootstrap: Vec<SocketAddr>) -> Node {
    node_with_identity(
        name,
        Identity::generate().unwrap(),
        disc_port,
        tcp_port,
        bootstrap,
    )
    .await
}

async fn node_with_identity(
    name: &str,
    identity: Identity,
    disc_port: u16,
    tcp_port: u16,
    bootstrap: Vec<SocketAddr>,
) -> Node {
    let mut config = NodeConfig::new(identity, StaticKeys::generate().unwrap(), name);
    config.discovery_bind = SocketAddr::from(([127, 0, 0, 1], disc_port));
    config.listen_addr = SocketAddr::from(([127, 0, 0, 1], tcp_port));
    config.broadcast_addr = DEAD_BROADCAST;
    config.bootstrap = bootstrap;
    // 心跳调快,发现收敛在亚秒级
    config.heartbeat_every = Duration::from_millis(150);
    config.peer_timeout = Duration::from_secs(60);
    Node::start(config).await.expect("节点启动失败")
}

/// 等待本节点发现目标对端。
async fn wait_peer(node: &Node, target: NodeId) -> fq_net::PeerInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if let Some(peer) = node.peer(&target) {
            return peer;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("8s 内未发现对端 {target}");
}

/// 等待下一条匹配的事件。
async fn next_event(
    rx: &mut tokio::sync::broadcast::Receiver<NodeEvent>,
    pred: impl Fn(&NodeEvent) -> bool,
) -> NodeEvent {
    let deadline = Duration::from_secs(8);
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

/// 确认一段时间内再无新事件(用于断言"只投递一次")。
async fn assert_no_event(rx: &mut tokio::sync::broadcast::Receiver<NodeEvent>, ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    let mut leaked = Vec::new();
    while let Ok(event) = rx.try_recv() {
        leaked.push(event);
    }
    assert!(leaked.is_empty(), "预期无事件,实际收到 {leaked:?}");
}

fn text_envelope(from: NodeId, to: NodeId, body: &str) -> Envelope {
    Envelope::direct(
        from,
        to,
        Kind::Text(TextBody {
            body: body.to_string(),
            format: TextFormat::Plain,
            reply_to: None,
            mentions: vec![],
            group_id: None,
            group_name: None,
        }),
    )
}

fn forged_presence(
    identity: &Identity,
    static_keys: &StaticKeys,
    signature: &[u8; 64],
    display_name: &str,
) -> Envelope {
    Envelope::broadcast(
        identity.node_id(),
        Kind::Presence(PresenceInfo {
            event: PresenceEvent::Announce,
            display_name: display_name.to_string(),
            host_name: "attacker-host".to_string(),
            status: PresenceStatus::Online,
            group: None,
            port: 1,
            endpoints: vec![],
            public_key: identity.public_key().to_vec(),
            noise_static: static_keys.public().to_vec(),
            binding_signature: signature.to_vec(),
            capabilities: Capabilities::TEXT,
            avatar_sha256: None,
            app_version: None,
        }),
    )
}

#[tokio::test]
async fn two_nodes_discover_each_other_and_exchange_text() {
    let a = node_named(
        "Alice",
        25101,
        25102,
        vec![SocketAddr::from(([127, 0, 0, 1], 25111))],
    )
    .await;
    let b = node_named(
        "Bob",
        25111,
        25112,
        vec![SocketAddr::from(([127, 0, 0, 1], 25101))],
    )
    .await;

    // ── 双向发现 ──
    let peer_b = wait_peer(&a, b.node_id()).await;
    assert_eq!(peer_b.display_name, "Bob");
    assert!(
        !peer_b.endpoints.is_empty(),
        "可达端点必须由 UDP 源地址推导"
    );
    let peer_a = wait_peer(&b, a.node_id()).await;
    assert_eq!(peer_a.display_name, "Alice");
    assert_eq!(
        peer_a.noise_static,
        a.static_public(),
        "对端表必须记录通告里的静态公钥"
    );

    // ── A → B ──
    let mut b_events = b.events();
    a.send_text(b.node_id(), "你好,飞秋重构版!🚀")
        .await
        .unwrap();
    let event = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::MessageReceived { .. })
    })
    .await;
    let NodeEvent::MessageReceived { from, envelope } = event else {
        unreachable!()
    };
    assert_eq!(from, a.node_id());
    let Kind::Text(text) = envelope.kind else {
        panic!("应为文本消息");
    };
    assert_eq!(text.body, "你好,飞秋重构版!🚀");

    // ── B → A ──
    let mut a_events = a.events();
    b.send_text(a.node_id(), "收到!").await.unwrap();
    let event = next_event(&mut a_events, |e| {
        matches!(e, NodeEvent::MessageReceived { .. })
    })
    .await;
    let NodeEvent::MessageReceived { from, .. } = event else {
        unreachable!()
    };
    assert_eq!(from, b.node_id());

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn forged_presence_is_ignored() {
    let b = node_named("Bob", 25121, 25122, vec![]).await;
    let b_disc = SocketAddr::from(([127, 0, 0, 1], 25121));
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // ── 攻击 A1:冒用别人的 NodeId(声明的 from 与公钥派生不一致)──
    let impostor = Identity::generate().unwrap();
    let impostor_static = StaticKeys::generate().unwrap();
    let real_signature = fq_crypto::sign_static_key_binding(&impostor, &impostor_static.public());
    let mut a1 = forged_presence(&impostor, &impostor_static, &real_signature, "冒名顶替");
    // 签名是真实有效的,但把 from 换成受害者
    a1.from = NodeId::from_bytes([0x99; 16]);
    sock.send_to(&codec::encode_framed(&a1).unwrap(), b_disc)
        .await
        .unwrap();

    // ── 攻击 A2:NodeId 与公钥一致,但绑定签名签在另一把静态密钥上 ──
    let attacker2 = Identity::generate().unwrap();
    let attacker2_static = StaticKeys::generate().unwrap();
    let wrong_key_signature =
        fq_crypto::sign_static_key_binding(&attacker2, &StaticKeys::generate().unwrap().public());
    let a2 = forged_presence(&attacker2, &attacker2_static, &wrong_key_signature, "李四");
    sock.send_to(&codec::encode_framed(&a2).unwrap(), b_disc)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;
    let peers = b.peers();
    assert!(
        peers.is_empty(),
        "两种伪造通告都必须被拦截,实际进入了对端表: {peers:?}"
    );

    b.shutdown();
}

#[tokio::test]
async fn duplicate_message_is_delivered_exactly_once() {
    let a = node_named(
        "Alice",
        25131,
        25132,
        vec![SocketAddr::from(([127, 0, 0, 1], 25141))],
    )
    .await;
    let b = node_named(
        "Bob",
        25141,
        25142,
        vec![SocketAddr::from(([127, 0, 0, 1], 25131))],
    )
    .await;
    wait_peer(&a, b.node_id()).await;
    wait_peer(&b, a.node_id()).await;

    let mut b_events = b.events();
    // 同一个 MsgId 投递两次(模拟重传)
    let duplicate = text_envelope(a.node_id(), b.node_id(), "只投递一次");
    a.send(duplicate.clone()).await.unwrap();
    a.send(duplicate).await.unwrap();

    next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::MessageReceived { .. })
    })
    .await;
    assert_no_event(&mut b_events, 400).await;

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn reconnect_after_explicit_disconnect() {
    let a = node_named(
        "Alice",
        25151,
        25152,
        vec![SocketAddr::from(([127, 0, 0, 1], 25161))],
    )
    .await;
    let b = node_named(
        "Bob",
        25161,
        25162,
        vec![SocketAddr::from(([127, 0, 0, 1], 25151))],
    )
    .await;
    wait_peer(&a, b.node_id()).await;
    wait_peer(&b, a.node_id()).await;

    let mut b_events = b.events();
    a.send_text(b.node_id(), "第一次").await.unwrap();
    next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::MessageReceived { .. })
    })
    .await;

    // 强制断开 + 给对端一点清理时间
    a.disconnect(&b.node_id());
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 再发 → 自动重拨 → 照常送达
    a.send_text(b.node_id(), "重连后的第二条").await.unwrap();
    let event = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::MessageReceived { .. })
    })
    .await;
    let NodeEvent::MessageReceived { envelope, .. } = event else {
        unreachable!()
    };
    let Kind::Text(text) = envelope.kind else {
        panic!("应为文本消息");
    };
    assert_eq!(text.body, "重连后的第二条");

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn trust_warning_on_changed_static_key() {
    let identity = Identity::from_seed(&[0x77; 32]);

    // B 先上线;A(合法身份 + 第一把静态密钥)与 B 互相发现 → TOFU 固定
    let b = node_named("Bob", 25171, 25172, vec![]).await;
    let a = node_with_identity(
        "Alice",
        identity.clone(),
        25173,
        25174,
        vec![SocketAddr::from(([127, 0, 0, 1], 25171))],
    )
    .await;
    let peer_a = wait_peer(&b, a.node_id()).await;
    let pinned_static = peer_a.noise_static;

    // "Alice" 换了一台机器:同一身份种子,但新的静态握手密钥 → 中间人或重装
    let a2 = node_with_identity(
        "Alice",
        identity,
        25175,
        25176,
        vec![SocketAddr::from(([127, 0, 0, 1], 25171))],
    )
    .await;

    let mut b_events = b.events();
    let warning = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::TrustWarning { .. })
    })
    .await;
    let NodeEvent::TrustWarning {
        node_id,
        pinned,
        presented,
    } = warning
    else {
        unreachable!()
    };
    assert_eq!(node_id, a.node_id(), "告警必须指向被冒充的 NodeId");
    assert_ne!(pinned, presented, "新旧指纹必须不同");

    // 对端表不被污染:仍保留旧的静态公钥,新密钥未被接受
    let still = b.peer(&a.node_id()).expect("对端条目必须保留");
    assert_eq!(still.noise_static, pinned_static);
    assert_ne!(still.noise_static, a2.static_public());

    a.shutdown();
    a2.shutdown();
    b.shutdown();
}
