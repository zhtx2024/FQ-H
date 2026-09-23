//! feiqiu-r 命令行客户端(P6 MVP 验收入口)。
//!
//! # 用法
//!
//! **交互模式**(真实局域网,两台机器各跑一条,广播自动发现):
//! ```text
//! fq-cli chat --name 张三 --data-dir ./data-a
//! fq-cli chat --name 李四 --data-dir ./data-b --listen-port 24251   # 同机演示需错开 TCP 端口
//! ```
//!
//! **脚本模式**(自动化验收/冒烟,`--script` 逐行执行,EOF 视为 /quit):
//! ```text
//! /wait-peer 李四
//! /to 李四
//! 你好,飞秋重构版!
//! /file ./设计稿.zip
//! /sleep 2
//! /quit
//! ```
//!
//! # 命令一览
//! `/peers` 成员列表 · `/to <名>` 选会话 · 文本直发 · `/file <路径>` 发文件 ·
//! `/history [n]` 历史 · `/read` 已读回执 · `/wait-peer <名>` 等对端 ·
//! `/expect <子串>` 等消息 · `/expect-file <相对路径>` 等文件 ·
//! `/update [名|ID前缀]` 索取更新包 · `/sleep <秒>` · `/status` · `/quit`
//!
//! # 输出协议(供脚本/测试解析)
//! `[START]` `[PEER]` `[OFFLINE]` `[RECV]` `[SENT]` `[QUEUED]` `[DELIVERED]`
//! `[READ]` `[FILE]` `[TRUST-WARN]` `[ERROR]` `[OK]` `[FAIL]`
//!
//! 退出码:0 成功;1 启动失败或 `/expect*`、`/wait-peer` 超时。

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use fq_core::{App, AppConfig, AppEvent, SendOutcome};
use fq_net::NodeEvent;
use fq_proto::{AckStatus, Kind, NodeId};

/// 收到消息的共享日志((发送方名, 正文) 列表,供 /expect 检索)。
type ReceivedLog = Arc<Mutex<VecDeque<(String, String)>>>;

/// 初始化 stderr 日志(尊重 `RUST_LOG`;默认 `warn`,避免刷屏)。
///
/// 之前 CLI 完全没有日志订阅 → fq-net/fq-core 的日志全部被丢弃,排查只能用猜。
fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init();
}

fn main() -> std::process::ExitCode {
    init_logging();
    let cli = Cli::parse();
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        eprintln!("[ERROR] tokio 运行时创建失败");
        return std::process::ExitCode::from(1);
    };

    let exit_code = match cli.command {
        Command::Chat(args) => runtime.block_on(run_chat(*args)),
        Command::Id { data_dir } => runtime.block_on(async move {
            match fq_core::App::start(AppConfig::new(&data_dir, "探针")).await {
                Ok(app) => {
                    println!("node_id    = {}", app.node_id());
                    println!("fingerprint= {}", app.fingerprint());
                    println!("data_dir   = {}", app.data_dir().display());
                    app.shutdown();
                    0
                }
                Err(e) => {
                    eprintln!("[ERROR] {e}");
                    1
                }
            }
        }),
    };
    std::process::ExitCode::from(u8::try_from(exit_code).unwrap_or(1))
}

#[derive(Parser)]
#[command(
    name = "fq-cli",
    version,
    about = "飞秋(FeiQiu)现代化重构 - 命令行客户端"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动聊天客户端(交互 REPL,或 --script 脚本驱动)
    Chat(Box<ChatArgs>),
    /// 打印数据目录的身份信息(NodeId/指纹)
    Id {
        /// 数据目录
        #[arg(long, default_value = "feiqiu-data")]
        data_dir: PathBuf,
    },
}

#[derive(clap::Args)]
struct ChatArgs {
    /// 展示昵称
    #[arg(long, default_value = "匿名")]
    name: String,
    /// 分组
    #[arg(long)]
    group: Option<String>,
    /// 数据目录(身份/信任/数据库)
    #[arg(long, default_value = "feiqiu-data")]
    data_dir: PathBuf,
    /// UDP 发现绑定端口
    #[arg(long, default_value_t = fq_proto::DEFAULT_PORT)]
    discovery_port: u16,
    /// TCP 监听端口(同机多实例需错开)
    #[arg(long, default_value_t = fq_proto::DEFAULT_PORT)]
    listen_port: u16,
    /// 绑定地址
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,
    /// 广播目标(默认 255.255.255.255:发现端口;禁广播网络可指向可达单播)
    #[arg(long)]
    broadcast: Option<SocketAddr>,
    /// bootstrap 单播目标(可重复;发现直连退路/同机测试)
    #[arg(long = "bootstrap")]
    bootstraps: Vec<SocketAddr>,
    /// 接收文件保存目录(默认 数据目录/downloads)
    #[arg(long)]
    download_dir: Option<PathBuf>,
    /// 脚本文件:逐行执行命令,EOF 视为 /quit
    #[arg(long)]
    script: Option<PathBuf>,
    /// 心跳间隔毫秒(测试用;默认 20000)
    #[arg(long)]
    heartbeat_ms: Option<u64>,
    /// 对外宣称的版本号(仅测试/演示:用于验证自动更新把本机当成老版本)
    #[arg(long)]
    pretend_version: Option<String>,
    /// 对外提供的更新包路径(运维/测试用;默认本机可执行文件)
    #[arg(long)]
    serve_package: Option<PathBuf>,
}

async fn run_chat(args: ChatArgs) -> i32 {
    let mut config = AppConfig::new(args.data_dir.clone(), &args.name);
    config.group = args.group.clone();
    config.bind = args.bind;
    config.discovery_port = args.discovery_port;
    config.listen_port = args.listen_port;
    config.broadcast = args.broadcast.or_else(|| {
        Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::BROADCAST),
            args.discovery_port,
        ))
    });
    config.bootstrap = args.bootstraps.clone();
    config.download_dir = args.download_dir.clone();
    if let Some(ms) = args.heartbeat_ms {
        config.heartbeat_every = Duration::from_millis(ms.max(50));
    }
    // 测试/演示:冒充老版本节点(仅影响通告里的版本号,不改变二进制)
    if let Some(v) = args.pretend_version.clone() {
        println!("[WARN] 以版本 {v} 对外通告(仅测试/演示用)");
        config.app_version = Some(v);
    }
    if args.serve_package.is_some() {
        config.update_package = args.serve_package.clone();
    }

    let app = match App::start(config).await {
        Ok(app) => Arc::new(app),
        Err(e) => {
            eprintln!("[ERROR] 启动失败: {e}");
            return 1;
        }
    };

    println!(
        "[START] name={} id={} fingerprint={}",
        args.name,
        app.node_id(),
        app.fingerprint()
    );
    println!(
        "[START] data_dir={} discovery={}/tcp {}",
        args.data_dir.display(),
        args.discovery_port,
        args.listen_port
    );
    println!("[START] 输入 /peers 查看成员,/to <名> 选择会话,直接输入文本发送;/quit 退出");

    let received: ReceivedLog = Arc::new(Mutex::new(VecDeque::new()));
    let printer = tokio::spawn(event_printer(
        Arc::clone(&app),
        app.events(),
        Arc::clone(&received),
    ));

    let mut session: Option<NodeId> = None;
    let mut exit_code = 0;

    if let Some(path) = &args.script {
        // 脚本模式:逐行执行,EOF 视为 /quit
        let lines = match std::fs::read_to_string(path) {
            Ok(content) => content.lines().map(str::to_string).collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("[ERROR] 读取脚本失败: {e}");
                printer.abort();
                return 1;
            }
        };
        for line in lines {
            let (code, quit) = handle_line(&app, line.trim(), &mut session, &received, &args).await;
            exit_code = exit_code.max(code);
            if quit {
                break;
            }
        }
    } else {
        // 交互模式:主线程同步读 stdin。
        // 用的是多线程 runtime,事件打印与网络任务在其它工作线程上继续运转。
        loop {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let (code, quit) =
                        handle_line(&app, line.trim(), &mut session, &received, &args).await;
                    exit_code = exit_code.max(code);
                    if quit {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("[ERROR] 读取输入失败: {e}");
                    break;
                }
            }
        }
    }

    printer.abort();
    // 释放 Arc 以便消费 shutdown
    match Arc::try_unwrap(app) {
        Ok(app) => app.shutdown(),
        Err(_) => {
            // 打印任务仍持有引用(已被 abort,很快释放);不影响数据落盘
        }
    }
    exit_code
}

/// 处理一行输入,返回 (退出码增量, 是否退出)。
async fn handle_line(
    app: &App,
    line: &str,
    session: &mut Option<NodeId>,
    received: &ReceivedLog,
    args: &ChatArgs,
) -> (i32, bool) {
    if line.is_empty() {
        return (0, false);
    }
    if let Some(rest) = line.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or_default();
        let arg = parts.next().map(str::trim).unwrap_or_default();
        match cmd {
            "quit" | "exit" => {
                println!("[OK] 退出");
                return (0, true);
            }
            "peers" => match app.peers().await {
                Ok(peers) if peers.is_empty() => println!("[PEERS] (空,等待发现…)"),
                Ok(peers) => {
                    for (index, peer) in peers.iter().enumerate() {
                        println!(
                            "[PEER] #{} {} id={} online={} group={:?}",
                            index + 1,
                            peer.display_name,
                            peer.node_id,
                            peer.online,
                            peer.group.clone().unwrap_or_else(|| "-".into())
                        );
                    }
                }
                Err(e) => eprintln!("[ERROR] {e}"),
            },
            "to" => match resolve_peer(app, arg).await {
                Some((id, name)) => {
                    println!("[OK] 当前会话 → {name} ({id})");
                    *session = Some(id);
                }
                None => eprintln!("[ERROR] 找不到对端: {arg:?}(先 /peers 查看)"),
            },
            "msg" => {
                if let Err(code) = send_text(app, *session, arg).await {
                    return (code, false);
                }
            }
            "file" => {
                let Some(to) = *session else {
                    eprintln!("[ERROR] 先用 /to 选择会话");
                    return (0, false);
                };
                match app.send_file(to, arg, None).await {
                    Ok(token) => println!("[FILE] 开始发送 {arg}(token={token})"),
                    Err(e) => {
                        eprintln!("[ERROR] 发送失败: {e}");
                        return (1, false);
                    }
                }
            }
            "history" => {
                let Some(to) = *session else {
                    eprintln!("[ERROR] 先用 /to 选择会话");
                    return (0, false);
                };
                let limit = arg.parse().unwrap_or(20u32);
                match app.history(to, limit).await {
                    Ok(rows) if rows.is_empty() => println!("[HISTORY] (无记录)"),
                    Ok(rows) => {
                        for row in rows {
                            let mark = match (row.delivered_ms, row.read_ms) {
                                (Some(_), Some(_)) => "✓✓",
                                (Some(_), None) => "✓ ",
                                _ => "  ",
                            };
                            let who = if row.is_outgoing { "我" } else { "对方" };
                            println!(
                                "[HISTORY] {} {who} {}",
                                mark,
                                row.body.unwrap_or_else(|| format!("<{}>", row.kind))
                            );
                        }
                    }
                    Err(e) => eprintln!("[ERROR] {e}"),
                }
            }
            "read" => {
                let Some(to) = *session else {
                    eprintln!("[ERROR] 先用 /to 选择会话");
                    return (0, false);
                };
                let _ = app.mark_read(to).await;
                println!("[OK] 已发送已读回执");
            }
            "status" => {
                let pending = app.pending_count().await.unwrap_or(0);
                println!(
                    "[STATUS] id={} fingerprint={} 待发={}",
                    app.node_id(),
                    app.fingerprint(),
                    pending
                );
            }
            "update" => {
                // 向版本更高的对端索取更新包(对端以 UpdateOffer 回发,自动接收)
                let target = if arg.is_empty() {
                    *session
                } else {
                    resolve_peer(app, arg).await.map(|(id, _)| id)
                };
                match target {
                    Some(id) => match app.request_update(id).await {
                        Ok(()) => println!("[UPDATE] 已向 {id} 索取更新包,等待对方回发…"),
                        Err(e) => {
                            eprintln!("[ERROR] 索取更新包失败: {e}");
                            return (1, false);
                        }
                    },
                    None => {
                        eprintln!("[ERROR] 未指定对端:用 /update <名|ID前缀>,或先 /to 选会话");
                    }
                }
            }
            "wait-peer" => {
                if wait_peer(app, arg, Duration::from_secs(120)).await {
                    println!("[OK] 对端已出现: {arg}");
                } else {
                    eprintln!("[FAIL] 等待对端超时: {arg}");
                    return (1, false);
                }
            }
            "expect" => {
                if wait_message(received, arg, Duration::from_secs(60)) {
                    println!("[OK] 收到包含 {arg:?} 的消息");
                } else {
                    eprintln!("[FAIL] 等待消息超时: {arg:?}");
                    return (1, false);
                }
            }
            "expect-file" => {
                let dir = args
                    .download_dir
                    .clone()
                    .unwrap_or_else(|| args.data_dir.join("downloads"));
                let target = dir.join(arg);
                if wait_file(&target, Duration::from_secs(90)) {
                    println!("[OK] 文件已就位: {}", target.display());
                } else {
                    eprintln!("[FAIL] 等待文件超时: {}", target.display());
                    return (1, false);
                }
            }
            "sleep" => {
                let secs: f64 = arg.parse().unwrap_or(1.0);
                tokio::time::sleep(Duration::from_millis((secs * 1000.0) as u64)).await;
            }
            other => eprintln!("[ERROR] 未知命令: /{other}"),
        }
    } else if let Err(code) = send_text(app, *session, line).await {
        return (code, false);
    }
    let _ = std::io::stdout().flush();
    (0, false)
}

async fn send_text(app: &App, session: Option<NodeId>, text: &str) -> Result<(), i32> {
    let Some(to) = session else {
        eprintln!("[ERROR] 先用 /to 选择会话");
        return Err(0);
    };
    match app.send_text(to, text).await {
        Ok(outcome) => match outcome {
            SendOutcome::Sent(_) => {
                println!("[SENT] {text}");
            }
            SendOutcome::Queued(_) => {
                println!("[QUEUED] 对端不在线,已入队待补发: {text}");
            }
        },
        Err(e) => {
            eprintln!("[ERROR] {e}");
            return Err(1);
        }
    }
    Ok(())
}

/// 按昵称前缀或 NodeId hex 前缀解析对端。
async fn resolve_peer(app: &App, prefix: &str) -> Option<(NodeId, String)> {
    if prefix.is_empty() {
        return None;
    }
    let peers = app.peers().await.ok()?;
    peers
        .iter()
        .find(|p| p.display_name.contains(prefix) || p.node_id.to_hex().starts_with(prefix))
        .map(|p| (p.node_id, p.display_name.clone()))
}

async fn wait_peer(app: &App, prefix: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if resolve_peer(app, prefix).await.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn wait_message(received: &ReceivedLog, substr: &str, timeout: Duration) -> bool {
    let start = received.lock().map(|log| log.len()).unwrap_or(0);
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let hit = received
            .lock()
            .map(|log| {
                log.iter()
                    .skip(start)
                    .any(|(_, body)| body.contains(substr))
            })
            .unwrap_or(false);
        if hit {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn wait_file(target: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if target.is_file() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// 事件打印任务:把 AppEvent 转成带前缀的可解析行。
async fn event_printer(
    app: Arc<App>,
    mut events: tokio::sync::broadcast::Receiver<AppEvent>,
    received: ReceivedLog,
) {
    loop {
        let Ok(event) = events.recv().await else {
            return;
        };
        match event {
            AppEvent::Node(inner) => match *inner {
                NodeEvent::MessageReceived { from, envelope } => match envelope.kind {
                    Kind::Text(body) => {
                        let name = peer_display_name(&app, from).await;
                        println!("[RECV] {name}: {}", body.body);
                        if let Ok(mut log) = received.lock() {
                            log.push_back((name, body.body));
                            while log.len() > 256 {
                                log.pop_front();
                            }
                        }
                    }
                    Kind::Ack(ack) => match ack.status {
                        AckStatus::Delivered => {
                            println!("[DELIVERED] {} 已送达", short_id(ack.ack_id));
                        }
                        AckStatus::Read => {
                            println!("[READ] {} 已读", short_id(ack.ack_id));
                        }
                        _ => {}
                    },
                    _ => {}
                },
                NodeEvent::PeerDiscovered {
                    peer,
                    first_contact,
                } => {
                    println!(
                        "[PEER] 上线: {} ({}){}",
                        peer.display_name,
                        peer.node_id,
                        if first_contact {
                            " [首次接触,指纹已固定]"
                        } else {
                            ""
                        }
                    );
                }
                NodeEvent::PeerLost { node_id } => {
                    println!("[OFFLINE] {node_id}");
                }
                NodeEvent::TrustWarning {
                    node_id,
                    pinned,
                    presented,
                } => {
                    eprintln!(
                        "[TRUST-WARN] {node_id} 静态密钥变更!原指纹 {pinned} → 新指纹 {presented}(可能是重装,也可能是中间人;请当面核实)"
                    );
                }
                NodeEvent::FileOfferReceived {
                    from,
                    token,
                    manifest,
                    ..
                } => {
                    println!(
                        "[FILE] 收到来自 {} 的传输 {token}:{} 个条目,共 {} 字节(自动接受)",
                        peer_display_name(&app, from).await,
                        manifest.entries.len(),
                        manifest.total_bytes
                    );
                }
                NodeEvent::FileProgress {
                    direction,
                    token,
                    path,
                    transferred,
                    total,
                    ..
                } => {
                    let arrow = if direction == fq_net::TransferDirection::Sending {
                        "↑"
                    } else {
                        "↓"
                    };
                    println!("[FILE] {arrow} {path} {transferred}/{total} ({token})");
                }
                NodeEvent::FileEntryDone {
                    direction,
                    path,
                    verified,
                    sha256,
                    ..
                } => {
                    let arrow = if direction == fq_net::TransferDirection::Sending {
                        "↑"
                    } else {
                        "↓"
                    };
                    println!(
                        "[FILE] {arrow} {path} 完成(校验{},{sha256:.12}…)",
                        if verified {
                            "通过"
                        } else {
                            "由接收方核对"
                        }
                    );
                }
                NodeEvent::FileTransferCompleted {
                    direction, token, ..
                } => {
                    let arrow = if direction == fq_net::TransferDirection::Sending {
                        "↑"
                    } else {
                        "↓"
                    };
                    println!("[FILE] {arrow} 传输完成 ({token})");
                }
                NodeEvent::FileTransferFailed {
                    direction,
                    token,
                    path,
                    reason,
                    ..
                } => {
                    let arrow = if direction == fq_net::TransferDirection::Sending {
                        "↑"
                    } else {
                        "↓"
                    };
                    eprintln!("[FILE] {arrow} 传输失败 ({token}) path={path:?} 原因: {reason}");
                }
                other => {
                    let _ = other;
                }
            },
            AppEvent::MessageSaved { .. } | AppEvent::QueueFlushed { .. } => {}
            AppEvent::PeerAvatar { node_id, sha256 } => {
                eprintln!(
                    "[AVATAR] 已缓存 {node_id} 的头像({}…)",
                    &sha256[..sha256.len().min(8)]
                );
            }
            AppEvent::PeerAvatarRemoved { node_id } => {
                eprintln!("[AVATAR] {node_id} 已移除头像,本地缓存清理");
            }
            AppEvent::Shaken { from } => {
                eprintln!("[SHAKE] {from} 抖了你一下");
            }
            AppEvent::UpdateReady {
                from,
                version,
                path,
            } => {
                eprintln!("[UPDATE] 更新包 v{version} 已下载并校验通过(来自 {from})");
                eprintln!("[UPDATE] 文件: {path}");
                eprintln!("[UPDATE] 桌面端会提示一键重启安装;CLI 下可手动替换可执行文件。");
            }
        }
    }
}

async fn peer_display_name(app: &App, id: NodeId) -> String {
    app.peers()
        .await
        .ok()
        .and_then(|peers| {
            peers
                .iter()
                .find(|p| p.node_id == id)
                .map(|p| p.display_name.clone())
        })
        .unwrap_or_else(|| id.to_hex())
}

fn short_id(id: fq_proto::MsgId) -> String {
    id.to_string().chars().take(8).collect()
}
