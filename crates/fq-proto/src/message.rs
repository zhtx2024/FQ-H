//! 报文类型定义(协议 v1)。
//!
//! # 前向兼容契约(强制)
//!
//! * 所有结构体字段都带 `#[serde(default)]` —— 新版本新增字段不会让老版本解码失败。
//! * [`Kind`] 的未知 `kind` 取值落入 [`Kind::Unknown`] 而非报错。
//! * 所有字符串枚举(状态/格式/文件类型)遇到未知取值时落到各自的 `Unknown` 兜底值,
//!   不抛错、不丢包。
//! * **禁止**使用固定长度数组承载可扩展结构(MessagePack 数组是定长的,加字段即破坏兼容)。
//!   因此本协议统一使用 **map 编码**(`rmp_serde::to_vec_named`),详见 [`crate::codec`]。
//!
//! # 字段命名空间约束
//!
//! [`Kind`] 采用内部标签(`#[serde(tag = "kind")]`),即载荷字段与 [`Envelope`] 字段
//! **共用同一个 map 命名空间**。因此载荷字段名不得使用:`v` / `id` / `from` / `to` /
//! `ts_ms` / `kind`。

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;
use crate::capability::Capabilities;
use crate::ids::{MsgId, NodeId};

/// 生成"字符串枚举 + 未知值兜底 + 双向转换"的样板代码。
///
/// 契约:必须用 `catch_all = ...` 指定兜底变体;未知取值一律映射到它,
/// 从而保证协议前向兼容。
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $lit:literal ),+ $(,)?
        }
        catch_all = $catch:ident => $catch_lit:literal;
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(from = "String", into = "String")]
        $vis enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// 未知取值兜底:解码不会因对端新增取值而失败。
            $catch,
        }

        impl $name {
            /// 全部已知取值(不含兜底值)。
            $vis const ALL: &'static [$name] = &[ $($name::$variant),+ ];

            /// 转为线上字符串字面量。
            $vis const fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $lit, )+
                    $name::$catch => $catch_lit,
                }
            }

            /// 解析线上字符串;未知取值落到兜底变体。
            $vis fn parse(raw: &str) -> Self {
                match raw {
                    $( $lit => $name::$variant, )+
                    _ => $name::$catch,
                }
            }
        }

        impl From<String> for $name {
            fn from(raw: String) -> Self {
                Self::parse(&raw)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.as_str().to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum! {
    /// 在线状态。
    #[derive(Default)]
    pub enum PresenceStatus {
        /// 在线可用。
        #[default] Online => "online",
        /// 离开。
        Away => "away",
        /// 忙碌。
        Busy => "busy",
        /// 勿扰。
        Dnd => "dnd",
        /// 离线。
        Offline => "offline",
    }
    catch_all = Unknown => "unknown";
}

string_enum! {
    /// 在线事件类型。
    pub enum PresenceEvent {
        /// 上线通告。
        Announce => "announce",
        /// 状态/昵称等更新。
        Update => "update",
        /// 主动下线。
        Leave => "leave",
        /// 心跳保活。
        Heartbeat => "heartbeat",
    }
    catch_all = Unknown => "unknown";
}

string_enum! {
    /// 文本消息格式。
    #[derive(Default)]
    pub enum TextFormat {
        /// 纯文本。
        #[default] Plain => "plain",
        /// Markdown 富文本。
        Markdown => "markdown",
    }
    catch_all = Unknown => "unknown";
}

string_enum! {
    /// 回执状态。
    pub enum AckStatus {
        /// 已送达对端。
        Delivered => "delivered",
        /// 对端已读。
        Read => "read",
    }
    catch_all = Unknown => "unknown";
}

string_enum! {
    /// 正在输入状态。
    pub enum TypingState {
        /// 开始输入。
        Started => "started",
        /// 停止输入。
        Stopped => "stopped",
    }
    catch_all = Unknown => "unknown";
}

string_enum! {
    /// 文件条目类型。
    pub enum FileKind {
        /// 普通文件。
        File => "file",
        /// 目录。
        Dir => "dir",
        /// 符号链接(仅记录,不跟随)。
        Symlink => "symlink",
    }
    catch_all = Unknown => "unknown";
}

/// 当前 Unix 毫秒时间戳。系统时钟异常时回退为 0(不 panic)。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 报文信封:所有报文的公共头部。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// 协议版本。解码方自行决定是否兼容(见 [`Envelope::is_protocol_compatible`])。
    pub v: u16,
    /// 消息唯一 ID(UUIDv7,可按时间排序)。
    pub id: MsgId,
    /// 发送方节点 ID。
    pub from: NodeId,
    /// 目标节点 ID;`None` 表示广播/群组。
    #[serde(default)]
    pub to: Option<NodeId>,
    /// 发送时刻的 Unix 毫秒时间戳。
    pub ts_ms: i64,
    /// 载荷。
    ///
    /// 使用 `flatten` 把内部标签枚举的 `kind` 标签与载荷字段**内联**到本结构体的
    /// map 中,得到扁平线缆格式:
    ///
    /// ```text
    /// { v, id, from, to, ts_ms, kind: "text", body, format, … }
    /// ```
    ///
    /// 若不 flatten,会退化成 `{…, kind: { kind: "text", body: … }}` —— 同名键嵌套,
    /// 既浪费字节也让其它语言实现容易写错。
    #[serde(flatten)]
    pub kind: Kind,
}

impl Envelope {
    /// 构造一条报文。
    pub fn new(from: NodeId, to: Option<NodeId>, kind: Kind) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: MsgId::now_v7(),
            from,
            to,
            ts_ms: now_ms(),
            kind,
        }
    }

    /// 构造点对点报文。
    pub fn direct(from: NodeId, to: NodeId, kind: Kind) -> Self {
        Self::new(from, Some(to), kind)
    }

    /// 构造广播报文。
    pub fn broadcast(from: NodeId, kind: Kind) -> Self {
        Self::new(from, None, kind)
    }

    /// 本端是否能处理该版本。
    pub fn is_protocol_compatible(&self) -> bool {
        self.v <= PROTOCOL_VERSION
    }

    /// 载荷类型的线缆名称(日志/指标用)。
    pub fn kind_name(&self) -> &'static str {
        self.kind.name()
    }
}

/// 在线状态信息(发现与心跳共用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceInfo {
    /// 事件类型。
    pub event: PresenceEvent,
    /// 展示昵称。
    pub display_name: String,
    /// 主机名。
    pub host_name: String,
    /// 在线状态。
    #[serde(default)]
    pub status: PresenceStatus,
    /// 所属分组(可选)。
    #[serde(default)]
    pub group: Option<String>,
    /// 监听端口。
    #[serde(default = "default_port")]
    pub port: u16,
    /// 本机可用地址(`ip:port`),用于多网卡场景下直连。
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Ed25519 公钥(32 字节),身份与 NodeId 派生的依据。
    #[serde(default, with = "serde_bytes")]
    pub public_key: Vec<u8>,
    /// Noise IK 静态公钥(X25519,32 字节)。发起加密握手必需。
    #[serde(default, with = "serde_bytes")]
    pub noise_static: Vec<u8>,
    /// 身份密钥对 `noise_static` 的绑定签名(Ed25519,64 字节,
    /// 域 `feiqiu-r/v1/noise-static-key-binding`)。
    /// 接收方**必须**验证:签名有效且 `NodeId == SHA-256(public_key)[0..16]`,
    /// 否则整条通告按伪造丢弃。
    #[serde(default, with = "serde_bytes")]
    pub binding_signature: Vec<u8>,
    /// 能力声明。
    #[serde(default)]
    pub capabilities: Capabilities,
    /// 头像内容的 SHA-256(hex);为空表示无头像。
    #[serde(default)]
    pub avatar_sha256: Option<String>,
    /// 软件版本(如 `0.1.0`);用于局域网内的版本发现与更新提示。
    #[serde(default)]
    pub app_version: Option<String>,
}

/// 协议默认端口。
///
/// 刻意避开 IPMSG 的 2425:本协议不与飞秋/飞鸽互通,同机若同时运行老客户端,
/// 监听同一端口会导致双方都收到无法解析的报文。
pub const DEFAULT_PORT: u16 = 24250;

const fn default_port() -> u16 {
    DEFAULT_PORT
}

/// 文本消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBody {
    /// 消息正文(UTF-8)。
    pub body: String,
    /// 正文格式。
    #[serde(default)]
    pub format: TextFormat,
    /// 引用的原消息 ID。
    #[serde(default)]
    pub reply_to: Option<MsgId>,
    /// 被 @ 的节点。
    #[serde(default)]
    pub mentions: Vec<NodeId>,
    /// 群组标识;为 `None` 表示单聊。
    #[serde(default)]
    pub group_id: Option<String>,
    /// 群名称(发送方携带,便于接收方自动建群/显示;单聊为空)。
    #[serde(default)]
    pub group_name: Option<String>,
}

/// 送达/已读回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckBody {
    /// 被确认的消息 ID。
    pub ack_id: MsgId,
    /// 回执状态。
    pub status: AckStatus,
}

/// 正在输入提示(瞬态,不应落库)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypingBody {
    /// 输入状态。
    pub state: TypingState,
    /// 关联会话/消息(可选)。
    #[serde(default)]
    pub thread: Option<MsgId>,
}

/// 文件或目录清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifest {
    /// 传输根名称(单文件为文件名,目录为目录名)。
    pub root_name: String,
    /// 全部条目字节数之和。
    pub total_bytes: u64,
    /// 条目列表(目录传输时按相对路径排列)。
    pub entries: Vec<FileEntry>,
}

impl FileManifest {
    /// 条目数量。
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// 是否包含目录条目。
    pub fn has_directories(&self) -> bool {
        self.entries.iter().any(|e| e.kind == FileKind::Dir)
    }
}

/// 清单中的单个条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对路径,统一用 `/` 分隔。
    pub path: String,
    /// 字节大小(目录为 0)。
    pub size: u64,
    /// 修改时间(Unix 毫秒)。
    #[serde(default)]
    pub mtime_ms: i64,
    /// 条目类型。
    pub kind: FileKind,
    /// 文件内容 SHA-256(hex);目录为空。
    #[serde(default)]
    pub sha256: Option<String>,
}

impl FileEntry {
    /// 是否为目录。
    pub fn is_dir(&self) -> bool {
        self.kind == FileKind::Dir
    }

    /// 是否为普通文件。
    pub fn is_file(&self) -> bool {
        self.kind == FileKind::File
    }
}

/// 文件传输要约。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    /// 传输令牌(同一次传输的所有报文共用)。
    pub token: String,
    /// 清单。
    pub manifest: FileManifest,
    /// 附带留言。
    #[serde(default)]
    pub message: Option<String>,
}

/// 更新包要约:与文件要约同形,但接收方按**更新**处理(自动接收 + 就绪后走安装流程)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateOffer {
    /// 传输令牌。
    pub token: String,
    /// 清单。
    pub manifest: FileManifest,
    /// 发送方版本(如 `0.4.0`);接收方据此确认确实更新。
    pub version: String,
    /// 附带说明。
    #[serde(default)]
    pub message: Option<String>,
}

/// 更新包请求(老版本节点 → 新版本节点)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRequest {
    /// 请求方当前版本。
    pub requester_version: String,
    /// 请求方主机名(服务端日志/审计用)。
    #[serde(default)]
    pub requester_host: Option<String>,
}

/// 窗口抖动(飞秋/IPMSG 的经典功能:提醒对方注意)。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ShakeBody {
    /// 附带说明(可选)。
    #[serde(default)]
    pub reason: Option<String>,
}

/// 撤回消息(仅发送方在时限内可撤回自己的消息)。
///
/// 只带被撤回的消息 ID —— 消息 ID 是全局唯一 UUIDv7,接收方按 ID 在自己的
/// 历史里标记为「已撤回」即可,不需要复制正文(与引用回复同一思路)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallBody {
    /// 被撤回的消息 ID。
    pub message_id: MsgId,
}

/// 头像内容(整图一次发完;接收方**必须**校验 SHA-256)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarPayload {
    /// 图片内容的 SHA-256(hex)。
    pub sha256: String,
    /// MIME 类型(如 `image/png`),供展示方判断。
    #[serde(default)]
    pub mime: String,
    /// 图片原始字节(PNG/JPEG)。
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl AvatarPayload {
    /// 内容哈希是否与声明一致。
    pub fn verify(&self) -> bool {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&self.data);
        hex::encode(hasher.finalize()) == self.sha256
    }

    /// 从原始字节构造(自动计算哈希)。
    pub fn new(data: Vec<u8>, mime: impl Into<String>) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&data);
        Self {
            sha256: hex::encode(hasher.finalize()),
            mime: mime.into(),
            data,
        }
    }
}

/// 头像请求(索取对端头像原图)。
///
/// 头像内容只应在**变更时**拉取:请求方带上已知哈希,对端相同则无需回发。
/// 请求方**顺带捎上自己的头像**(`mine`),于是一次往返即可双向同步 ——
/// 双方各自发起会"互拨"替换连接并 RST 掉在途报文(踩过坑)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarRequest {
    /// 请求方当前缓存的**对方**头像哈希(hex;`None` = 尚未缓存)。
    #[serde(default)]
    pub known_sha256: Option<String>,
    /// 请求方自己的头像(可为 `None` = 没有头像)。
    #[serde(default)]
    pub mine: Option<AvatarPayload>,
}

/// 头像应答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarReply {
    /// 对端头像内容。
    pub avatar: AvatarPayload,
}

/// 请求文件数据(支持断点续传)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRequest {
    /// 传输令牌。
    pub token: String,
    /// 目标条目相对路径。
    pub path: String,
    /// 起始偏移(续传时由接收方给出已落盘长度)。
    pub offset: u64,
    /// 建议分块大小(字节);为空表示由发送方决定。
    #[serde(default)]
    pub chunk_size: Option<u32>,
}

/// 文件数据分块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunk {
    /// 传输令牌。
    pub token: String,
    /// 条目相对路径。
    pub path: String,
    /// 本块在文件中的偏移。
    pub offset: u64,
    /// 数据(MessagePack bin 编码,避免按整数数组膨胀 3~5 倍)。
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// 单个文件传输完成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDone {
    /// 传输令牌。
    pub token: String,
    /// 条目相对路径。
    pub path: String,
    /// 整个文件的 SHA-256(hex),接收方据此校验。
    #[serde(default)]
    pub sha256: Option<String>,
}

/// 中止传输。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAbort {
    /// 传输令牌。
    pub token: String,
    /// 条目相对路径。
    pub path: String,
    /// 中止原因(人类可读)。
    pub reason: String,
}

/// 保活探测/应答载荷。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingBody {
    /// 随机数,应答需原样回填。
    pub nonce: u64,
}

/// 报文载荷。
///
/// 线缆表现为 `{"kind": "<tag>", ...载荷字段}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// 在线状态(发现/心跳)。
    Presence(PresenceInfo),
    /// 文本消息。
    Text(TextBody),
    /// 回执。
    Ack(AckBody),
    /// 正在输入。
    Typing(TypingBody),
    /// 窗口抖动(飞秋经典功能;老版本会忽略,不影响兼容)。
    Shake(ShakeBody),
    /// 消息撤回(接收方按消息 ID 标记为已撤回;老版本会走 Unknown 分支忽略)。
    Recall(RecallBody),
    /// 文件要约。
    FileOffer(FileOffer),
    /// 更新包要约(接收方自动接收并走安装流程)。
    UpdateOffer(UpdateOffer),
    /// 更新包请求(向新版本节点索取安装包)。
    UpdateRequest(UpdateRequest),
    /// 头像请求(对端头像变更时拉取)。
    AvatarRequest(AvatarRequest),
    /// 头像应答(整图一次发完)。
    AvatarReply(AvatarReply),
    /// 文件数据请求。
    FileRequest(FileRequest),
    /// 文件数据分块。
    FileChunk(FileChunk),
    /// 单文件完成。
    FileDone(FileDone),
    /// 传输中止。
    FileAbort(FileAbort),
    /// 保活探测。
    Ping(PingBody),
    /// 保活应答。
    Pong(PingBody),
    /// 未知类型:保证前向兼容,解码不失败。载荷内容被丢弃(仅记录日志)。
    #[serde(other)]
    Unknown,
}

impl Kind {
    /// 线缆上的类型名。
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Presence(_) => "presence",
            Kind::Text(_) => "text",
            Kind::Ack(_) => "ack",
            Kind::Typing(_) => "typing",
            Kind::Shake(_) => "shake",
            Kind::Recall(_) => "recall",
            Kind::FileOffer(_) => "file_offer",
            Kind::UpdateOffer(_) => "update_offer",
            Kind::UpdateRequest(_) => "update_request",
            Kind::AvatarRequest(_) => "avatar_request",
            Kind::AvatarReply(_) => "avatar_reply",
            Kind::FileRequest(_) => "file_request",
            Kind::FileChunk(_) => "file_chunk",
            Kind::FileDone(_) => "file_done",
            Kind::FileAbort(_) => "file_abort",
            Kind::Ping(_) => "ping",
            Kind::Pong(_) => "pong",
            Kind::Unknown => "unknown",
        }
    }

    /// 是否为瞬态报文(不应落库/不应重传)。
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Kind::Typing(_) | Kind::Ping(_) | Kind::Pong(_) | Kind::Presence(_)
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn string_enums_roundtrip_all_variants() {
        macro_rules! check {
            ($ty:ty) => {
                for value in <$ty>::ALL {
                    let text = value.as_str();
                    assert_eq!(<$ty>::parse(text), *value, "{} 往返失败", text);
                    assert_eq!(<$ty>::from(text.to_string()), *value);
                }
            };
        }
        check!(PresenceStatus);
        check!(PresenceEvent);
        check!(TextFormat);
        check!(AckStatus);
        check!(TypingState);
        check!(FileKind);
    }

    #[test]
    fn string_enums_fall_back_on_unknown_value() {
        assert_eq!(
            PresenceStatus::parse("in-a-meeting"),
            PresenceStatus::Unknown
        );
        assert_eq!(PresenceEvent::parse("future-event"), PresenceEvent::Unknown);
        assert_eq!(FileKind::parse("socket"), FileKind::Unknown);
        assert_eq!(AckStatus::parse(""), AckStatus::Unknown);
    }

    #[test]
    fn envelope_carries_protocol_version() {
        let env = Envelope::broadcast(
            NodeId::from_bytes([1u8; 16]),
            Kind::Ping(PingBody { nonce: 7 }),
        );
        assert_eq!(env.v, PROTOCOL_VERSION);
        assert!(env.is_protocol_compatible());
        assert_eq!(env.kind_name(), "ping");
    }

    #[test]
    fn transient_classification() {
        assert!(
            Kind::Typing(TypingBody {
                state: TypingState::Started,
                thread: None
            })
            .is_transient()
        );
        assert!(
            !Kind::Text(TextBody {
                body: "hi".into(),
                format: TextFormat::Plain,
                reply_to: None,
                mentions: vec![],
                group_id: None,
                group_name: None
            })
            .is_transient()
        );
    }
}
