//! 接收确认流测试:飞秋式"对方要给你发文件 → 接收/拒绝"。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{next_event, node_pair, pseudo_random, temp_dir};
use fq_net::{Node, NodeConfig, NodeEvent};

#[tokio::test]
async fn offer_waits_for_user_acceptance_and_honors_directory_choice() {
    let default_dir = temp_dir("accept-default");
    let chosen_dir = temp_dir("accept-chosen");
    let (a, b) = node_pair("accept", (26201, 26202), (26211, 26212), default_dir.clone()).await;

    // B 关闭自动接受(桌面模式)
    b.shutdown();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut b_config = NodeConfig::new(
        fq_crypto::Identity::from_seed(&[0x99; 32]),
        fq_crypto::StaticKeys::generate().unwrap(),
        "Bob",
    );
    b_config.discovery_bind = std::net::SocketAddr::from(([127, 0, 0, 1], 26211));
    b_config.listen_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 26212));
    b_config.broadcast_addr = common::DEAD_BROADCAST;
    b_config.bootstrap = vec![std::net::SocketAddr::from(([127, 0, 0, 1], 26201))];
    b_config.heartbeat_every = Duration::from_millis(150);
    b_config.download_dir = default_dir.clone();
    b_config.auto_accept_files = false;
    let b = Node::start(b_config).await.unwrap();
    // 等 B 重新发现 A
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if b.peer(&a.node_id()).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let payload = pseudo_random(300 * 1024, 0x5A5A);
    let src_dir = temp_dir("accept-send");
    let src = src_dir.join("确认流.bin");
    std::fs::write(&src, &payload).unwrap();

    let mut b_events = b.events();
    let token = a.send_file(b.node_id(), &src, None).await.unwrap();

    // ① 收到要约
    let offer = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileOfferReceived { .. })
    })
    .await;
    let NodeEvent::FileOfferReceived { manifest, .. } = offer else { unreachable!() };
    assert_eq!(manifest.total_bytes, payload.len() as u64);

    // ② 决策前:1 秒内不得出现任何数据(没有偷偷开始)
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let mut leaked = false;
    while let Ok(event) = b_events.try_recv() {
        if matches!(
            event,
            NodeEvent::FileProgress { .. } | NodeEvent::FileEntryDone { .. }
        ) {
            leaked = true;
        }
    }
    assert!(!leaked, "用户确认前不得开始接收数据");

    // ③ 同意,并且这次保存到自选目录
    assert!(b.accept_file_offer(&token, Some(chosen_dir.clone())));
    next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileTransferCompleted { .. })
    })
    .await;

    let received = std::fs::read(chosen_dir.join("确认流.bin")).expect("应保存在自选目录");
    assert_eq!(received, payload);
    assert!(!default_dir.join("确认流.bin").exists(), "默认目录不应有文件");
    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn rejected_offer_stops_sender_with_failure_event() {
    let download = temp_dir("reject");
    let (a, b) = node_pair("reject", (26221, 26222), (26231, 26232), download.clone()).await;

    b.shutdown();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut b_config = NodeConfig::new(
        fq_crypto::Identity::from_seed(&[0x98; 32]),
        fq_crypto::StaticKeys::generate().unwrap(),
        "Bob",
    );
    b_config.discovery_bind = std::net::SocketAddr::from(([127, 0, 0, 1], 26231));
    b_config.listen_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 26232));
    b_config.broadcast_addr = common::DEAD_BROADCAST;
    b_config.bootstrap = vec![std::net::SocketAddr::from(([127, 0, 0, 1], 26221))];
    b_config.heartbeat_every = Duration::from_millis(150);
    b_config.download_dir = download.clone();
    b_config.auto_accept_files = false;
    let b = Node::start(b_config).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if b.peer(&a.node_id()).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let src_dir = temp_dir("reject-send");
    let src = src_dir.join("拒绝.bin");
    std::fs::write(&src, pseudo_random(64 * 1024, 0x1111)).unwrap();

    let mut a_events = a.events();
    let mut b_events = b.events();
    let token = a.send_file(b.node_id(), &src, None).await.unwrap();

    next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileOfferReceived { .. })
    })
    .await;
    assert!(b.reject_file_offer(&token));

    // 发送方收到"对方中止",传输失败
    let failed = next_event(&mut a_events, |e| {
        matches!(e, NodeEvent::FileTransferFailed { .. })
    })
    .await;
    let NodeEvent::FileTransferFailed { reason, .. } = failed else { unreachable!() };
    assert!(reason.contains("拒绝"), "失败原因应说明被拒绝: {reason}");
    assert!(!download.join("拒绝.bin").exists());

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn download_dir_change_applies_to_new_transfers() {
    let first_dir = temp_dir("dir-first");
    let second_dir = temp_dir("dir-second");
    let (a, b) = node_pair("dirchange", (26241, 26242), (26251, 26252), first_dir.clone()).await;

    let src_dir = temp_dir("dirchange-send");
    let src = src_dir.join("改目录.bin");
    std::fs::write(&src, b"new-home").unwrap();

    // 运行期改目录 → 新传输落到新目录
    b.set_download_dir(second_dir.clone());
    let mut b_events = b.events();
    let token = a.send_file(b.node_id(), &src, None).await.unwrap();
    let done = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileTransferCompleted { .. })
    })
    .await;
    let NodeEvent::FileTransferCompleted { token: t, .. } = done else { unreachable!() };
    assert_eq!(t, token);
    assert!(second_dir.join("改目录.bin").exists(), "应保存到修改后的目录");
    assert!(!first_dir.join("改目录.bin").exists());

    a.shutdown();
    b.shutdown();
}
