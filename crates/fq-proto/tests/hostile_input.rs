//! 恶意输入测试:解码路径**只允许返回 `Err`,绝不允许 panic / abort**。
//!
//! 局域网里任何一台机器都能向本端发包,因此解码器必须把"对端是恶意的"
//! 当作默认假设。本文件用确定性 PRNG 做轻量模糊测试,不引入额外依赖。

// 测试里用 panic!/assert! 表达断言失败是正当的;生产代码的同类 lint 不受影响。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fq_proto::{
    Envelope, FrameDecoder, Kind, MAX_FRAME_BYTES, NodeId, PingBody, TextBody, TextFormat, codec,
};

/// xorshift64:确定性、无依赖,失败可复现。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

fn sample_envelope() -> Envelope {
    Envelope::direct(
        NodeId::from_bytes([0x11; 16]),
        NodeId::from_bytes([0x22; 16]),
        Kind::Text(TextBody {
            body: "恶意输入模糊测试样本".to_string(),
            format: TextFormat::Plain,
            reply_to: None,
            mentions: vec![NodeId::from_bytes([1; 16])],
            group_id: None,
            group_name: None,
        }),
    )
}

#[test]
fn random_bytes_never_decode_successfully_but_never_panic() {
    let mut rng = Rng::new(0x5EED_1234_ABCD_0001);
    let mut successes = 0usize;

    for _ in 0..4000 {
        let len = rng.below(257);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();

        if codec::decode(&bytes).is_ok() {
            successes += 1;
        }
    }

    assert_eq!(successes, 0, "随机字节不应被解析为合法报文(说明校验过松)");
}

#[test]
fn random_bytes_are_safe_for_decoder_even_with_valid_prefix() {
    // 一半随机输入以合法报文开头,专门测试"前缀合法 + 尾部垃圾"的情况
    let mut rng = Rng::new(0x5EED_1234_ABCD_0002);
    let valid = codec::encode(&sample_envelope()).unwrap();

    for _ in 0..2000 {
        let mut bytes = valid.clone();
        let extra = rng.below(64);
        for _ in 0..extra {
            bytes.push(rng.next_u64() as u8);
        }
        if extra == 0 {
            // 未追加垃圾字节时,原报文必须仍然可解(基准对照)
            assert!(codec::decode(&bytes).is_ok(), "原始报文必须可解");
        } else {
            // 尾部垃圾必须被拒绝,否则存在报文走私 / 解析歧义风险
            assert!(
                codec::decode(&bytes).is_err(),
                "合法前缀 + {extra} 字节尾部垃圾必须报错"
            );
        }
    }
}

#[test]
fn bit_flip_mutations_never_panic() {
    let mut rng = Rng::new(0x5EED_1234_ABCD_0003);
    let valid = codec::encode(&sample_envelope()).unwrap();

    for _ in 0..4000 {
        let mut bytes = valid.clone();
        let flips = 1 + rng.below(4);
        for _ in 0..flips {
            let index = rng.below(bytes.len());
            let bit = rng.below(8);
            bytes[index] ^= 1u8 << bit;
        }
        // 结论无所谓(畸形/仍合法都行),关键是不 panic
        let _ = codec::decode(&bytes);
    }
}

#[test]
fn every_truncation_is_rejected() {
    let valid = codec::encode(&sample_envelope()).unwrap();

    for cut in 0..valid.len() {
        assert!(
            codec::decode(&valid[..cut]).is_err(),
            "截断到 {cut} 字节必须报错"
        );
    }
}

#[test]
fn crafted_huge_lengths_are_rejected() {
    // str32 声明 4 GiB
    assert!(codec::decode(&[0xdb, 0xff, 0xff, 0xff, 0xff]).is_err());
    // bin32 声明 4 GiB
    assert!(codec::decode(&[0xc6, 0xff, 0xff, 0xff, 0xff]).is_err());
    // array32 声明 42 亿个元素
    assert!(codec::decode(&[0xdd, 0xff, 0xff, 0xff, 0xff]).is_err());
    // map32 声明 42 亿个键值对
    assert!(codec::decode(&[0xdf, 0xff, 0xff, 0xff, 0xff]).is_err());
    // 保留标记 0xc1
    assert!(codec::decode(&[0xc1]).is_err());
}

#[test]
fn deeply_nested_input_is_rejected_without_stack_overflow() {
    // 20000 层嵌套 fixarray
    let bytes = vec![0x91u8; 20_000];
    let err = codec::decode(&bytes).unwrap_err();
    assert!(
        matches!(err, fq_proto::Error::MalformedMsgpack(_)),
        "超深嵌套必须在深度上限处被拒绝,实际: {err}"
    );
}

#[test]
fn frame_decoder_survives_random_streams() {
    let mut rng = Rng::new(0x5EED_1234_ABCD_0004);
    let mut decoder = FrameDecoder::with_default_limit();

    for _ in 0..2000 {
        let len = rng.below(512);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();

        // 非法长度声明会返回 Err,这是预期行为;关键是不得 panic
        if decoder.feed(&bytes).is_err() {
            decoder = FrameDecoder::with_default_limit();
            continue;
        }
        while let Ok(Some(frame)) = decoder.next_frame() {
            assert!(frame.len() <= MAX_FRAME_BYTES, "分帧器返回了超过上限的帧");
            let _ = codec::decode(&frame);
        }
    }
}

#[test]
fn empty_input_is_rejected() {
    assert!(codec::decode(&[]).is_err());
    assert!(codec::decode(b"").is_err());
}

#[test]
fn utf8_strings_roundtrip_precisely() {
    let samples = [
        "中文简体",
        "繁體中文",
        "emoji 🚀🎉👨‍👩‍👧‍👦",
        "混合 mixed 文本 with ASCII",
        "零宽\u{200b}字符",
        "换行\n制表\t回车\r",
    ];

    for body in samples {
        let envelope = Envelope::broadcast(
            NodeId::from_bytes([0x44; 16]),
            Kind::Text(TextBody {
                body: body.to_string(),
                format: TextFormat::Plain,
                reply_to: None,
                mentions: vec![],
                group_id: None,
                group_name: None,
            }),
        );
        let decoded = codec::decode(&codec::encode(&envelope).unwrap()).unwrap();
        let Kind::Text(text) = decoded.kind else {
            panic!("载荷类型错误");
        };
        assert_eq!(text.body, body, "UTF-8 内容必须逐字节保留");
    }
}

#[test]
fn ping_nonce_extremes_roundtrip() {
    for nonce in [0u64, 1, u64::MAX, u64::MAX / 2] {
        let envelope =
            Envelope::broadcast(NodeId::from_bytes([3; 16]), Kind::Ping(PingBody { nonce }));
        assert_eq!(
            codec::decode(&codec::encode(&envelope).unwrap()).unwrap(),
            envelope
        );
    }
}
