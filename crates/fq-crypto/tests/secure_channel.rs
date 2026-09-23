//! 端到端安全流程测试:绑定验证 → TOFU → IK 握手 → 加密传输。
//!
//! 这里模拟的是攻击者视角下的完整信任链,任何一环被跳过都应有对应测试失败。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fq_crypto::{
    Error, HandshakeInitiator, HandshakeResponder, Identity, StaticKeys, TofuStore, TrustDecision,
    load_or_create_identity, load_tofu, save_tofu, sign_static_key_binding, static_key_fingerprint,
    verify_static_key_binding,
};
use fq_proto::NodeId;

/// 一对完成握手的通道(Alice 发起 → Bob 响应)。
fn established_channel() -> (fq_crypto::SecureChannel, fq_crypto::SecureChannel) {
    let alice_static = StaticKeys::generate().unwrap();
    let bob_static = StaticKeys::generate().unwrap();

    let mut alice = HandshakeInitiator::start(&alice_static, &bob_static.public()).unwrap();
    let msg1 = alice.first_message(b"").unwrap();
    let (msg2, bob_channel) = HandshakeResponder::listen(&bob_static)
        .unwrap()
        .respond(&msg1)
        .unwrap();
    let alice_channel = alice.finish(&msg2).unwrap();
    (alice_channel, bob_channel)
}

#[test]
fn full_trust_chain_happy_path() {
    // ── Alice 的身份与静态密钥 ──
    let alice_identity = Identity::generate().unwrap();
    let alice_static = StaticKeys::generate().unwrap();

    // ── Alice 在发现报文里声明:(Ed25519 公钥, X25519 静态公钥, 绑定签名) ──
    let signature = sign_static_key_binding(&alice_identity, &alice_static.public());

    // ── Bob 侧 ① 验证绑定:静态密钥确实属于该 NodeId ──
    let verified_node = verify_static_key_binding(
        &alice_identity.public_key(),
        &alice_static.public(),
        &signature,
    )
    .unwrap();
    assert_eq!(verified_node, alice_identity.node_id());

    // ── Bob 侧 ② TOFU:首次见面 → 固定 ──
    let mut tofu = TofuStore::new();
    assert_eq!(
        tofu.verify(&verified_node, &alice_static.public()),
        TrustDecision::FirstUse
    );
    tofu.pin(verified_node, &alice_static.public());
    assert_eq!(
        tofu.verify(&verified_node, &alice_static.public()),
        TrustDecision::Trusted
    );

    // ── Bob 侧 ③ 用已验证的静态公钥完成 IK 握手 ──
    let bob_static = StaticKeys::generate().unwrap();
    let mut alice = HandshakeInitiator::start(&alice_static, &bob_static.public()).unwrap();
    let msg1 = alice.first_message(b"").unwrap();
    let (msg2, mut bob_channel) = HandshakeResponder::listen(&bob_static)
        .unwrap()
        .respond(&msg1)
        .unwrap();
    let mut alice_channel = alice.finish(&msg2).unwrap();

    // 关键交叉核对:握手期认证的静态公钥 == 发现报文里声明的那把
    assert_eq!(bob_channel.remote_static(), alice_static.public());

    // 信任链闭合后即可通信
    let ciphertext = alice_channel.encrypt("你好".as_bytes()).unwrap();
    assert_eq!(bob_channel.decrypt(&ciphertext).unwrap(), "你好".as_bytes());
}

#[test]
fn handshake_and_transport_roundtrip_both_directions() {
    let (mut alice, mut bob) = established_channel();

    // Alice → Bob
    let ct = alice.encrypt("你好,Bob 🚀".as_bytes()).unwrap();
    assert_eq!(bob.decrypt(&ct).unwrap(), "你好,Bob 🚀".as_bytes());

    // Bob → Alice
    let ct = bob.encrypt("收到".as_bytes()).unwrap();
    assert_eq!(alice.decrypt(&ct).unwrap(), "收到".as_bytes());

    // 多轮往返
    for i in 0..50 {
        let ct = alice.encrypt(format!("msg-{i}").as_bytes()).unwrap();
        assert_eq!(bob.decrypt(&ct).unwrap(), format!("msg-{i}").as_bytes());
    }
}

#[test]
fn responder_sees_initiator_static_key() {
    let alice_static = StaticKeys::generate().unwrap();
    let bob_static = StaticKeys::generate().unwrap();

    let mut alice = HandshakeInitiator::start(&alice_static, &bob_static.public()).unwrap();
    let msg1 = alice.first_message(b"").unwrap();
    let (_msg2, bob_channel) = HandshakeResponder::listen(&bob_static)
        .unwrap()
        .respond(&msg1)
        .unwrap();

    assert_eq!(bob_channel.remote_static(), alice_static.public());
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let (mut alice, mut bob) = established_channel();
    let mut ct = alice.encrypt(b"transfer 100 to bob").unwrap();

    // 篡改一个字节
    let last = ct.len() - 1;
    ct[last] ^= 0x01;
    assert!(
        matches!(bob.decrypt(&ct), Err(Error::Decrypt)),
        "被篡改的密文必须解密失败"
    );

    // snow 语义:解密失败不推进接收 nonce。发送方已经加密了 nonce-0 的这条消息,
    // 接收方仍在等 nonce-0 —— 于是后续所有消息都会失败。
    // 结论:中途被篡改的会话不可恢复,上层必须断开重连(见 channel 模块文档)。
    let next = alice.encrypt(b"next message").unwrap();
    assert!(
        matches!(bob.decrypt(&next), Err(Error::Decrypt)),
        "流被篡改后必然 nonce 失步,会话必须废弃"
    );
}

#[test]
fn replayed_transport_message_is_rejected() {
    let (mut alice, mut bob) = established_channel();
    let ct = alice.encrypt("只处理一次".as_bytes()).unwrap();

    assert!(bob.decrypt(&ct).is_ok());
    // 重放同一条密文 → nonce 已消费,必须失败
    assert!(
        matches!(bob.decrypt(&ct), Err(Error::Decrypt)),
        "重放的密文必须被拒绝"
    );
}

#[test]
fn out_of_order_messages_are_rejected() {
    let (mut alice, mut bob) = established_channel();
    let first = alice.encrypt(b"#1").unwrap();
    let second = alice.encrypt(b"#2").unwrap();

    // 乱序:先投递 #2(nonce 1),接收方在等 nonce 0 → AEAD 校验失败
    assert!(
        matches!(bob.decrypt(&second), Err(Error::Decrypt)),
        "乱序消息必须解密失败"
    );

    // 失败的读取不消耗 nonce,因此 #1(nonce 0)仍然可以解出。
    // 这意味着"单条损坏不会立刻毒化整条流"—— 直到发送方越过该 nonce。
    assert_eq!(bob.decrypt(&first).unwrap(), b"#1");
}

#[test]
fn handshake_with_wrong_remote_static_fails() {
    let alice_static = StaticKeys::generate().unwrap();
    let real_bob = StaticKeys::generate().unwrap();
    let fake_bob = StaticKeys::generate().unwrap();

    // Alice 拿到的静态公钥被攻击者替换(绑定验证被跳过的场景)
    let mut alice = HandshakeInitiator::start(&alice_static, &fake_bob.public()).unwrap();
    let msg1 = alice.first_message(b"").unwrap();

    // 真 Bob 无法解开这条消息
    let result = HandshakeResponder::listen(&real_bob)
        .unwrap()
        .respond(&msg1);
    assert!(result.is_err(), "密钥不匹配的握手必须失败");
}

#[test]
fn random_handshake_messages_never_panic() {
    let bob_static = StaticKeys::generate().unwrap();

    let mut state = 0xDEAD_BEEF_1234_5678u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for len in 1..=200usize {
        let msg: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        // 任意畸形输入只允许 Err,不允许 panic
        let responder = HandshakeResponder::listen(&bob_static).unwrap();
        let _ = responder.respond(&msg);
    }
}

#[test]
fn tofu_changed_key_surfaces_fingerprints() {
    let node = NodeId::from_bytes([0x33; 16]);
    let mut tofu = TofuStore::new();
    let old_key = StaticKeys::generate().unwrap();
    let new_key = StaticKeys::generate().unwrap();

    tofu.pin(node, &old_key.public());
    match tofu.verify(&node, &new_key.public()) {
        TrustDecision::Changed { pinned, presented } => {
            assert_eq!(pinned, static_key_fingerprint(&old_key.public()));
            assert_eq!(presented, static_key_fingerprint(&new_key.public()));
            assert_ne!(pinned, presented);
        }
        other => panic!("应为 Changed,实际 {other:?}"),
    }
}

#[test]
fn persistence_keeps_identity_and_tofu_stable() {
    let dir = std::env::temp_dir()
        .join("fq-crypto-tests")
        .join(format!("e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let identity_path = dir.join("identity.json");
    let tofu_path = dir.join("tofu.json");

    // 首次启动:创建身份 + 固定一个同伴
    let identity = load_or_create_identity(&identity_path).unwrap();
    let peer = Identity::generate().unwrap();
    let mut tofu = TofuStore::new();
    tofu.pin(peer.node_id(), &StaticKeys::generate().unwrap().public());
    save_tofu(&tofu_path, &tofu).unwrap();

    // 二次启动:身份与信任必须原样恢复
    let reloaded = load_or_create_identity(&identity_path).unwrap();
    assert_eq!(reloaded.node_id(), identity.node_id(), "重启后身份不得漂移");
    let tofu2 = load_tofu(&tofu_path).unwrap();
    assert_eq!(tofu2, tofu, "重启后 TOFU 固定表不得漂移");

    let _ = std::fs::remove_dir_all(&dir);
}
