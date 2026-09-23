//! feiqiu-r 桌面客户端:Tauri v2 壳 + fq-core 应用核心。
//!
//! 架构:
//! ```text
//! React(WebView) ──invoke──▶ tauri commands ──▶ fq_core::App(网络/存储/安全)
//! React(WebView) ◀──fq://event── 事件转发任务 ◀── AppEvent 流
//! ```
//! 桌面行为:关闭按钮 = 最小化到托盘;托盘菜单可显示/退出。

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fq_proto::NodeId;
use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::broadcast::Receiver;

/// 数据目录里保存昵称/分组/直连/端口的小配置。
///
/// ```json
/// {
///   "name": "张三",
///   "group": "研发",
///   "bootstrap": ["192.168.1.20:24250"],
///   "discovery_port": 24250,
///   "listen_port": 24250
/// }
/// ```
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct Profile {
    name: Option<String>,
    group: Option<String>,
    /// 单播直连目标(禁广播网络/跨网段时使用),可多填。
    bootstrap: Option<Vec<String>>,
    discovery_port: Option<u16>,
    listen_port: Option<u16>,
    /// 接收文件默认保存目录(绝对路径)。
    download_dir: Option<String>,
    /// 是否自动获取并安装局域网内的新版本(默认关闭 → 先询问)。
    auto_update: Option<bool>,
    /// 是否自动接收文件传输(默认关闭 → 弹窗确认)。
    auto_accept_files: Option<bool>,
}

fn load_profile(dir: &std::path::Path) -> Profile {
    let path = dir.join("profile.json");
    let Some(raw) = std::fs::read_to_string(&path).ok() else {
        return Profile::default();
    };
    // 容忍 UTF-8 BOM:记事本/PowerShell 保存的 JSON 常带 BOM,
    // serde_json 会直接报错并让用户"配置静默失效"(曾踩过)。
    let cleaned = raw.trim_start_matches('\u{feff}').trim_start();
    serde_json::from_str(cleaned).unwrap_or_else(|e| {
        tracing::warn!(target = "fq_desktop", path = %path.display(), %e, "profile.json 解析失败,使用默认配置");
        Profile::default()
    })
}

fn save_profile(dir: &std::path::Path, profile: &Profile) -> Result<(), String> {
    let path = dir.join("profile.json");
    let json = serde_json::to_string_pretty(profile).map_err(|e| e.to_string())?;
    // 明确写 UTF-8 无 BOM:带 BOM 的 JSON 会让部分解析器(包括 serde_json)直接失败
    std::fs::write(&path, json.as_bytes()).map_err(|e| e.to_string())
}

fn default_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "feiqiu-r 用户".into())
}

/// 托管在 Tauri 状态里的应用核心。
struct FqState {
    app: Arc<fq_core::App>,
    name: String,
}

// ── 前端 DTO ──────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SelfInfoDto {
    node_id: String,
    name: String,
    fingerprint: String,
    /// 本机全部可用 IPv4(多网卡会有多个;告诉同伴连哪个)。
    local_ips: Vec<String>,
    /// 当前接收文件保存目录。
    download_dir: String,
    /// 本机头像 data URL(未设置为 None)。
    avatar: Option<String>,
    /// 当前分组名。
    group: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct PeerDto {
    node_id: String,
    name: String,
    group: Option<String>,
    online: bool,
    last_seen_ms: i64,
    /// 对端可达 IP(展示用,去重)。
    ips: Vec<String>,
    /// 对端软件版本。
    app_version: Option<String>,
    /// 对端头像哈希(有头像时非空;离线也带缓存哈希)。
    avatar_sha256: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct MessageDto {
    id: String,
    outgoing: bool,
    kind: String,
    body: Option<String>,
    ts_ms: i64,
    delivered: bool,
    read: bool,
    /// 发送状态:`pending`(在待发队列)/ `sent` / `delivered` / `read`。
    status: String,
}

fn status_of(
    message: &fq_store::StoredMessage,
    pending: &std::collections::HashSet<String>,
) -> String {
    if message.read_ms.is_some() {
        "read".into()
    } else if message.delivered_ms.is_some() {
        "delivered".into()
    } else if message.is_outgoing && pending.contains(&message.id) {
        "pending".into()
    } else {
        "sent".into()
    }
}

fn message_dto(
    m: fq_store::StoredMessage,
    pending: &std::collections::HashSet<String>,
) -> MessageDto {
    MessageDto {
        status: status_of(&m, pending),
        id: m.id,
        outgoing: m.is_outgoing,
        kind: m.kind,
        body: m.body,
        ts_ms: m.ts_ms,
        delivered: m.delivered_ms.is_some(),
        read: m.read_ms.is_some(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SendResultDto {
    id: String,
    queued: bool,
}

/// 统一前端事件(fq://event)。
#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FqEventDto {
    PeerUp {
        node_id: String,
        name: String,
        group: Option<String>,
    },
    PeerDown {
        node_id: String,
    },
    Message {
        from: String,
        from_name: String,
        id: String,
        body: String,
        ts_ms: i64,
    },
    Delivered {
        id: String,
    },
    Read {
        id: String,
    },
    QueuedFlushed {
        to: String,
        count: usize,
    },
    FileOffer {
        from_name: String,
        token: String,
        entries: usize,
        total_bytes: u64,
    },
    FileProgress {
        direction: &'static str,
        token: String,
        path: String,
        transferred: u64,
        total: u64,
    },
    FileDone {
        direction: &'static str,
        token: String,
        path: String,
        verified: bool,
    },
    FileCompleted {
        direction: &'static str,
        token: String,
    },
    FileFailed {
        direction: &'static str,
        token: String,
        path: Option<String>,
        reason: String,
    },
    TrustWarning {
        node_id: String,
        pinned: String,
        presented: String,
    },
    /// 收到更新包(自动接收,不需要用户点"接收")。
    UpdateIncoming {
        from_name: String,
        version: String,
        token: String,
        file_name: String,
        total_bytes: u64,
    },
    /// 更新包已下载并校验通过,可一键重启安装。
    UpdateReady {
        from: String,
        from_name: String,
        version: String,
        path: String,
    },
    /// 某对端头像已更新(前端重新拉取展示)。
    PeerAvatar {
        node_id: String,
    },
    /// 某对端已移除头像(前端清掉展示)。
    PeerAvatarRemoved {
        node_id: String,
    },
}

fn direction_str(d: fq_net::TransferDirection) -> &'static str {
    match d {
        fq_net::TransferDirection::Sending => "send",
        fq_net::TransferDirection::Receiving => "recv",
    }
}

/// 初始化文件日志(`<数据目录>/logs/fq-desktop.log`,按天滚动)。
///
/// GUI 子进程没有可见的控制台,`tracing` 默认输出会丢失;写到文件后,
/// 支持"出问题 → 发日志"这一最基本的排查路径(默认 info 级,可用 RUST_LOG 覆盖)。
fn init_file_logging(data_dir: &std::path::Path) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let log_dir = data_dir.join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    // 保留 7 天,避免长期运行把磁盘写满
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("fq-desktop")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&log_dir);
    let Ok(appender) = appender else {
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("fq_core=info,fq_net=info,fq_desktop=info")
    });
    let subscriber = tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_writer(appender)
            .with_ansi(false),
    );
    let _ = subscriber.try_init();
}

// ── 应用装配 ──────────────────────────────────────────────

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let data_dir: PathBuf = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("无法确定数据目录: {e}"))?;
            // 文件日志:桌面端是 GUI 子进程,没有控制台可看 —— 出问题时日志是唯一线索
            init_file_logging(&data_dir);
            let profile = load_profile(&data_dir);
            let name = profile.name.clone().unwrap_or_else(default_name);

            let mut config = fq_core::AppConfig::new(&data_dir, &name);
            config.group = profile.group.clone();
            if let Some(port) = profile.discovery_port {
                config.discovery_port = port;
            }
            if let Some(port) = profile.listen_port {
                config.listen_port = port;
            }
            config.bootstrap = profile
                .bootstrap
                .unwrap_or_default()
                .iter()
                .filter_map(|raw| raw.parse().ok())
                .collect();
            if let Some(dir) = &profile.download_dir {
                config.download_dir = Some(PathBuf::from(dir));
            }
            // 桌面端默认关闭自动接受:文件要约走 UI 确认流
            // (可在 profile.json 里设 "auto_accept_files": true 免确认,适合无人值守/演示)
            config.auto_accept_files = profile.auto_accept_files.unwrap_or(false);
            // 版本号随通告广播:局域网内可发现更新版本
            config.app_version = Some(env!("CARGO_PKG_VERSION").to_string());
            let fq = tauri::async_runtime::block_on(fq_core::App::start(config))
                .map_err(|e| format!("应用核心启动失败: {e}"))?;
            let fq = Arc::new(fq);

            app.manage(FqState {
                app: Arc::clone(&fq),
                name: name.clone(),
            });

            // 事件转发:fq_core → WebView
            let events = fq.events();
            tauri::async_runtime::spawn(forward_events(
                app.handle().clone(),
                Arc::clone(&fq),
                events,
            ));

            setup_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭 = 最小化到托盘(数据已实时落盘,直接退出也没有损失)
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_self_info,
            list_peers,
            send_text,
            send_file_to,
            history,
            mark_read,
            accept_file_offer,
            reject_file_offer,
            set_download_dir,
            refresh_peers,
            remove_peer,
            take_screenshot,
            open_file,
            pick_image,
            read_image_base64,
            paste_image,
            set_profile,
            search_messages,
            list_groups,
            create_group,
            delete_group,
            send_group_text,
            list_conversations,
            mark_conversation_read,
            history_before,
            check_update,
            list_transfer_history,
            clear_transfer_history,
            delete_transfer_history,
            cancel_transfer,
            request_update,
            install_update,
            get_preferences,
            set_auto_update,
            choose_avatar,
            clear_avatar,
            get_self_avatar,
            get_peer_avatar,
            delete_conversation
        ])
        .run(tauri::generate_context!())
        .map_err(|e| eprintln!("[feiqiu-r] 运行失败: {e}"))
        .err();
}

fn setup_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let icon = app.default_window_icon().cloned().ok_or("缺少窗口图标")?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("feiqiu-r")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// 事件转发任务:AppEvent → FqEventDto → `fq://event`。
///
/// 同时维护 NodeId → 昵称 的缓存(消息事件里带上可读的发送方名)。
async fn forward_events(
    handle: AppHandle,
    fq_app: Arc<fq_core::App>,
    mut events: Receiver<fq_core::AppEvent>,
) {
    let names: Arc<Mutex<HashMap<NodeId, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let name_of = |id: NodeId| -> String {
        names
            .lock()
            .ok()
            .and_then(|cache| cache.get(&id).cloned())
            .unwrap_or_else(|| id.to_hex())
    };

    loop {
        let Ok(event) = events.recv().await else {
            return;
        };
        let dto = match event {
            fq_core::AppEvent::MessageSaved { .. } => None,
            fq_core::AppEvent::QueueFlushed { to, count } => Some(FqEventDto::QueuedFlushed {
                to: to.to_hex(),
                count,
            }),
            fq_core::AppEvent::UpdateReady {
                from,
                version,
                path,
            } => Some(FqEventDto::UpdateReady {
                from: from.to_hex(),
                from_name: name_of(from),
                version,
                path,
            }),
            fq_core::AppEvent::PeerAvatar { node_id, .. } => Some(FqEventDto::PeerAvatar {
                node_id: node_id.to_hex(),
            }),
            fq_core::AppEvent::PeerAvatarRemoved { node_id } => {
                Some(FqEventDto::PeerAvatarRemoved {
                    node_id: node_id.to_hex(),
                })
            }
            fq_core::AppEvent::Node(inner) => match *inner {
                fq_net::NodeEvent::PeerDiscovered { peer, .. }
                | fq_net::NodeEvent::PeerUpdated { peer, .. } => {
                    if let Ok(mut cache) = names.lock() {
                        cache.insert(peer.node_id, peer.display_name.clone());
                    }
                    Some(FqEventDto::PeerUp {
                        node_id: peer.node_id.to_hex(),
                        name: peer.display_name,
                        group: peer.group,
                    })
                }
                fq_net::NodeEvent::PeerLost { node_id } => Some(FqEventDto::PeerDown {
                    node_id: node_id.to_hex(),
                }),
                fq_net::NodeEvent::TrustWarning {
                    node_id,
                    pinned,
                    presented,
                } => {
                    notify(&handle, "feiqiu-r 信任告警", "检测到静态密钥变更,请核实");
                    Some(FqEventDto::TrustWarning {
                        node_id: node_id.to_hex(),
                        pinned,
                        presented,
                    })
                }
                fq_net::NodeEvent::MessageReceived { from, envelope } => match envelope.kind {
                    fq_proto::Kind::Text(body) => Some(FqEventDto::Message {
                        from: from.to_hex(),
                        from_name: name_of(from),
                        id: envelope.id.to_string(),
                        body: body.body,
                        ts_ms: envelope.ts_ms,
                    }),
                    fq_proto::Kind::Ack(ack) => match ack.status {
                        fq_proto::AckStatus::Delivered => Some(FqEventDto::Delivered {
                            id: ack.ack_id.to_string(),
                        }),
                        fq_proto::AckStatus::Read => Some(FqEventDto::Read {
                            id: ack.ack_id.to_string(),
                        }),
                        _ => None,
                    },
                    _ => None,
                },
                fq_net::NodeEvent::FileOfferReceived {
                    from,
                    token,
                    manifest,
                    ..
                } => {
                    let from_name = name_of(from);
                    notify(
                        &handle,
                        "feiqiu-r 收到文件",
                        &format!("{from_name} 发来 {} 个文件", manifest.entries.len()),
                    );
                    Some(FqEventDto::FileOffer {
                        from_name,
                        token,
                        entries: manifest.entries.len(),
                        total_bytes: manifest.total_bytes,
                    })
                }
                fq_net::NodeEvent::FileProgress {
                    direction,
                    token,
                    path,
                    transferred,
                    total,
                    ..
                } => Some(FqEventDto::FileProgress {
                    direction: direction_str(direction),
                    token,
                    path,
                    transferred,
                    total,
                }),
                fq_net::NodeEvent::FileEntryDone {
                    direction,
                    token,
                    path,
                    verified,
                    peer,
                    ..
                } => {
                    // 接收方文件完成(校验通过)→ 插入聊天历史(图片/文件气泡)
                    if direction == fq_net::TransferDirection::Receiving && verified {
                        let file_name =
                            path.rsplit(['/', '\\']).next().unwrap_or(&path).to_string();
                        let local_path = fq_app.download_dir().join(&path);
                        let size = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
                        let path_str = local_path.display().to_string();
                        let fq = Arc::clone(&fq_app);
                        tokio::spawn(async move {
                            if let Err(e) = fq
                                .insert_file_message(peer, false, &file_name, size, &path_str)
                                .await
                            {
                                tracing::warn!(target = "fq_desktop", %e, "接收文件消息入库失败");
                            }
                        });
                    }
                    Some(FqEventDto::FileDone {
                        direction: direction_str(direction),
                        token,
                        path,
                        verified,
                    })
                }
                fq_net::NodeEvent::UpdateOfferReceived {
                    from,
                    token,
                    version,
                    manifest,
                } => {
                    let from_name = name_of(from);
                    notify(
                        &handle,
                        "feiqiu-r 正在获取更新",
                        &format!("正在从 {from_name} 下载 v{version}"),
                    );
                    Some(FqEventDto::UpdateIncoming {
                        from_name,
                        version,
                        token,
                        file_name: manifest.root_name.clone(),
                        total_bytes: manifest.total_bytes,
                    })
                }
                fq_net::NodeEvent::UpdatePackageReady {
                    from,
                    version,
                    path,
                    ..
                } => {
                    notify(
                        &handle,
                        "feiqiu-r 更新就绪",
                        &format!("v{version} 已下载并校验通过,重启即可完成更新"),
                    );
                    Some(FqEventDto::UpdateReady {
                        from: from.to_hex(),
                        from_name: name_of(from),
                        version,
                        path,
                    })
                }
                fq_net::NodeEvent::FileTransferCompleted {
                    direction, token, ..
                } => Some(FqEventDto::FileCompleted {
                    direction: direction_str(direction),
                    token,
                }),
                fq_net::NodeEvent::FileTransferFailed {
                    direction,
                    token,
                    path,
                    reason,
                    ..
                } => Some(FqEventDto::FileFailed {
                    direction: direction_str(direction),
                    token,
                    path,
                    reason,
                }),
            },
        };
        if let Some(dto) = dto {
            let _ = handle.emit("fq://event", &dto);
        }
    }
}

fn notify(handle: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = handle
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

fn parse_node_id(raw: &str) -> Result<NodeId, String> {
    NodeId::from_hex(raw).map_err(|e| format!("非法节点 ID: {e}"))
}

// ── Commands ──────────────────────────────────────────────

#[tauri::command]
fn get_self_info(state: State<'_, FqState>) -> Result<SelfInfoDto, String> {
    Ok(SelfInfoDto {
        node_id: state.app.node_id().to_hex(),
        name: state.name.clone(),
        fingerprint: state.app.fingerprint(),
        local_ips: fq_net::local_ipv4_addresses()
            .into_iter()
            .map(|ip| ip.to_string())
            .collect(),
        download_dir: state.app.download_dir().display().to_string(),
        avatar: Some(avatar_data_url(state.app.as_ref())).filter(|s| !s.is_empty()),
        group: state.app.profile().1,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct VersionReportDto {
    /// 本机版本。
    local_version: String,
    /// 协议版本(fq-proto)。
    protocol_version: u16,
    /// 局域网内发现的最高版本(没有更高时为 None)。
    latest_version: Option<String>,
    /// 携带最高版本的对端(便于找人要安装包)。
    latest_from: Option<String>,
    /// 是否发现更高版本。
    update_available: bool,
    /// 各在线对端版本:`[名称, 版本, NodeId]`。
    peers: Vec<PeerVersionDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct PeerVersionDto {
    node_id: String,
    name: String,
    version: String,
    /// 与本机版本比较:newer / same / older
    relation: &'static str,
}

/// 版本号比较(数字分段;段数不足补 0)。返回 Ordering 语义:`-1/0/1`。
fn compare_versions(a: &str, b: &str) -> i32 {
    let parse = |s: &str| -> Vec<u64> {
        s.split(['.', '-', '+'])
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return if x > y { 1 } else { -1 };
        }
    }
    0
}

/// 更新检查:本机版本 vs 局域网内各对端版本(纯 P2P,无需更新服务器)。
#[tauri::command]
fn check_update(state: State<'_, FqState>) -> Result<VersionReportDto, String> {
    let local = env!("CARGO_PKG_VERSION");
    let (_, peers_raw) = state.app.version_report(local);
    tracing::debug!(target = "fq_desktop", local, peers = ?peers_raw.iter().map(|(_, n, v)| format!("{n}={v}")).collect::<Vec<_>>(), "版本报告");

    let mut latest: Option<(String, String)> = None;
    let peers: Vec<PeerVersionDto> = peers_raw
        .into_iter()
        .map(|(node_id, name, version)| {
            let relation = match compare_versions(&version, local) {
                1 => "newer",
                0 => "same",
                _ => "older",
            };
            if relation == "newer" {
                let better = latest
                    .as_ref()
                    .map(|(_, v)| compare_versions(&version, v) == 1)
                    .unwrap_or(true);
                if better {
                    latest = Some((name.clone(), version.clone()));
                }
            }
            PeerVersionDto {
                node_id,
                name,
                version,
                relation,
            }
        })
        .collect();

    Ok(VersionReportDto {
        local_version: local.to_string(),
        protocol_version: fq_proto::PROTOCOL_VERSION,
        update_available: latest.is_some(),
        latest_version: latest.as_ref().map(|(_, v)| v.clone()),
        latest_from: latest.map(|(n, _)| n),
        peers,
    })
}

#[tauri::command]
async fn list_peers(state: State<'_, FqState>) -> Result<Vec<PeerDto>, String> {
    state
        .app
        .peers()
        .await
        .map(|peers| {
            peers
                .into_iter()
                .map(|p| PeerDto {
                    node_id: p.node_id.to_hex(),
                    name: p.display_name,
                    group: p.group,
                    online: p.online,
                    last_seen_ms: p.last_seen_ms,
                    ips: {
                        let mut seen = std::collections::HashSet::new();
                        p.endpoints
                            .iter()
                            .map(|addr| addr.ip().to_string())
                            .filter(|ip| seen.insert(ip.clone()))
                            .collect()
                    },
                    app_version: p.app_version,
                    avatar_sha256: p.avatar_sha256,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// 同意接收文件要约;`dir` 为本次保存位置(可选)。
#[tauri::command]
fn accept_file_offer(
    state: State<'_, FqState>,
    token: String,
    dir: Option<String>,
) -> Result<bool, String> {
    Ok(state.app.accept_file_offer(&token, dir.map(PathBuf::from)))
}

/// 拒绝接收文件要约。
#[tauri::command]
fn reject_file_offer(state: State<'_, FqState>, token: String) -> Result<bool, String> {
    Ok(state.app.reject_file_offer(&token))
}

/// 修改接收文件默认保存目录:立即生效 + 持久化到 profile.json。
#[tauri::command]
fn set_download_dir(state: State<'_, FqState>, path: String) -> Result<(), String> {
    let dir = PathBuf::from(&path);
    state.app.set_download_dir(dir);
    let mut profile = load_profile(state.app.data_dir());
    profile.download_dir = Some(path);
    save_profile(state.app.data_dir(), &profile)
}

/// 手动刷新:立即广播在线通告(连续三轮,覆盖所有网卡路径),并把已知对端写回联系人表。
///
/// 写回这一步很关键:删掉的联系人只要还在局域网里,**刷新就会自动回来**(飞秋语义)。
#[tauri::command]
async fn refresh_peers(state: State<'_, FqState>) -> Result<usize, String> {
    state.app.node().announce_now().await;
    // 间隔 200ms 再发两轮,给 ANSENTRY 回发留时间窗口
    for _ in 0..2 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        state.app.node().announce_now().await;
    }
    // 通告发完再等一小会儿,让对端的回发有机会被收到,然后统一写回联系人表
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    state
        .app
        .refresh_contacts()
        .await
        .map_err(|e| e.to_string())
}

/// 删除联系人(只移出列表;刷新或对方再上线会自动回来,聊天记录保留)。
#[tauri::command]
async fn remove_peer(state: State<'_, FqState>, node_id: String) -> Result<bool, String> {
    let id = parse_node_id(&node_id)?;
    state.app.remove_peer(id).await.map_err(|e| e.to_string())
}

/// 用系统默认程序打开文件/文件夹。
#[tauri::command]
fn open_file(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &path])
            .spawn()
            .map_err(|e| format!("打开失败: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 图片选择对话框(只筛选图片文件)。
#[tauri::command]
async fn pick_image(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let file_path = app
        .dialog()
        .file()
        .add_filter("图片", &["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico"])
        .blocking_pick_file();
    Ok(file_path.map(|p| p.to_string()))
}

/// 读取剪贴板图片并存为临时 PNG;剪贴板没有图片时返回 None。
#[tauri::command]
async fn paste_image() -> Result<Option<String>, String> {
    let maybe_image = {
        use std::sync::Mutex;
        static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());
        let _guard = CLIPBOARD_LOCK.lock();
        arboard::Clipboard::new()
            .ok()
            .and_then(|mut clipboard| clipboard.get_image().ok())
    };
    let Some(image_data) = maybe_image else {
        return Ok(None);
    };
    let path = std::env::temp_dir().join(format!(
        "fq-paste-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let img = image::RgbaImage::from_raw(
        image_data.width as u32,
        image_data.height as u32,
        image_data.bytes.to_vec(),
    )
    .ok_or("剪贴板图片数据无效")?;
    img.save(&path).map_err(|e| format!("保存图片失败: {e}"))?;
    Ok(Some(path.display().to_string()))
}

/// 选择并设置头像:压缩成 256×256 PNG 存到数据目录,然后重新通告全网。
#[tauri::command]
async fn choose_avatar(
    app: AppHandle,
    state: State<'_, FqState>,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app
        .dialog()
        .file()
        .add_filter("图片", &["png", "jpg", "jpeg", "webp", "bmp", "gif"])
        .blocking_pick_file();
    let Some(file) = picked else {
        return Ok(None); // 用户取消
    };
    let path = file
        .into_path()
        .map_err(|e| format!("无法解析所选路径: {e}"))?;
    set_avatar_from(state.app.as_ref(), &path).await?;
    Ok(Some(avatar_data_url(state.app.as_ref())))
}

/// 从指定路径设置头像(也供命令行/测试使用)。
async fn set_avatar_from(app: &fq_core::App, source: &std::path::Path) -> Result<(), String> {
    let img = image::open(source).map_err(|e| format!("无法读取图片: {e}"))?;
    // 统一成 256×256 方图:等比缩放后居中裁切,避免变形
    let thumb = img.resize_to_fill(256, 256, image::imageops::FilterType::Lanczos3);
    let target = app.avatar_path().to_path_buf();
    thumb
        .save(&target)
        .map_err(|e| format!("保存头像失败: {e}"))?;
    let hash = app.reload_avatar().await;
    tracing::info!(
        target = "fq_desktop",
        path = %target.display(),
        hash = hash.as_deref().unwrap_or("-"),
        "头像已更新并广播"
    );
    Ok(())
}

/// 移除头像。
#[tauri::command]
async fn clear_avatar(state: State<'_, FqState>) -> Result<(), String> {
    state.app.clear_avatar().await.map_err(|e| e.to_string())
}

/// 本机头像(data URL;未设置为 None)。
#[tauri::command]
fn get_self_avatar(state: State<'_, FqState>) -> Option<String> {
    Some(avatar_data_url(state.app.as_ref())).filter(|s| !s.is_empty())
}

/// 对端头像(data URL;未缓存为 None)。
#[tauri::command]
async fn get_peer_avatar(
    state: State<'_, FqState>,
    node_id: String,
) -> Result<Option<String>, String> {
    let avatar = state
        .app
        .peer_avatar(&node_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(avatar.map(|(_, mime, data)| format!("data:{mime};base64,{}", base64_encode(&data))))
}

/// 把本机头像文件转成 data URL(空字符串表示没有)。
fn avatar_data_url(app: &fq_core::App) -> String {
    app.local_avatar()
        .map(|data| format!("data:image/png;base64,{}", base64_encode(&data)))
        .unwrap_or_default()
}

/// 从"最近会话"里删除一个会话(聊天记录与联系人保留)。
#[tauri::command]
async fn delete_conversation(state: State<'_, FqState>, peer: String) -> Result<bool, String> {
    state
        .app
        .delete_conversation(&peer)
        .await
        .map_err(|e| e.to_string())
}

/// 修改昵称/分组:立即通告 + 持久化 profile.json。
#[tauri::command]
async fn set_profile(
    state: State<'_, FqState>,
    name: Option<String>,
    group: Option<String>,
) -> Result<(), String> {
    let (current_name, _) = state.app.profile();
    let new_name = name.unwrap_or(current_name);
    state.app.set_profile(&new_name, group.as_deref()).await;

    let mut profile = load_profile(state.app.data_dir());
    profile.name = Some(new_name);
    profile.group = group;
    save_profile(state.app.data_dir(), &profile)
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct PreferencesDto {
    /// 是否自动获取并安装更新。
    auto_update: bool,
    /// 本机版本(便于前端展示/比较)。
    local_version: String,
}

/// 读取偏好设置(自动更新开关等)。
#[tauri::command]
fn get_preferences(state: State<'_, FqState>) -> Result<PreferencesDto, String> {
    let profile = load_profile(state.app.data_dir());
    Ok(PreferencesDto {
        auto_update: profile.auto_update.unwrap_or(false),
        local_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// 设置"自动获取并安装更新"。
#[tauri::command]
fn set_auto_update(state: State<'_, FqState>, enabled: bool) -> Result<(), String> {
    let mut profile = load_profile(state.app.data_dir());
    profile.auto_update = Some(enabled);
    save_profile(state.app.data_dir(), &profile)
}

/// 读取图片文件为 base64(用于聊天内缩略图预览)。
#[tauri::command]
fn read_image_base64(path: String) -> Result<String, String> {
    let data = std::fs::read(&path).map_err(|e| format!("读取图片失败: {e}"))?;
    Ok(base64_encode(&data))
}

/// 标准 base64 编码(仅用 `+/` 表,不带换行)。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 截屏发送(目标可为单聊 NodeId 或 `group:...` 群 ID)。
#[tauri::command]
async fn take_screenshot(
    app: AppHandle,
    state: State<'_, FqState>,
    target: String,
) -> Result<(), String> {
    let is_group = target.starts_with("group:");
    if !is_group {
        // 提前校验目标,避免截完图才发现非法
        let _ = parse_node_id(&target)?;
    }
    // 1. 最小化主窗口,让用户看到屏幕
    let window = app.get_webview_window("main").ok_or("找不到主窗口")?;
    let _ = window.minimize();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // 2. 清空剪贴板(避免读到旧图)
    {
        use std::sync::Mutex;
        static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());
        let _guard = CLIPBOARD_LOCK.lock();
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.clear();
        }
    }

    // 3. 调用 Windows 截图工具(Snipping Tool,用户拖选区域)
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", "ms-screenclip:"])
        .spawn();

    // 4. 轮询剪贴板等待截图(最长 30 秒,500ms 间隔)
    let start = std::time::Instant::now();
    let screenshot_path: std::path::PathBuf;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        if start.elapsed() > std::time::Duration::from_secs(30) {
            let _ = window.unminimize();
            return Err("截图超时(30 秒内未检测到截图)".into());
        }

        // 尝试读取剪贴板中的位图
        let maybe_image = {
            use std::sync::Mutex;
            static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());
            let _guard = CLIPBOARD_LOCK.lock();
            arboard::Clipboard::new()
                .ok()
                .and_then(|mut clipboard| clipboard.get_image().ok())
        };

        if let Some(image_data) = maybe_image {
            // 5. RGBA → PNG
            let path = std::env::temp_dir().join(format!(
                "fq-screenshot-{}.png",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ));
            let img = image::RgbaImage::from_raw(
                image_data.width as u32,
                image_data.height as u32,
                image_data.bytes.to_vec(),
            )
            .ok_or("截图数据无效")?;
            img.save(&path).map_err(|e| format!("保存截图失败: {e}"))?;
            screenshot_path = path;
            break;
        }
    }

    // 6. 发送截图文件(单聊/群聊分发)
    let path_str = screenshot_path.display().to_string();
    let send_result = if is_group {
        state
            .app
            .send_group_file(&target, &path_str, Some("截图".into()))
            .await
            .map(|_| ())
    } else {
        let to = parse_node_id(&target)?;
        state
            .app
            .send_file(to, &path_str, Some("截图".into()))
            .await
            .map(|_| ())
    };
    send_result.map_err(|e| format!("发送截图失败: {e}"))?;

    // 7. 恢复窗口
    let _ = window.unminimize();
    let _ = window.set_focus();
    Ok(())
}

#[tauri::command]
async fn send_text(
    state: State<'_, FqState>,
    node_id: String,
    body: String,
) -> Result<SendResultDto, String> {
    let to = parse_node_id(&node_id)?;
    let outcome = state
        .app
        .send_text(to, &body)
        .await
        .map_err(|e| e.to_string())?;
    Ok(SendResultDto {
        id: outcome.message_id().to_string(),
        queued: outcome.is_queued(),
    })
}

/// 发送文件到目标会话:目标可为单聊 NodeId(hex)或 `group:...` 群 ID。
///
/// 返回本次发起的传输令牌列表(群聊为每个可达成员一个)。
#[tauri::command]
async fn send_file_to(
    state: State<'_, FqState>,
    target: String,
    path: String,
) -> Result<Vec<String>, String> {
    if target.starts_with("group:") {
        state
            .app
            .send_group_file(&target, path, None)
            .await
            .map_err(|e| e.to_string())
    } else {
        let to = parse_node_id(&target)?;
        state
            .app
            .send_file(to, path, None)
            .await
            .map(|token| vec![token])
            .map_err(|e| e.to_string())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SearchHitDto {
    peer: String,
    id: String,
    outgoing: bool,
    kind: String,
    body: Option<String>,
    ts_ms: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct GroupDto {
    id: String,
    name: String,
    members: Vec<String>,
    member_count: usize,
}

#[tauri::command]
async fn list_groups(state: State<'_, FqState>) -> Result<Vec<GroupDto>, String> {
    state
        .app
        .list_groups()
        .await
        .map(|groups| {
            groups
                .into_iter()
                .map(|g| GroupDto {
                    member_count: g.members.len(),
                    id: g.id,
                    name: g.name,
                    members: g.members,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn create_group(
    state: State<'_, FqState>,
    name: String,
    members: Vec<String>,
) -> Result<String, String> {
    let member_ids: Vec<_> = members
        .iter()
        .filter_map(|m| NodeId::from_hex(m).ok())
        .collect();
    state
        .app
        .create_group(&name, &member_ids)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_group(state: State<'_, FqState>, group_id: String) -> Result<bool, String> {
    state
        .app
        .delete_group(&group_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_group_text(
    state: State<'_, FqState>,
    group_id: String,
    body: String,
) -> Result<(usize, usize), String> {
    state
        .app
        .send_group_text(&group_id, &body)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ConversationDto {
    peer: String,
    last_msg_ms: i64,
    preview: String,
    unread: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TransferHistoryDto {
    token: String,
    peer: String,
    peer_name: String,
    direction: String,
    path: String,
    size: u64,
    status: String,
    detail: Option<String>,
    started_ms: i64,
    finished_ms: Option<i64>,
}

/// 向对端索取更新包(对端版本更高时自动回发并接收)。
#[tauri::command]
async fn request_update(state: State<'_, FqState>, node_id: String) -> Result<(), String> {
    let to = parse_node_id(&node_id)?;
    tracing::info!(target = "fq_desktop", to = %to, "UI 请求更新包");
    state
        .app
        .request_update(to)
        .await
        .map_err(|e| e.to_string())
}

/// 安装已下载并校验通过的更新包(会重启应用)。
#[tauri::command]
fn install_update(state: State<'_, FqState>, path: String) -> Result<(), String> {
    state
        .app
        .install_update_and_restart(&path)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_transfer_history(
    state: State<'_, FqState>,
    limit: Option<u32>,
) -> Result<Vec<TransferHistoryDto>, String> {
    let limit = limit.unwrap_or(200).clamp(1, 1000);
    // 对端昵称解析:优先在线对端,其次群组名,最后 ID 前缀
    let peers: std::collections::HashMap<String, String> = state
        .app
        .peers()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|p| (p.node_id.to_hex(), p.display_name))
        .collect();
    let groups: std::collections::HashMap<String, String> = state
        .app
        .list_groups()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|g| (g.id, g.name))
        .collect();

    state
        .app
        .transfer_history(limit)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|t| {
                    let peer_name = peers
                        .get(&t.peer)
                        .or_else(|| groups.get(&t.peer))
                        .cloned()
                        .unwrap_or_else(|| format!("{}…", &t.peer[..t.peer.len().min(8)]));
                    TransferHistoryDto {
                        token: t.token,
                        peer: t.peer,
                        peer_name,
                        direction: t.direction,
                        path: t.path,
                        size: t.size,
                        status: t.status,
                        detail: t.detail,
                        started_ms: t.started_ms,
                        finished_ms: t.finished_ms,
                    }
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_transfer_history(state: State<'_, FqState>) -> Result<usize, String> {
    state
        .app
        .clear_transfer_history()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_transfer_history(state: State<'_, FqState>, token: String) -> Result<bool, String> {
    state
        .app
        .delete_transfer_history(&token)
        .await
        .map_err(|e| e.to_string())
}

/// 取消进行中的传输。
#[tauri::command]
fn cancel_transfer(state: State<'_, FqState>, token: String) -> Result<bool, String> {
    Ok(state.app.cancel_transfer(&token))
}

#[tauri::command]
async fn list_conversations(state: State<'_, FqState>) -> Result<Vec<ConversationDto>, String> {
    state
        .app
        .conversations()
        .await
        .map(|convs| {
            convs
                .into_iter()
                .map(|c| ConversationDto {
                    peer: c.peer,
                    last_msg_ms: c.last_msg_ms,
                    preview: c.preview,
                    unread: c.unread,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mark_conversation_read(state: State<'_, FqState>, peer: String) -> Result<(), String> {
    state
        .app
        .mark_conversation_read(&peer)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn history_before(
    state: State<'_, FqState>,
    peer: String,
    before_ms: i64,
    limit: Option<u32>,
) -> Result<Vec<MessageDto>, String> {
    let limit = limit.unwrap_or(50).clamp(1, 200);
    let pending: std::collections::HashSet<String> = state
        .app
        .pending_ids(&peer)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    state
        .app
        .history_before(&peer, before_ms, limit)
        .await
        .map(|rows| rows.into_iter().map(|m| message_dto(m, &pending)).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn search_messages(
    state: State<'_, FqState>,
    query: String,
    limit: Option<u32>,
) -> Result<Vec<SearchHitDto>, String> {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.unwrap_or(80).clamp(1, 300);
    state
        .app
        .search(&query, limit)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|m| SearchHitDto {
                    peer: m.peer,
                    id: m.id,
                    outgoing: m.is_outgoing,
                    kind: m.kind,
                    body: m.body,
                    ts_ms: m.ts_ms,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn history(
    state: State<'_, FqState>,
    node_id: String,
    limit: Option<u32>,
) -> Result<Vec<MessageDto>, String> {
    let limit = limit.unwrap_or(100).clamp(1, 500);
    let pending: std::collections::HashSet<String> = state
        .app
        .pending_ids(&node_id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    state
        .app
        .history_by_key(&node_id, limit)
        .await
        .map(|rows| rows.into_iter().map(|m| message_dto(m, &pending)).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mark_read(state: State<'_, FqState>, node_id: String) -> Result<(), String> {
    let peer = parse_node_id(&node_id)?;
    state.app.mark_read(peer).await.map_err(|e| e.to_string())
}
