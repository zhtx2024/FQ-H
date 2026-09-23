//! 传输会话:发送方/接收方状态机。
//!
//! 流程(见 docs/PROTOCOL.md §6.3):
//!
//! ```text
//! 发送方                                     接收方
//!   │ file_offer(token, manifest) ───────────▶ (自动接受,登记会话)
//!   │ ◀────────── file_request(path, offset) │   offset = 已落盘 .part 大小
//!   │ file_chunk × N(offset 递增) ───────────▶ (追加写 + 增量哈希)
//!   │ file_done(全文件 SHA-256) ─────────────▶ (校验 → .part 改名正式文件)
//!   │ ◀────────── file_request(下一 path)    │   逐条目推进
//!   │            全部条目完成 → 会话结束
//! ```
//!
//! 断点续传:落盘文件是 `<目标>.part`;重新发送同一文件时接收方按
//! `.part` 现有长度请求 offset,双方各自补算前缀哈希,**不做二次全量读**。
//!
//! 流控:会话队列(64 条 × ≤512KiB)写满时,读任务在 dispatch 处 await,
//! TCP 背压自然传导到发送方 —— 无需额外窗口协议。

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fq_proto::{
    Envelope, FileAbort, FileChunk, FileDone, FileEntry, FileKind, FileManifest, FileOffer,
    FileRequest, Kind, MsgId, NodeId,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::Shared;
use crate::error::Result;
use crate::transfer::manifest::sanitize_entry_path;

/// 默认分块大小(字节)。
pub const CHUNK_SIZE: usize = 256 * 1024;
/// 会话事件队列深度(条)。
const SESSION_QUEUE: usize = 64;
/// 会话空闲超时:超时后判传输中断。
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 接收确认阶段的等待上限(比发送方空闲超时长,让对端的中止自然结束)。
const PENDING_TIMEOUT: Duration = Duration::from_secs(300);
/// 进度事件最小间隔(避免刷爆事件通道)。
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// 传输方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// 本端发送。
    Sending,
    /// 本端接收。
    Receiving,
}

/// 传输会话注册表:按 (对端, token) 把报文派发给对应会话任务。
#[derive(Debug, Default)]
pub struct TransferManager {
    sessions: std::sync::Mutex<HashMapKeyed>,
}

/// 会话任务收到的消息:网络报文,或本地控制决定(接收确认流的用户决策)。
#[allow(clippy::large_enum_variant)] // Envelope 大但常见;装箱反而增加每次收发的开销
pub(crate) enum SessionMsg {
    /// 网络侧报文(分块/完成/中止/请求)。
    Envelope(Envelope),
    /// 用户同意接收;`dir` 为本次保存位置覆盖(可选)。
    Accept { dir: Option<PathBuf> },
    /// 用户拒绝接收。
    Reject,
    /// 用户取消进行中的传输。
    Cancel,
}

/// 会话注册项:发送端 + 取消标志(流式发送在分块之间检查)。
#[derive(Debug)]
pub(crate) struct SessionEntry {
    tx: mpsc::Sender<SessionMsg>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

type HashMapKeyed = std::collections::HashMap<String, SessionEntry>;

impl TransferManager {
    /// 空管理器。
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, token: String, tx: mpsc::Sender<SessionMsg>) {
        self.lock().insert(
            token,
            SessionEntry {
                tx,
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
    }

    fn remove(&self, token: &str) {
        self.lock().remove(token);
    }

    fn contains(&self, token: &str) -> bool {
        self.lock().contains_key(token)
    }

    /// 取某会话的取消标志(流式发送在分块循环里轮询)。
    fn cancel_flag(&self, token: &str) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.lock().get(token).map(|e| e.cancel.clone())
    }

    /// 派发一个传输报文给会话;没有会话(已结束/不存在)返回 false。
    async fn dispatch(&self, token: String, envelope: Envelope) -> bool {
        let Some(entry) = self.lock().get(&token).map(|e| e.tx.clone()) else {
            return false;
        };
        // 队列满时挂起 → 连接读循环停 → TCP 背压传导到发送方
        entry.send(SessionMsg::Envelope(envelope)).await.is_ok()
    }

    /// 用户对接收要约的裁决(接收确认流)。
    ///
    /// 返回 false = 没有对应会话(已结束或不存在)。
    pub(crate) fn decide(&self, token: &str, accepted: bool, dir: Option<PathBuf>) -> bool {
        let Some(entry) = self.lock().get(token).map(|e| e.tx.clone()) else {
            return false;
        };
        let message = if accepted {
            SessionMsg::Accept { dir }
        } else {
            SessionMsg::Reject
        };
        // 决策只有一条,队列 64 深度,try_send 足够
        entry.try_send(message).is_ok()
    }

    /// 取消一个进行中的传输:置位取消标志并通知会话任务。
    ///
    /// 返回 false = 没有对应会话(可能已结束)。
    pub(crate) fn cancel(&self, token: &str) -> bool {
        let entry = {
            let guard = self.lock();
            let Some(entry) = guard.get(token) else {
                return false;
            };
            entry
                .cancel
                .store(true, std::sync::atomic::Ordering::SeqCst);
            entry.tx.clone()
        };
        entry.try_send(SessionMsg::Cancel).is_ok()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMapKeyed> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn token_of(kind: &Kind) -> Option<&str> {
    match kind {
        Kind::FileOffer(o) => Some(&o.token),
        Kind::UpdateOffer(o) => Some(&o.token),
        Kind::FileRequest(r) => Some(&r.token),
        Kind::FileChunk(c) => Some(&c.token),
        Kind::FileDone(d) => Some(&d.token),
        Kind::FileAbort(a) => Some(&a.token),
        _ => None,
    }
}

/// 判断报文是否属于**传输会话**流量(由节点读循环调用路由)。
///
/// 注意:`UpdateRequest` 是控制消息,不进会话,由应用层处理。
pub fn is_transfer_kind(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::FileOffer(_)
            | Kind::UpdateOffer(_)
            | Kind::FileRequest(_)
            | Kind::FileChunk(_)
            | Kind::FileDone(_)
            | Kind::FileAbort(_)
    )
}

/// 读循环入口:把传输类报文路由到会话;新的 file_offer / update_offer 启动接收会话。
pub(crate) async fn route(shared: &std::sync::Arc<Shared>, envelope: Envelope) {
    let from = envelope.from;
    let Some(token) = token_of(&envelope.kind).map(str::to_string) else {
        return;
    };

    // 更新包要约:与文件要约同机制,但**自动接受**并带版本标记
    if let Kind::UpdateOffer(offer) = &envelope.kind {
        if shared.transfers.contains(&token) {
            tracing::debug!(target = "fq_net::transfer", %token, "重复的 update_offer,忽略");
            return;
        }
        let (tx, rx) = mpsc::channel::<SessionMsg>(SESSION_QUEUE);
        shared.transfers.register(token.clone(), tx.clone());
        shared.emit(crate::NodeEvent::UpdateOfferReceived {
            from,
            token: offer.token.clone(),
            version: offer.version.clone(),
            manifest: offer.manifest.clone(),
        });
        // 更新包无需用户确认(用户已在设置里同意更新),直接接受
        let _ = tx.try_send(SessionMsg::Accept { dir: None });
        let token = offer.token.clone();
        let manifest = offer.manifest.clone();
        let update_version = Some(offer.version.clone());
        let driver = tokio::spawn(receiver_driver(
            std::sync::Arc::clone(shared),
            from,
            token,
            manifest,
            update_version,
            rx,
        ));
        shared.register_task(&driver);
        return;
    }

    if let Kind::FileOffer(offer) = &envelope.kind {
        if shared.transfers.contains(&token) {
            tracing::debug!(target = "fq_net::transfer", %token, "重复的 file_offer,忽略");
            return;
        }
        let (tx, rx) = mpsc::channel::<SessionMsg>(SESSION_QUEUE);
        shared.transfers.register(token.clone(), tx.clone());
        shared.emit(crate::NodeEvent::FileOfferReceived {
            from,
            token: offer.token.clone(),
            manifest: offer.manifest.clone(),
            message: offer.message.clone(),
        });
        // 自动接受模式(CLI/无 UI):立即给自己发一条 Accept 决策;
        // 桌面 UI 模式则等待用户通过 NodeHandle::accept_file_offer 裁决
        if shared.auto_accept_files {
            let _ = tx.try_send(SessionMsg::Accept { dir: None });
        }
        let token = offer.token.clone();
        let manifest = offer.manifest.clone();
        let driver = tokio::spawn(receiver_driver(
            std::sync::Arc::clone(shared),
            from,
            token,
            manifest,
            None,
            rx,
        ));
        shared.register_task(&driver);
        return;
    }

    if !shared.transfers.dispatch(token, envelope).await {
        tracing::debug!(target = "fq_net::transfer", %from, "传输报文没有对应会话,丢弃");
    }
}

/// 发起一次文件/目录发送,返回传输令牌。
pub(crate) async fn start_send(
    shared: std::sync::Arc<Shared>,
    to: NodeId,
    path: PathBuf,
    message: Option<String>,
) -> Result<String> {
    let built = crate::transfer::manifest::build_manifest(&path).await?;
    let token = MsgId::now_v7().to_string();

    // 先注册会话再发 offer:对端的回包可能比本端 spawn 更快
    let (tx, rx) = mpsc::channel::<SessionMsg>(SESSION_QUEUE);
    shared.transfers.register(token.clone(), tx);

    let offer = Envelope::direct(
        shared.self_id,
        to,
        Kind::FileOffer(FileOffer {
            token: token.clone(),
            manifest: built.manifest.clone(),
            message,
        }),
    );
    if let Err(e) = shared.send_direct(offer).await {
        shared.transfers.remove(&token);
        return Err(e);
    }

    let manifest = built.manifest;
    let sources = built.sources;
    let driver = tokio::spawn(sender_driver(
        std::sync::Arc::clone(&shared),
        to,
        token.clone(),
        manifest,
        sources,
        rx,
    ));
    shared.register_task(&driver);
    Ok(token)
}

/// 发起一次**更新包**发送(对端将自动接收并走安装流程)。
pub(crate) async fn start_send_update(
    shared: std::sync::Arc<Shared>,
    to: NodeId,
    path: PathBuf,
    version: String,
) -> Result<String> {
    let built = crate::transfer::manifest::build_manifest(&path).await?;
    let token = MsgId::now_v7().to_string();

    let (tx, rx) = mpsc::channel::<SessionMsg>(SESSION_QUEUE);
    shared.transfers.register(token.clone(), tx);

    let offer = Envelope::direct(
        shared.self_id,
        to,
        Kind::UpdateOffer(crate::UpdateOffer {
            token: token.clone(),
            manifest: built.manifest.clone(),
            version,
            message: Some("自动更新包(由对端主动提供)".into()),
        }),
    );
    if let Err(e) = shared.send_direct(offer).await {
        shared.transfers.remove(&token);
        return Err(e);
    }

    let manifest = built.manifest;
    let sources = built.sources;
    let driver = tokio::spawn(sender_driver(
        std::sync::Arc::clone(&shared),
        to,
        token.clone(),
        manifest,
        sources,
        rx,
    ));
    shared.register_task(&driver);
    Ok(token)
}

async fn abort_and_report(
    shared: &std::sync::Arc<Shared>,
    peer: NodeId,
    token: &str,
    path: &str,
    direction: TransferDirection,
    reason: &str,
) {
    let abort = Envelope::direct(
        shared.self_id,
        peer,
        Kind::FileAbort(FileAbort {
            token: token.to_string(),
            path: path.to_string(),
            reason: reason.to_string(),
        }),
    );
    let _ = shared.send_direct(abort).await;
    shared.emit(crate::NodeEvent::FileTransferFailed {
        direction,
        token: token.to_string(),
        peer,
        path: Some(path.to_string()),
        reason: reason.to_string(),
    });
    shared.transfers.remove(token);
}

// ─────────────────────────── 发送方 ───────────────────────────

async fn sender_driver(
    shared: std::sync::Arc<Shared>,
    to: NodeId,
    token: String,
    manifest: FileManifest,
    sources: Vec<PathBuf>,
    mut rx: mpsc::Receiver<SessionMsg>,
) {
    let entries: Vec<(FileEntry, PathBuf)> =
        manifest.entries.into_iter().zip(sources).collect();
    let total_entries = entries.len();
    let mut served: HashSet<String> = HashSet::new();
    // 取消标志:stream_entry 在分块之间轮询,实现流式中断
    let cancel = shared
        .transfers
        .cancel_flag(&token)
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));

    loop {
        let envelope = match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
            Ok(Some(SessionMsg::Envelope(envelope))) => envelope,
            Ok(Some(SessionMsg::Accept { .. })) | Ok(Some(SessionMsg::Reject)) => continue,
            Ok(Some(SessionMsg::Cancel)) => {
                abort_and_report(
                    &shared,
                    to,
                    &token,
                    "",
                    TransferDirection::Sending,
                    "已取消发送",
                )
                .await;
                return;
            }
            Ok(None) => {
                tracing::debug!(target = "fq_net::transfer", %token, "发送会话通道关闭");
                return;
            }
            Err(_) => {
                shared.emit(crate::NodeEvent::FileTransferFailed {
                    direction: TransferDirection::Sending,
                    token: token.clone(),
                    peer: to,
                    path: None,
                    reason: "对端长时间未请求下一文件,传输超时".into(),
                });
                shared.transfers.remove(&token);
                return;
            }
        };

        match envelope.kind {
            // 对端拒绝或中止:停止发送,报告原因
            Kind::FileAbort(abort) => {
                shared.emit(crate::NodeEvent::FileTransferFailed {
                    direction: TransferDirection::Sending,
                    token: token.clone(),
                    peer: to,
                    path: None,
                    reason: format!("对方中止:{}", abort.reason),
                });
                shared.transfers.remove(&token);
                return;
            }
            Kind::FileRequest(request) => {
                let Some((entry, source)) = entries.iter().find(|(e, _)| e.path == request.path)
                else {
                    abort_and_report(
                        &shared,
                        to,
                        &token,
                        &request.path,
                        TransferDirection::Sending,
                        "请求了清单中不存在的条目",
                    )
                    .await;
                    return;
                };

                let entry = entry.clone();
                let source = source.clone();
                if let Err(reason) = stream_entry(
                    &shared,
                    to,
                    &token,
                    (&entry, &source),
                    request.offset,
                    request.chunk_size,
                    &cancel,
                )
                .await
                {
                    if reason.contains("取消") {
                        abort_and_report(
                            &shared,
                            to,
                            &token,
                            &entry.path,
                            TransferDirection::Sending,
                            "已取消发送",
                        )
                        .await;
                    } else {
                        abort_and_report(
                            &shared,
                            to,
                            &token,
                            &entry.path,
                            TransferDirection::Sending,
                            &reason,
                        )
                        .await;
                    }
                    return;
                }

                served.insert(entry.path);
                if served.len() == total_entries {
                    shared.emit(crate::NodeEvent::FileTransferCompleted {
                        direction: TransferDirection::Sending,
                        token: token.clone(),
                        peer: to,
                    });
                    shared.transfers.remove(&token);
                    return;
                }
            }
            _ => continue,
        }
    }
}

/// 流式发送单个条目:补算前缀哈希 → 分块读发 → 增量哈希 → file_done。
async fn stream_entry(
    shared: &std::sync::Arc<Shared>,
    to: NodeId,
    token: &str,
    target: (&FileEntry, &std::path::Path),
    offset: u64,
    chunk_hint: Option<u32>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::result::Result<(), String> {
    let (entry, source) = target;
    let chunk = chunk_hint
        .map(|c| (c as usize).clamp(64 * 1024, 512 * 1024))
        .unwrap_or(CHUNK_SIZE);

    let mut file = tokio::fs::File::open(source)
        .await
        .map_err(|e| format!("打开 {}: {e}", source.display()))?;
    let mut hasher = Sha256::new();

    if offset > 0 {
        if offset > entry.size {
            return Err(format!("续传偏移 {offset} 超过文件大小 {}", entry.size));
        }
        hash_prefix(&mut file, &mut hasher, offset)
            .await
            .map_err(|e| format!("补算前缀哈希失败: {e}"))?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| format!("定位续传偏移失败: {e}"))?;
    }

    let mut position = offset;
    let mut buf = vec![0u8; chunk];
    let mut last_progress: Option<Instant> = None;
    loop {
        // 用户取消:在分块边界中断流式发送
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("传输已取消".into());
        }
        let size = file.read(&mut buf).await.map_err(|e| format!("读取失败: {e}"))?;
        if size == 0 {
            break;
        }
        hasher.update(&buf[..size]);
        let chunk_envelope = Envelope::direct(
            shared.self_id,
            to,
            Kind::FileChunk(FileChunk {
                token: token.to_string(),
                path: entry.path.clone(),
                offset: position,
                data: buf[..size].to_vec(),
            }),
        );
        shared
            .send_direct(chunk_envelope)
            .await
            .map_err(|e| format!("发送分块失败: {e}"))?;
        position += size as u64;

        let now = Instant::now();
        let due = last_progress
            .map(|t| now.duration_since(t) >= PROGRESS_MIN_INTERVAL)
            .unwrap_or(true);
        if due {
            last_progress = Some(now);
            shared.emit(crate::NodeEvent::FileProgress {
                direction: TransferDirection::Sending,
                token: token.to_string(),
                peer: to,
                path: entry.path.clone(),
                transferred: position,
                total: entry.size,
            });
        }
    }

    let sha256 = hex::encode(hasher.finalize());
    let done = Envelope::direct(
        shared.self_id,
        to,
        Kind::FileDone(FileDone {
            token: token.to_string(),
            path: entry.path.clone(),
            sha256: Some(sha256.clone()),
        }),
    );
    shared
        .send_direct(done)
        .await
        .map_err(|e| format!("发送完成通知失败: {e}"))?;

    shared.emit(crate::NodeEvent::FileEntryDone {
        direction: TransferDirection::Sending,
        token: token.to_string(),
        peer: to,
        path: entry.path.clone(),
        sha256,
        // 发送方无法知道对端校验结果
        verified: false,
    });
    Ok(())
}

async fn hash_prefix(
    file: &mut tokio::fs::File,
    hasher: &mut Sha256,
    mut remaining: u64,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 128 * 1024];
    while remaining > 0 {
        let want = buf.len().min(remaining as usize);
        file.read_exact(&mut buf[..want]).await?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    Ok(())
}

// ─────────────────────────── 接收方 ───────────────────────────

struct CurrentRecv {
    entry: FileEntry,
    final_path: PathBuf,
    part_path: PathBuf,
    writer: tokio::fs::File,
    hasher: Sha256,
    received: u64,
    last_progress: Option<Instant>,
}

enum Setup {
    Dir(PathBuf),
    File(Box<CurrentRecv>),
}

async fn receiver_driver(
    shared: std::sync::Arc<Shared>,
    from: NodeId,
    token: String,
    manifest: FileManifest,
    update_version: Option<String>,
    mut rx: mpsc::Receiver<SessionMsg>,
) {
    // ── 接收确认阶段:等待用户裁决(自动接受模式下 route 已预先注入 Accept)──
    // 超时给得比发送方空闲超时更长,让发送方的 FileAbort 自然结束本会话
    let session_dir: Option<PathBuf> = loop {
        match tokio::time::timeout(PENDING_TIMEOUT, rx.recv()).await {
            Ok(Some(SessionMsg::Accept { dir })) => break dir,
            Ok(Some(SessionMsg::Reject)) => {
                let abort = Envelope::direct(
                    shared.self_id,
                    from,
                    Kind::FileAbort(FileAbort {
                        token: token.clone(),
                        path: String::new(),
                        reason: "对方拒绝了这次传输".into(),
                    }),
                );
                let _ = shared.send_direct(abort).await;
                shared.emit(crate::NodeEvent::FileTransferFailed {
                    direction: TransferDirection::Receiving,
                    token: token.clone(),
                    peer: from,
                    path: None,
                    reason: "已拒绝接收".into(),
                });
                shared.transfers.remove(&token);
                return;
            }
            Ok(Some(SessionMsg::Envelope(envelope))) => {
                // 确认前唯一合理的网络报文是发送方的中止
                if let Kind::FileAbort(abort) = envelope.kind {
                    shared.emit(crate::NodeEvent::FileTransferFailed {
                        direction: TransferDirection::Receiving,
                        token: token.clone(),
                        peer: from,
                        path: None,
                        reason: format!("对方中止:{}", abort.reason),
                    });
                    shared.transfers.remove(&token);
                    return;
                }
            }
            Ok(Some(SessionMsg::Cancel)) => {
                // 确认阶段就取消:告知发送方并结束
                let abort = Envelope::direct(
                    shared.self_id,
                    from,
                    Kind::FileAbort(FileAbort {
                        token: token.clone(),
                        path: String::new(),
                        reason: "已取消接收".into(),
                    }),
                );
                let _ = shared.send_direct(abort).await;
                shared.emit(crate::NodeEvent::FileTransferFailed {
                    direction: TransferDirection::Receiving,
                    token: token.clone(),
                    peer: from,
                    path: None,
                    reason: "已取消接收".into(),
                });
                shared.transfers.remove(&token);
                return;
            }
            Ok(None) | Err(_) => {
                // 通道关闭或长时间无人决策:静默结束(发送方也会超时)
                shared.transfers.remove(&token);
                return;
            }
        }
    };

    let mut queue: VecDeque<FileEntry> = manifest.entries.clone().into_iter().collect();
    let mut current: Option<CurrentRecv> = None;
    // 最后一个成功落盘的文件的实际路径(供更新包就绪事件使用)
    let mut last_final: Option<PathBuf> = None;
    // 接收侧取消标志
    let cancel = shared
        .transfers
        .cancel_flag(&token)
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));

    loop {
        // 用户取消:中断接收(保留 .part 供续传)
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            let abort = Envelope::direct(
                shared.self_id,
                from,
                Kind::FileAbort(FileAbort {
                    token: token.clone(),
                    path: current.as_ref().map(|c| c.entry.path.clone()).unwrap_or_default(),
                    reason: "已取消接收".into(),
                }),
            );
            let _ = shared.send_direct(abort).await;
            shared.emit(crate::NodeEvent::FileTransferFailed {
                direction: TransferDirection::Receiving,
                token: token.clone(),
                peer: from,
                path: current.as_ref().map(|c| c.entry.path.clone()),
                reason: "已取消接收(.part 已保留,可续传)".into(),
            });
            shared.transfers.remove(&token);
            return;
        }
        // 推进队列:目录直接创建,文件初始化接收状态并发起请求
        while current.is_none() {
            match queue.pop_front() {
                None => {
                    // 全部条目完成:更新包额外发"就绪"事件(含版本与实际落盘路径)
                    if let Some(version) = &update_version {
                        // 实际路径:优先用驱动记录的落盘路径(同名冲突会被自动改名)
                        let path = match &last_final {
                            Some(final_path) => final_path.display().to_string(),
                            None => {
                                let download_dir = shared
                                    .download_dir
                                    .read()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .clone();
                                let name = manifest
                                    .entries
                                    .first()
                                    .map(|e| e.path.clone())
                                    .unwrap_or_else(|| manifest.root_name.clone());
                                download_dir.join(&name).display().to_string()
                            }
                        };
                        shared.emit(crate::NodeEvent::UpdatePackageReady {
                            from,
                            version: version.clone(),
                            path,
                            token: token.clone(),
                        });
                    }
                    shared.emit(crate::NodeEvent::FileTransferCompleted {
                        direction: TransferDirection::Receiving,
                        token: token.clone(),
                        peer: from,
                    });
                    shared.transfers.remove(&token);
                    return;
                }
                Some(entry) => {
                    match setup_entry(&shared, session_dir.as_deref(), &entry, &token).await {
                        Ok(Setup::Dir(dir)) => {
                            if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                                abort_and_report(
                                    &shared,
                                    from,
                                    &token,
                                    &entry.path,
                                    TransferDirection::Receiving,
                                    &format!("创建目录 {} 失败: {e}", dir.display()),
                                )
                                .await;
                                return;
                            }
                        }
                        Ok(Setup::File(recv)) => {
                            let request = Envelope::direct(
                                shared.self_id,
                                from,
                                Kind::FileRequest(FileRequest {
                                    token: token.clone(),
                                    path: recv.entry.path.clone(),
                                    offset: recv.received,
                                    chunk_size: Some(CHUNK_SIZE as u32),
                                }),
                            );
                            if let Err(e) = shared.send_direct(request).await {
                                abort_and_report(
                                    &shared,
                                    from,
                                    &token,
                                    &recv.entry.path,
                                    TransferDirection::Receiving,
                                    &format!("发送续传请求失败: {e}"),
                                )
                                .await;
                                return;
                            }
                            let path = recv.entry.path.clone();
                            let offset = recv.received;
                            let total = recv.entry.size;
                            shared.emit(crate::NodeEvent::FileProgress {
                                direction: TransferDirection::Receiving,
                                token: token.clone(),
                                peer: from,
                                path,
                                transferred: offset,
                                total,
                            });
                            current = Some(*recv);
                        }
                        Err(reason) => {
                            abort_and_report(
                                &shared,
                                from,
                                &token,
                                &entry.path,
                                TransferDirection::Receiving,
                                &reason,
                            )
                            .await;
                            return;
                        }
                    }
                }
            }
        }

        // 等待数据
        let envelope = match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
            Ok(Some(SessionMsg::Envelope(envelope))) => envelope,
            Ok(Some(SessionMsg::Accept { .. })) | Ok(Some(SessionMsg::Reject)) => continue,
            Ok(Some(SessionMsg::Cancel)) => continue, // 取消已在循环顶部统一处理
            Ok(None) | Err(_) => {
                // 中断:保留 .part 供下次续传
                let path = current.as_ref().map(|c| c.entry.path.clone());
                shared.emit(crate::NodeEvent::FileTransferFailed {
                    direction: TransferDirection::Receiving,
                    token: token.clone(),
                    peer: from,
                    path,
                    reason: "传输中断或超时(.part 已保留,可续传)".into(),
                });
                shared.transfers.remove(&token);
                return;
            }
        };

        match envelope.kind {
            Kind::FileChunk(chunk) => {
                let Some(recv) = current.as_mut() else {
                    continue;
                };
                if chunk.path != recv.entry.path {
                    continue;
                }
                if chunk.offset != recv.received {
                    let mismatch = format!(
                        "分块偏移不连续: 期望 {},收到 {}",
                        recv.received, chunk.offset
                    );
                    let path = recv.entry.path.clone();
                    abort_and_report(
                        &shared,
                        from,
                        &token,
                        &path,
                        TransferDirection::Receiving,
                        &mismatch,
                    )
                    .await;
                    return;
                }
                recv.hasher.update(&chunk.data);
                if let Err(e) = recv.writer.write_all(&chunk.data).await {
                    let reason = format!("写入失败: {e}");
                    let path = recv.entry.path.clone();
                    abort_and_report(&shared, from, &token, &path, TransferDirection::Receiving, &reason)
                        .await;
                    return;
                }
                recv.received += chunk.data.len() as u64;

                let now = Instant::now();
                let due = recv
                    .last_progress
                    .map(|t| now.duration_since(t) >= PROGRESS_MIN_INTERVAL)
                    .unwrap_or(true);
                if due {
                    recv.last_progress = Some(now);
                    shared.emit(crate::NodeEvent::FileProgress {
                        direction: TransferDirection::Receiving,
                        token: token.clone(),
                        peer: from,
                        path: recv.entry.path.clone(),
                        transferred: recv.received,
                        total: recv.entry.size,
                    });
                }
            }
            Kind::FileDone(done) => {
                let Some(mut recv) = current.take() else {
                    continue;
                };
                if done.path != recv.entry.path {
                    current = Some(recv);
                    continue;
                }
                if let Err(e) = recv.writer.flush().await {
                    abort_and_report(
                        &shared,
                        from,
                        &token,
                        &recv.entry.path,
                        TransferDirection::Receiving,
                        &format!("落盘失败: {e}"),
                    )
                    .await;
                    return;
                }
                let actual = hex::encode(recv.hasher.finalize());
                let verified = done.sha256.as_deref() == Some(actual.as_str());
                shared.emit(crate::NodeEvent::FileEntryDone {
                    direction: TransferDirection::Receiving,
                    token: token.clone(),
                    peer: from,
                    path: recv.entry.path.clone(),
                    sha256: actual.clone(),
                    verified,
                });
                if verified {
                    if let Err(e) = tokio::fs::rename(&recv.part_path, &recv.final_path).await {
                        abort_and_report(
                            &shared,
                            from,
                            &token,
                            &recv.entry.path,
                            TransferDirection::Receiving,
                            &format!("完成改名失败: {e}"),
                        )
                        .await;
                        return;
                    }
                    // 记下**实际**落盘路径(同名冲突时会被 unique_destination 改名)
                    last_final = Some(recv.final_path.clone());
                } else {
                    // 校验失败:.part 是脏数据,删除
                    tokio::fs::remove_file(&recv.part_path).await.ok();
                    abort_and_report(
                        &shared,
                        from,
                        &token,
                        &recv.entry.path,
                        TransferDirection::Receiving,
                        "SHA-256 校验失败,已丢弃数据",
                    )
                    .await;
                    return;
                }
            }
            Kind::FileAbort(abort) => {
                // 对端中止:保留 .part 供续传
                shared.emit(crate::NodeEvent::FileTransferFailed {
                    direction: TransferDirection::Receiving,
                    token: token.clone(),
                    peer: from,
                    path: Some(abort.path),
                    reason: format!("对端中止: {}", abort.reason),
                });
                shared.transfers.remove(&token);
                return;
            }
            _ => {}
        }
    }
}

/// 初始化一个条目的接收落点。
///
/// 条目路径**自带根前缀**(发送方从 `根名/子路径` 构建),因此统一落在
/// `download_dir` 之下:目录清单的根条目负责创建 `download_dir/根名`,
/// 单文件清单的条目路径恰为根名,直接落在 `download_dir/根名`。
async fn setup_entry(
    shared: &std::sync::Arc<Shared>,
    session_dir: Option<&std::path::Path>,
    entry: &FileEntry,
    token: &str,
) -> std::result::Result<Setup, String> {
    let components = sanitize_entry_path(&entry.path)
        .map_err(|e| format!("清单路径不安全: {e}"))?;

    // 保存位置:本次会话覆盖(接收确认时选的目录)> 全局设置 > 数据目录默认值
    let base = match session_dir {
        Some(dir) => dir.to_path_buf(),
        None => shared
            .download_dir
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    };
    let final_path = base.join(components.join(std::path::MAIN_SEPARATOR_STR));

    if entry.kind == FileKind::Dir {
        return Ok(Setup::Dir(final_path));
    }
    if entry.kind != FileKind::File {
        return Err(format!("不支持的条目类型: {}", entry.kind));
    }

    if let Some(parent) = final_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("创建父目录失败: {e}"))?;
    }

    // 已完整存在的同名文件 → 换名保存,不覆盖用户数据
    let final_path = unique_destination(&final_path).await;

    // 续传:同名 .part 存在且不超过声明大小 → 从其长度续传;过大视为陈旧数据重写
    let part_path = append_suffix(&final_path, ".part");
    let existing = tokio::fs::metadata(&part_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let offset = if existing <= entry.size { existing } else { 0 };

    let mut hasher = Sha256::new();
    let writer = if offset > 0 {
        let mut prefix = tokio::fs::File::open(&part_path)
            .await
            .map_err(|e| format!("打开续传文件失败: {e}"))?;
        let mut remaining = offset;
        let mut buf = vec![0u8; 128 * 1024];
        while remaining > 0 {
            let want = buf.len().min(remaining as usize);
            prefix
                .read_exact(&mut buf[..want])
                .await
                .map_err(|e| format!("读取续传前缀失败: {e}"))?;
            hasher.update(&buf[..want]);
            remaining -= want as u64;
        }
        drop(prefix);
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&part_path)
            .await
            .map_err(|e| format!("以追加模式打开续传文件失败: {e}"))?
    } else {
        tokio::fs::File::create(&part_path)
            .await
            .map_err(|e| format!("创建落盘文件失败: {e}"))?
    };

    tracing::info!(
        target = "fq_net::transfer",
        %token,
        path = %entry.path,
        offset,
        size = entry.size,
        "开始接收条目"
    );

    Ok(Setup::File(Box::new(CurrentRecv {
        entry: entry.clone(),
        final_path,
        part_path,
        writer,
        hasher,
        received: offset,
        last_progress: None,
    })))
}

fn append_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// 目标已存在时生成 `name (n).ext`,绝不覆盖已有文件。
async fn unique_destination(path: &std::path::Path) -> PathBuf {
    if tokio::fs::metadata(path).await.is_err() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..=9999u32 {
        let candidate = path.with_file_name(format!("{stem} ({n}){ext}"));
        if tokio::fs::metadata(&candidate).await.is_err() {
            return candidate;
        }
    }
    // 兜底:带上时间戳,几乎不可能走到
    path.with_file_name(format!(
        "{stem} ({}{}){ext}",
        fq_proto::now_ms(),
        ""
    ))
}
