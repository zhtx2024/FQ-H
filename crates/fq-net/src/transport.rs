//! TCP 安全传输:Noise IK 握手 + 分帧加密读写。
//!
//! 连接形态:
//!
//! ```text
//! 发起方(已知对端静态公钥)                响应方
//!   │ frame(msg1) ──────────────────────────▶│
//!   │◀────────────────────────── frame(msg2) │
//!   │        双方进入传输模式                  │
//!   │ frame(Noise密文(Envelope)) ◀──────────▶│
//! ```
//!
//! 关键约束(**必须**遵守,否则 nonce 失步导致会话报废):
//! * 加密与写入在**同一个**写任务内串行完成 —— 多个发送方把明文丢进队列即可,
//!   绝不能各自加密后入队(入队顺序可能与加密顺序颠倒);
//! * 解密失败/读失败 → 连接整体废弃,由连接管理器负责重建。

use std::net::SocketAddr;
use std::sync::Arc;

use fq_crypto::{HandshakeInitiator, HandshakeResponder, SecureChannel, StaticKeys, STATIC_KEY_LEN};
use fq_proto::{Envelope, FrameDecoder, MAX_FRAME_BYTES, codec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::error::{Error, Result};

/// 握手阶段的单次读写超时:防止半开连接把拨号流程挂死。
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Noise 协议的单条消息总长上限(65535 字节)。
///
/// snow/Noise 的**硬限制**:超出即加密失败("input error")。注意 snow 的判定是
/// `payload + TAGLEN > MAXMSGLEN`,即明文实际可用上限是 65519 字节
/// (要给 16 字节 AEAD 标签留位)—— 少算这 16 字节会得到一个极其隐蔽的故障。
///
/// 因此本层实现透明分段:大报文拆成多个段,每段一个 Noise 消息/一帧,
/// 接收端重组。协议层(fq-proto)因此可以自由发送最大 1 MiB 的报文
/// (例如 256 KiB 的文件分块),完全不必感知这个限制。
const NOISE_MAX_MESSAGE: usize = 65_535;
/// AEAD 标签长度(ChaChaPoly)。
const NOISE_TAG_LEN: usize = 16;
/// 明文实际可用上限。
const NOISE_MAX_PLAINTEXT: usize = NOISE_MAX_MESSAGE - NOISE_TAG_LEN;
/// 每段 1 字节控制头。
const SEGMENT_HEADER: usize = 1;
/// 控制位:还有后续分段。
const SEGMENT_MORE: u8 = 0x01;
/// 单段最大数据量。
const MAX_SEGMENT: usize = NOISE_MAX_PLAINTEXT - SEGMENT_HEADER;

/// 一条已建立的加密传输(读写 halves + 共享的 Noise 通道)。
#[derive(Debug)]
pub struct Transport {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    channel: Arc<Mutex<SecureChannel>>,
    decoder: FrameDecoder,
    remote_static: [u8; STATIC_KEY_LEN],
}

impl Transport {
    /// 作为发起方拨号并完成 IK 握手。
    pub async fn connect_out(
        local: &StaticKeys,
        remote_static: &[u8; STATIC_KEY_LEN],
        addr: SocketAddr,
    ) -> Result<Self> {
        let mut stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true).ok();

        let mut initiator = HandshakeInitiator::start(local, remote_static)?;
        let msg1 = initiator.first_message(b"")?;
        write_frame(&mut stream, &msg1).await?;

        let mut decoder = FrameDecoder::with_default_limit();
        let msg2 = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream, &mut decoder))
            .await
            .map_err(|_| Error::PeerUnreachable(format!("对端 {addr} 握手超时")))??;
        let channel = initiator.finish(&msg2)?;
        let remote_static = channel.remote_static();
        let (reader, writer) = stream.into_split();

        Ok(Self {
            reader,
            writer,
            channel: Arc::new(Mutex::new(channel)),
            decoder,
            remote_static,
        })
    }

    /// 作为响应方接受连接并完成 IK 握手。
    ///
    /// 返回传输与对端静态公钥(调用方用它反查对端身份并复核 TOFU)。
    pub async fn accept_in(
        stream: TcpStream,
        local: &StaticKeys,
    ) -> Result<(Self, [u8; STATIC_KEY_LEN])> {
        let mut stream = stream;
        stream.set_nodelay(true).ok();

        let mut decoder = FrameDecoder::with_default_limit();
        let msg1 = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream, &mut decoder))
            .await
            .map_err(|_| Error::Protocol("入站握手超时".into()))??;

        let (msg2, channel) = HandshakeResponder::listen(local)?.respond(&msg1)?;
        write_frame(&mut stream, &msg2).await?;

        let remote_static = channel.remote_static();
        let (reader, writer) = stream.into_split();

        let transport = Self {
            reader,
            writer,
            channel: Arc::new(Mutex::new(channel)),
            decoder,
            remote_static,
        };
        Ok((transport, remote_static))
    }

    /// 读取并解密下一个报文。任何失败都意味着连接应当废弃。
    pub async fn recv_envelope(&mut self) -> Result<Envelope> {
        let ciphertext = read_frame(&mut self.reader, &mut self.decoder).await?;
        let plaintext = self
            .channel
            .lock()
            .await
            .decrypt(&ciphertext)
            .map_err(|e| {
                tracing::warn!(target = "fq_net::transport", %e, "解密失败,废弃连接");
                e
            })?;
        Ok(codec::decode_framed(&plaintext)?)
    }

    /// 对端静态公钥(握手期认证的那个)。
    pub fn remote_static(&self) -> [u8; STATIC_KEY_LEN] {
        self.remote_static
    }

    /// 拆出(读半 + 共享通道)与(写半 + 共享通道),分别交给读/写任务。
    pub fn into_parts(
        self,
    ) -> (
        ReadPart,
        WritePart,
    ) {
        let channel = self.channel;
        let remote_static = self.remote_static;
        (
            ReadPart {
                reader: self.reader,
                decoder: self.decoder,
                channel: Arc::clone(&channel),
            },
            WritePart {
                writer: self.writer,
                channel,
                remote_static,
            },
        )
    }
}

/// 读半:属于单个读任务。
#[derive(Debug)]
pub struct ReadPart {
    reader: OwnedReadHalf,
    decoder: FrameDecoder,
    channel: Arc<Mutex<SecureChannel>>,
}

impl ReadPart {
    /// 读取并解密下一个报文(自动重组传输分段)。
    pub async fn recv_envelope(&mut self) -> Result<Envelope> {
        let mut assembled: Vec<u8> = Vec::new();
        loop {
            let ciphertext = read_frame(&mut self.reader, &mut self.decoder).await?;
            let segment = self.channel.lock().await.decrypt(&ciphertext)?;

            let Some((&control, data)) = segment.split_first() else {
                return Err(Error::Protocol("空传输段".into()));
            };
            assembled.extend_from_slice(data);
            // 恶意对端可以用无数"还有下一段"无限撑大缓冲,必须封顶
            if assembled.len() > fq_proto::MAX_FRAME_BYTES {
                return Err(Error::Protocol("重组后的报文超过帧上限".into()));
            }
            if control & SEGMENT_MORE == 0 {
                break;
            }
        }
        Ok(codec::decode_framed(&assembled)?)
    }
}

/// 写半:属于单个写任务 —— **所有加密都在这里串行发生**。
#[derive(Debug)]
pub struct WritePart {
    writer: OwnedWriteHalf,
    channel: Arc<Mutex<SecureChannel>>,
    remote_static: [u8; STATIC_KEY_LEN],
}

impl WritePart {
    /// 编码 + 分段 + 加密 + 写帧。
    ///
    /// 整个流程在写任务内串行,保证 nonce 顺序 = 写入顺序;
    /// 分段对外透明:协议层可以发送最大 1 MiB 的任意报文。
    pub async fn send_envelope(&mut self, envelope: &Envelope) -> Result<()> {
        let plaintext = codec::encode(envelope)?;
        if plaintext.len() + SEGMENT_HEADER <= NOISE_MAX_PLAINTEXT {
            return self.send_segment(&plaintext, false).await;
        }
        let total = plaintext.len();
        let mut sent = 0usize;
        while sent < total {
            let end = (sent + MAX_SEGMENT).min(total);
            let more = end < total;
            self.send_segment(&plaintext[sent..end], more).await?;
            sent = end;
        }
        Ok(())
    }

    async fn send_segment(&mut self, data: &[u8], more: bool) -> Result<()> {
        let mut message = Vec::with_capacity(SEGMENT_HEADER + data.len());
        message.push(if more { SEGMENT_MORE } else { 0 });
        message.extend_from_slice(data);
        let ciphertext = self.channel.lock().await.encrypt(&message)?;
        write_all_frame(&mut self.writer, &ciphertext).await
    }

    /// 对端静态公钥。
    pub fn remote_static(&self) -> [u8; STATIC_KEY_LEN] {
        self.remote_static
    }
}

async fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    stream
        .write_all(&fq_proto::encode_frame(bytes, MAX_FRAME_BYTES)?)
        .await?;
    stream.flush().await?;
    Ok(())
}

async fn write_all_frame(writer: &mut OwnedWriteHalf, bytes: &[u8]) -> Result<()> {
    writer
        .write_all(&fq_proto::encode_frame(bytes, MAX_FRAME_BYTES)?)
        .await?;
    writer.flush().await?;
    Ok(())
}

/// 持续读取直到凑满一帧;连接关闭返回 Err。
///
/// 泛型而非 `dyn AsyncRead`:trait 对象不是 `Send`,会让整个 future 无法
/// 进入 `tokio::spawn`。
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    decoder: &mut FrameDecoder,
) -> Result<Vec<u8>> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Some(payload) = decoder.next_frame()? {
            return Ok(payload);
        }
        let size = reader.read(&mut chunk).await?;
        if size == 0 {
            return Err(Error::Protocol("连接在帧完成前关闭".into()));
        }
        decoder.feed(&chunk[..size])?;
    }
}
