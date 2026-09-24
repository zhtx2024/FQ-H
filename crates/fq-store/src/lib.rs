//! # fq-store
//!
//! 本地存储(SQLite):消息历史、联系人、离线待发队列。
//!
//! 设计约定:
//! * **本 crate 保持"哑存储"**:只有字符串/整数,不感知 NodeId/Envelope 等领域类型,
//!   避免存储层与协议层耦合;类型转换由 fq-core 完成。
//! * **调用方负责并发**:内部是单个 `rusqlite::Connection`(WAL 模式),
//!   fq-core 用 `tokio::sync::Mutex` 串行化访问;所有查询都是本地短操作。
//! * **模式迁移**:用 `PRAGMA user_version` 记录版本,启动时逐级升级,
//!   保证老数据文件永远可以打开。
//!
//! ## 表结构
//!
//! ```text
//! messages     聊天历史(双向文本),按 (peer, ts_ms) 索引
//! peers        联系人(展示名/分组/首末次见面时间)
//! pending_messages  离线待发队列(Envelope 原始字节,可跨重启补发)
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod error;
pub mod store;

pub use error::{Error, Result};
pub use store::{
    ConversationRecord, GroupRecord, MessageMeta, NewMessage, PeerRecord, Store, StoredMessage,
    TransferRecord,
};
