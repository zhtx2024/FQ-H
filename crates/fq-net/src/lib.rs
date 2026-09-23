//! # fq-net
//!
//! feiqiu-r 网络层:UDP 发现 + TCP 安全传输 + 连接管理 + 去重。
//!
//! ## 组成
//!
//! ```text
//! ┌─ Node ────────────────────────────────────────────────┐
//! │  discovery(UDP) ──▶ 验证绑定签名 ──▶ TOFU ──▶ PeerTable │
//! │       ▲ 心跳通告(广播 + bootstrap 单播)                │
//! │                                                        │
//! │  transport(TCP) ── Noise IK 握手 ──▶ 加密帧收发         │
//! │       └─ 连接管理(按需拨号/复用/废弃重建) + 去重窗口    │
//! └──────────── NodeEvent(broadcast) ──▶ 上层/UI ──────────┘
//! ```
//!
//! ## 安全链(不可绕过)
//!
//! 收到的每条发现通告都必须依次通过:
//! 1. `NodeId == SHA-256(Ed25519 公钥)[0..16]`(身份一致)
//! 2. 绑定签名验证(静态握手密钥确实属于该身份)
//! 3. TOFU 校验(FirstUse 自动固定并提示 / Trusted 放行 /
//!    **Changed 拒绝并告警,绝不静默换绑**)
//!
//! 未通过验证的通告直接丢弃,不进对端表,也不可能建立连接。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fq_crypto::{
    Identity, StaticKeys, TofuStore, TrustDecision, verify_static_key_binding, STATIC_KEY_LEN,
};
use fq_proto::{
    Capabilities, Envelope, FrameDecoder, Kind, MsgId, NodeId, PresenceEvent, PresenceInfo,
    PresenceStatus, TextBody, TextFormat, codec,
};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};

pub mod dedup;
pub mod discovery;
pub mod error;
pub mod peers;
pub mod transfer;
pub mod transport;

pub use dedup::DedupWindow;
pub use discovery::{DiscoveryEndpoint, local_ipv4_addresses};
pub use error::{Error, Result};
pub use peers::{PeerChange, PeerInfo, PeerTable};
pub use transfer::{TransferDirection, build_manifest, sanitize_entry_path};
pub use transport::Transport;

/// 更新包类型(协议层定义,便于上层直接引用)。
pub use fq_proto::{AvatarPayload, AvatarReply, AvatarRequest, UpdateOffer, UpdateRequest};

/// 事件通道容量。
const EVENT_CAPACITY: usize = 1024;
/// 每连接的发送队列深度。
const CONN_QUEUE: usize = 256;
/// 拨号互斥等待轮询间隔。
const DIAL_POLL: Duration = Duration::from_millis(20);
/// 拨号互斥最长等待。
const DIAL_WAIT: Duration = Duration::from_secs(8);

/// 节点对外事件。
#[derive(Debug, Clone)]
pub enum NodeEvent {
    /// 发现新对端。
    PeerDiscovered {
        /// 对端快照。
        peer: PeerInfo,
        /// 是否首次接触(TOFU 刚刚自动固定,UI 应提示指纹)。
        first_contact: bool,
    },
    /// 对端信息更新(昵称/状态/端点变化)。
    PeerUpdated {
        ///对端快照。
        peer: PeerInfo,
    },
    /// 对端离线(主动下线或心跳超时)。
    PeerLost {
        /// 对端 NodeId。
        node_id: NodeId,
    },
    /// TOFU 信任告警:同一 NodeId 换了静态密钥,可能被中间人替换。
    /// 已自动拒绝该通告;是否信任新密钥由用户决定。
    TrustWarning {
        /// 对端 NodeId。
        node_id: NodeId,
        /// 之前固定的指纹。
        pinned: String,
        /// 本次呈现的指纹。
        presented: String,
    },
    /// 收到一条新消息(已完成去重,不含文件传输类报文)。
    MessageReceived {
        /// 发送方。
        from: NodeId,
        /// 完整报文。
        envelope: Envelope,
    },
    /// 收到文件传输要约(P4 阶段自动接受;P7 UI 接管为用户确认)。
    FileOfferReceived {
        /// 发送方。
        from: NodeId,
        /// 传输令牌。
        token: String,
        /// 传输清单。
        manifest: fq_proto::FileManifest,
        /// 附带留言。
        message: Option<String>,
    },
    /// 传输进度(发送/接收双向,节流发出)。
    FileProgress {
        /// 方向。
        direction: TransferDirection,
        /// 传输令牌。
        token: String,
        /// 对端。
        peer: NodeId,
        /// 条目相对路径。
        path: String,
        /// 已传输字节。
        transferred: u64,
        /// 条目总字节。
        total: u64,
    },
    /// 单个条目结束。接收方 `verified` 表示 SHA-256 校验结果;发送方恒为 false。
    FileEntryDone {
        /// 方向。
        direction: TransferDirection,
        /// 传输令牌。
        token: String,
        /// 对端。
        peer: NodeId,
        /// 条目相对路径。
        path: String,
        /// 本端计算的全文件 SHA-256。
        sha256: String,
        /// 接收方校验结果。
        verified: bool,
    },
    /// 整个传输(全部条目)完成。
    FileTransferCompleted {
        /// 方向。
        direction: TransferDirection,
        /// 传输令牌。
        token: String,
        /// 对端。
        peer: NodeId,
    },
    /// 传输失败(中断/校验失败/磁盘错误)。接收中断时 `.part` 保留供续传。
    FileTransferFailed {
        /// 方向。
        direction: TransferDirection,
        /// 传输令牌。
        token: String,
        /// 对端。
        peer: NodeId,
        /// 相关条目(整体失败时可能为空)。
        path: Option<String>,
        /// 失败原因。
        reason: String,
    },
    /// 收到**更新包**要约(已自动接收,无需用户裁决)。
    UpdateOfferReceived {
        /// 提供方。
        from: NodeId,
        /// 传输令牌。
        token: String,
        /// 提供方版本。
        version: String,
        /// 清单(通常只有一个文件:安装包/可执行文件)。
        manifest: fq_proto::FileManifest,
    },
    /// 更新包已下载并通过 SHA-256 校验,可以安装。
    UpdatePackageReady {
        /// 提供更新包的对端。
        from: NodeId,
        /// 更新版本。
        version: String,
        /// 落盘路径。
        path: String,
        /// 传输令牌。
        token: String,
    },
}

/// 节点配置。
#[derive(Debug)]
pub struct NodeConfig {
    /// 本端身份(Ed25519)。
    pub identity: Identity,
    /// 本端 Noise 静态密钥(X25519)。
    pub static_keys: StaticKeys,
    /// 展示昵称。
    pub display_name: String,
    /// 主机名。
    pub host_name: String,
    /// 分组。
    pub group: Option<String>,
    /// 初始在线状态。
    pub status: PresenceStatus,
    /// UDP 发现绑定地址(端口 0 表示随机)。
    pub discovery_bind: SocketAddr,
    /// 广播目标地址。
    pub broadcast_addr: SocketAddr,
    /// bootstrap 单播目标(发现直连退路 / 测试用)。
    pub bootstrap: Vec<SocketAddr>,
    /// TCP 监听地址(端口 0 表示随机)。
    pub listen_addr: SocketAddr,
    /// 心跳间隔。
    pub heartbeat_every: Duration,
    /// 对端心跳超时(超过判离线)。
    pub peer_timeout: Duration,
    /// TOFU 固定表(初始为空或从磁盘加载)。
    pub tofu: TofuStore,
    /// 接收文件的保存目录。
    pub download_dir: PathBuf,
    /// 自动接受文件传输(CLI 等无人值守场景;桌面 UI 设为 false 走确认流)。
    pub auto_accept_files: bool,
    /// 软件版本(随通告广播,用于局域网版本发现)。
    pub app_version: Option<String>,
    /// 头像内容 SHA-256(hex;随通告广播,空表示无头像)。
    pub avatar_sha256: Option<String>,
}

impl NodeConfig {
    /// 最小配置:默认端口 24250、心跳 20s、超时 60s。
    pub fn new(identity: Identity, static_keys: StaticKeys, display_name: &str) -> Self {
        Self {
            identity,
            static_keys,
            display_name: display_name.to_string(),
            host_name: default_host_name(),
            group: None,
            status: PresenceStatus::Online,
            discovery_bind: SocketAddr::from(([0, 0, 0, 0], fq_proto::DEFAULT_PORT)),
            broadcast_addr: SocketAddr::from(([255, 255, 255, 255], fq_proto::DEFAULT_PORT)),
            bootstrap: Vec::new(),
            listen_addr: SocketAddr::from(([0, 0, 0, 0], fq_proto::DEFAULT_PORT)),
            heartbeat_every: Duration::from_secs(20),
            peer_timeout: Duration::from_secs(60),
            tofu: TofuStore::new(),
            download_dir: PathBuf::from("downloads"),
            auto_accept_files: true,
            app_version: None,
            avatar_sha256: None,
        }
    }
}

fn default_host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// 节点内部共享状态。
struct Shared {
    self_id: NodeId,
    identity_public: [u8; 32],
    static_keys: Arc<StaticKeys>,
    binding_signature: [u8; 64],
    listen_port: u16,
    profile: Mutex<Profile>,
    peers: PeerTable,
    tofu: Arc<Mutex<TofuStore>>,
    conns: Mutex<HashMap<NodeId, ConnHandle>>,
    dialing: Mutex<HashSet<NodeId>>,
    dedup: Mutex<DedupWindow>,
    events: broadcast::Sender<NodeEvent>,
    discovery: Arc<DiscoveryEndpoint>,
    transfers: transfer::TransferManager,
    /// 接收文件的保存目录(可运行期修改,读取走短临界区)。
    download_dir: std::sync::RwLock<PathBuf>,
    /// 自动接受文件传输(无 UI 的场景如 CLI;桌面端关闭走接收确认流)。
    auto_accept_files: bool,
    /// 软件版本(随通告广播)。
    app_version: Option<String>,
    /// 头像内容哈希(变更后重新通告,对端据此拉取)。
    avatar_sha256: Mutex<Option<String>>,
    /// 所有派生任务(连接读写、传输驱动)的中止句柄注册表:
    /// shutdown 时必须全部中止,否则它们持有的 Arc<Shared> 会让 UDP socket
    /// 变成僵尸 —— 后续同端口重绑会"成功"但数据报仍投给僵尸 socket。
    spawned_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

#[derive(Debug, Clone)]
struct Profile {
    display_name: String,
    host_name: String,
    group: Option<String>,
    status: PresenceStatus,
}

struct ConnHandle {
    tx: mpsc::Sender<Envelope>,
    tasks: Vec<JoinHandle<()>>,
}

/// 节点的可克隆句柄。
///
/// 事件泵、CLI、GUI 等多处需要同时操作节点(发消息、查对端、订阅事件),
/// 而 [`Node`] 本体独占后台任务所有权、不可克隆 —— 各方持有 [`NodeHandle`] 即可。
#[derive(Clone)]
pub struct NodeHandle {
    shared: Arc<Shared>,
}

impl fmt::Debug for NodeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 不泄露任何密钥材料
        write!(
            f,
            "NodeHandle(id={}, listen_port={})",
            self.shared.self_id, self.shared.listen_port
        )
    }
}

impl std::ops::Deref for Node {
    type Target = NodeHandle;
    fn deref(&self) -> &NodeHandle {
        &self.handle
    }
}

/// 运行中的节点。
///
/// 停止方式:drop(后台任务随 runtime 结束)或显式 [`Node::shutdown`]。
pub struct Node {
    handle: NodeHandle,
    tasks: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.handle.fmt(f)
    }
}

impl Node {
    /// 启动节点:绑定 UDP/TCP、拉起后台任务、立即通告一次。
    pub async fn start(config: NodeConfig) -> Result<Self> {
        let NodeConfig {
            identity,
            static_keys,
            display_name,
            host_name,
            group,
            status,
            discovery_bind,
            broadcast_addr,
            bootstrap,
            listen_addr,
            heartbeat_every,
            peer_timeout,
            tofu,
            download_dir,
            auto_accept_files,
            app_version,
            avatar_sha256,
        } = config;

        let identity_public = identity.public_key();
        let noise_public = static_keys.public();
        let binding_signature = fq_crypto::sign_static_key_binding(&identity, &noise_public);

        let discovery = Arc::new(DiscoveryEndpoint::bind(
            discovery_bind,
            broadcast_addr,
            bootstrap,
        )?);
        let listener = TcpListener::bind(listen_addr).await?;
        let listen_port = listener
            .local_addr()?
            .port();

        let (events, _unused) = broadcast::channel(EVENT_CAPACITY);
        let shared = Arc::new(Shared {
            self_id: identity.node_id(),
            identity_public,
            static_keys: Arc::new(static_keys),
            binding_signature,
            listen_port,
            profile: Mutex::new(Profile {
                display_name,
                host_name,
                group,
                status,
            }),
            peers: PeerTable::new(),
            tofu: Arc::new(Mutex::new(tofu)),
            conns: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashSet::new()),
            dedup: Mutex::new(DedupWindow::new(
                Duration::from_secs(300),
                4096,
            )),
            events,
            discovery,
            transfers: transfer::TransferManager::new(),
            download_dir: std::sync::RwLock::new(download_dir),
            auto_accept_files,
            app_version,
            avatar_sha256: Mutex::new(avatar_sha256),
            spawned_tasks: Mutex::new(Vec::new()),
        });

        let tasks = vec![
            tokio::spawn(udp_loop(Arc::clone(&shared))),
            tokio::spawn(accept_loop(listener, Arc::clone(&shared))),
            tokio::spawn(heartbeat_loop(Arc::clone(&shared), heartbeat_every)),
            tokio::spawn(sweep_loop(shared.clone(), heartbeat_every, peer_timeout)),
        ];

        let node = Self {
            handle: NodeHandle {
                shared: Arc::clone(&shared),
            },
            tasks,
        };
        node.handle.announce_now().await;
        Ok(node)
    }

    /// 节点句柄(可克隆,供事件泵/CLI/GUI 使用)。
    pub fn handle(&self) -> &NodeHandle {
        &self.handle
    }
}

impl NodeHandle {
    /// 取消一个进行中的传输(发送方中断分块流 / 接收方中断接收并保留 .part)。
    ///
    /// 返回 false = 没有对应会话(可能已结束)。
    pub fn cancel_transfer(&self, token: &str) -> bool {
        self.shared.transfers.cancel(token)
    }

    /// 本节点 ID。
    pub fn node_id(&self) -> NodeId {
        self.shared.self_id
    }

    /// 本节点 Noise 静态公钥(可公开)。
    pub fn static_public(&self) -> [u8; STATIC_KEY_LEN] {
        self.shared.static_keys.public()
    }

    /// 本节点静态密钥指纹(UI 展示用)。
    pub fn fingerprint(&self) -> String {
        fq_crypto::static_key_fingerprint(&self.static_public())
    }

    /// 本节点软件版本(随通告广播,可为 None)。
    pub fn app_version(&self) -> Option<String> {
        self.shared.app_version.clone()
    }

    /// 更新本节点头像哈希并立即通告(对端据此拉取新头像)。
    ///
    /// 传 `None` 表示移除头像。
    pub async fn set_avatar_hash(&self, sha256: Option<String>) {
        *self
            .shared
            .avatar_sha256
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = sha256;
        if let Ok(bytes) = self.shared.build_announce(PresenceEvent::Update) {
            self.shared.discovery.announce(&bytes).await;
        }
    }

    /// 忘记某对端(从对端表移除)。
    ///
    /// 用途:联系人被"删除"后,下一次通告应重新走一遍发现流程(`Discovered`),
    /// 从而重新入库上屏 —— 否则对端表里还有它,心跳不产生变化就不会触发入库。
    /// 返回 `true` 表示原本存在。
    pub fn forget_peer(&self, node_id: NodeId) -> bool {
        self.shared.peers.remove(&node_id).is_some()
    }

    /// 索取对端头像;`known_sha256` 为本端已缓存的对方头像哈希,
    /// `mine` 为本端自己的头像(顺带捎上,省一次往返)。
    pub async fn request_avatar(
        &self,
        to: NodeId,
        known_sha256: Option<String>,
        mine: Option<AvatarPayload>,
    ) -> Result<()> {
        tracing::debug!(target = "fq_net::node", %to, ?known_sha256, "发送头像请求");
        let envelope = Envelope::direct(
            self.shared.self_id,
            to,
            Kind::AvatarRequest(crate::AvatarRequest {
                known_sha256,
                mine,
            }),
        );
        self.shared.send_direct(envelope).await
    }

    /// 发送头像给对端(整图一次发完;调用方需保证 ≤ 帧上限)。
    pub async fn send_avatar(&self, to: NodeId, avatar: AvatarPayload) -> Result<()> {
        let envelope = Envelope::direct(
            self.shared.self_id,
            to,
            Kind::AvatarReply(crate::AvatarReply { avatar }),
        );
        self.shared.send_direct(envelope).await
    }

    /// TCP 监听端口。
    pub fn listen_port(&self) -> u16 {
        self.shared.listen_port
    }

    /// 订阅事件流。
    pub fn events(&self) -> broadcast::Receiver<NodeEvent> {
        self.shared.events.subscribe()
    }

    /// 当前 TOFU 固定表快照(持久化用)。
    pub fn tofu_snapshot(&self) -> TofuStore {
        self.shared
            .tofu
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 当前接收文件保存目录。
    pub fn download_dir(&self) -> PathBuf {
        self.shared
            .download_dir
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 运行期修改接收文件保存目录(对后续开始接收的传输生效)。
    pub fn set_download_dir(&self, dir: PathBuf) {
        let mut guard = self
            .shared
            .download_dir
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = dir;
    }

    /// 同意接收一个文件要约(可选:本次保存位置覆盖)。
    ///
    /// 返回 false = 会话不存在(已结束或从未有此要约)。
    pub fn accept_file_offer(&self, token: &str, dir: Option<PathBuf>) -> bool {
        self.shared.transfers.decide(token, true, dir)
    }

    /// 拒绝接收一个文件要约;发送方会收到中止并停止发送。
    pub fn reject_file_offer(&self, token: &str) -> bool {
        self.shared.transfers.decide(token, false, None)
    }

    /// 当前对端快照(按昵称排序)。
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.shared.peers.list()
    }

    /// 查询单个对端。
    pub fn peer(&self, node_id: &NodeId) -> Option<PeerInfo> {
        self.shared.peers.get(node_id)
    }

    /// 立即广播一次在线通告(不必等心跳)。
    pub async fn announce_now(&self) {
        let datagram = self.shared.build_announce(PresenceEvent::Announce);
        if let Ok(bytes) = datagram {
            self.shared.discovery.announce(&bytes).await;
        }
    }

    /// 更新自己的在线状态并立即通告。
    pub async fn set_status(&self, status: PresenceStatus) {
        self.shared.profile.lock().unwrap_or_else(|p| p.into_inner()).status = status;
        if let Ok(bytes) = self.shared.build_announce(PresenceEvent::Update) {
            self.shared.discovery.announce(&bytes).await;
        }
    }

    /// 更新自己的昵称与分组并立即通告(对方会收到 `[PEER] 上线/更新`)。
    pub async fn set_profile(&self, display_name: &str, group: Option<&str>) {
        {
            let mut profile = self.shared.profile.lock().unwrap_or_else(|p| p.into_inner());
            if !display_name.trim().is_empty() {
                profile.display_name = display_name.trim().to_string();
            }
            profile.group = group
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_string);
        }
        if let Ok(bytes) = self.shared.build_announce(PresenceEvent::Update) {
            self.shared.discovery.announce(&bytes).await;
        }
    }

    /// 当前自己的昵称与分组副本。
    pub fn profile_snapshot(&self) -> (String, Option<String>) {
        let profile = self.shared.profile.lock().unwrap_or_else(|p| p.into_inner());
        (profile.display_name.clone(), profile.group.clone())
    }

    /// 发送文本消息,返回消息 ID。
    pub async fn send_text(&self, to: NodeId, body: &str) -> Result<MsgId> {
        let envelope = Envelope::direct(
            self.shared.self_id,
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
        self.send(envelope).await?;
        Ok(id)
    }

    /// 发送任意报文(自动确保连接可用)。
    ///
    /// 报文的 `to` 必须是点对端(广播/群发在 P3 未实现)。
    pub async fn send(&self, envelope: Envelope) -> Result<()> {
        self.shared.send_direct(envelope).await
    }

    /// 发送一个文件或目录(递归),返回传输令牌。
    ///
    /// 接收方自动接受并保存到 `download_dir`;进度与结果通过
    /// [`NodeEvent::FileProgress`] / [`NodeEvent::FileEntryDone`] /
    /// [`NodeEvent::FileTransferCompleted`] / [`NodeEvent::FileTransferFailed`] 报告。
    pub async fn send_file(
        &self,
        to: NodeId,
        path: impl Into<PathBuf>,
        message: Option<String>,
    ) -> Result<String> {
        transfer::start_send(Arc::clone(&self.shared), to, path.into(), message).await
    }

    /// 发送**更新包**(接收方自动接收;完成后发 [`NodeEvent::UpdatePackageReady`])。
    pub async fn send_update_package(
        &self,
        to: NodeId,
        path: impl Into<PathBuf>,
        version: String,
    ) -> Result<String> {
        transfer::start_send_update(Arc::clone(&self.shared), to, path.into(), version).await
    }

    /// 向对端索取更新包(对端若版本更高会以 UpdateOffer 回发安装文件)。
    pub async fn request_update(&self, to: NodeId) -> Result<()> {
        let envelope = Envelope::direct(
            self.shared.self_id,
            to,
            Kind::UpdateRequest(UpdateRequest {
                requester_version: self
                    .shared
                    .app_version
                    .clone()
                    .unwrap_or_else(|| "0.0.0".to_string()),
                requester_host: Some(
                    self.shared
                        .profile
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .host_name
                        .clone(),
                ),
            }),
        );
        self.shared.send_direct(envelope).await
    }

    /// 主动断开与某对端的连接(测试/管理用;之后发送会自动重连)。
    pub fn disconnect(&self, node_id: &NodeId) {
        if let Some(handle) = self.shared.conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(node_id)
        {
            for task in handle.tasks {
                task.abort();
            }
        }
    }
}

impl Node {
    /// 停止节点(后台任务全部中止;句柄随 drop 失效)。
    ///
    /// 必须同时中止派生任务(连接读写、传输驱动):否则它们持有的
    /// `Arc<Shared>` 会让 UDP socket 保持打开,同端口重绑后数据报
    /// 仍投给僵尸 socket(Windows REUSEADDR 语义)。
    pub fn shutdown(self) {
        for task in self.tasks {
            task.abort();
        }
        self.handle.shared.abort_spawned();
    }
}

impl Shared {
    /// 登记一个派生任务(连接读写/传输驱动),shutdown 时统一中止。
    pub(crate) fn register_task(&self, handle: &JoinHandle<()>) {
        self.lock_tasks().push(handle.abort_handle());
    }

    /// 中止全部派生任务。
    fn abort_spawned(&self) {
        for task in self.lock_tasks().drain(..) {
            task.abort();
        }
    }

    fn lock_tasks(&self) -> std::sync::MutexGuard<'_, Vec<tokio::task::AbortHandle>> {
        self.spawned_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 发送一条点对端报文(连接管理入口;传输驱动也用它发分块)。
    ///
    /// 以 `self: &Arc<Self>` 接收:拨号路径需要把 `Arc<Shared>` 交给连接任务,
    /// 这是"从方法内部拿到自身 Arc"的标准形式。
    pub(crate) async fn send_direct(self: &Arc<Self>, envelope: Envelope) -> Result<()> {
        let to = envelope
            .to
            .ok_or_else(|| Error::Protocol("广播/群发尚未实现,必须指定接收方".into()))?;
        let tx = self.ensure_connection(to).await?;
        tx.send(envelope)
            .await
            .map_err(|_| Error::PeerUnreachable("发送队列已关闭(连接刚被废弃),请重试".into()))
    }

    /// 确保与对端的连接存在,返回其发送队列。
    async fn ensure_connection(self: &Arc<Self>, to: NodeId) -> Result<mpsc::Sender<Envelope>> {
        let deadline = tokio::time::Instant::now() + DIAL_WAIT;
        loop {
            if let Some(handle) = self.conns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&to)
            {
                return Ok(handle.tx.clone());
            }

            // 抢拨号权;抢不到就等别人拨完
            let acquired = self.dialing
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(to);
            if !acquired {
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::PeerUnreachable("并发拨号等待超时".into()));
                }
                sleep(DIAL_POLL).await;
                continue;
            }

            let result = self.dial(to).await;
            self.dialing
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&to);
            return result;
        }
    }

    /// 对目标对端逐端点拨号,成功后建立读写任务并入表。
    async fn dial(self: &Arc<Self>, to: NodeId) -> Result<mpsc::Sender<Envelope>> {
        let peer = self.peers.get(&to).ok_or_else(|| {
            Error::PeerUnreachable(format!("尚未发现对端 {to},无法获知其地址"))
        })?;

        let mut last_error = Error::PeerUnreachable("对端没有可用端点".into());
        for endpoint in &peer.endpoints {
            match Transport::connect_out(&self.static_keys, &peer.noise_static, *endpoint)
                .await
            {
                Ok(transport) => {
                    return Ok(register_connection(transport, to, Arc::clone(self)));
                }
                Err(e) => {
                    tracing::debug!(target = "fq_net::node", %e, %endpoint, "拨号失败,尝试下一端点");
                    last_error = e;
                }
            }
        }
        Err(last_error)
    }
}

impl Shared {
    fn emit(&self, event: NodeEvent) {
        // 无订阅者是常态(例如 CLI 未监听事件),忽略发送错误
        let _ = self.events.send(event);
    }

    fn capabilities() -> Capabilities {
        Capabilities::TEXT
            | Capabilities::NOISE_IK
            | Capabilities::READ_RECEIPT
            | Capabilities::TYPING
            | Capabilities::AVATAR
    }

    fn build_announce(&self, event: PresenceEvent) -> Result<Vec<u8>> {
        let profile = self.profile.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let noise_static = self.static_keys.public();
        let info = PresenceInfo {
            event,
            display_name: profile.display_name,
            host_name: profile.host_name,
            status: profile.status,
            group: profile.group,
            port: self.listen_port,
            // 可达端点由接收方从 UDP 源地址推导;多网卡通告留待 fq-core 实现
            endpoints: vec![],
            public_key: self.identity_public.to_vec(),
            noise_static: noise_static.to_vec(),
            binding_signature: self.binding_signature.to_vec(),
            capabilities: Self::capabilities(),
            avatar_sha256: self
                .avatar_sha256
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            app_version: self.app_version.clone(),
        };
        let envelope = Envelope::broadcast(self.self_id, Kind::Presence(info));
        Ok(codec::encode_framed(&envelope)?)
    }

    /// 处理一个发现数据报(完整安全链在此执行)。
    async fn handle_datagram(&self, datagram: &[u8], source: SocketAddr) {
        if let Err(e) = self.handle_datagram_inner(datagram, source).await {
            tracing::debug!(target = "fq_net::node", %e, %source, "丢弃无效发现报文");
        }
    }

    async fn handle_datagram_inner(&self, datagram: &[u8], source: SocketAddr) -> Result<()> {
        let mut decoder = FrameDecoder::with_default_limit();
        decoder.feed(datagram)?;
        let payload = decoder
            .next_frame()?
            .ok_or_else(|| Error::Protocol("空数据报".into()))?;
        let envelope = codec::decode_framed(&payload)?;

        // 自己的广播回环
        if envelope.from == self.self_id {
            return Ok(());
        }

        let Kind::Presence(info) = envelope.kind else {
            return Err(Error::Protocol(format!(
                "UDP 发现通道只接受 presence,收到 {}",
                envelope.kind_name()
            )));
        };

        // ① 身份一致性:声明的 from 必须由公钥派生
        let identity_public: [u8; 32] = info
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| Error::Protocol("Ed25519 公钥长度错误".into()))?;
        let noise_static: [u8; STATIC_KEY_LEN] = info
            .noise_static
            .as_slice()
            .try_into()
            .map_err(|_| Error::Protocol("Noise 静态公钥长度错误".into()))?;
        let signature: [u8; 64] = info
            .binding_signature
            .as_slice()
            .try_into()
            .map_err(|_| Error::Protocol("绑定签名长度错误".into()))?;
        if NodeId::from_public_key(&identity_public) != envelope.from {
            return Err(Error::PeerUntrusted("NodeId 与公钥不匹配".into()));
        }

        // ② 绑定签名:静态握手密钥确实属于该身份
        let node_id = verify_static_key_binding(&identity_public, &noise_static, &signature)
            .map_err(|e| Error::PeerUntrusted(format!("绑定签名无效: {e}")))?;

        // ③ TOFU(独立作用域:MutexGuard 不是 Send,不能跨 .await 存活)
        let first_contact = {
            let mut tofu = self.tofu.lock().unwrap_or_else(|p| p.into_inner());
            match tofu.verify(&node_id, &noise_static) {
                TrustDecision::FirstUse => {
                    // 首次接触自动固定(SSH 模型);密钥变化时会触发告警
                    tofu.pin(node_id, &noise_static);
                    true
                }
                TrustDecision::Trusted => false,
                TrustDecision::Changed { pinned, presented } => {
                    drop(tofu);
                    tracing::warn!(target: "fq_net::node", %node_id, %pinned, %presented, "TOFU 密钥变更,拒绝通告");
                    self.emit(NodeEvent::TrustWarning {
                        node_id,
                        pinned,
                        presented,
                    });
                    return Err(Error::PeerUntrusted("静态密钥与固定值不一致".into()));
                }
            }
        };

        // Leave 通告
        if info.event == PresenceEvent::Leave {
            if self.peers.mark_offline(&node_id) {
                self.emit(NodeEvent::PeerLost { node_id });
            }
            return Ok(());
        }

        // 端点:通告自带的 + 由 UDP 源地址推导的(最可靠,放 hint)
        let mut endpoints = Vec::new();
        for raw in &info.endpoints {
            if let Ok(addr) = raw.parse::<SocketAddr>() {
                endpoints.push(addr);
            }
        }
        let hint = SocketAddr::new(source.ip(), info.port);

        let peer = PeerInfo {
            node_id,
            display_name: info.display_name,
            host_name: info.host_name,
            status: if info.status == PresenceStatus::Unknown {
                PresenceStatus::Online
            } else {
                info.status
            },
            group: info.group,
            capabilities: info.capabilities,
            ed25519_public_key: identity_public,
            noise_static,
            endpoints,
            app_version: info.app_version,
            avatar_sha256: info.avatar_sha256,
            last_seen: std::time::Instant::now(),
        };

        // 事件类型要在移动字段前取出(下面回发逻辑要判断是不是 Announce)
        let presence_event = info.event;

        match self.peers.apply(peer.clone(), hint) {
            Some(PeerChange::Discovered) => {
                tracing::info!(target = "fq_net::node", %node_id, name = %peer.display_name, "发现新对端");
                // ANSENTRY 等效:对方是新节点,立即单播回自己的通告,
                // 让对方不用等我们的下一个心跳就能发现我们(IPMSG 协议核心机制)
                if let Ok(bytes) = self.build_announce(PresenceEvent::Announce) {
                    let socket = self.discovery.socket();
                    if let Err(e) = socket.send_to(&bytes, source).await {
                        tracing::debug!(target = "fq_net::node", %e, %source, "ANSENTRY 回发失败");
                    }
                }
                self.emit(NodeEvent::PeerDiscovered {
                    peer,
                    first_contact,
                });
            }
            Some(PeerChange::Updated) => {
                self.emit(NodeEvent::PeerUpdated { peer });
            }
            None => {
                // 已知对端**主动通告**(Announce,例如对方刚点"刷新"或刚删过我):
                // 单播回一发,让对方立刻重新发现我。
                // 刻意用 `Update` 而不是 `Announce` —— 否则双方会互相回发形成风暴。
                // 心跳(Heartbeat)不触发,避免每 20s 的无谓流量。
                if presence_event == PresenceEvent::Announce {
                    if let Ok(bytes) = self.build_announce(PresenceEvent::Update) {
                        let socket = self.discovery.socket();
                        if let Err(e) = socket.send_to(&bytes, source).await {
                            tracing::debug!(target = "fq_net::node", %e, %source, "刷新回发失败");
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// UDP 接收循环。
async fn udp_loop(shared: Arc<Shared>) {
    let socket = shared.discovery.socket();
    let mut buf = vec![0u8; fq_proto::MAX_FRAME_BYTES + 64];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((size, source)) => {
                shared.handle_datagram(&buf[..size], source).await;
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::ConnectionReset {
                    // Windows/UDP:ICMP port-unreachable 的上报;即使禁用了
                    // SIO_UDP_CONNRESET 也可能偶发 —— 直接重试,不睡也不刷日志
                    continue;
                }
                tracing::warn!(target = "fq_net::node", %e, "UDP 接收失败");
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// TCP 接受循环。
async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    match Transport::accept_in(stream, &shared.static_keys).await {
                        Ok((transport, remote_static)) => {
                            let Some(node_id) = shared.peers.find_by_noise_static(&remote_static)
                            else {
                                // 诊断信息:对端表为空通常说明"同机双实例共享 UDP 24250,
                                // 单播通告被另一个进程收走"(等心跳广播即可恢复)
                                let known: Vec<String> = shared
                                    .peers
                                    .list()
                                    .iter()
                                    .map(|p| hex::encode(&p.noise_static[..4]))
                                    .collect();
                                tracing::debug!(
                                    target = "fq_net::node",
                                    %peer_addr,
                                    presented = %hex::encode(&remote_static[..4]),
                                    ?known,
                                    "入站连接的静态密钥没有对应已知对端,拒绝"
                                );
                                return;
                            };
                            register_connection(transport, node_id, Arc::clone(&shared));
                        }
                        Err(e) => {
                            tracing::debug!(target = "fq_net::node", %e, %peer_addr, "入站握手失败");
                        }
                    }
                });
            }
            Err(e) => {
                tracing::warn!(target = "fq_net::node", %e, "TCP accept 失败");
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// 把一条已握手的传输注册进连接表并拉起读写任务,返回发送队列。
///
/// 策略:**新连接替换旧连接**。旧连接可能是对端重连前的半死连接,
/// 保留它会让重连后的消息全部写进黑洞;替换则最坏代价是并发双向拨号时
/// 多一次握手(拨号互斥已把概率压到很低)。
fn register_connection(
    transport: Transport,
    node_id: NodeId,
    shared: Arc<Shared>,
) -> mpsc::Sender<Envelope> {
    let mut conns = shared.conns.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(old) = conns.remove(&node_id) {
        tracing::debug!(target = "fq_net::node", %node_id, "替换旧连接");
        for task in old.tasks {
            task.abort();
        }
    }
    let (read, write) = transport.into_parts();
    let (tx, rx) = mpsc::channel(CONN_QUEUE);
    let tasks = vec![
        tokio::spawn(writer_task(write, rx)),
        tokio::spawn(reader_task(read, node_id, tx.clone(), Arc::clone(&shared))),
    ];
    // 登记:shutdown 时统一中止,防止 Arc<Shared> 泄漏拖住 UDP socket
    shared.register_task(&tasks[0]);
    shared.register_task(&tasks[1]);
    conns.insert(node_id, ConnHandle { tx: tx.clone(), tasks });
    tx
}

/// 写任务:唯一做"加密 + 写帧"的地方(串行 = nonce 顺序安全)。
async fn writer_task(mut write: transport::WritePart, mut rx: mpsc::Receiver<Envelope>) {
    while let Some(envelope) = rx.recv().await {
        if let Err(e) = write.send_envelope(&envelope).await {
            tracing::debug!(target = "fq_net::node", %e, "写连接失败,写任务退出");
            return;
        }
    }
}

/// 读任务:解密 → 去重 → 事件;任何失败清理连接。
async fn reader_task(
    mut read: transport::ReadPart,
    node_id: NodeId,
    tx: mpsc::Sender<Envelope>,
    shared: Arc<Shared>,
) {
    loop {
        match read.recv_envelope().await {
            Ok(envelope) => {
                let fresh = shared.dedup
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(envelope.id);
                if !fresh {
                    tracing::debug!(target = "fq_net::node", id = %envelope.id, "丢弃重复消息");
                    continue;
                }
                let from = envelope.from;
                if transfer::is_transfer_kind(&envelope.kind) {
                    // 文件传输报文交给会话状态机,不作为普通消息上报
                    transfer::route(&shared, envelope).await;
                } else {
                    shared.emit(NodeEvent::MessageReceived { from, envelope });
                }
            }
            Err(e) => {
                tracing::debug!(target = "fq_net::node", %e, %node_id, "读连接终止");
                // 仅当表里还是"本代"连接时清理,避免误删后来重建的连接
                let stale = {
                    let conns = shared.conns.lock().unwrap_or_else(|p| p.into_inner());
                    match conns.get(&node_id) {
                        Some(handle) => !handle.tx.same_channel(&tx),
                        None => false,
                    }
                };
                if !stale {
                    shared.conns
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&node_id);
                }
                return;
            }
        }
    }
}

/// 心跳循环:周期性通告。
async fn heartbeat_loop(shared: Arc<Shared>, every: Duration) {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // 首个 tick 立即返回,跳过
    loop {
        ticker.tick().await;
        if let Ok(bytes) = shared.build_announce(PresenceEvent::Heartbeat) {
            shared.discovery.announce(&bytes).await;
        }
    }
}

/// 清扫循环:心跳超时 → 离线事件。
async fn sweep_loop(shared: Arc<Shared>, heartbeat: Duration, peer_timeout: Duration) {
    let every = (peer_timeout / 3).max(heartbeat).min(Duration::from_secs(30));
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        for node_id in shared.peers.sweep(peer_timeout) {
            tracing::info!(target = "fq_net::node", %node_id, "对端心跳超时,标记离线");
            shared.emit(NodeEvent::PeerLost { node_id });
        }
    }
}
