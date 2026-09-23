//! 身份与 TOFU 存储的文件持久化。
//!
//! 设计约束:
//!
//! * **原子写**:先写 `.tmp` 再 rename,进程崩溃不会留下半截文件
//!   (Windows 的 `std::fs::rename` 会覆盖已存在目标)。
//! * **损坏即报错,绝不 panic**:文件内容是"持久化的网络输入",按不可信数据处理;
//!   加载时校验种子与记录的 NodeId 一致,不一致视为损坏。
//! * **格式带版本号**,未来迁移有据可依。
//!
//! 已知限制(v1 接受,记录在案):私钥以明文 hex 存在用户目录。
//! 后续硬化方向是 Windows DPAPI / macOS Keychain / Secret Service,
//! 存储层已收敛在本模块,替换成本可控。

use std::fs;
use std::path::Path;

use fq_proto::NodeId;
use serde::{Deserialize, Serialize};

use crate::channel::STATIC_KEY_LEN;
use crate::channel::StaticKeys;
use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::tofu::TofuStore;

/// 身份文件格式。
#[derive(Serialize, Deserialize)]
struct IdentityFile {
    v: u16,
    /// Ed25519 种子,64 个 hex 字符。明文存储(v1 限制,见模块文档)。
    seed: String,
    created_ms: i64,
    /// 冗余记录的 NodeId,加载时用于一致性校验。
    node_id: String,
}

/// TOFU 固定表文件格式。
#[derive(Serialize, Deserialize)]
struct TofuFile {
    v: u16,
    /// NodeId(hex) → 静态公钥(hex)。
    pins: Vec<(String, String)>,
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn decode_fixed_hex(raw: &str, expect_len: usize, what: &str) -> Result<Vec<u8>> {
    let bytes = hex::decode(raw)
        .map_err(|e| Error::CorruptedStore(format!("{what} 不是合法 hex: {e}")))?;
    if bytes.len() != expect_len {
        return Err(Error::CorruptedStore(format!(
            "{what} 长度应为 {expect_len} 字节,实际 {}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// 保存身份(原子写)。
pub fn save_identity(path: &Path, identity: &Identity) -> Result<()> {
    let file = IdentityFile {
        v: 1,
        seed: hex::encode(identity.seed_bytes().as_slice()),
        created_ms: fq_proto::now_ms(),
        node_id: identity.node_id().to_hex(),
    };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| Error::CorruptedStore(format!("身份序列化失败: {e}")))?;
    atomic_write(path, json.as_bytes())
}

/// 加载身份;文件不存在返回 `Ok(None)`。
pub fn load_identity(path: &Path) -> Result<Option<Identity>> {
    let Some(raw) = fs::read_to_string(path).ok() else {
        return Ok(None);
    };
    let file: IdentityFile = serde_json::from_str(&raw)
        .map_err(|e| Error::CorruptedStore(format!("身份文件不是合法 JSON: {e}")))?;
    if file.v != 1 {
        return Err(Error::CorruptedStore(format!("不支持的身份文件版本 v{}", file.v)));
    }

    let seed_bytes = decode_fixed_hex(&file.seed, 32, "身份种子")?;
    let seed: [u8; 32] = seed_bytes
        .try_into()
        .map_err(|_| Error::CorruptedStore("身份种子长度异常".into()))?;
    let identity = Identity::from_seed(&seed);

    // 一致性校验:文件里的 NodeId 必须与种子派生结果一致
    let recorded = NodeId::from_hex(&file.node_id)
        .map_err(|e| Error::CorruptedStore(format!("记录的 NodeId 非法: {e}")))?;
    if recorded != identity.node_id() {
        return Err(Error::CorruptedStore(
            "身份文件自相矛盾:种子派生的 NodeId 与记录值不一致".into(),
        ));
    }

    Ok(Some(identity))
}

/// 加载身份;不存在则生成新身份并保存。
///
/// 这是应用启动的标准入口 —— 保证 NodeId 跨重启稳定,TOFU 信任才不会被反复重置。
pub fn load_or_create_identity(path: &Path) -> Result<Identity> {
    if let Some(existing) = load_identity(path)? {
        return Ok(existing);
    }
    let identity = Identity::generate()?;
    save_identity(path, &identity)?;
    Ok(identity)
}

/// 保存 Noise 静态密钥对(原子写)。
///
/// 静态密钥**必须**跨重启稳定 —— 否则 NodeId 不变而握手密钥变了,
/// 所有同伴都会收到 TOFU 密钥变更告警。
pub fn save_static_keys(path: &Path, keys: &StaticKeys) -> Result<()> {
    #[derive(Serialize, Deserialize)]
    struct StaticKeysFile {
        v: u16,
        private: String,
        public: String,
    }
    let file = StaticKeysFile {
        v: 1,
        private: hex::encode(keys.private()),
        public: hex::encode(keys.public()),
    };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| Error::CorruptedStore(format!("静态密钥序列化失败: {e}")))?;
    atomic_write(path, json.as_bytes())
}

/// 加载静态密钥对;文件不存在返回 `Ok(None)`。
pub fn load_static_keys(path: &Path) -> Result<Option<StaticKeys>> {
    #[derive(Serialize, Deserialize)]
    struct StaticKeysFile {
        v: u16,
        private: String,
        public: String,
    }
    let Some(raw) = fs::read_to_string(path).ok() else {
        return Ok(None);
    };
    let file: StaticKeysFile = serde_json::from_str(&raw)
        .map_err(|e| Error::CorruptedStore(format!("静态密钥文件不是合法 JSON: {e}")))?;
    if file.v != 1 {
        return Err(Error::CorruptedStore(format!(
            "不支持的静态密钥文件版本 v{}",
            file.v
        )));
    }
    let private_bytes = decode_fixed_hex(&file.private, 32, "静态私钥")?;
    let public_bytes = decode_fixed_hex(&file.public, 32, "静态公钥")?;
    let private: [u8; 32] = private_bytes
        .try_into()
        .map_err(|_| Error::CorruptedStore("静态私钥长度异常".into()))?;
    let public: [u8; 32] = public_bytes
        .try_into()
        .map_err(|_| Error::CorruptedStore("静态公钥长度异常".into()))?;
    Ok(Some(StaticKeys::from_bytes(&private, &public)))
}

/// 加载静态密钥对;不存在则生成并保存。
pub fn load_or_create_static_keys(path: &Path) -> Result<StaticKeys> {
    if let Some(existing) = load_static_keys(path)? {
        return Ok(existing);
    }
    let keys = StaticKeys::generate()?;
    save_static_keys(path, &keys)?;
    Ok(keys)
}

/// 保存 TOFU 固定表(原子写)。
pub fn save_tofu(path: &Path, store: &TofuStore) -> Result<()> {
    let pins = store
        .iter()
        .map(|(node_id, key)| (node_id.to_hex(), hex::encode(key)))
        .collect();
    let file = TofuFile { v: 1, pins };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| Error::CorruptedStore(format!("TOFU 序列化失败: {e}")))?;
    atomic_write(path, json.as_bytes())
}

/// 加载 TOFU 固定表;文件不存在返回空表。
pub fn load_tofu(path: &Path) -> Result<TofuStore> {
    let Some(raw) = fs::read_to_string(path).ok() else {
        return Ok(TofuStore::new());
    };
    let file: TofuFile = serde_json::from_str(&raw)
        .map_err(|e| Error::CorruptedStore(format!("TOFU 文件不是合法 JSON: {e}")))?;
    if file.v != 1 {
        return Err(Error::CorruptedStore(format!("不支持的 TOFU 文件版本 v{}", file.v)));
    }

    let mut pairs = Vec::with_capacity(file.pins.len());
    for (node_raw, key_raw) in &file.pins {
        let node_id = NodeId::from_hex(node_raw)
            .map_err(|e| Error::CorruptedStore(format!("TOFU 记录的 NodeId 非法: {e}")))?;
        let key_bytes = decode_fixed_hex(key_raw, STATIC_KEY_LEN, "静态公钥")?;
        let key: [u8; STATIC_KEY_LEN] = key_bytes
            .try_into()
            .map_err(|_| Error::CorruptedStore("静态公钥长度异常".into()))?;
        pairs.push((node_id, key));
    }
    Ok(TofuStore::from_pairs(pairs))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("fq-crypto-tests")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_roundtrip_preserves_node_id() {
        let dir = temp_dir("identity-roundtrip");
        let path = dir.join("identity.json");

        let identity = Identity::generate().unwrap();
        save_identity(&path, &identity).unwrap();

        let loaded = load_identity(&path).unwrap().expect("应能加载");
        assert_eq!(loaded.node_id(), identity.node_id());
        assert_eq!(loaded.public_key(), identity.public_key());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_is_idempotent() {
        let dir = temp_dir("identity-idempotent");
        let path = dir.join("identity.json");

        let first = load_or_create_identity(&path).unwrap();
        let second = load_or_create_identity(&path).unwrap();
        assert_eq!(
            first.node_id(),
            second.node_id(),
            "同一文件二次加载必须是同一身份"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupted_identity_files_are_rejected_not_panicked() {
        let dir = temp_dir("identity-corrupt");
        let path = dir.join("identity.json");

        let cases: Vec<&str> = vec![
            "",
            "not json at all",
            "{\"v\":1}",
            "{\"v\":2,\"seed\":\"00\",\"created_ms\":0,\"node_id\":\"00\"}",
            // 种子与 node_id 不一致
            "{\"v\":1,\"seed\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"created_ms\":0,\"node_id\":\"00112233445566778899aabbccddeeff\"}",
            // 种子长度错误
            "{\"v\":1,\"seed\":\"abcd\",\"created_ms\":0,\"node_id\":\"00112233445566778899aabbccddeeff\"}",
        ];
        for (i, case) in cases.iter().enumerate() {
            fs::write(&path, case).unwrap();
            let result = load_identity(&path);
            assert!(result.is_err(), "损坏样例 #{i} 必须报错: {case}");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tofu_roundtrip_and_corruption() {
        let dir = temp_dir("tofu");
        let path = dir.join("tofu.json");

        let node = NodeId::from_bytes([0x0A; 16]);
        let mut store = TofuStore::new();
        store.pin(node, &[0x5Cu8; STATIC_KEY_LEN]);
        save_tofu(&path, &store).unwrap();

        let loaded = load_tofu(&path).unwrap();
        assert_eq!(loaded, store);

        // 损坏文件 → Err 而不是 panic
        fs::write(&path, "{\"v\":1,\"pins\":[[\"bad hex\",\"also bad\"]]}").unwrap();
        assert!(load_tofu(&path).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
