//! 前向兼容测试:手工构造"未来版本"发来的报文,验证本端不会解码失败、不会丢包。
//!
//! 这里刻意不使用 serde 编码器来构造输入,而是手写 MessagePack 字节 ——
//! 只有这样才能构造出"未来版本才有的字段/类型",真正测试兼容策略。

// 测试里用 panic!/assert! 表达断言失败是正当的;生产代码的同类 lint 不受影响。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fq_proto::{Kind, NodeId, codec};

/// 编码一个 MessagePack 字符串(fixstr / str8 / str16)。
fn mp_str(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out = Vec::with_capacity(bytes.len() + 3);
    if len < 32 {
        out.push(0xa0 | len as u8);
    } else if len < 256 {
        out.push(0xd9);
        out.push(len as u8);
    } else {
        out.push(0xda);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    }
    out.extend_from_slice(bytes);
    out
}

/// 编码一个 MessagePack 非负整数(选择最短表示)。
fn mp_u64(value: u64) -> Vec<u8> {
    if value < 128 {
        vec![value as u8]
    } else if value <= u64::from(u8::MAX) {
        vec![0xcc, value as u8]
    } else if value <= u64::from(u16::MAX) {
        let mut out = vec![0xcd];
        out.extend_from_slice(&(value as u16).to_be_bytes());
        out
    } else if value <= u64::from(u32::MAX) {
        let mut out = vec![0xce];
        out.extend_from_slice(&(value as u32).to_be_bytes());
        out
    } else {
        let mut out = vec![0xcf];
        out.extend_from_slice(&value.to_be_bytes());
        out
    }
}

/// 编码一个 MessagePack fixmap(`pairs` 不超过 15 对)。
fn mp_map(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
    assert!(pairs.len() <= 15, "测试助手只支持 fixmap");
    let mut out = vec![0x80 | pairs.len() as u8];
    for (key, value) in pairs {
        out.extend_from_slice(&mp_str(key));
        out.extend_from_slice(value);
    }
    out
}

const VALID_NODE_HEX: &str = "00112233445566778899aabbccddeeff";
const VALID_UUID: &str = "018f2b3c-4d5e-7f80-9abc-def012345678";

/// 构造一个最小合法信封的字段集合(不含 kind)。
///
/// 键固定为 `'static` 字面量,返回拥有所有权的值,便于调用方继续 push 字段。
fn base_pairs(extra: &[(&'static str, Vec<u8>)]) -> Vec<(&'static str, Vec<u8>)> {
    let mut pairs: Vec<(&str, Vec<u8>)> = vec![
        ("v", mp_u64(1)),
        ("id", mp_str(VALID_UUID)),
        ("from", mp_str(VALID_NODE_HEX)),
        ("ts_ms", mp_u64(1_700_000_000_000)),
    ];
    pairs.extend_from_slice(extra);
    pairs
}

#[test]
fn unknown_message_kind_decodes_as_unknown_instead_of_failing() {
    // 模拟未来版本引入的新报文类型
    let mut pairs = base_pairs(&[("kind", mp_str("quantum_teleport"))]);
    pairs.push(("payload", mp_str("whatever")));
    let bytes = mp_map(&pairs);

    let envelope = codec::decode(&bytes).expect("未知报文类型不得导致解码失败");
    assert_eq!(envelope.kind, Kind::Unknown);
    assert_eq!(envelope.kind_name(), "unknown");
    assert_eq!(envelope.from, NodeId::from_hex(VALID_NODE_HEX).unwrap());
}

#[test]
fn unknown_fields_are_ignored() {
    // 模拟未来版本在同一报文里追加了字段
    let mut pairs = base_pairs(&[
        ("kind", mp_str("ping")),
        ("nonce", mp_u64(7)),
    ]);
    pairs.push(("future_field", mp_str("ignored")));
    pairs.push(("another_new_field", mp_u64(99)));
    let bytes = mp_map(&pairs);

    let envelope = codec::decode(&bytes).expect("未知字段不得导致解码失败");
    match envelope.kind {
        Kind::Ping(body) => assert_eq!(body.nonce, 7),
        other => panic!("期望 Ping,实际 {}", other.name()),
    }
}

#[test]
fn optional_fields_may_be_absent() {
    // 只有必填字段 + kind,省略 to / 以及载荷内的全部可选字段
    let bytes = mp_map(&base_pairs(&[("kind", mp_str("ping")), ("nonce", mp_u64(1))]));

    let envelope = codec::decode(&bytes).expect("可选字段缺失时必须使用默认值");
    assert_eq!(envelope.to, None);
}

#[test]
fn presence_without_optional_fields_falls_back_to_defaults() {
    let bytes = mp_map(&base_pairs(&[
        ("kind", mp_str("presence")),
        ("event", mp_str("announce")),
        ("display_name", mp_str("未来版本用户")),
        ("host_name", mp_str("future-host")),
    ]));

    let envelope = codec::decode(&bytes).expect("Presence 的可选字段应回退默认值");
    let Kind::Presence(info) = envelope.kind else {
        panic!("期望 Presence");
    };
    assert_eq!(info.display_name, "未来版本用户");
    assert_eq!(info.status, fq_proto::PresenceStatus::Online, "默认应为在线");
    assert_eq!(info.capabilities.bits(), 0);
    assert!(info.public_key.is_empty());
}

#[test]
fn unknown_enum_value_falls_back_instead_of_failing() {
    // 未来版本新增了一种在线状态
    let bytes = mp_map(&base_pairs(&[
        ("kind", mp_str("presence")),
        ("event", mp_str("announce")),
        ("display_name", mp_str("用户")),
        ("host_name", mp_str("host")),
        ("status", mp_str("in-a-meeting")),
    ]));

    let envelope = codec::decode(&bytes).expect("未知枚举取值不得导致解码失败");
    let Kind::Presence(info) = envelope.kind else {
        panic!("期望 Presence");
    };
    assert_eq!(info.status, fq_proto::PresenceStatus::Unknown);
}

#[test]
fn missing_required_field_is_rejected() {
    // 缺少 from(必填)
    let bytes = mp_map(&[
        ("v", mp_u64(1)),
        ("id", mp_str(VALID_UUID)),
        ("ts_ms", mp_u64(1)),
        ("kind", mp_str("ping")),
    ]);

    let err = codec::decode(&bytes).unwrap_err();
    assert!(
        matches!(err, fq_proto::Error::Decode(_)),
        "缺少必填字段必须报错,实际: {err}"
    );
}

#[test]
fn malformed_node_id_is_rejected() {
    let bytes = mp_map(&[
        ("v", mp_u64(1)),
        ("id", mp_str(VALID_UUID)),
        ("from", mp_str("not-a-valid-hex-node-id")),
        ("ts_ms", mp_u64(1)),
        ("kind", mp_str("ping")),
    ]);

    assert!(codec::decode(&bytes).is_err());
}

#[test]
fn nested_kind_map_is_rejected_because_wire_format_must_be_flat() {
    // 回归锁:曾出现过 `{"kind":{"kind":"ping",…}}` 的嵌套格式。
    // 扁平格式是本协议的既有约定(见 Envelope::kind 的文档),必须拒绝嵌套变体,
    // 否则会出现"两种都能解"的歧义输入。
    let nested_kind = mp_map(&[("kind", mp_str("ping")), ("nonce", mp_u64(1))]);
    let bytes = mp_map(&base_pairs(&[("kind", nested_kind)]));

    assert!(
        codec::decode(&bytes).is_err(),
        "嵌套 kind 格式必须被拒绝,只接受扁平格式"
    );
}

