//! 线缆格式转储工具:打印每种报文的 MessagePack 字节与分层展开。
//!
//! 用途:
//! * 撰写/校对 `docs/PROTOCOL.md` 里的字节级样例
//! * 其它语言(TS / Python / Go)实现本协议时做交叉比对
//! * 排查"为什么对端解不开"时,肉眼比对字段名与类型
//!
//! 运行:`cargo run -p fq-proto --example wire_dump`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use fq_proto::{
    AckBody, AckStatus, Capabilities, Envelope, FileChunk, FileEntry, FileKind, FileManifest,
    FileOffer, Kind, MsgId, NodeId, PingBody, PresenceEvent, PresenceInfo, PresenceStatus, TextBody,
    TextFormat, codec,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// 把 MessagePack 字节里的可打印 ASCII 片段抽取出来,便于快速肉眼核对字段名。
fn printable_runs(bytes: &[u8]) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for &byte in bytes {
        if byte.is_ascii_graphic() || byte == b' ' {
            current.push(byte as char);
        } else {
            if current.len() >= 3 {
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() >= 3 {
        runs.push(current);
    }
    runs
}

fn dump(label: &str, envelope: &Envelope) {
    let bytes = codec::encode(envelope).expect("编码失败");
    println!("── {label} ────────────────────────────────────────");
    println!("字节数: {}", bytes.len());
    println!("hex   : {}", hex(&bytes));
    println!("字符串: {}", printable_runs(&bytes).join(" | "));
    // 自校验:转储出来的样例必须能被自己解回来
    let decoded = codec::decode(&bytes).expect("自解码失败");
    assert_eq!(&decoded, envelope, "{label} 转储样例自校验失败");
    println!();
}

fn main() {
    let me = NodeId::from_bytes([
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ]);
    let peer = NodeId::from_bytes([0x11; 16]);

    println!("fq-proto 协议 v{} 线缆格式转储\n", codec::PROTOCOL_VERSION);

    dump(
        "Ping(最小报文)",
        &Envelope::broadcast(me, Kind::Ping(PingBody { nonce: 1 })),
    );

    dump(
        "Presence(上线通告)",
        &Envelope::broadcast(
            me,
            Kind::Presence(PresenceInfo {
                event: PresenceEvent::Announce,
                display_name: "张三".to_string(),
                host_name: "DESKTOP-01".to_string(),
                status: PresenceStatus::Online,
                group: None,
                port: fq_proto::DEFAULT_PORT,
                endpoints: vec!["192.168.1.20:24250".to_string()],
                public_key: vec![0u8; 32],
                noise_static: vec![0u8; 32],
                binding_signature: vec![0u8; 64],
                capabilities: Capabilities::TEXT
                    | Capabilities::NOISE_IK
                    | Capabilities::FILE_TRANSFER,
                avatar_sha256: None,
                app_version: Some("0.1.0".into()),
            }),
        ),
    );

    dump(
        "Text(点对点文本)",
        &Envelope::direct(
            me,
            peer,
            Kind::Text(TextBody {
                body: "你好".to_string(),
                format: TextFormat::Plain,
                reply_to: None,
                mentions: vec![],
                group_id: None,
                group_name: None,
            }),
        ),
    );

    dump(
        "Ack(已读回执)",
        &Envelope::direct(
            me,
            peer,
            Kind::Ack(AckBody {
                ack_id: MsgId::from_uuid(uuid::Uuid::nil()),
                status: AckStatus::Read,
            }),
        ),
    );

    dump(
        "FileOffer(文件要约)",
        &Envelope::direct(
            me,
            peer,
            Kind::FileOffer(FileOffer {
                token: "t-1".to_string(),
                manifest: FileManifest {
                    root_name: "a.txt".to_string(),
                    total_bytes: 5,
                    entries: vec![FileEntry {
                        path: "a.txt".to_string(),
                        size: 5,
                        mtime_ms: 1,
                        kind: FileKind::File,
                        sha256: None,
                    }],
                },
                message: None,
            }),
        ),
    );

    let chunk = vec![0xABu8; 32];
    dump(
        "FileChunk(bin 编码的数据块)",
        &Envelope::direct(
            me,
            peer,
            Kind::FileChunk(FileChunk {
                token: "t-1".to_string(),
                path: "a.txt".to_string(),
                offset: 0,
                data: chunk.clone(),
            }),
        ),
    );

    println!(
        "提示:FileChunk 的数据字段必须走 MessagePack bin(0xc4/0xc5/0xc6);\
         若出现 32 个连续的整数元素,说明对端误用了默认的 Vec<u8> 编码。"
    );
}
