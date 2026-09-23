//! 协议编解码往返测试:覆盖每一种报文类型。

// 测试里用 panic!/assert! 表达断言失败是正当的;生产代码的同类 lint 不受影响。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fq_proto::{
    AckBody, AckStatus, AvatarPayload, Capabilities, Envelope, FileAbort, FileChunk, FileDone,
    FileEntry, FileKind, FileManifest, FileOffer, FileRequest, FrameDecoder, Kind, MsgId, NodeId,
    PingBody, PresenceEvent, PresenceInfo, PresenceStatus, TextBody, TextFormat, TypingBody,
    TypingState, codec,
};

fn node(seed: u8) -> NodeId {
    NodeId::from_bytes([seed; 16])
}

fn presence() -> PresenceInfo {
    PresenceInfo {
        event: PresenceEvent::Announce,
        display_name: "张三 🚀".to_string(),
        host_name: "DESKTOP-测试机".to_string(),
        status: PresenceStatus::Busy,
        group: Some("研发组".to_string()),
        port: 24250,
        endpoints: vec![
            "192.168.1.20:24250".to_string(),
            "[fe80::1]:24250".to_string(),
        ],
        public_key: vec![0xAB; 32],
        noise_static: vec![0xCD; 32],
        binding_signature: vec![0xEF; 64],
        capabilities: Capabilities::TEXT
            | Capabilities::FILE_TRANSFER
            | Capabilities::NOISE_IK
            | Capabilities::MARKDOWN,
        avatar_sha256: Some("d".repeat(64)),
        app_version: Some("0.1.0".into()),
    }
}

fn manifest() -> FileManifest {
    FileManifest {
        root_name: "设计稿".to_string(),
        total_bytes: 2048,
        entries: vec![
            FileEntry {
                path: "设计稿/封面.png".to_string(),
                size: 1024,
                mtime_ms: 1_700_000_000_000,
                kind: FileKind::File,
                sha256: Some("a".repeat(64)),
            },
            FileEntry {
                path: "设计稿/子目录".to_string(),
                size: 0,
                mtime_ms: 1_700_000_000_000,
                kind: FileKind::Dir,
                sha256: None,
            },
        ],
    }
}

/// 构造覆盖全部 `Kind` 变体的报文样例。
fn all_kinds() -> Vec<Kind> {
    let token = "0f8fad5b-d9cb-469f-a165-70867728950e".to_string();
    vec![
        Kind::Presence(presence()),
        Kind::Text(TextBody {
            body: "你好,世界!Hello 🌏".to_string(),
            format: TextFormat::Markdown,
            reply_to: Some(MsgId::now_v7()),
            mentions: vec![node(1), node(2)],
            group_id: Some("研发组".to_string()),
            group_name: None,
        }),
        Kind::Ack(AckBody {
            ack_id: MsgId::now_v7(),
            status: AckStatus::Read,
        }),
        Kind::Typing(TypingBody {
            state: TypingState::Started,
            thread: Some(MsgId::now_v7()),
        }),
        Kind::FileOffer(FileOffer {
            token: token.clone(),
            manifest: manifest(),
            message: Some("发你两个文件".to_string()),
        }),
        Kind::UpdateOffer(fq_proto::UpdateOffer {
            token: token.clone(),
            manifest: manifest(),
            version: "0.4.0".to_string(),
            message: Some("自动更新包".to_string()),
        }),
        Kind::UpdateRequest(fq_proto::UpdateRequest {
            requester_version: "0.3.0".to_string(),
            requester_host: Some("DESKTOP-01".to_string()),
        }),
        Kind::AvatarRequest(fq_proto::AvatarRequest {
            known_sha256: Some("a".repeat(64)),
            mine: Some(AvatarPayload::new(
                vec![0x89, b'P', b'N', b'G', 1, 2, 3],
                "image/png",
            )),
        }),
        Kind::AvatarReply(fq_proto::AvatarReply {
            avatar: AvatarPayload::new(
                vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                "image/png",
            ),
        }),
        Kind::FileRequest(FileRequest {
            token: token.clone(),
            path: "设计稿/封面.png".to_string(),
            offset: 4096,
            chunk_size: Some(262_144),
        }),
        Kind::FileChunk(FileChunk {
            token: token.clone(),
            path: "设计稿/封面.png".to_string(),
            offset: 8192,
            data: vec![0x5A; 4096],
        }),
        Kind::FileDone(FileDone {
            token: token.clone(),
            path: "设计稿/封面.png".to_string(),
            sha256: Some("b".repeat(64)),
        }),
        Kind::FileAbort(FileAbort {
            token,
            path: "设计稿/封面.png".to_string(),
            reason: "接收方磁盘空间不足".to_string(),
        }),
        Kind::Ping(PingBody { nonce: u64::MAX }),
        Kind::Pong(PingBody { nonce: 0 }),
    ]
}

#[test]
fn every_kind_roundtrips() {
    for kind in all_kinds() {
        let name = kind.name();
        let envelope = Envelope::direct(node(0x11), node(0x22), kind);

        let bytes = codec::encode(&envelope).unwrap_or_else(|e| panic!("{name} 编码失败: {e}"));
        let decoded = codec::decode(&bytes).unwrap_or_else(|e| panic!("{name} 解码失败: {e}"));

        assert_eq!(decoded, envelope, "{name} 往返不一致");
        assert_eq!(decoded.v, codec::PROTOCOL_VERSION, "{name} 版本号错误");
        assert!(decoded.is_protocol_compatible());
    }
}

#[test]
fn message_ids_and_sender_survive_roundtrip() {
    let envelope = Envelope::broadcast(node(0x33), Kind::Ping(PingBody { nonce: 1 }));
    let decoded = codec::decode(&codec::encode(&envelope).unwrap()).unwrap();

    assert_eq!(decoded.id, envelope.id, "消息 ID 必须原样保留");
    assert_eq!(decoded.from, envelope.from, "发送方 ID 必须原样保留");
    assert_eq!(decoded.to, None, "广播报文的 to 必须为 None");
    assert_eq!(decoded.ts_ms, envelope.ts_ms);
}

#[test]
fn broadcast_and_direct_are_distinguishable() {
    let direct = Envelope::direct(node(1), node(2), Kind::Ping(PingBody { nonce: 5 }));
    let broadcast = Envelope::broadcast(node(1), Kind::Ping(PingBody { nonce: 5 }));

    let decoded_direct = codec::decode(&codec::encode(&direct).unwrap()).unwrap();
    let decoded_broadcast = codec::decode(&codec::encode(&broadcast).unwrap()).unwrap();

    assert_eq!(decoded_direct.to, Some(node(2)));
    assert_eq!(decoded_broadcast.to, None);
}

#[test]
fn file_chunk_data_uses_binary_encoding_not_int_array() {
    let data = vec![0xEEu8; 64 * 1024];
    let envelope = Envelope::direct(
        node(1),
        node(2),
        Kind::FileChunk(FileChunk {
            token: "t".to_string(),
            path: "a.bin".to_string(),
            offset: 0,
            data: data.clone(),
        }),
    );

    let bytes = codec::encode(&envelope).unwrap();

    // 若误用默认的 Vec<u8> 编码(每个字节编成一个整数),体积会膨胀到 2 倍以上。
    assert!(
        bytes.len() < data.len() + 1024,
        "文件分块必须按 bin 编码;实际 {} 字节,数据 {} 字节",
        bytes.len(),
        data.len()
    );
    assert_eq!(codec::decode(&bytes).unwrap(), envelope);
}

#[test]
fn framed_transport_roundtrip_through_decoder() {
    let envelope = Envelope::direct(
        node(9),
        node(8),
        Kind::Text(TextBody {
            body: "跨分帧层的往返".to_string(),
            format: TextFormat::Plain,
            reply_to: None,
            mentions: vec![],
            group_id: None,
            group_name: None,
        }),
    );

    let wire = codec::encode_framed(&envelope).unwrap();

    // 故意逐字节喂入,验证半包重组
    let mut decoder = FrameDecoder::with_default_limit();
    let mut frames = Vec::new();
    for byte in &wire {
        decoder.feed(std::slice::from_ref(byte)).unwrap();
        while let Some(frame) = decoder.next_frame().unwrap() {
            frames.push(frame);
        }
    }

    assert_eq!(frames.len(), 1);
    let decoded = codec::decode_framed(&frames[0]).unwrap();
    assert_eq!(decoded, envelope);
}

#[test]
fn oversized_chunk_is_rejected_by_frame_layer() {
    let envelope = Envelope::direct(
        node(1),
        node(2),
        Kind::FileChunk(FileChunk {
            token: "t".to_string(),
            path: "big.bin".to_string(),
            offset: 0,
            data: vec![0u8; fq_proto::MAX_FRAME_BYTES + 1024],
        }),
    );

    let err = codec::encode_framed(&envelope).unwrap_err();
    assert!(
        matches!(err, fq_proto::Error::FrameTooLarge { .. }),
        "超过帧上限的分块必须在编码分帧时被拒绝,实际: {err}"
    );
}

#[test]
fn version_is_enforced_only_in_checked_decode() {
    let mut envelope = Envelope::broadcast(node(4), Kind::Ping(PingBody { nonce: 3 }));
    envelope.v = codec::PROTOCOL_VERSION + 1;

    let bytes = codec::encode(&envelope).unwrap();

    // 宽松解码:能解出来,由调用方决定策略(例如回一条"请升级")
    let relaxed = codec::decode(&bytes).unwrap();
    assert!(!relaxed.is_protocol_compatible());

    // 严格解码:直接拒绝
    let err = codec::decode_checked(&bytes).unwrap_err();
    assert!(matches!(err, fq_proto::Error::UnsupportedVersion { .. }));
}

#[test]
fn capabilities_survive_roundtrip_including_unknown_bits() {
    let mut info = presence();
    // 模拟对端声明了一个本端未定义的未来能力位
    info.capabilities = Capabilities::from_bits_retain(
        Capabilities::TEXT.bits() | Capabilities::NOISE_IK.bits() | (1u64 << 45),
    );

    let envelope = Envelope::broadcast(node(6), Kind::Presence(info.clone()));
    let decoded = codec::decode(&codec::encode(&envelope).unwrap()).unwrap();

    let Kind::Presence(decoded_info) = decoded.kind else {
        panic!("载荷类型错误");
    };
    assert_eq!(decoded_info.capabilities.bits(), info.capabilities.bits());
    assert_ne!(decoded_info.capabilities.unknown_bits(), 0);
}

#[test]
fn avatar_payload_verifies_content_hash() {
    let data = vec![1u8, 2, 3, 4, 5];
    let good = AvatarPayload::new(data.clone(), "image/png");
    assert_eq!(good.sha256, sha256_hex(&data), "构造时应自动计算哈希");
    assert!(good.verify(), "哈希一致应通过校验");

    let tampered = AvatarPayload {
        sha256: sha256_hex(&data),
        data: vec![1u8, 2, 3, 4, 6], // 内容被改
        mime: "image/png".into(),
    };
    assert!(!tampered.verify(), "内容被篡改必须校验失败");
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
