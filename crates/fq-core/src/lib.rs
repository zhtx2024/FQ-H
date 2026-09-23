//! # fq-core
//!
//! 应用核心:把网络层([`fq_net`])、安全层([`fq_crypto`])、存储层([`fq_store`])
//! 装配成一个可直接驱动 CLI/GUI 的 [`App`]。
//!
//! ## 职责
//!
//! * **身份装配**:首次启动生成并持久化身份(Ed25519)、Noise 静态密钥、TOFU 表;
//!   重启后三者原样恢复 —— NodeId 稳定、同伴不受密钥变更告警
//! * **事件泵**:消费 [`fq_net::NodeEvent`] 流,完成
//!   收到的文本入库 → 自动回"送达"回执;收到回执 → 更新消息状态;
//!   对端上线 → 冲洗离线待发队列
//! * **离线补发**:对端不可达时消息进入 SQLite 队列(跨重启),
//!   对端重新出现时自动按序补发
//! * **会话查询**:历史消息、联系人(含在线状态合并)
//!
//! ## 数据目录布局
//!
//! ```text
//! data_dir/
//! ├─ identity.json   身份种子(见 fq_crypto::persist 的明文限制说明)
//! ├─ static.json     Noise 静态密钥
//! ├─ tofu.json       信任固定表
//! └─ fq.db           消息/联系人/待发队列(SQLite, WAL)
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fq_net::{Node, NodeEvent, NodeHandle};
use fq_proto::{
    AckBody, AckStatus, Envelope, Kind, MsgId, NodeId, ShakeBody, TextBody, TextFormat, codec,
    now_ms,
};
use fq_store::{GroupRecord, NewMessage, PeerRecord, Store, StoredMessage};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

pub mod error;

pub use error::{Error, Result};

/// 应用层事件(UI 订阅这个,而不是直接订阅网络层)。
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// 网络层事件透传(进度/信任告警/对端上下线等)。
    /// 装箱:NodeEvent 含清单等大字段,避免拉大其余变体的体积。
    Node(Box<NodeEvent>),
    /// 一条消息已写入历史(收到的或发出的)。
    MessageSaved {
        /// 会话对方。
        peer: NodeId,
        /// 是否本端发出。
        outgoing: bool,
        /// 消息 ID。
        id: MsgId,
    },
    /// 离线队列冲洗完成。
    QueueFlushed {
        /// 补发目标。
        to: NodeId,
        /// 补发条数。
        count: usize,
    },
    /// 更新包已下载并通过校验,可以安装(UI 应提示并调用 [`crate::App::install_update_and_restart`])。
    UpdateReady {
        /// 提供方。
        from: NodeId,
        /// 更新版本。
        version: String,
        /// 更新包落盘路径。
        path: String,
    },
    /// 已缓存某对端的新头像(UI 应重新拉取展示)。
    PeerAvatar {
        /// 对端。
        node_id: NodeId,
        /// 头像哈希。
        sha256: String,
    },
    /// 某对端已移除头像(UI 应清掉本地展示)。
    PeerAvatarRemoved {
        /// 对端。
        node_id: NodeId,
    },
    /// 收到窗口抖动(UI 应晃动窗口并留下一条记录)。
    Shaken {
        /// 发起方。
        from: NodeId,
    },
}

/// 发送结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// 已发出(进入加密连接)。
    Sent(MsgId),
    /// 对端不可达,已入队待补发。
    Queued(MsgId),
}

impl SendOutcome {
    /// 消息 ID。
    pub fn message_id(self) -> MsgId {
        match self {
            SendOutcome::Sent(id) | SendOutcome::Queued(id) => id,
        }
    }

    /// 是否已入队。
    pub fn is_queued(self) -> bool {
        matches!(self, SendOutcome::Queued(_))
    }
}

/// 联系人摘要(存储记录 + 实时在线状态合并)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSummary {
    /// NodeId。
    pub node_id: NodeId,
    /// 展示名。
    pub display_name: String,
    /// 分组。
    pub group: Option<String>,
    /// 当前是否在线(实时)。
    pub online: bool,
    /// 最近见面(Unix 毫秒)。
    pub last_seen_ms: i64,
    /// 对端可达端点(IP:port)。
    pub endpoints: Vec<SocketAddr>,
    /// 对端软件版本。
    pub app_version: Option<String>,
    /// 对端头像哈希(未设置头像时为 None)。
    pub avatar_sha256: Option<String>,
}

/// 应用配置。
#[derive(Debug)]
pub struct AppConfig {
    /// 数据目录(身份/信任/数据库都放这里)。
    pub data_dir: PathBuf,
    /// 展示昵称。
    pub display_name: String,
    /// 分组。
    pub group: Option<String>,
    /// 绑定地址(默认全网卡)。
    pub bind: IpAddr,
    /// UDP 发现端口。
    pub discovery_port: u16,
    /// TCP 监听端口。
    pub listen_port: u16,
    /// 广播目标(默认 255.255.255.255:发现端口)。
    pub broadcast: Option<SocketAddr>,
    /// bootstrap 单播目标(禁广播网络/测试用)。
    pub bootstrap: Vec<SocketAddr>,
    /// 心跳间隔。
    pub heartbeat_every: Duration,
    /// 对端心跳超时。
    pub peer_timeout: Duration,
    /// 接收文件保存目录(默认 数据目录/downloads)。
    pub download_dir: Option<PathBuf>,
    /// 自动接受文件传输(CLI 无人值守 = true;桌面 UI 设 false 走接收确认流)。
    pub auto_accept_files: bool,
    /// 软件版本(随通告广播,用于局域网版本发现;CLI 传 env!("CARGO_PKG_VERSION"))。
    pub app_version: Option<String>,
    /// 对外提供的更新包路径(默认本机可执行文件;运维/测试可指定安装包)。
    pub update_package: Option<PathBuf>,
    /// 发送方向限速(字节/秒;0 = 不限速)。
    pub send_limit_bytes: u64,
    /// 初始在线状态(在线/忙碌/勿扰/离开)。
    pub status: fq_proto::PresenceStatus,
}

impl AppConfig {
    /// 最小配置,端口用协议默认值。
    pub fn new(data_dir: impl Into<PathBuf>, display_name: &str) -> Self {
        Self {
            data_dir: data_dir.into(),
            display_name: display_name.to_string(),
            group: None,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            discovery_port: fq_proto::DEFAULT_PORT,
            listen_port: fq_proto::DEFAULT_PORT,
            broadcast: None,
            bootstrap: Vec::new(),
            heartbeat_every: Duration::from_secs(20),
            peer_timeout: Duration::from_secs(60),
            download_dir: None,
            auto_accept_files: true,
            app_version: None,
            update_package: None,
            send_limit_bytes: 0,
            status: fq_proto::PresenceStatus::Online,
        }
    }
}

/// 运行中的应用。
#[derive(Debug)]
pub struct App {
    handle: NodeHandle,
    node: Option<Node>,
    store: Arc<Mutex<Store>>,
    events: broadcast::Sender<AppEvent>,
    pump: JoinHandle<()>,
    data_dir: PathBuf,
    /// 对外提供的更新包路径(默认本机可执行文件)。
    update_package: Option<PathBuf>,
    /// 本机头像文件路径(`<数据目录>/avatar.png`)。
    avatar_path: PathBuf,
}

impl App {
    /// 启动应用:加载/创建身份与密钥,打开数据库,启动网络节点与事件泵。
    pub async fn start(config: AppConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|e| Error::Start(format!("创建数据目录失败: {e}")))?;

        let identity_path = config.data_dir.join("identity.json");
        let static_path = config.data_dir.join("static.json");
        let tofu_path = config.data_dir.join("tofu.json");
        let db_path = config.data_dir.join("fq.db");

        let identity = fq_crypto::load_or_create_identity(&identity_path)?;
        let static_keys = fq_crypto::load_or_create_static_keys(&static_path)?;
        let tofu = fq_crypto::load_tofu(&tofu_path)?;
        let store = Store::open(&db_path)?;
        // 上次进程残留的进行中传输 → 标记为中断(避免历史里永远"传输中")
        let _ = store.mark_stale_transfers();

        // 本机头像:固定文件名 `avatar.png`(桌面端负责压缩成 256×256 PNG)
        let avatar_path = config.data_dir.join("avatar.png");
        let avatar_sha256 = read_avatar(Some(&avatar_path)).map(|(hash, _)| hash);

        let mut node_config =
            fq_net::NodeConfig::new(identity.clone(), static_keys, &config.display_name);
        node_config.group = config.group.clone();
        node_config.discovery_bind = SocketAddr::new(config.bind, config.discovery_port);
        node_config.listen_addr = SocketAddr::new(config.bind, config.listen_port);
        node_config.broadcast_addr = config.broadcast.unwrap_or_else(|| {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), config.discovery_port)
        });
        node_config.bootstrap = config.bootstrap.clone();
        node_config.heartbeat_every = config.heartbeat_every;
        node_config.peer_timeout = config.peer_timeout;
        node_config.tofu = tofu;
        node_config.download_dir = config
            .download_dir
            .unwrap_or_else(|| config.data_dir.join("downloads"));
        node_config.auto_accept_files = config.auto_accept_files;
        node_config.app_version = config.app_version.clone();
        node_config.avatar_sha256 = avatar_sha256;
        node_config.send_limit_bytes = config.send_limit_bytes;
        node_config.status = config.status;

        let node = Node::start(node_config).await?;
        let handle = node.handle().clone();

        let store = Arc::new(Mutex::new(store));
        let (events, _) = broadcast::channel(1024);
        // 对外提供的更新包:配置指定 > 本机可执行文件(启动时解析一次,失败则功能不可用)
        let update_package = config
            .update_package
            .clone()
            .or_else(|| std::env::current_exe().ok());
        let pump = tokio::spawn(event_pump(
            handle.clone(),
            Arc::clone(&store),
            events.clone(),
            tofu_path,
            update_package,
            Some(avatar_path.clone()),
        ));

        Ok(Self {
            handle,
            node: Some(node),
            store,
            events,
            pump,
            data_dir: config.data_dir,
            update_package: config.update_package,
            avatar_path,
        })
    }

    /// 本端 NodeId(跨重启稳定)。
    pub fn node_id(&self) -> NodeId {
        self.handle.node_id()
    }

    /// 本端静态密钥指纹(UI 展示用)。
    pub fn fingerprint(&self) -> String {
        self.handle.fingerprint()
    }

    /// 网络句柄(高级用法:自定义报文、断开连接等)。
    pub fn node(&self) -> &NodeHandle {
        &self.handle
    }

    /// 数据目录。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 订阅应用事件。
    pub fn events(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    /// 发送文本:对端可达即发,不可达则入队待补发。
    pub async fn send_text(&self, to: NodeId, body: &str) -> Result<SendOutcome> {
        let envelope = Envelope::direct(
            self.node_id(),
            to,
            Kind::Text(TextBody {
                body: body.to_string(),
                format: TextFormat::Plain,
                reply_to: None,
                mentions: vec![],
                group_id: None,
                group_name: None,
            }),
        );
        let id = envelope.id;
        match self.handle.send(envelope.clone()).await {
            Ok(()) => {
                self.store
                    .lock()
                    .await
                    .insert_message(&new_message(&envelope, true))
                    .map_err(Error::Store)?;
                let _ = self.events.send(AppEvent::MessageSaved {
                    peer: to,
                    outgoing: true,
                    id,
                });
                Ok(SendOutcome::Sent(id))
            }
            // 网络层不可达(未发现/离线/拨号失败)→ 入队,等对端上线补发
            Err(_) => {
                let blob = codec::encode(&envelope).map_err(Error::Proto)?;
                self.store
                    .lock()
                    .await
                    .enqueue(&id.to_string(), &to.to_hex(), &blob, now_ms())
                    .map_err(Error::Store)?;
                Ok(SendOutcome::Queued(id))
            }
        }
    }

    /// 发送窗口抖动(飞秋经典功能:提醒对方注意;对方会晃动窗口并留下一条记录)。
    ///
    /// 与文本一样走"可达即发、不可达入队"的路径,保证对端上线后仍能收到。
    pub async fn send_shake(&self, to: NodeId) -> Result<SendOutcome> {
        let envelope =
            Envelope::direct(self.node_id(), to, Kind::Shake(ShakeBody { reason: None }));
        let id = envelope.id;
        match self.handle.send(envelope.clone()).await {
            Ok(()) => {
                self.store
                    .lock()
                    .await
                    .insert_message(&new_message(&envelope, true))
                    .map_err(Error::Store)?;
                let _ = self.events.send(AppEvent::MessageSaved {
                    peer: to,
                    outgoing: true,
                    id,
                });
                Ok(SendOutcome::Sent(id))
            }
            Err(_) => {
                let blob = codec::encode(&envelope).map_err(Error::Proto)?;
                self.store
                    .lock()
                    .await
                    .enqueue(&id.to_string(), &to.to_hex(), &blob, now_ms())
                    .map_err(Error::Store)?;
                Ok(SendOutcome::Queued(id))
            }
        }
    }

    /// 本地删除一条历史消息(只删本机;对端不受影响)。
    pub async fn delete_message(&self, id: &str) -> Result<bool> {
        self.store
            .lock()
            .await
            .delete_message(id)
            .map_err(Error::Store)
    }

    /// 设置在线状态(在线/忙碌/勿扰/离开)并立即通告。
    pub async fn set_status(&self, status: fq_proto::PresenceStatus) {
        self.handle.set_status(status).await;
    }

    /// 当前在线状态。
    pub fn status(&self) -> fq_proto::PresenceStatus {
        self.handle.status()
    }

    /// 设置发送方向限速(字节/秒;0 = 不限速)。
    pub fn set_send_limit(&self, bytes_per_sec: u64) {
        self.handle.set_send_limit(bytes_per_sec);
    }

    /// 向指定地址定向通告一次(手动探测:对方首次见我们会立即回发,从而互相发现)。
    pub async fn announce_to(&self, target: SocketAddr) {
        self.handle.announce_to(target).await;
    }

    /// 取消一个进行中的传输(发送中断分块流 / 接收中断并保留 .part)。
    pub fn cancel_transfer(&self, token: &str) -> bool {
        self.handle.cancel_transfer(token)
    }

    // ─────────────── 自动更新 ───────────────

    /// 向对端索取更新包(对端版本更高时会回发 UpdateOffer,自动接收)。
    pub async fn request_update(&self, to: NodeId) -> Result<()> {
        self.handle.request_update(to).await.map_err(Error::Net)
    }

    /// 提供本机安装包给对端(供更新请求响应;本机可执行文件或配置指定的包)。
    pub async fn serve_update_package(&self, to: NodeId) -> Result<String> {
        let exe = self
            .update_package
            .clone()
            .ok_or_else(|| Error::Start("无法定位本机可执行文件,无法提供更新包".into()))?;
        let version = self.app_version().to_string();
        self.handle
            .send_update_package(to, exe, version)
            .await
            .map_err(Error::Net)
    }

    /// 本机软件版本。
    pub fn app_version(&self) -> &str {
        APP_VERSION
    }

    // ─────────────── 头像 ───────────────

    /// 本机头像文件路径(桌面端写入压缩后的 PNG)。
    pub fn avatar_path(&self) -> &std::path::Path {
        &self.avatar_path
    }

    /// 重新加载本机头像:重算哈希、重新通告,并把新头像**推给在线对端**。
    ///
    /// 推送很关键:否则"另一方拉取"要靠 15s 兜底延时才能拿到(踩过坑)。
    /// 返回新哈希(无头像时 `None`)。
    pub async fn reload_avatar(&self) -> Option<String> {
        let hash = read_avatar(Some(&self.avatar_path)).map(|(hash, _)| hash);
        self.handle.set_avatar_hash(hash.clone()).await;
        self.push_avatar().await;
        hash
    }

    /// 移除本机头像(删除文件 + 通告 + 通知对端清缓存)。
    pub async fn clear_avatar(&self) -> Result<()> {
        if self.avatar_path.exists() {
            std::fs::remove_file(&self.avatar_path)
                .map_err(|e| Error::Start(format!("删除头像文件失败: {e}")))?;
        }
        self.handle.set_avatar_hash(None).await;
        self.push_avatar().await;
        Ok(())
    }

    /// 把本机头像推送给所有在线且支持头像的对端,返回推送成功数。
    ///
    /// 复用 `avatar_request`:`mine` 捎上本机头像,对方据此直接缓存;
    /// `known_sha256` 仍传"我们缓存的对方头像哈希",对方无需回发时就不回发。
    pub async fn push_avatar(&self) -> usize {
        let mine = local_avatar_payload(Some(&self.avatar_path));
        let mut sent = 0;
        for peer in self.handle.peers() {
            if peer.status == fq_proto::PresenceStatus::Offline
                || !peer.capabilities.contains(fq_proto::Capabilities::AVATAR)
            {
                continue;
            }
            let cached = self
                .store
                .lock()
                .await
                .peer_avatar_hash(&peer.node_id.to_hex())
                .unwrap_or(None);
            if self
                .handle
                .request_avatar(peer.node_id, cached, mine.clone())
                .await
                .is_ok()
            {
                sent += 1;
            }
        }
        sent
    }

    /// 本机头像原始字节(PNG)。
    pub fn local_avatar(&self) -> Option<Vec<u8>> {
        read_avatar(Some(&self.avatar_path)).map(|(_, bytes)| bytes)
    }

    /// 某对端已缓存的头像(哈希 + MIME + 字节)。
    pub async fn peer_avatar(&self, node_id: &str) -> Result<Option<(String, String, Vec<u8>)>> {
        self.store
            .lock()
            .await
            .peer_avatar(node_id)
            .map_err(Error::Store)
    }

    /// 删除一条最近会话(只从"最近会话"移除,聊天记录与联系人保留)。
    pub async fn delete_conversation(&self, peer: &str) -> Result<bool> {
        self.store
            .lock()
            .await
            .delete_conversation(peer)
            .map_err(Error::Store)
    }

    /// 安装已下载并校验通过的更新包,然后重启应用。
    ///
    /// Windows 上运行中的 exe 文件被锁定,无法直接覆盖 —— 采用标准做法:
    /// 生成一个延迟批处理脚本(等待本进程退出 → 备份旧版 → 覆盖 → 重新启动 → 自删),
    /// 由 `cmd` 脱离当前进程执行。
    ///
    /// 安全约束:只接受**下载目录内**的 `.exe` 且校验 PE 魔数 ——
    /// 即便 UI 被注入,也无法用它替换任意文件。
    pub fn install_update_and_restart(&self, package_path: &str) -> Result<()> {
        let current_exe = std::env::current_exe()
            .map_err(|e| Error::Start(format!("无法定位本机可执行文件: {e}")))?;
        let package = std::path::PathBuf::from(package_path);
        if !package.is_file() {
            return Err(Error::Start(format!("更新包不存在: {package_path}")));
        }

        // ① 必须在下载目录内(规范化后比较,防 `..` 绕行)
        let download_dir = std::fs::canonicalize(self.download_dir())
            .map_err(|e| Error::Start(format!("下载目录不可用: {e}")))?;
        let package_real = std::fs::canonicalize(&package)
            .map_err(|e| Error::Start(format!("更新包路径不可用: {e}")))?;
        if !package_real.starts_with(&download_dir) {
            return Err(Error::Start(format!(
                "拒绝安装下载目录之外的文件: {}",
                package_real.display()
            )));
        }

        // ② 必须是 .exe 且带 PE 魔数
        let is_exe = package_real
            .extension()
            .map(|ext| ext.eq_ignore_ascii_case("exe"))
            .unwrap_or(false);
        if !is_exe {
            return Err(Error::Start("更新包必须是 .exe".into()));
        }
        // ③ 文件名校验:更新包必须是**同类应用**(与本机可执行文件同名)。
        // 防止局域网里运行其他程序(如构建产物 fq-cli.exe)的节点把本机覆盖成别的程序。
        let local_name = current_exe
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let package_name = package_real
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if package_name != local_name {
            return Err(Error::Start(format!(
                "更新包文件名 {package_name} 与本机程序 {local_name} 不一致,拒绝安装"
            )));
        }
        let mut head = [0u8; 2];
        {
            use std::io::Read;
            let mut file = std::fs::File::open(&package_real)
                .map_err(|e| Error::Start(format!("读取更新包失败: {e}")))?;
            file.read_exact(&mut head)
                .map_err(|e| Error::Start(format!("更新包不完整: {e}")))?;
        }
        if &head != b"MZ" {
            return Err(Error::Start("更新包不是有效的可执行文件".into()));
        }

        #[cfg(target_os = "windows")]
        {
            let script_path =
                std::env::temp_dir().join(format!("fq-update-{}.cmd", MsgId::now_v7()));
            let script = build_update_script(&package_real, &current_exe);
            std::fs::write(&script_path, script)
                .map_err(|e| Error::Start(format!("写入更新脚本失败: {e}")))?;

            std::process::Command::new("cmd")
                .args(["/C", "start", "", "/MIN"])
                .arg(&script_path)
                .spawn()
                .map_err(|e| Error::Start(format!("启动更新脚本失败: {e}")))?;

            tracing::info!(target = "fq_core", script = %script_path.display(), "已启动更新脚本,进程即将退出");
            std::process::exit(0);
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = (package_real, current_exe);
            Err(Error::Start("当前平台暂不支持自动安装".into()))
        }
    }

    /// 传输历史(按开始时间倒序)。
    pub async fn transfer_history(&self, limit: u32) -> Result<Vec<fq_store::TransferRecord>> {
        self.store
            .lock()
            .await
            .list_transfers(limit)
            .map_err(Error::Store)
    }

    /// 清空传输历史(不影响进行中的传输)。
    pub async fn clear_transfer_history(&self) -> Result<usize> {
        self.store
            .lock()
            .await
            .clear_transfers()
            .map_err(Error::Store)
    }

    /// 删除单条传输历史。
    pub async fn delete_transfer_history(&self, token: &str) -> Result<bool> {
        self.store
            .lock()
            .await
            .delete_transfer(token)
            .map_err(Error::Store)
    }

    /// 发送文件/目录(直发,不做离线队列);成功后往聊天历史里插一条 file 消息。
    pub async fn send_file(
        &self,
        to: NodeId,
        path: impl Into<PathBuf>,
        message: Option<String>,
    ) -> Result<String> {
        let path = path.into();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "文件".into());
        let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let is_image = is_image_extension(&file_name);

        let token = self
            .handle
            .send_file(to, path.clone(), message)
            .await
            .map_err(Error::Net)?;

        // 传输历史:记录开始(结束由事件泵更新)
        let _ = self.store.lock().await.record_transfer_start(
            &token,
            &to.to_hex(),
            "send",
            &file_name,
            file_size,
            now_ms(),
        );

        // 插入聊天历史(文件条目)
        let body = serde_json::json!({
            "n": file_name,
            "s": file_size,
            "p": path.display().to_string(),
            "i": is_image,
        });
        let envelope_id = fq_proto::MsgId::now_v7();
        let saved = NewMessage {
            id: envelope_id.to_string(),
            peer: to.to_hex(),
            is_outgoing: true,
            from_node: self.node_id().to_hex(),
            kind: if is_image { "image" } else { "file" }.into(),
            body: Some(body.to_string()),
            format: None,
            ts_ms: fq_proto::now_ms(),
            created_ms: fq_proto::now_ms(),
        };
        if let Err(e) = self.store.lock().await.insert_message(&saved) {
            tracing::warn!(target = "fq_core", %e, "文件消息入库失败");
        }

        Ok(token)
    }

    /// 同意接收一个文件要约;`dir` 为本次保存位置(可选,缺省用全局设置)。
    ///
    /// 返回 false = 会话不存在或已结束。
    pub fn accept_file_offer(&self, token: &str, dir: Option<PathBuf>) -> bool {
        self.handle.accept_file_offer(token, dir)
    }

    /// 拒绝接收一个文件要约;发送方会收到中止事件。
    pub fn reject_file_offer(&self, token: &str) -> bool {
        self.handle.reject_file_offer(token)
    }

    /// 当前接收文件保存目录。
    pub fn download_dir(&self) -> PathBuf {
        self.handle.download_dir()
    }

    /// 运行期修改接收文件保存目录(对后续开始接收的传输生效)。
    pub fn set_download_dir(&self, dir: PathBuf) {
        self.handle.set_download_dir(dir);
    }

    /// 修改昵称与分组并立即通告(对方 `[PEER]` 更新)。
    pub async fn set_profile(&self, display_name: &str, group: Option<&str>) {
        self.handle.set_profile(display_name, group).await;
    }

    /// 当前昵称与分组。
    pub fn profile(&self) -> (String, Option<String>) {
        self.handle.profile_snapshot()
    }

    // ─────────────── 群聊(LAN 多播扇出)───────────────

    /// 创建本地群组,返回群 ID。
    ///
    /// 群组是**本地定义 + 消息扇出**:发群消息时逐成员单播(带 `group_id`),
    /// 不依赖任何服务器。成员在自己那边收到带群信息的消息会自动建群/入群。
    pub async fn create_group(&self, name: &str, members: &[NodeId]) -> Result<String> {
        let id = format!("group:{}", MsgId::now_v7());
        let member_hex: Vec<String> = members.iter().map(|m| m.to_hex()).collect();
        self.store
            .lock()
            .await
            .create_group(&id, name.trim(), &member_hex)
            .map_err(Error::Store)?;
        Ok(id)
    }

    /// 列出全部群组。
    pub async fn list_groups(&self) -> Result<Vec<GroupRecord>> {
        self.store.lock().await.list_groups().map_err(Error::Store)
    }

    /// 删除群组(本地;历史消息保留)。
    pub async fn delete_group(&self, group_id: &str) -> Result<bool> {
        self.store
            .lock()
            .await
            .delete_group(group_id)
            .map_err(Error::Store)
    }

    /// 发送群消息:逐成员扇出(离线成员入各自待发队列),并落一条群历史。
    ///
    /// 返回 `(成功直发数, 入队数)`。
    pub async fn send_group_text(&self, group_id: &str, body: &str) -> Result<(usize, usize)> {
        let group = self
            .store
            .lock()
            .await
            .get_group(group_id)
            .map_err(Error::Store)?
            .ok_or_else(|| Error::Start(format!("群组不存在: {group_id}")))?;

        let mut sent = 0usize;
        let mut queued = 0usize;
        for member_hex in &group.members {
            let Ok(member) = NodeId::from_hex(member_hex) else {
                continue;
            };
            let envelope = Envelope::direct(
                self.node_id(),
                member,
                Kind::Text(TextBody {
                    body: body.to_string(),
                    format: TextFormat::Plain,
                    reply_to: None,
                    mentions: vec![],
                    group_id: Some(group.id.clone()),
                    group_name: Some(group.name.clone()),
                }),
            );
            match self.handle.send(envelope.clone()).await {
                Ok(()) => sent += 1,
                Err(_) => {
                    // 离线成员:消息按成员入队,群上下文随 envelope 一起保留
                    if let Ok(blob) = codec::encode(&envelope) {
                        let _ = self.store.lock().await.enqueue(
                            &envelope.id.to_string(),
                            member_hex,
                            &blob,
                            now_ms(),
                        );
                        queued += 1;
                    }
                }
            }
        }

        // 群历史:每个群只有一条记录(peer = group_id)
        let saved = NewMessage {
            id: MsgId::now_v7().to_string(),
            peer: group.id.clone(),
            is_outgoing: true,
            from_node: self.node_id().to_hex(),
            kind: "text".into(),
            body: Some(body.to_string()),
            format: Some("plain".into()),
            ts_ms: now_ms(),
            created_ms: now_ms(),
        };
        self.store
            .lock()
            .await
            .insert_message(&saved)
            .map_err(Error::Store)?;
        let _ = self.events.send(AppEvent::MessageSaved {
            peer: self.node_id(),
            outgoing: true,
            id: MsgId::now_v7(),
        });
        Ok((sent, queued))
    }

    /// 发送群文件/图片:逐成员发起独立传输(每人一个 token),并落一条群历史。
    ///
    /// 返回成功发起的**传输令牌列表**(每个成员一个);进度在事件流里分别上报。
    pub async fn send_group_file(
        &self,
        group_id: &str,
        path: impl Into<PathBuf>,
        message: Option<String>,
    ) -> Result<Vec<String>> {
        let path = path.into();
        let group = self
            .store
            .lock()
            .await
            .get_group(group_id)
            .map_err(Error::Store)?
            .ok_or_else(|| Error::Start(format!("群组不存在: {group_id}")))?;

        let mut tokens = Vec::new();
        let mut last_error: Option<Error> = None;
        // 文件元信息(循环内记录传输历史用,提前算好)
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "文件".into());
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        for member_hex in &group.members {
            let Ok(member) = NodeId::from_hex(member_hex) else {
                continue;
            };
            match self
                .handle
                .send_file(member, path.clone(), message.clone())
                .await
            {
                Ok(token) => {
                    let _ = self.store.lock().await.record_transfer_start(
                        &token,
                        &group.id,
                        "send",
                        &file_name,
                        size,
                        now_ms(),
                    );
                    tokens.push(token);
                }
                Err(e) => last_error = Some(Error::Net(e)),
            }
        }
        if tokens.is_empty() {
            return Err(
                last_error.unwrap_or_else(|| Error::Start("群成员均不可达,文件未能发出".into()))
            );
        }

        // 群历史:一条文件/图片消息(与单聊共用同一种 JSON body 约定)
        let is_img = is_image_extension(&file_name);
        let body = serde_json::json!({
            "n": file_name,
            "s": size,
            "p": path.display().to_string(),
            "i": is_img,
        });
        let saved = NewMessage {
            id: MsgId::now_v7().to_string(),
            peer: group.id.clone(),
            is_outgoing: true,
            from_node: self.node_id().to_hex(),
            kind: if is_img { "image" } else { "file" }.into(),
            body: Some(body.to_string()),
            format: None,
            ts_ms: now_ms(),
            created_ms: now_ms(),
        };
        self.store
            .lock()
            .await
            .insert_message(&saved)
            .map_err(Error::Store)?;
        let _ = self.events.send(AppEvent::MessageSaved {
            peer: self.node_id(),
            outgoing: true,
            id: MsgId::now_v7(),
        });
        Ok(tokens)
    }

    /// 删除联系人(只移出联系人列表;聊天记录/会话/头像缓存都保留)。
    ///
    /// 飞秋语义:局域网里再发现到该对端就会自动回到列表 —— 删除只影响"当前列表",
    /// 不存在永久隐藏,也没有"恢复"按钮。
    ///
    /// 实现要点:除了删数据库行,还要**忘记网络层对端表里的它** —— 否则对端表
    /// 仍有该条目、心跳不产生"变化"事件,联系人永远回不来(踩过)。
    pub async fn remove_peer(&self, node_id: NodeId) -> Result<bool> {
        let deleted = self
            .store
            .lock()
            .await
            .delete_peer(&node_id.to_hex())
            .map_err(Error::Store)?;
        if self.handle.forget_peer(node_id) {
            tracing::info!(target = "fq_core", %node_id, "已忘记对端,下次通告会重新发现");
        }
        Ok(deleted)
    }

    /// 刷新联系人:立即广播一次通告,并把当前已知对端**全部写回联系人表**。
    ///
    /// 手动点"刷新"时调用:包括之前删掉的人,只要还在局域网里就重新回到列表。
    /// 返回本次写回的联系人数量。
    pub async fn refresh_contacts(&self) -> Result<usize> {
        // 先通告 + 让对端也回发(新上线的节点会立刻被我们发现)
        self.handle.announce_now().await;
        let peers = self.handle.peers();
        let mut count = 0;
        let store = self.store.lock().await;
        for peer in peers {
            if let Err(e) = store.upsert_peer(&peer_record(&peer)) {
                tracing::warn!(target = "fq_core", %e, "联系人写回失败");
                continue;
            }
            count += 1;
        }
        Ok(count)
    }

    /// 与某会话的最近历史(时间升序)。
    pub async fn history(&self, peer: NodeId, limit: u32) -> Result<Vec<StoredMessage>> {
        self.store
            .lock()
            .await
            .history(&peer.to_hex(), limit)
            .map_err(Error::Store)
    }

    /// 按会话键取历史(支持 `group:...` 群 ID;桌面端与本端测试用)。
    pub async fn history_by_key(&self, key: &str, limit: u32) -> Result<Vec<StoredMessage>> {
        self.store
            .lock()
            .await
            .history(key, limit)
            .map_err(Error::Store)
    }

    /// 分页取更早的历史(滚动加载):`before_ms` 之前最近 `limit` 条。
    pub async fn history_before(
        &self,
        key: &str,
        before_ms: i64,
        limit: u32,
    ) -> Result<Vec<StoredMessage>> {
        self.store
            .lock()
            .await
            .history_before(key, before_ms, limit)
            .map_err(Error::Store)
    }

    /// 会话列表(最近消息倒序,含未读)。
    pub async fn conversations(&self) -> Result<Vec<fq_store::ConversationRecord>> {
        self.store
            .lock()
            .await
            .list_conversations()
            .map_err(Error::Store)
    }

    /// 局域网版本情况:本机版本 + 各在线对端的版本(更新检查用)。
    pub fn version_report(&self, local_version: &str) -> (String, Vec<(String, String, String)>) {
        let peers: Vec<(String, String, String)> = self
            .handle
            .peers()
            .into_iter()
            .filter(|p| p.status != fq_proto::PresenceStatus::Offline)
            .filter_map(|p| {
                p.app_version
                    .map(|v| (p.node_id.to_hex(), p.display_name, v))
            })
            .collect();
        (local_version.to_string(), peers)
    }

    /// 某会话仍在待发队列里的消息 ID(推导 `pending` 状态用)。
    pub async fn pending_ids(&self, key: &str) -> Result<Vec<String>> {
        self.store
            .lock()
            .await
            .pending_ids(key)
            .map_err(Error::Store)
    }

    /// 标记会话已读(清零未读并记录位置)。
    pub async fn mark_conversation_read(&self, key: &str) -> Result<()> {
        self.store
            .lock()
            .await
            .mark_conversation_read(key, now_ms())
            .map_err(Error::Store)
    }

    /// 全文搜索历史消息(跨会话,时间倒序)。
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<StoredMessage>> {
        self.store
            .lock()
            .await
            .search_messages(query, limit)
            .map_err(Error::Store)
    }

    /// 待发队列长度。
    pub async fn pending_count(&self) -> Result<usize> {
        self.store
            .lock()
            .await
            .pending_count()
            .map_err(Error::Store)
    }

    /// 标记会话已读:向对方补发"已读"回执(对最近一条收到的文本)。
    pub async fn mark_read(&self, peer: NodeId) -> Result<()> {
        let history = self.history(peer, 64).await?;
        let last_incoming = history
            .iter()
            .rev()
            .find(|m| !m.is_outgoing && m.kind == "text");
        let Some(last) = last_incoming else {
            return Ok(());
        };
        let ack_id = MsgId::parse(&last.id).map_err(Error::Proto)?;
        let ack = Envelope::direct(
            self.node_id(),
            peer,
            Kind::Ack(AckBody {
                ack_id,
                status: AckStatus::Read,
            }),
        );
        let _ = self.handle.send(ack).await;
        Ok(())
    }

    /// 联系人列表(存储记录 + 实时在线状态)。
    pub async fn peers(&self) -> Result<Vec<PeerSummary>> {
        let records = self.store.lock().await.list_peers().map_err(Error::Store)?;
        // 已缓存头像的哈希:在线对端优先用实时哈希(变更需重新拉取),
        // 离线对端用缓存哈希 —— 这样离线联系人依然能显示头像。
        let cached: std::collections::HashMap<String, String> = self
            .store
            .lock()
            .await
            .peer_avatar_hashes()
            .map_err(Error::Store)?
            .into_iter()
            .collect();
        Ok(records
            .into_iter()
            .filter_map(|record| {
                let node_id = NodeId::from_hex(&record.node_id).ok()?;
                let live = self.handle.peer(&node_id);
                let online = live
                    .as_ref()
                    .is_some_and(|p| p.status != fq_proto::PresenceStatus::Offline);
                let endpoints = live
                    .as_ref()
                    .map(|p| p.endpoints.clone())
                    .unwrap_or_default();
                let app_version = live.as_ref().and_then(|p| p.app_version.clone());
                let avatar_sha256 = live
                    .as_ref()
                    .and_then(|p| p.avatar_sha256.clone())
                    .or_else(|| cached.get(&record.node_id).cloned());
                Some(PeerSummary {
                    node_id,
                    display_name: record.display_name,
                    group: record.group_name,
                    online,
                    last_seen_ms: record.last_seen_ms,
                    endpoints,
                    app_version,
                    avatar_sha256,
                })
            })
            .collect())
    }

    /// 停止应用(网络、事件泵全部停止;数据已落盘,SQLite WAL 自动恢复)。
    pub fn shutdown(mut self) {
        self.pump.abort();
        if let Some(node) = self.node.take() {
            node.shutdown();
        }
    }

    /// 接收完成后插入文件消息到聊天历史(由桌面端事件转发调用)。
    pub async fn insert_file_message(
        &self,
        peer: NodeId,
        outgoing: bool,
        name: &str,
        size: u64,
        local_path: &str,
    ) -> Result<()> {
        let is_img = is_image_extension(name);
        let body = serde_json::json!({
            "n": name,
            "s": size,
            "p": local_path,
            "i": is_img,
        });
        let saved = NewMessage {
            id: MsgId::now_v7().to_string(),
            peer: peer.to_hex(),
            is_outgoing: outgoing,
            from_node: if outgoing {
                self.node_id().to_hex()
            } else {
                peer.to_hex()
            },
            kind: if is_img { "image" } else { "file" }.into(),
            body: Some(body.to_string()),
            format: None,
            ts_ms: fq_proto::now_ms(),
            created_ms: fq_proto::now_ms(),
        };
        self.store
            .lock()
            .await
            .insert_message(&saved)
            .map_err(Error::Store)
    }
}

/// 读取头像文件并计算 SHA-256(hex)。文件不存在/为空返回 `None`。
fn read_avatar(path: Option<&std::path::Path>) -> Option<(String, Vec<u8>)> {
    use sha2::{Digest, Sha256};
    let path = path?;
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some((hex::encode(hasher.finalize()), bytes))
}

/// 本机头像的可发送载荷(超过 `AVATAR_MAX_BYTES` 时不捎带,避免撑爆单帧)。
fn local_avatar_payload(path: Option<&std::path::Path>) -> Option<fq_proto::AvatarPayload> {
    let (_, bytes) = read_avatar(path)?;
    if bytes.len() > AVATAR_MAX_BYTES {
        tracing::warn!(
            target = "fq_core",
            bytes = bytes.len(),
            "本机头像过大,不随请求捎带(建议压缩到 256×256)"
        );
        return None;
    }
    Some(fq_proto::AvatarPayload::new(bytes, "image/png"))
}

/// 头像载荷上限(单帧上限 1 MiB,留足信封余量)。
const AVATAR_MAX_BYTES: usize = 256 * 1024;

/// 生成 Windows 覆盖安装脚本:等待旧进程退出 → 备份 → 覆盖(失败重试) → 重启 → 自删。
///
/// 抽成纯函数便于单测 —— 这段批处理只在应用退出后执行,必须逐行可读可核对。
pub fn build_update_script(package: &std::path::Path, current_exe: &std::path::Path) -> String {
    format!(
        "@echo off\r\n\
         rem feiqiu-r 自动更新脚本\r\n\
         rem 1) 等旧进程退出(文件锁释放) 2) 备份旧版 3) 覆盖(失败重试) 4) 重启 5) 自删\r\n\
         ping -n 3 127.0.0.1 >nul\r\n\
         if exist \"{cur}.bak\" del /Q \"{cur}.bak\" >nul 2>&1\r\n\
         copy /Y \"{cur}\" \"{cur}.bak\" >nul 2>&1\r\n\
         :retry\r\n\
         copy /Y \"{pkg}\" \"{cur}\" >nul 2>&1\r\n\
         if errorlevel 1 (\r\n\
           ping -n 2 127.0.0.1 >nul\r\n\
           goto retry\r\n\
         )\r\n\
         start \"\" \"{cur}\"\r\n\
         del \"%~f0\"\r\n",
        pkg = package.display(),
        cur = current_exe.display()
    )
}

/// 事件泵:网络事件 → 落库/回执/补发 → 应用事件。
///
/// 补发采用"事件触发 + 定时重试"双通道:对端上线事件立即冲洗;
/// 若对端此刻尚未完成反向发现(入站握手会被安全拒绝),由每秒的
/// 重试时钟兜底 —— 队列非空就一直重试到成功。
async fn event_pump(
    handle: NodeHandle,
    store: Arc<Mutex<Store>>,
    out: broadcast::Sender<AppEvent>,
    tofu_path: PathBuf,
    update_package: Option<PathBuf>,
    avatar_path: Option<PathBuf>,
) {
    let mut rx = handle.events();
    let mut retry_ticker = tokio::time::interval(Duration::from_secs(1));
    retry_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    retry_ticker.tick().await; // 首个 tick 立即返回,跳过
    // 头像重试状态:节点 → 拉取状态
    let mut avatar_pulls: HashMap<String, AvatarPull> = HashMap::new();

    loop {
        let event = tokio::select! {
            received = rx.recv() => match received {
                Ok(event) => event,
                Err(_) => return,
            },
            _ = retry_ticker.tick() => {
                // 待发队列兜底重试(队列为空时开销为一次本地查询)
                let pending = store.lock().await.pending_count().unwrap_or(0);
                if pending > 0 {
                    for peer in handle.peers() {
                        if peer.status != fq_proto::PresenceStatus::Offline {
                            flush_pending(&handle, &store, peer.node_id, &out).await;
                        }
                    }
                }
                // 头像拉取兜底重试(首次请求可能被同时拨号替换连接而丢掉)
                retry_avatars(
                    &handle,
                    &store,
                    &mut avatar_pulls,
                    avatar_path.as_deref(),
                    &out,
                )
                .await;
                continue;
            }
        };
        match &event {
            NodeEvent::MessageReceived { from, envelope } => {
                match &envelope.kind {
                    Kind::Text(body) => {
                        // 群消息:先确保本地有该群(自动建群 + 把发送者补进成员)
                        let peer_key = if let Some(group_id) = &body.group_id {
                            let guard = store.lock().await;
                            match guard.get_group(group_id) {
                                Ok(Some(_)) => {
                                    let _ = guard.add_group_member(group_id, &from.to_hex());
                                }
                                _ => {
                                    let group_name = body
                                        .group_name
                                        .clone()
                                        .unwrap_or_else(|| "局域网群聊".to_string());
                                    let _ =
                                        guard.create_group(group_id, &group_name, &[from.to_hex()]);
                                }
                            }
                            group_id.clone()
                        } else {
                            from.to_hex()
                        };

                        let saved = NewMessage {
                            id: envelope.id.to_string(),
                            peer: peer_key,
                            is_outgoing: false,
                            from_node: from.to_hex(),
                            kind: "text".into(),
                            body: Some(body.body.clone()),
                            format: Some(body.format.as_str().to_string()),
                            ts_ms: envelope.ts_ms,
                            created_ms: now_ms(),
                        };
                        if let Err(e) = store.lock().await.insert_message(&saved) {
                            tracing::warn!(target = "fq_core", %e, "消息入库失败");
                            continue;
                        }
                        // 单聊自动"送达"回执;群聊扇出无法逐条对账,不回执
                        if body.group_id.is_none() {
                            let ack = Envelope::direct(
                                handle.node_id(),
                                *from,
                                Kind::Ack(AckBody {
                                    ack_id: envelope.id,
                                    status: AckStatus::Delivered,
                                }),
                            );
                            if let Err(e) = handle.send(ack).await {
                                tracing::debug!(target = "fq_core", %e, "送达回执发送失败");
                            }
                        }
                        let _ = out.send(AppEvent::MessageSaved {
                            peer: *from,
                            outgoing: false,
                            id: envelope.id,
                        });
                    }
                    Kind::AvatarRequest(req) => {
                        // 对端索取头像:本地有则回发(对端已知哈希相同时跳过,省流量)
                        // 同时:请求里捎带的对端头像直接收下(一次往返双向同步)
                        if let Some(mine) = &req.mine {
                            if mine.verify() {
                                let node_hex = from.to_hex();
                                match store.lock().await.put_peer_avatar(
                                    &node_hex,
                                    &mine.sha256,
                                    &mine.mime,
                                    &mine.data,
                                    now_ms(),
                                ) {
                                    Ok(()) => {
                                        tracing::info!(
                                            target = "fq_core",
                                            from = %from,
                                            bytes = mine.data.len(),
                                            "随请求缓存对端头像"
                                        );
                                        let _ = out.send(AppEvent::PeerAvatar {
                                            node_id: *from,
                                            sha256: mine.sha256.clone(),
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!(target = "fq_core", %e, "头像入库失败")
                                    }
                                }
                            } else {
                                tracing::warn!(target = "fq_core", from = %from, "请求携带的头像校验失败,丢弃");
                            }
                        }
                        match read_avatar(avatar_path.as_deref()) {
                            Some((sha256, bytes)) => {
                                if req.known_sha256.as_deref() == Some(sha256.as_str()) {
                                    tracing::debug!(target = "fq_core", to = %from, "对端头像已是最新,跳过回发");
                                } else {
                                    tracing::info!(
                                        target = "fq_core",
                                        to = %from,
                                        bytes = bytes.len(),
                                        "回发本机头像"
                                    );
                                    let handle = handle.clone();
                                    let to = *from;
                                    tokio::spawn(async move {
                                        let payload =
                                            fq_proto::AvatarPayload::new(bytes, "image/png");
                                        if let Err(e) = handle.send_avatar(to, payload).await {
                                            tracing::warn!(target = "fq_core", %e, "发送头像失败");
                                        }
                                    });
                                }
                            }
                            None => {
                                tracing::debug!(target = "fq_core", to = %from, "本地无头像,无需回发");
                            }
                        }
                    }
                    Kind::AvatarReply(reply) => {
                        // 校验内容哈希 → 落库 → 通知 UI
                        if !reply.avatar.verify() {
                            tracing::warn!(
                                target = "fq_core",
                                from = %from,
                                "头像哈希校验失败,丢弃(可能被篡改或损坏)"
                            );
                        } else {
                            let node_hex = from.to_hex();
                            if let Err(e) = store.lock().await.put_peer_avatar(
                                &node_hex,
                                &reply.avatar.sha256,
                                &reply.avatar.mime,
                                &reply.avatar.data,
                                now_ms(),
                            ) {
                                tracing::warn!(target = "fq_core", %e, "头像入库失败");
                            } else {
                                tracing::info!(
                                    target = "fq_core",
                                    from = %from,
                                    bytes = reply.avatar.data.len(),
                                    "已缓存对端头像"
                                );
                                let _ = out.send(AppEvent::PeerAvatar {
                                    node_id: *from,
                                    sha256: reply.avatar.sha256.clone(),
                                });
                            }
                        }
                    }
                    Kind::UpdateRequest(req) => {
                        // 对端请求更新包:仅当本机版本更高时提供(避免降级/骚扰)
                        let local = handle.app_version().unwrap_or_else(|| "0.0.0".into());
                        if compare_versions(&local, &req.requester_version) > 0 {
                            match update_package.clone() {
                                Some(exe) => {
                                    tracing::info!(
                                        target = "fq_core",
                                        to = %from,
                                        requester = %req.requester_version,
                                        local = %local,
                                        package = %exe.display(),
                                        "响应更新请求,发送本机安装包"
                                    );
                                    let handle = handle.clone();
                                    let store = Arc::clone(&store);
                                    let to = *from;
                                    // 传输历史:更新包发送也记一笔(与普通文件发送一致)
                                    let file_name = exe
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "update".into());
                                    let file_size =
                                        std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
                                    tokio::spawn(async move {
                                        match handle.send_update_package(to, exe, local).await {
                                            Ok(token) => {
                                                let _ = store.lock().await.record_transfer_start(
                                                    &token,
                                                    &to.to_hex(),
                                                    "send",
                                                    &file_name,
                                                    file_size,
                                                    now_ms(),
                                                );
                                            }
                                            Err(e) => {
                                                tracing::warn!(target = "fq_core", %e, "发送更新包失败");
                                            }
                                        }
                                    });
                                }
                                None => {
                                    tracing::warn!(
                                        target = "fq_core",
                                        "无法定位本机可执行文件,无法提供更新包"
                                    );
                                }
                            }
                        } else {
                            tracing::debug!(
                                target = "fq_core",
                                "收到更新请求但本机版本不更高,忽略"
                            );
                        }
                    }
                    Kind::Ack(ack) => {
                        let now = now_ms();
                        let (delivered, read) = match ack.status {
                            AckStatus::Delivered => (Some(now), None),
                            AckStatus::Read => (Some(now), Some(now)),
                            _ => (None, None),
                        };
                        let id = ack.ack_id.to_string();
                        if let Err(e) = store.lock().await.update_ack(&id, delivered, read) {
                            tracing::warn!(target = "fq_core", %e, "回执更新失败");
                        }
                        // 送达回执 = 待发队列的出队依据(连接中途死亡不会误出队)
                        if delivered.is_some() {
                            let _ = store.lock().await.remove_pending(&id);
                        }
                    }
                    Kind::Shake(_) => {
                        // 窗口抖动:落一条记录(两边都有上下文),并通知 UI 抖窗口
                        let envelope_id = envelope.id;
                        let saved = NewMessage {
                            id: envelope_id.to_string(),
                            peer: from.to_hex(),
                            is_outgoing: false,
                            from_node: from.to_hex(),
                            kind: "shake".into(),
                            body: None,
                            format: None,
                            ts_ms: envelope.ts_ms,
                            created_ms: now_ms(),
                        };
                        if let Err(e) = store.lock().await.insert_message(&saved) {
                            tracing::warn!(target = "fq_core", %e, "抖动记录入库失败");
                        }
                        let _ = out.send(AppEvent::Shaken { from: *from });
                        let _ = out.send(AppEvent::MessageSaved {
                            peer: *from,
                            outgoing: false,
                            id: envelope_id,
                        });
                    }
                    _ => {}
                }
            }
            NodeEvent::PeerDiscovered {
                peer,
                first_contact,
            } => {
                // 首次接触:把 TOFU 固定表回写磁盘(跨重启锁定密钥)
                if *first_contact {
                    let snapshot = handle.tofu_snapshot();
                    if let Err(e) = fq_crypto::save_tofu(&tofu_path, &snapshot) {
                        tracing::warn!(target = "fq_core", %e, "TOFU 固定表保存失败");
                    }
                }
                on_peer_seen(&handle, &store, peer, &out).await;
            }
            NodeEvent::PeerUpdated { peer, .. } => {
                on_peer_seen(&handle, &store, peer, &out).await;
            }
            // ── 传输历史落库(开始/结束)──
            NodeEvent::UpdateOfferReceived {
                from,
                token,
                version,
                manifest,
            } => {
                let name = manifest
                    .entries
                    .first()
                    .map(|e| e.path.clone())
                    .unwrap_or_else(|| manifest.root_name.clone());
                tracing::info!(
                    target = "fq_core",
                    from = %from,
                    %version,
                    file = %name,
                    bytes = manifest.total_bytes,
                    "收到更新包要约,自动接收"
                );
                let _ = version;
                if let Err(e) = store.lock().await.record_transfer_start(
                    token,
                    &from.to_hex(),
                    "recv",
                    &name,
                    manifest.total_bytes,
                    now_ms(),
                ) {
                    tracing::warn!(target = "fq_core", %e, "传输历史记录失败");
                }
            }
            NodeEvent::FileOfferReceived {
                from,
                token,
                manifest,
                ..
            } => {
                let name = manifest
                    .entries
                    .first()
                    .map(|e| e.path.clone())
                    .unwrap_or_else(|| manifest.root_name.clone());
                if let Err(e) = store.lock().await.record_transfer_start(
                    token,
                    &from.to_hex(),
                    "recv",
                    &name,
                    manifest.total_bytes,
                    now_ms(),
                ) {
                    tracing::warn!(target = "fq_core", %e, "传输历史记录失败");
                }
            }
            NodeEvent::FileTransferCompleted { token, .. } => {
                let _ = store
                    .lock()
                    .await
                    .record_transfer_finish(token, "done", None, now_ms());
            }
            NodeEvent::UpdatePackageReady {
                from,
                version,
                path,
                ..
            } => {
                // 更新包已下载并校验通过:通知 UI 走安装流程
                tracing::info!(target = "fq_core", %version, %path, "更新包就绪");
                let _ = out.send(AppEvent::UpdateReady {
                    from: *from,
                    version: version.clone(),
                    path: path.clone(),
                });
            }
            NodeEvent::FileTransferFailed { token, reason, .. } => {
                let status = if reason.contains("取消") {
                    "cancelled"
                } else {
                    "failed"
                };
                let _ = store.lock().await.record_transfer_finish(
                    token,
                    status,
                    Some(reason),
                    now_ms(),
                );
            }
            _ => {}
        }
        let _ = out.send(AppEvent::Node(Box::new(event)));
    }
}

/// 本机软件版本(与 Cargo/安装包版本同源)。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 版本号比较(数字分段;段数不足补 0)。返回 `-1/0/1`。
pub fn compare_versions(a: &str, b: &str) -> i32 {
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

/// 判断文件名是否为常见图片扩展名。
fn is_image_extension(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".ico", ".svg", ".tiff",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// 对端出现(发现/更新):入库 + 上线时冲洗待发队列。
/// 隐藏(已删除)的联系人跳过,不再入库/上屏。
/// 由对端快照构造联系人记录(入库用)。
fn peer_record(peer: &fq_net::PeerInfo) -> PeerRecord {
    PeerRecord {
        node_id: peer.node_id.to_hex(),
        display_name: peer.display_name.clone(),
        host_name: Some(peer.host_name.clone()),
        group_name: peer.group.clone(),
        first_seen_ms: now_ms(),
        last_seen_ms: now_ms(),
    }
}

async fn on_peer_seen(
    handle: &NodeHandle,
    store: &Arc<Mutex<Store>>,
    peer: &fq_net::PeerInfo,
    out: &broadcast::Sender<AppEvent>,
) {
    // 联系人入库(删除过的也会重新出现:飞秋语义,列表由发现驱动)
    if let Err(e) = store.lock().await.upsert_peer(&peer_record(peer)) {
        tracing::warn!(target = "fq_core", %e, "联系人入库失败");
    }
    // 上线 → 冲洗该对端的待发队列
    if peer.status != fq_proto::PresenceStatus::Offline {
        flush_pending(handle, store, peer.node_id, out).await;
    }
}

/// 头像拉取兜底:对"声明了 AVATAR 能力、哈希与本地缓存不同"的在线对端重试索取。
///
/// 首次请求在发现事件里立即发出;这里负责**重试**(带冷却与次数上限),
/// 保证"同时拨号替换连接"这类一次性抖动不会让头像永远缺失。
/// 单个对端的头像拉取状态。
struct AvatarPull {
    /// 目标哈希(变化即重新开始计数)。
    sha: String,
    /// 首次发现"需要拉取"的时刻(弱侧兜底延时的起点)。
    noticed: tokio::time::Instant,
    /// 上次实际发送请求的时刻。
    last_attempt: Option<tokio::time::Instant>,
    /// 已尝试次数。
    tries: u8,
}

/// 头像拉取:确定性发起方 + 短退避重试。
///
/// 关键设计(踩过坑):双方**同时拨号**时后建的连接会替换旧连接,被替换连接上
/// 在途的报文会被 RST 掉 —— 若两端都主动拉取,就会"同步重拨"互相打断,头像
/// 迟迟到不了。因此:
///
/// 1. **只有 NodeId 较小的一方主动拉取**(确定性,永不互撞);
/// 2. 另一方的头像由**服务端顺带索取**获得(一次连接换双向同步,见事件泵);
/// 3. 若对方是老版本/不支持 AVATAR,较小方拉完就结束;较大方在 15s 兜底后
///    也允许自己发起(应对"对方永远不拉"的极端情况)。
async fn retry_avatars(
    handle: &NodeHandle,
    store: &Arc<Mutex<Store>>,
    pulls: &mut HashMap<String, AvatarPull>,
    avatar_path: Option<&std::path::Path>,
    out: &broadcast::Sender<AppEvent>,
) {
    // 每节点固定抖动:进一步降低与控制面对端撞车的概率
    let jitter = avatar_retry_offset(handle.node_id());
    let cached: HashMap<String, String> = store
        .lock()
        .await
        .peer_avatar_hashes()
        .unwrap_or_default()
        .into_iter()
        .collect();
    for peer in handle.peers() {
        if peer.status == fq_proto::PresenceStatus::Offline
            || !peer.capabilities.contains(fq_proto::Capabilities::AVATAR)
        {
            continue;
        }
        let Some(sha) = peer.avatar_sha256.clone() else {
            // 对端宣布"没有头像":清掉本地缓存(否则移除头像后对端界面仍显示旧图)
            let key = peer.node_id.to_hex();
            if cached.contains_key(&key) {
                let _ = store.lock().await.delete_peer_avatar(&key);
                pulls.remove(&key);
                tracing::info!(target = "fq_core", peer = %key, "对端已移除头像,清理本地缓存");
                let _ = out.send(AppEvent::PeerAvatarRemoved {
                    node_id: peer.node_id,
                });
            }
            continue;
        };
        let key = peer.node_id.to_hex();
        if cached.get(&key).map(String::as_str) == Some(sha.as_str()) {
            pulls.remove(&key); // 已同步,清掉状态
            continue;
        }
        // 新目标(首次需要 / 对端换了头像)→ 重新计时
        let pull = pulls.entry(key.clone()).or_insert_with(|| AvatarPull {
            sha: sha.clone(),
            noticed: tokio::time::Instant::now(),
            last_attempt: None,
            tries: 0,
        });
        if pull.sha != sha {
            *pull = AvatarPull {
                sha: sha.clone(),
                noticed: tokio::time::Instant::now(),
                last_attempt: None,
                tries: 0,
            };
        }

        // 发起方判定与错峰(关键,踩过两次坑):
        // * **首次同步**(本地无缓存):发起方(ID 小)立即可拉;另一方**错开 2s**
        //   —— 双方同时拨号会互相替换连接、RST 掉在途请求。发起方的请求里带了
        //   mine,通常一次就把双向都同步好,另一方 2s 后一看缓存已经在就跳过。
        // * **刷新**(本地有缓存,对端换了头像):发起方立即拉;另一方等对方
        //   **推送**(`reload_avatar` 会推),15s 兜底再自己拉。
        let first_sync = !cached.contains_key(&key);
        let i_am_initiator = handle.node_id() < peer.node_id;
        let wait = match (first_sync, i_am_initiator) {
            (true, true) => Duration::ZERO,
            (true, false) => AVATAR_FIRST_SYNC_STAGGER,
            (false, true) => Duration::ZERO,
            (false, false) => AVATAR_WEAK_SIDE_DELAY,
        };
        if pull.noticed.elapsed() < wait + jitter {
            continue;
        }
        if pull.tries >= AVATAR_RETRY_MAX {
            continue; // 放弃(对端一直不回)
        }
        if let Some(last) = pull.last_attempt {
            if last.elapsed() < avatar_retry_backoff(pull.tries) + jitter {
                continue;
            }
        }

        pull.last_attempt = Some(tokio::time::Instant::now());
        pull.tries = pull.tries.saturating_add(1);
        let known_hash = cached.get(&key).cloned();
        // 顺带捎上自己的头像:一次往返双向同步,避免对端也来拨号(互拨会 RST 在途报文)
        let mine = local_avatar_payload(avatar_path);
        if let Err(e) = handle.request_avatar(peer.node_id, known_hash, mine).await {
            tracing::debug!(target = "fq_core", %e, "头像重试索取失败");
        }
    }
}

/// 待发消息的最大存活时间:超过后放弃并丢弃(防止回执永久丢失时无限重发)。
const PENDING_MAX_AGE: i64 = 10 * 60 * 1000;

/// 头像拉取重试:退避与上限。
///
/// 发现瞬间双方会**同时拨号**,连接被替换时会 RST 掉在途帧,因此需要重试;
/// 但若两端都用同一周期重试,就会永远撞在一起("同步重拨")。因此:
/// **短退避(1s→2s→3s→6s)+ 按 NodeId 派生的固定抖动** —— 既快收敛又不锁步。
const AVATAR_RETRY_MAX: u8 = 12;

/// 非发起方等待多久后也允许自己拉取(兜底:对方可能不支持头像或一直不拉)。
const AVATAR_WEAK_SIDE_DELAY: Duration = Duration::from_secs(15);

/// 首次同步时,非发起方的错峰延时(避开与发起方"同时拨号")。
const AVATAR_FIRST_SYNC_STAGGER: Duration = Duration::from_millis(2000);

/// 第 `tries` 次重试前的等待时长(不含抖动)。
fn avatar_retry_backoff(tries: u8) -> Duration {
    match tries {
        0 | 1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        3 => Duration::from_secs(3),
        _ => Duration::from_secs(6),
    }
}

/// 按 NodeId 前 4 个 hex 字符派生 0..2000ms 的固定抖动。
fn avatar_retry_offset(node_id: NodeId) -> Duration {
    let hex = node_id.to_hex();
    let seed = u16::from_str_radix(&hex[..4.min(hex.len())], 16).unwrap_or(0);
    Duration::from_millis(u64::from(seed % 2000))
}

/// 把某对端待发队列里的消息按序补发。
///
/// 出队语义(**关键**):只有收到对端的"送达"回执才真正出队。
/// "已交给连接"不算送达 —— 连接可能在握手中途被拒/断开,消息会静默蒸发;
/// 留在队列里由重试时钟兜底,接收端用消息 ID 去重,重复投递无副作用。
async fn flush_pending(
    handle: &NodeHandle,
    store: &Arc<Mutex<Store>>,
    peer: NodeId,
    out: &broadcast::Sender<AppEvent>,
) {
    let queue = match store.lock().await.pending(&peer.to_hex()) {
        Ok(queue) => queue,
        Err(e) => {
            tracing::warn!(target = "fq_core", %e, "读取待发队列失败");
            return;
        }
    };
    let now = now_ms();
    let mut sent = 0usize;
    for (id, blob, queued_ms) in queue {
        // 超龄:对端长期无法送达,放弃(避免无限重发)
        if now - queued_ms > PENDING_MAX_AGE {
            tracing::warn!(target = "fq_core", %id, "待发消息超龄,放弃补发");
            let _ = store.lock().await.remove_pending(&id);
            continue;
        }
        let envelope = match codec::decode(&blob) {
            Ok(envelope) => envelope,
            Err(e) => {
                // 队列里的字节是本机写入的,损坏只能丢弃
                tracing::warn!(target = "fq_core", %e, "待发消息损坏,丢弃");
                let _ = store.lock().await.remove_pending(&id);
                continue;
            }
        };
        match handle.send(envelope.clone()).await {
            Ok(()) => {
                // 入历史(INSERT OR IGNORE,重发天然幂等);**留在队列**,
                // 等送达回执出队
                let saved = new_message(&envelope, true);
                let guard = store.lock().await;
                let _ = guard.insert_message(&saved);
                drop(guard);
                sent += 1;
            }
            Err(e) => {
                // 对端又不可达:停止本轮冲洗,留待下次上线
                tracing::debug!(target = "fq_core", %e, "补发中断,留待下次上线");
                break;
            }
        }
    }
    if sent > 0 {
        let _ = out.send(AppEvent::QueueFlushed {
            to: peer,
            count: sent,
        });
    }
}

fn new_message(envelope: &Envelope, outgoing: bool) -> NewMessage {
    let peer = envelope
        .to
        .map(|to| to.to_hex())
        .unwrap_or_else(|| envelope.from.to_hex());
    let (kind, body, format) = match &envelope.kind {
        Kind::Text(text) => (
            "text",
            Some(text.body.clone()),
            Some(text.format.as_str().to_string()),
        ),
        other => (other.name(), None, None),
    };
    NewMessage {
        id: envelope.id.to_string(),
        peer,
        is_outgoing: outgoing,
        from_node: envelope.from.to_hex(),
        kind: kind.into(),
        body,
        format,
        ts_ms: envelope.ts_ms,
        created_ms: now_ms(),
    }
}
