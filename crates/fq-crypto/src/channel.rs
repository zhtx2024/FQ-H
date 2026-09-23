//! Noise `IK` 安全通道。
//!
//! 模式:`Noise_IK_25519_ChaChaPoly_BLAKE2s`
//!
//! * **IK** = 发起方已知响应方静态公钥(来自发现报文)→ 1-RTT 握手 + 双向认证
//! * **Curve25519** = DH;**ChaChaPoly** = AEAD;**BLAKE2s** = 哈希
//! * 前向保密:每次握手双方都生成新的临时密钥,静态密钥泄露不影响历史会话
//!
//! 握手时序(1-RTT):
//!
//! ```text
//! 发起方                                   响应方
//!    │  (已知响应方 X25519 静态公钥)            │(持有自己的 X25519 静态私钥)
//!    │  msg1: e, es, s(加密), ss   ──────────▶ │
//!    │  ◀──────────  msg2: e, ee              │
//!    │            双方进入传输模式               │
//! ```
//!
//! 使用注意:**发起前必须先验证密钥绑定**(见 [`crate::binding`])并核对
//! TOFU 信任(见 [`crate::tofu`]),否则 IK 认证的只是"那把密钥",
//! 而不是"那个 NodeId"。

use std::fmt;

use snow::{Builder, HandshakeState, TransportState};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// 本协议 v1 固定使用的 Noise 参数。
pub const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Noise 消息在载荷之上的最大开销(按官方建议留 128 字节)。
const MAX_NOISE_OVERHEAD: usize = 128;

/// X25519 静态密钥长度(字节)。
pub const STATIC_KEY_LEN: usize = 32;

fn noise_params() -> Result<snow::params::NoiseParams> {
    NOISE_PATTERN
        .parse()
        .map_err(|e: snow::Error| Error::Crypto(format!("Noise 参数非法: {e}")))
}

/// X25519 静态密钥对(握手身份)。
///
/// `Debug` 输出刻意不含私钥。
pub struct StaticKeys {
    private: Zeroizing<[u8; STATIC_KEY_LEN]>,
    public: [u8; STATIC_KEY_LEN],
}

impl StaticKeys {
    /// 生成新的静态密钥对。
    pub fn generate() -> Result<Self> {
        let keypair = Builder::new(noise_params()?)
            .generate_keypair()
            .map_err(|e| Error::Crypto(format!("生成静态密钥失败: {e}")))?;

        let private: [u8; STATIC_KEY_LEN] = keypair
            .private
            .as_slice()
            .try_into()
            .map_err(|_| Error::Crypto("静态私钥长度异常".into()))?;
        let public: [u8; STATIC_KEY_LEN] = keypair
            .public
            .as_slice()
            .try_into()
            .map_err(|_| Error::Crypto("静态公钥长度异常".into()))?;

        Ok(Self {
            private: Zeroizing::new(private),
            public,
        })
    }

    /// 由已知字节构造(持久化恢复用)。
    pub fn from_bytes(private: &[u8; STATIC_KEY_LEN], public: &[u8; STATIC_KEY_LEN]) -> Self {
        Self {
            private: Zeroizing::new(*private),
            public: *public,
        }
    }

    /// 静态公钥(可公开,随发现报文分发)。
    pub fn public(&self) -> [u8; STATIC_KEY_LEN] {
        self.public
    }

    /// 静态私钥(绝对保密)。
    pub fn private(&self) -> &[u8; STATIC_KEY_LEN] {
        &self.private
    }
}

impl fmt::Debug for StaticKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 绝不输出私钥
        write!(f, "StaticKeys(public={})", hex::encode(self.public))
    }
}

/// 发起方握手状态。
pub struct HandshakeInitiator {
    state: HandshakeState,
}

impl HandshakeInitiator {
    /// 用本端静态密钥向已知的对端静态公钥发起 IK 握手。
    pub fn start(local: &StaticKeys, remote_static_public: &[u8; STATIC_KEY_LEN]) -> Result<Self> {
        let state = Builder::new(noise_params()?)
            .local_private_key(local.private())
            .and_then(|b| b.remote_public_key(remote_static_public))
            .and_then(|b| b.build_initiator())
            .map_err(|e| Error::Handshake(e.to_string()))?;
        Ok(Self { state })
    }

    /// 生成第一条握手消息(可携带少量加密载荷)。
    pub fn first_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut message = vec![0u8; payload.len() + MAX_NOISE_OVERHEAD];
        let n = self
            .state
            .write_message(payload, &mut message)
            .map_err(|e| Error::Handshake(e.to_string()))?;
        message.truncate(n);
        Ok(message)
    }

    /// 消费响应方的回复,完成握手,得到传输通道。
    pub fn finish(mut self, reply: &[u8]) -> Result<SecureChannel> {
        let mut payload = vec![0u8; reply.len()];
        self.state
            .read_message(reply, &mut payload)
            .map_err(|e| Error::Handshake(format!("对端回复无效: {e}")))?;

        let remote_static = self.remote_static_bytes()?;
        let transport = self
            .state
            .into_transport_mode()
            .map_err(|e| Error::Handshake(e.to_string()))?;
        Ok(SecureChannel {
            transport,
            remote_static,
            initiator: true,
        })
    }

    fn remote_static_bytes(&self) -> Result<[u8; STATIC_KEY_LEN]> {
        self.state
            .get_remote_static()
            .ok_or(Error::Handshake("无法获取对端静态公钥".into()))?
            .try_into()
            .map_err(|_| Error::Handshake("对端静态公钥长度异常".into()))
    }
}

impl fmt::Debug for HandshakeInitiator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HandshakeInitiator(..)")
    }
}

/// 响应方握手状态。
pub struct HandshakeResponder {
    state: HandshakeState,
}

impl HandshakeResponder {
    /// 以本端静态密钥进入监听状态。
    pub fn listen(local: &StaticKeys) -> Result<Self> {
        let state = Builder::new(noise_params()?)
            .local_private_key(local.private())
            .and_then(|b| b.build_responder())
            .map_err(|e| Error::Handshake(e.to_string()))?;
        Ok(Self { state })
    }

    /// 处理发起方的第一条消息并生成回复,一步完成握手。
    ///
    /// 返回 `(回复消息, 传输通道)`。发起方静态公钥可从通道的
    /// [`SecureChannel::remote_static`] 读取 —— **必须**用它复核密钥绑定与 TOFU。
    pub fn respond(mut self, first_message: &[u8]) -> Result<(Vec<u8>, SecureChannel)> {
        let mut payload = vec![0u8; first_message.len()];
        self.state
            .read_message(first_message, &mut payload)
            .map_err(|e| Error::Handshake(format!("握手消息无效(静态密钥不匹配或被篡改): {e}")))?;

        let remote_static = self.remote_static_bytes()?;
        let mut reply = vec![0u8; MAX_NOISE_OVERHEAD];
        let n = self
            .state
            .write_message(&[], &mut reply)
            .map_err(|e| Error::Handshake(e.to_string()))?;
        reply.truncate(n);

        let transport = self
            .state
            .into_transport_mode()
            .map_err(|e| Error::Handshake(e.to_string()))?;
        Ok((
            reply,
            SecureChannel {
                transport,
                remote_static,
                initiator: false,
            },
        ))
    }

    fn remote_static_bytes(&self) -> Result<[u8; STATIC_KEY_LEN]> {
        self.state
            .get_remote_static()
            .ok_or(Error::Handshake("无法获取对端静态公钥".into()))?
            .try_into()
            .map_err(|_| Error::Handshake("对端静态公钥长度异常".into()))
    }
}

impl fmt::Debug for HandshakeResponder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HandshakeResponder(..)")
    }
}

/// 已建立的加密通道(传输模式)。
///
/// `Debug` 输出不含任何密钥材料。
pub struct SecureChannel {
    transport: TransportState,
    remote_static: [u8; STATIC_KEY_LEN],
    initiator: bool,
}

impl SecureChannel {
    /// 加密一条消息。
    ///
    /// 明文超过 Noise 单条消息上限(65519 字节 = 65535 − 16 字节 AEAD 标签)时
    /// 直接报错而不是透传 snow 隐晦的 "input error" —— 分段职责在上层
    /// (`fq-net::transport`)。
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        const NOISE_MAX_PLAINTEXT: usize = 65_535 - 16;
        if plaintext.len() > NOISE_MAX_PLAINTEXT {
            return Err(Error::Crypto(format!(
                "消息 {} 字节超过 Noise 单条上限 {NOISE_MAX_PLAINTEXT} 字节,需要分段",
                plaintext.len()
            )));
        }
        let mut message = vec![0u8; plaintext.len() + MAX_NOISE_OVERHEAD];
        let n = self
            .transport
            .write_message(plaintext, &mut message)
            .map_err(map_transport_error)?;
        message.truncate(n);
        Ok(message)
    }

    /// 解密一条消息。篡改、乱序、重放都会返回 [`Error::Decrypt`]。
    ///
    /// # Nonce 语义(实现选型,上层必须知晓)
    ///
    /// * 解密**失败不推进**接收 nonce:单条被破坏的消息不会立刻毒化整条流;
    /// * 但若发送方已继续推进(它无法知道某条被丢了),流将永久失步 ——
    ///   此时唯一正确的做法是**废弃会话并重新握手**,绝不能跳过消息续用;
    /// * 因此本通道必须运行在可靠有序传输(TCP)之上;UDP 场景需上层自带重排。
    pub fn decrypt(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let mut plaintext = vec![0u8; message.len()];
        let n = self
            .transport
            .read_message(message, &mut plaintext)
            .map_err(map_transport_error)?;
        plaintext.truncate(n);
        Ok(plaintext)
    }

    /// 对端 X25519 静态公钥(握手期间认证的那个)。
    pub fn remote_static(&self) -> [u8; STATIC_KEY_LEN] {
        self.remote_static
    }

    /// 本端是否为握手发起方。
    pub fn is_initiator(&self) -> bool {
        self.initiator
    }
}

impl fmt::Debug for SecureChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SecureChannel(initiator={}, remote_static={})",
            self.initiator,
            hex::encode(self.remote_static)
        )
    }
}

fn map_transport_error(e: snow::Error) -> Error {
    match e {
        snow::Error::Decrypt => Error::Decrypt,
        other => Error::Crypto(other.to_string()),
    }
}
