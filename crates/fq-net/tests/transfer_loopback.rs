//! 文件传输端到端验收:真实套接字 + 真实磁盘 IO。
//!
//! 覆盖:单文件多分块 + 哈希校验、目录树还原、断点续传、已有文件不覆盖。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{next_event, node_pair, pseudo_random, sha256_hex, temp_dir};
use fq_net::{NodeEvent, TransferDirection};

fn is_receiving_completed(token: &str) -> impl Fn(&NodeEvent) -> bool + '_ {
    move |event| {
        matches!(
            event,
            NodeEvent::FileTransferCompleted { direction: TransferDirection::Receiving, token: t, .. }
                if t == token
        )
    }
}

#[tokio::test]
async fn multi_chunk_file_transfer_with_hash_verification() {
    let download = temp_dir("file-recv");
    let (a, b) = node_pair("file", (25201, 25202), (25211, 25212), download.clone()).await;

    // 1.5 MiB 随机数据,跨越多个 256 KiB 分块
    let payload = pseudo_random(1_572_864, 0xABCD_1234);
    let src_dir = temp_dir("file-send");
    let src = src_dir.join("随机数据.bin");
    std::fs::write(&src, &payload).expect("写入源文件失败");

    let mut b_events = b.events();
    let token = a
        .send_file(b.node_id(), &src, Some("发你一个文件".to_string()))
        .await
        .expect("发起发送失败");

    // 要约可见,附带留言
    let offer = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileOfferReceived { .. })
    })
    .await;
    let NodeEvent::FileOfferReceived { manifest, message, .. } = offer else { unreachable!() };
    assert_eq!(message.as_deref(), Some("发你一个文件"));
    assert_eq!(manifest.total_bytes, payload.len() as u64);
    assert_eq!(manifest.entries.len(), 1);

    // 进度事件(多分块必有)
    let _progress = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileProgress { .. })
    })
    .await;

    // 条目完成且哈希校验通过
    let entry = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileEntryDone { .. })
    })
    .await;
    let NodeEvent::FileEntryDone { sha256, verified, .. } = entry else { unreachable!() };
    assert!(verified, "接收方 SHA-256 校验必须通过");
    assert_eq!(sha256, sha256_hex(&payload));

    // 整体完成
    next_event(&mut b_events, is_receiving_completed(&token)).await;

    // 落盘内容逐字节一致
    let received = std::fs::read(download.join("随机数据.bin")).expect("目标文件应存在");
    assert_eq!(received, payload);

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn directory_transfer_preserves_tree_including_empty_dirs() {
    let download = temp_dir("dir-recv");
    let (a, b) = node_pair("dir", (25231, 25232), (25241, 25242), download.clone()).await;

    let src_root = temp_dir("dir-send");
    std::fs::create_dir_all(src_root.join("设计稿/子目录")).unwrap();
    std::fs::create_dir_all(src_root.join("设计稿/空目录")).unwrap();
    let file_a = pseudo_random(1000, 0x1111);
    let file_b = pseudo_random(300 * 1024, 0x2222); // 跨分块
    std::fs::write(src_root.join("设计稿/a.txt"), &file_a).unwrap();
    std::fs::write(src_root.join("设计稿/子目录/b.bin"), &file_b).unwrap();

    let mut b_events = b.events();
    let token = a
        .send_file(b.node_id(), &src_root.join("设计稿"), None)
        .await
        .expect("发起发送失败");
    next_event(&mut b_events, is_receiving_completed(&token)).await;

    let root = download.join("设计稿");
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), file_a);
    assert_eq!(std::fs::read(root.join("子目录/b.bin")).unwrap(), file_b);
    // 空目录也被保留
    assert!(root.join("空目录").is_dir());

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn update_package_transfer_auto_accepts_and_reports_ready() {
    let download = temp_dir("update-recv");
    let (a, b) = node_pair("update", (25431, 25432), (25441, 25442), download.clone()).await;

    // 模拟安装包(小文件;校验流程与真实安装包一致)
    let payload = pseudo_random(400_000, 0x5151_2626);
    let pkg_dir = temp_dir("update-pkg");
    let pkg = pkg_dir.join("feiqiu-r_setup.exe");
    std::fs::write(&pkg, &payload).expect("写入安装包失败");

    let mut b_events = b.events();
    let token = a
        .send_update_package(b.node_id(), &pkg, "9.9.9".to_string())
        .await
        .expect("发起更新包发送失败");

    // 接收方拿到的是**更新要约**(不是普通文件要约 → 无需人工点"接收")
    let offer = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::UpdateOfferReceived { .. })
    })
    .await;
    let NodeEvent::UpdateOfferReceived { version, manifest, .. } = offer else { unreachable!() };
    assert_eq!(version, "9.9.9");
    assert_eq!(manifest.total_bytes, payload.len() as u64);

    // 传输完成 → 校验通过并发出"更新就绪"
    let ready = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::UpdatePackageReady { .. })
    })
    .await;
    let NodeEvent::UpdatePackageReady { version, path, token: ready_token, .. } = ready else {
        unreachable!()
    };
    assert_eq!(ready_token, token);
    assert_eq!(version, "9.9.9");
    let received = std::fs::read(&path).expect("更新包应落盘");
    assert_eq!(received, payload, "更新包内容必须逐字节一致");

    // 完成后会话注销
    assert!(!a.cancel_transfer(&token), "已结束的会话不应再可取消");

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn update_package_ready_path_reflects_renamed_target() {
    // 下载目录里已存在同名安装包 → 接收方换名保存,就绪事件必须报**实际**路径
    let download = temp_dir("update-dup-recv");
    std::fs::create_dir_all(&download).unwrap();
    let stale = download.join("feiqiu-r_setup.exe");
    std::fs::write(&stale, b"stale-package-from-an-older-version").unwrap();

    let (a, b) = node_pair("update-dup", (25451, 25452), (25461, 25462), download.clone()).await;
    let payload = pseudo_random(200_000, 0x0BAD_F00D);
    let pkg_dir = temp_dir("update-dup-pkg");
    let pkg = pkg_dir.join("feiqiu-r_setup.exe");
    std::fs::write(&pkg, &payload).unwrap();

    let mut b_events = b.events();
    a.send_update_package(b.node_id(), &pkg, "9.9.10".to_string())
        .await
        .unwrap();

    let ready = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::UpdatePackageReady { .. })
    })
    .await;
    let NodeEvent::UpdatePackageReady { version, path, .. } = ready else { unreachable!() };
    assert_eq!(version, "9.9.10");

    let actual = std::path::PathBuf::from(&path);
    assert_ne!(actual, stale, "同名冲突时不应复用旧包");
    assert_eq!(
        std::fs::read(&actual).expect("就绪事件给出的路径必须可读"),
        payload,
        "就绪事件的路径必须指向新下载的包"
    );
    // 旧包保持原样,不被覆盖
    assert_eq!(
        std::fs::read(&stale).unwrap(),
        b"stale-package-from-an-older-version"
    );

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn transfer_resumes_from_existing_part_file() {
    let download = temp_dir("resume-recv");
    // 预先放置 .part:正好是源文件的前缀(256 KiB + 1000 字节)
    let payload = pseudo_random(1024 * 1024, 0x7777);
    let prefix_len = 256 * 1024 + 1000;
    std::fs::write(download.join("断点.bin.part"), &payload[..prefix_len]).expect("预置 .part 失败");

    let (a, b) = node_pair("resume", (25251, 25252), (25261, 25262), download.clone()).await;

    let src_dir = temp_dir("resume-send");
    let src = src_dir.join("断点.bin");
    std::fs::write(&src, &payload).unwrap();

    let mut b_events = b.events();
    let token = a.send_file(b.node_id(), &src, None).await.expect("发起发送失败");

    // 第一个进度事件应从续传偏移起步(而不是从 0)
    let first_progress = next_event(&mut b_events, |e| {
        matches!(e, NodeEvent::FileProgress { .. })
    })
    .await;
    let NodeEvent::FileProgress { transferred, .. } = first_progress else { unreachable!() };
    assert!(
        transferred >= prefix_len as u64,
        "续传必须从已有 {prefix_len} 字节起步,实际从 {transferred} 开始"
    );

    next_event(&mut b_events, is_receiving_completed(&token)).await;
    let received = std::fs::read(download.join("断点.bin")).expect("目标文件应存在");
    assert_eq!(received.len(), payload.len());
    assert_eq!(received, payload, "续传结果必须与源文件逐字节一致");

    // .part 已被改名为正式文件
    assert!(!download.join("断点.bin.part").exists());

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn existing_file_is_never_overwritten() {
    let download = temp_dir("collide-recv");
    // 已存在的同名文件:绝不能被覆盖
    std::fs::write(download.join("重要数据.txt"), "原有内容,不可破坏".as_bytes()).unwrap();

    let (a, b) = node_pair("collide", (25271, 25272), (25281, 25282), download.clone()).await;

    let src_dir = temp_dir("collide-send");
    let src = src_dir.join("重要数据.txt");
    std::fs::write(&src, "新内容".as_bytes()).unwrap();

    let mut b_events = b.events();
    let token = a.send_file(b.node_id(), &src, None).await.expect("发起发送失败");
    next_event(&mut b_events, is_receiving_completed(&token)).await;

    // 原文件原封不动,新文件换名保存
    assert_eq!(
        std::fs::read(download.join("重要数据.txt")).unwrap(),
        "原有内容,不可破坏".as_bytes()
    );
    assert_eq!(
        std::fs::read(download.join("重要数据 (1).txt")).unwrap(),
        "新内容".as_bytes()
    );

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn oversize_stale_part_is_restarted_from_zero() {
    let download = temp_dir("stale-recv");
    // .part 比声明的文件大 → 陈旧数据,必须从 0 重写
    let payload = pseudo_random(100_000, 0x4242);
    std::fs::write(download.join("陈旧.bin.part"), vec![0xFFu8; 500_000]).unwrap();

    let (a, b) = node_pair("stale", (25291, 25292), (25301, 25302), download.clone()).await;
    let src_dir = temp_dir("stale-send");
    let src = src_dir.join("陈旧.bin");
    std::fs::write(&src, &payload).unwrap();

    let mut b_events = b.events();
    let token = a.send_file(b.node_id(), &src, None).await.expect("发起发送失败");
    next_event(&mut b_events, is_receiving_completed(&token)).await;

    assert_eq!(std::fs::read(download.join("陈旧.bin")).unwrap(), payload);
    assert!(!download.join("陈旧.bin.part").exists());

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn sending_nonexistent_path_fails_cleanly() {
    let download = temp_dir("missing-recv");
    let (a, b) = node_pair("missing", (25311, 25312), (25321, 25322), download).await;

    let result = a.send_file(b.node_id(), "不存在的路径.bin", None).await;
    assert!(result.is_err(), "发送不存在的路径必须返回错误");

    a.shutdown();
    b.shutdown();
}
