//! fq-core 集成验收:历史落库、送达/已读回执、离线补发、跨重启稳定性。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use fq_core::{App, AppConfig, AppEvent, SendOutcome};
use fq_net::NodeEvent;

/// 不可达"广播"目标(测试不触碰真实网卡)。
const DEAD_BROADCAST: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1);

fn app_config(
    dir: PathBuf,
    name: &str,
    ports: (u16, u16),
    bootstrap: Vec<SocketAddr>,
) -> AppConfig {
    let mut config = AppConfig::new(dir, name);
    config.bind = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    config.discovery_port = ports.0;
    config.listen_port = ports.1;
    config.broadcast = Some(DEAD_BROADCAST);
    config.bootstrap = bootstrap;
    config.heartbeat_every = Duration::from_millis(150);
    config.peer_timeout = Duration::from_secs(2);
    config
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("fq-core-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_pair(tag: &str, a_ports: (u16, u16), b_ports: (u16, u16)) -> (App, App) {
    let a = App::start(app_config(
        temp_dir(&format!("{tag}-a")),
        "Alice",
        a_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))],
    ))
    .await
    .unwrap();
    let b = App::start(app_config(
        temp_dir(&format!("{tag}-b")),
        "Bob",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    ))
    .await
    .unwrap();
    wait_discovery(&a, &b).await;
    (a, b)
}

async fn wait_discovery(a: &App, b: &App) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        let a_knows_b = a.node().peer(&b.node_id()).is_some();
        let b_knows_a = b.node().peer(&a.node_id()).is_some();
        if a_knows_b && b_knows_a {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "discovery timeout: a_peers={:?} b_peers={:?}",
        a.node()
            .peers()
            .iter()
            .map(|p| p.node_id.to_hex())
            .collect::<Vec<_>>(),
        b.node()
            .peers()
            .iter()
            .map(|p| p.node_id.to_hex())
            .collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn group_text_fans_out_and_receiver_auto_creates_group() {
    let (a, b) = start_pair("group", (26301, 26302), (26311, 26312)).await;
    let b_id = b.node_id();

    // A 建群,拉 B 进来
    let group_id = a.create_group("项目组", &[b_id]).await.unwrap();
    assert!(group_id.starts_with("group:"), "群 ID 前缀应为 group:");

    // A 发群消息
    let (sent, queued) = a
        .send_group_text(&group_id, "大家好,这是群消息")
        .await
        .unwrap();
    assert_eq!(sent, 1, "B 在线应直发");
    assert_eq!(queued, 0);

    // A 侧群历史(peer = group_id)
    let a_history = a.history_by_key(&group_id, 10).await.unwrap();
    assert_eq!(a_history.len(), 1);
    assert_eq!(a_history[0].body.as_deref(), Some("大家好,这是群消息"));

    // B 侧:自动建群(名字随消息带来)+ 收到消息
    let group_name = eventually(
        || async {
            b.list_groups()
                .await
                .unwrap()
                .into_iter()
                .find(|g| g.id == group_id)
                .map(|g| g.name)
        },
        "B 自动建群",
    )
    .await;
    assert_eq!(group_name, "项目组", "群名应随消息带到对端");

    let b_history = b.history_by_key(&group_id, 10).await.unwrap();
    assert_eq!(b_history.len(), 1, "B 侧群历史应有 1 条");
    assert!(!b_history[0].is_outgoing);

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn group_message_queued_for_offline_member_and_flushed() {
    let a_dir = temp_dir("group-offline-a");
    let b_dir = temp_dir("group-offline-b");
    let a_ports = (26321, 26322);
    let b_ports = (26331, 26332);

    let a = App::start(app_config(
        a_dir,
        "Alice",
        a_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))],
    ))
    .await
    .unwrap();
    let b = App::start(app_config(
        b_dir.clone(),
        "Bob",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    ))
    .await
    .unwrap();
    wait_discovery(&a, &b).await;
    let b_id = b.node_id();
    let group_id = a.create_group("离线群", &[b_id]).await.unwrap();
    b.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // B 离线:群消息应入队
    let (sent, queued) = a
        .send_group_text(&group_id, "你不在时的群消息")
        .await
        .unwrap();
    assert_eq!(sent, 0);
    assert_eq!(queued, 1, "离线成员应入队");

    // B 回归 → 自动补发 → 自动建群
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b2 = App::start(app_config(
        b_dir,
        "Bob",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    ))
    .await
    .unwrap();
    assert_eq!(b2.node_id(), b_id);

    eventually(
        || async {
            b2.history_by_key(&group_id, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.body.as_deref() == Some("你不在时的群消息"))
        },
        "离线群消息补发",
    )
    .await;
    eventually(
        || async {
            b2.list_groups()
                .await
                .unwrap()
                .into_iter()
                .find(|g| g.id == group_id)
        },
        "B 补发后自动建群",
    )
    .await;

    a.shutdown();
    b2.shutdown();
}

#[tokio::test]
async fn group_file_fans_out_to_each_member() {
    let (a, b) = start_pair("gfile", (26341, 26342), (26351, 26352)).await;
    // 测试节点默认自动接受文件,落在各自数据目录的 downloads/ 下
    let download_b = b.data_dir().join("downloads");

    // A 建群(成员 B),发一个文件
    let group_id = a.create_group("文件群", &[b.node_id()]).await.unwrap();
    let src_dir = temp_dir("gfile-send");
    let src = src_dir.join("群文件.bin");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i * 17 + 3) as u8).collect();
    std::fs::write(&src, &payload).unwrap();

    let tokens = a.send_group_file(&group_id, &src, None).await.unwrap();
    assert_eq!(tokens.len(), 1, "一个可达成员应产生一个传输");

    // 等 B 侧接收完成(自动接受)
    let mut b_events = b.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("15s 内未完成群文件传输");
        }
        match tokio::time::timeout(Duration::from_millis(500), b_events.recv()).await {
            Ok(Ok(fq_core::AppEvent::Node(inner))) => {
                if let fq_net::NodeEvent::FileTransferCompleted {
                    direction: fq_net::TransferDirection::Receiving,
                    ..
                } = *inner
                {
                    break;
                }
            }
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(_)) => panic!("事件流关闭"),
        }
    }

    let received = std::fs::read(download_b.join("群文件.bin")).expect("文件应落盘");
    assert_eq!(received, payload, "群文件内容逐字节一致");

    // A 侧群历史有 1 条图片/文件消息
    let history = a.history_by_key(&group_id, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].kind, "file");

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn cancel_sender_transfer_midway_reports_cancelled() {
    let download_b = temp_dir("cancel-recv");
    let (a, b) = start_pair("cancel", (26361, 26362), (26371, 26372)).await;
    let _ = download_b;

    // 大文件(16 MiB),保证取消时还在传输中
    let src_dir = temp_dir("cancel-send");
    let src = src_dir.join("大文件.bin");
    std::fs::write(&src, vec![0x5Au8; 16 * 1024 * 1024]).unwrap();

    let mut a_events = a.events();
    let token = a.send_file(b.node_id(), &src, None).await.unwrap();

    // 等第一批进度事件后再取消(说明确实传起来了)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("10s 内未开始传输");
        }
        match tokio::time::timeout(Duration::from_millis(500), a_events.recv()).await {
            Ok(Ok(fq_core::AppEvent::Node(inner))) => {
                if let fq_net::NodeEvent::FileProgress { .. } = *inner {
                    break;
                }
            }
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(_)) => panic!("事件流关闭"),
        }
    }

    // 取消
    assert!(a.cancel_transfer(&token), "取消应命中会话");

    // 发送方收到"已取消"失败事件
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("10s 内未收到取消事件");
        }
        match tokio::time::timeout(Duration::from_millis(500), a_events.recv()).await {
            Ok(Ok(fq_core::AppEvent::Node(inner))) => {
                if let fq_net::NodeEvent::FileTransferFailed { reason, .. } = *inner {
                    assert!(reason.contains("取消"), "原因应说明取消: {reason}");
                    break;
                }
            }
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(_)) => panic!("事件流关闭"),
        }
    }

    // 传输历史应记录为 cancelled
    let history = a.transfer_history(20).await.unwrap();
    let record = history
        .iter()
        .find(|t| t.token == token)
        .expect("应有该传输的记录");
    assert_eq!(record.status, "cancelled", "历史状态应为 cancelled");

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn transfer_history_records_done_and_clear_works() {
    let download_b = temp_dir("thist-recv");
    let (a, b) = start_pair("thist", (26381, 26382), (26391, 26392)).await;

    let src_dir = temp_dir("thist-send");
    let src = src_dir.join("历史.bin");
    std::fs::write(&src, vec![7u8; 50_000]).unwrap();

    let mut a_events = a.events();
    let token = a.send_file(b.node_id(), &src, None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("15s 内未完成传输");
        }
        match tokio::time::timeout(Duration::from_millis(500), a_events.recv()).await {
            Ok(Ok(fq_core::AppEvent::Node(inner))) => {
                if let fq_net::NodeEvent::FileTransferCompleted { token: t, .. } = *inner {
                    if t == token {
                        break;
                    }
                }
            }
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(_)) => panic!("事件流关闭"),
        }
    }

    // 发送方历史:done(事件泵异步写库,轮询等待)
    let record = eventually(
        || async {
            a.transfer_history(20)
                .await
                .unwrap()
                .into_iter()
                .find(|t| t.token == token && t.status == "done")
        },
        "发送方历史写入 done",
    )
    .await;
    assert_eq!(record.direction, "send");
    assert!(record.size >= 50_000);
    assert!(record.finished_ms.is_some());

    // 接收方历史:recv + done
    let b_record = eventually(
        || async {
            b.transfer_history(20)
                .await
                .unwrap()
                .into_iter()
                .find(|t| t.direction == "recv" && t.status == "done")
        },
        "接收方历史写入 done",
    )
    .await;
    assert_eq!(b_record.direction, "recv");

    // 清空(只清已结束的)
    let cleared = a.clear_transfer_history().await.unwrap();
    assert!(cleared >= 1);
    assert!(a.transfer_history(20).await.unwrap().is_empty());

    a.shutdown();
    b.shutdown();
    let _ = download_b;
}

/// 版本号比较:数字分段,段数不足补 0。
#[test]
fn version_compare_handles_missing_segments() {
    use fq_core::compare_versions;
    assert_eq!(compare_versions("0.4.0", "0.3.9"), 1);
    assert_eq!(compare_versions("0.3.0", "0.3.0"), 0);
    assert_eq!(compare_versions("0.3", "0.3.0"), 0);
    assert_eq!(compare_versions("1.0.0", "0.99.99"), 1);
    assert_eq!(compare_versions("0.2.0", "0.10.0"), -1);
}

/// 老版本节点向新版本节点索取更新包:对端以 UpdateOffer 回发(自动接收)。
#[tokio::test]
async fn update_request_pulls_package_from_newer_peer() {
    let a_ports = (26401, 26402);
    let b_ports = (26411, 26412);
    let mut a_cfg = app_config(
        temp_dir("upd-a"),
        "OldNode",
        a_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))],
    );
    a_cfg.app_version = Some("0.0.1".into());
    let mut b_cfg = app_config(
        temp_dir("upd-b"),
        "NewNode",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    );
    b_cfg.app_version = Some("9.9.9".into());

    let a = App::start(a_cfg).await.unwrap();
    let b = App::start(b_cfg).await.unwrap();
    wait_discovery(&a, &b).await;

    // A 侧版本发现也应看到更高版本
    let (local, peers) = a.version_report("0.0.1");
    assert_eq!(local, "0.0.1");
    assert!(
        peers.iter().any(|(_, _, v)| v == "9.9.9"),
        "应看到对端版本 9.9.9,实际 {peers:?}"
    );

    let mut a_events = a.events();
    a.request_update(b.node_id()).await.expect("索取更新包失败");

    // B 版本更高 → 回发本机安装包,A 侧自动接收(UpdateOffer 随之到达)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut got: Option<(String, String)> = None;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), a_events.recv()).await {
            Ok(Ok(AppEvent::Node(inner))) => {
                if let NodeEvent::UpdateOfferReceived { version, token, .. } = *inner {
                    got = Some((version, token));
                    break;
                }
            }
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(_)) => break,
        }
    }
    let (version, token) = got.expect("20s 内未收到对端的更新包要约");
    assert_eq!(version, "9.9.9", "更新包版本应为对端版本");

    // 测试二进制体积巨大,拿到要约即取消,避免白搬字节(取消能力上一轮已验证)
    let _ = a.cancel_transfer(&token);

    a.shutdown();
    b.shutdown();
}

/// 本机版本不比请求方高时不提供更新包(避免降级/骚扰)。
#[tokio::test]
async fn update_request_ignored_when_not_newer() {
    let a_ports = (26421, 26422);
    let b_ports = (26431, 26432);
    let mut a_cfg = app_config(
        temp_dir("upd2-a"),
        "Same1",
        a_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))],
    );
    a_cfg.app_version = Some("1.2.3".into());
    let mut b_cfg = app_config(
        temp_dir("upd2-b"),
        "Same2",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    );
    b_cfg.app_version = Some("1.2.3".into());

    let a = App::start(a_cfg).await.unwrap();
    let b = App::start(b_cfg).await.unwrap();
    wait_discovery(&a, &b).await;

    let mut a_events = a.events();
    a.request_update(b.node_id()).await.unwrap();

    // 2 秒内不应出现更新包要约
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), a_events.recv()).await {
            Ok(Ok(AppEvent::Node(inner))) => {
                if let NodeEvent::UpdateOfferReceived { .. } = *inner {
                    panic!("版本相同不应提供更新包");
                }
            }
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(_)) => break,
        }
    }

    a.shutdown();
    b.shutdown();
}

/// 更新脚本:关键步骤必须齐备(等锁 → 备份 → 覆盖重试 → 重启 → 自删)。
#[test]
fn update_script_contains_critical_steps() {
    let script = fq_core::build_update_script(
        std::path::Path::new("C:/dl/fq-desktop.exe"),
        std::path::Path::new("C:/apps/fq-desktop.exe"),
    );
    assert!(script.starts_with("@echo off"), "脚本应为批处理");
    assert!(script.contains("ping -n 3 127.0.0.1"), "应等待旧进程退出");
    assert!(
        script.contains("copy /Y \"C:/apps/fq-desktop.exe\" \"C:/apps/fq-desktop.exe.bak\""),
        "应备份旧版本: {script}"
    );
    assert!(
        script.contains(":retry") && script.contains("goto retry"),
        "覆盖失败应重试"
    );
    assert!(
        script.contains("copy /Y \"C:/dl/fq-desktop.exe\" \"C:/apps/fq-desktop.exe\""),
        "应覆盖自身 exe"
    );
    assert!(
        script.contains("start \"\" \"C:/apps/fq-desktop.exe\""),
        "应重启应用"
    );
    assert!(script.contains("del \"%~f0\""), "应自删脚本");
}

/// 安装入口的安全护栏:越界/非 exe/异名包一律拒绝(不会触发覆盖)。
#[tokio::test]
async fn install_update_rejects_unsafe_packages() {
    let app = App::start(app_config(
        temp_dir("install-guard"),
        "Guard",
        (26441, 26442),
        vec![],
    ))
    .await
    .unwrap();
    let download = app.download_dir();
    std::fs::create_dir_all(&download).unwrap();

    // ① 下载目录之外
    let outside_dir = temp_dir("install-outside");
    let outside = outside_dir.join("fq-desktop.exe");
    std::fs::write(&outside, b"MZ fake").unwrap();
    let err = app
        .install_update_and_restart(&outside.display().to_string())
        .expect_err("下载目录之外必须拒绝");
    assert!(err.to_string().contains("下载目录之外"), "实际: {err}");

    // ② 非 exe 扩展名
    let txt = download.join("update.txt");
    std::fs::write(&txt, b"MZ fake").unwrap();
    let err = app
        .install_update_and_restart(&txt.display().to_string())
        .expect_err("非 exe 必须拒绝");
    assert!(err.to_string().contains("必须是 .exe"), "实际: {err}");

    // ③ 与本机程序不同名(例如局域网里跑着构建产物 fq-cli.exe)
    let other = download.join("fq-cli.exe");
    std::fs::write(&other, b"MZ fake").unwrap();
    let err = app
        .install_update_and_restart(&other.display().to_string())
        .expect_err("异名包必须拒绝");
    assert!(err.to_string().contains("不一致"), "实际: {err}");

    // ④ 与本机同名但内容不是 PE(校验魔数,防止把坏包覆盖到自身)
    let local_name = std::env::current_exe()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let fake = download.join(&local_name);
    std::fs::write(&fake, b"not-a-pe-file").unwrap();
    let err = app
        .install_update_and_restart(&fake.display().to_string())
        .expect_err("非 PE 内容必须拒绝");
    assert!(
        err.to_string().contains("不是有效的可执行文件"),
        "实际: {err}"
    );

    // 不存在的路径
    let missing = download.join("nope.exe");
    assert!(
        app.install_update_and_restart(&missing.display().to_string())
            .is_err()
    );

    app.shutdown();
}

/// 头像:通告哈希 → 对端自动拉取 → 校验落库 → 换头像自动更新 → 移除。
#[tokio::test]
async fn avatar_is_announced_fetched_cached_and_refreshed() {
    let a_ports = (26451, 26452);
    let b_ports = (26461, 26462);
    let a_dir = temp_dir("avatar-a");
    let b_dir = temp_dir("avatar-b");
    // 头像内容任意字节即可(校验只看哈希一致性,不解码图片)
    let a_png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
    let b_png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 9, 9, 9, 9, 9];
    std::fs::write(a_dir.join("avatar.png"), &a_png).unwrap();
    std::fs::write(b_dir.join("avatar.png"), &b_png).unwrap();

    let a = App::start(app_config(
        a_dir,
        "AvatarA",
        a_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))],
    ))
    .await
    .unwrap();
    let b = App::start(app_config(
        b_dir,
        "AvatarB",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    ))
    .await
    .unwrap();
    wait_discovery(&a, &b).await;

    // 调试:确认通告里确实带了头像哈希与 AVATAR 能力
    if let Some(live) = a.node().peer(&b.node_id()) {
        assert!(
            live.avatar_sha256.is_some(),
            "通告必须携带头像哈希(对方据此拉取)"
        );
        assert!(
            live.capabilities.contains(fq_proto::Capabilities::AVATAR),
            "节点必须声明 AVATAR 能力"
        );
    }

    let a_hex = a.node_id().to_hex();
    let b_hex = b.node_id().to_hex();

    // ① A 发现 B 的头像哈希与本地缓存不同 → 自动索取(首次可能因同时拨号丢失,靠重试兜底)
    let fetched = eventually_within(
        || async { a.peer_avatar(&b_hex).await.unwrap() },
        "A 自动拉取到 B 的头像",
        Duration::from_secs(25),
    )
    .await;
    assert_eq!(fetched.2, b_png, "头像内容必须逐字节一致");
    assert_eq!(fetched.1, "image/png");

    // ② 反向同理
    let fetched_b = eventually_within(
        || async { b.peer_avatar(&a_hex).await.unwrap() },
        "B 自动拉取到 A 的头像",
        Duration::from_secs(25),
    )
    .await;
    assert_eq!(fetched_b.2, a_png);

    // ③ 联系人摘要携带头像哈希(离线联系人也能据此显示头像)
    let summary = a
        .peers()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.node_id == b.node_id())
        .unwrap();
    assert!(summary.avatar_sha256.is_some(), "摘要应带头像哈希");

    // ④ A 换头像 → 重新通告 → B 自动刷新缓存
    let new_png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 7, 7, 7, 7];
    std::fs::write(a.avatar_path(), &new_png).unwrap();
    let new_hash = a.reload_avatar().await;
    assert_eq!(a.local_avatar().as_deref(), Some(new_png.as_slice()));
    assert_eq!(
        new_hash.as_deref().map(str::len),
        Some(64),
        "哈希应为 64 位 hex"
    );
    let updated = eventually_within(
        || async {
            b.peer_avatar(&a_hex)
                .await
                .unwrap()
                .filter(|(_, _, data)| data == &new_png)
        },
        "B 刷新到 A 的新头像",
        Duration::from_secs(25),
    )
    .await;
    assert_eq!(updated.2, new_png);

    // ⑤ A 移除头像 → 本地为空,且重新通告不再带头像哈希
    a.clear_avatar().await.unwrap();
    assert!(a.local_avatar().is_none());
    assert!(a.reload_avatar().await.is_none());

    a.shutdown();
    b.shutdown();
}

/// 联系人删除语义(飞秋):删除只是移出列表 → 刷新/再发现自动回来,聊天记录保留。
#[tokio::test]
async fn removed_contact_returns_on_refresh_like_feiqiu() {
    let (a, b) = start_pair("peerremove", (26471, 26472), (26481, 26482)).await;
    let b_id = b.node_id();

    // 先聊一句,证明"删联系人不动历史"
    a.send_text(b_id, "删我之前聊过").await.unwrap();

    assert!(
        a.peers().await.unwrap().iter().any(|p| p.node_id == b_id),
        "发现后联系人应入库"
    );

    // 删除:立即从列表消失
    assert!(a.remove_peer(b_id).await.unwrap(), "应有联系人行被删除");
    assert!(
        !a.peers().await.unwrap().iter().any(|p| p.node_id == b_id),
        "删除后不应出现在联系人列表"
    );
    assert_eq!(
        a.history(b_id, 10).await.unwrap().len(),
        1,
        "聊天记录必须保留"
    );

    // ① 对方的下一次通告会把它带回来(发现驱动)
    let back = eventually(
        || async {
            a.peers()
                .await
                .unwrap()
                .into_iter()
                .find(|p| p.node_id == b_id)
        },
        "对方再通告后联系人自动回来",
    )
    .await;
    assert_eq!(back.display_name, "Bob");

    // ② 再删一次,这次用"刷新联系人"显式补回(桌面端刷新按钮的真实流程:
    //    广播 → 等对端回发 → 写回列表)
    assert!(a.remove_peer(b_id).await.unwrap());
    assert!(!a.peers().await.unwrap().iter().any(|p| p.node_id == b_id));
    a.node().announce_now().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let restored = a.refresh_contacts().await.unwrap();
    assert!(restored >= 1, "刷新应把已知对端写回联系人表");
    assert!(
        a.peers().await.unwrap().iter().any(|p| p.node_id == b_id),
        "刷新后联系人应回到列表"
    );

    a.shutdown();
    b.shutdown();
}

/// 轮询直到异步谓词返回 Some(默认 8s 超时)。
async fn eventually<T, F>(probe: impl FnMut() -> F, what: &str) -> T
where
    F: Future<Output = Option<T>>,
{
    eventually_within(probe, what, Duration::from_secs(8)).await
}

/// 同 [`eventually`],但可指定超时(网络类断言在全量并行跑测试时需要更宽裕)。
async fn eventually_within<T, F>(mut probe: impl FnMut() -> F, what: &str, timeout: Duration) -> T
where
    F: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{timeout:?} 内未满足: {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn messages_persisted_and_receipts_flow() {
    let (a, b) = start_pair("receipts", (25601, 25602), (25611, 25612)).await;

    // A → B
    let outcome = a
        .send_text(b.node_id(), "你好,这里是回执测试 🚀")
        .await
        .unwrap();
    assert!(matches!(outcome, SendOutcome::Sent(_)), "在线时必须直发");

    // B 侧入库
    let b_copy_msg = eventually(
        || async {
            b.history(a.node_id(), 10)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.body.as_deref() == Some("你好,这里是回执测试 🚀"))
        },
        "B 收到并入库",
    )
    .await;
    assert!(!b_copy_msg.is_outgoing);

    // A 侧收到自动"送达"回执
    eventually(
        || async {
            a.history(b.node_id(), 10)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.delivered_ms.is_some())
        },
        "A 收到送达回执",
    )
    .await;

    // B 标记已读 → A 收到已读回执
    b.mark_read(a.node_id()).await.unwrap();
    eventually(
        || async {
            a.history(b.node_id(), 10)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.read_ms.is_some())
        },
        "A 收到已读回执",
    )
    .await;

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn offline_queue_flushed_when_peer_returns() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let a_dir = temp_dir("offline-a");
    let b_dir = temp_dir("offline-b");
    let a_ports = (25701, 25702);
    let b_ports = (25711, 25712);

    let a = App::start(app_config(
        a_dir.clone(),
        "Alice",
        a_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))],
    ))
    .await
    .unwrap();
    let b = App::start(app_config(
        b_dir.clone(),
        "Bob",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    ))
    .await
    .unwrap();
    wait_discovery(&a, &b).await;
    let b_id = b.node_id();
    b.shutdown();

    // 给 A 一点时间感知断开(连接 EOF 清理)
    tokio::time::sleep(Duration::from_millis(300)).await;

    // B 离线 → A 的消息进入待发队列
    let outcome = a.send_text(b_id, "你不在时发出的消息").await.unwrap();
    assert!(
        matches!(outcome, SendOutcome::Queued(_)),
        "对端离线必须入队"
    );
    assert_eq!(a.pending_count().await.unwrap(), 1);
    // 队列中的消息尚未写入历史(发出后才算)
    assert!(a.history(b_id, 10).await.unwrap().is_empty());

    // B 重新上线(同一数据目录 → 同一身份/密钥/端口)
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b2 = App::start(app_config(
        b_dir,
        "Bob",
        b_ports,
        vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))],
    ))
    .await
    .unwrap();
    assert_eq!(b2.node_id(), b_id, "同一数据目录重启后身份必须稳定");

    // A 自动补发,B 收到并入库
    eventually(
        || async {
            b2.history(a.node_id(), 10)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.body.as_deref() == Some("你不在时发出的消息"))
        },
        "B 回归后收到补发消息",
    )
    .await;

    // A 的队列清空,消息进入历史(且最终拿到送达回执)
    eventually(
        || async {
            let pending = a.pending_count().await.unwrap();
            (pending == 0).then_some(())
        },
        "待发队列清空",
    )
    .await;
    eventually(
        || async {
            a.history(b_id, 10).await.unwrap().into_iter().find(|m| {
                m.body.as_deref() == Some("你不在时发出的消息") && m.delivered_ms.is_some()
            })
        },
        "补发消息入库且带回执",
    )
    .await;

    a.shutdown();
    b2.shutdown();
}

#[tokio::test]
async fn identity_and_history_survive_restart() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let a_dir = temp_dir("restart-a");
    let b_dir = temp_dir("restart-b");
    let a_ports = (25801, 25802);
    let b_ports = (25811, 25812);
    let a_bootstrap = vec![SocketAddr::from(([127, 0, 0, 1], b_ports.0))];
    let b_bootstrap = vec![SocketAddr::from(([127, 0, 0, 1], a_ports.0))];

    let a = App::start(app_config(
        a_dir.clone(),
        "Alice",
        a_ports,
        a_bootstrap.clone(),
    ))
    .await
    .unwrap();
    let b = App::start(app_config(b_dir, "Bob", b_ports, b_bootstrap.clone()))
        .await
        .unwrap();
    wait_discovery(&a, &b).await;
    let (a_id, b_id) = (a.node_id(), b.node_id());

    for i in 0..3 {
        a.send_text(b_id, &format!("重启前的第 {i} 条"))
            .await
            .unwrap();
    }
    eventually(
        || async {
            let history = b.history(a_id, 10).await.unwrap();
            (history.len() == 3).then_some(())
        },
        "B 收齐 3 条",
    )
    .await;

    a.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 同一数据目录重启:身份、历史都保留
    let a2 = App::start(app_config(a_dir, "Alice", a_ports, a_bootstrap))
        .await
        .unwrap();
    assert_eq!(a2.node_id(), a_id, "重启后 NodeId 必须稳定");
    let history = a2.history(b_id, 10).await.unwrap();
    assert_eq!(history.len(), 3, "重启后历史必须保留");
    assert!(history.iter().all(|m| m.is_outgoing));

    // 重启后必须先重新双向发现,再继续通信
    wait_discovery(&a2, &b).await;
    // 重启后仍能继续通信(B 对 A 没有产生信任告警 —— 静态密钥持久化生效)
    let mut b_events = b.events();
    b.send_text(a_id, "你重启回来了").await.unwrap();
    eventually(
        || async {
            a2.history(b_id, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.body.as_deref() == Some("你重启回来了"))
        },
        "重启后继续收信",
    )
    .await;
    // 全程不得出现 TOFU 告警
    let mut warned = false;
    while let Ok(event) = b_events.try_recv() {
        if matches!(
            event,
            AppEvent::Node(inner) if matches!(*inner, NodeEvent::TrustWarning { .. })
        ) {
            warned = true;
        }
    }
    assert!(!warned, "静态密钥持久化后,重启不得触发信任告警");

    a2.shutdown();
    b.shutdown();
}
