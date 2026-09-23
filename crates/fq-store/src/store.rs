//! SQLite 存储实现。

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

/// 当前模式版本(`PRAGMA user_version`)。
const SCHEMA_VERSION: i64 = 7;

/// 一条待写入的历史消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    /// 消息 ID(MsgId 字符串)。
    pub id: String,
    /// 会话对方(NodeId hex 或群 ID)。
    pub peer: String,
    /// 是否本端发出。
    pub is_outgoing: bool,
    /// 实际发送方(NodeId hex)。
    pub from_node: String,
    /// 载荷类型(`text` 等)。
    pub kind: String,
    /// 文本内容(非文本消息为 None)。
    pub body: Option<String>,
    /// 文本格式(plain/markdown)。
    pub format: Option<String>,
    /// 报文时间戳(Unix 毫秒)。
    pub ts_ms: i64,
    /// 本地入库时间(Unix 毫秒)。
    pub created_ms: i64,
}

/// 一条已存储的历史消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// 同 [`NewMessage`] 各字段。
    pub id: String,
    /// 会话对方。
    pub peer: String,
    /// 是否本端发出。
    pub is_outgoing: bool,
    /// 实际发送方。
    pub from_node: String,
    /// 载荷类型。
    pub kind: String,
    /// 文本内容。
    pub body: Option<String>,
    /// 文本格式。
    pub format: Option<String>,
    /// 报文时间戳。
    pub ts_ms: i64,
    /// 入库时间。
    pub created_ms: i64,
    /// 对端确认送达时间(仅发出消息)。
    pub delivered_ms: Option<i64>,
    /// 对端已读时间(仅发出消息)。
    pub read_ms: Option<i64>,
}

/// 一条传输历史记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecord {
    /// 传输令牌。
    pub token: String,
    /// 对端(NodeId hex 或 `group:...`)。
    pub peer: String,
    /// 方向:`send` / `recv`。
    pub direction: String,
    /// 文件名(相对路径)。
    pub path: String,
    /// 字节大小。
    pub size: u64,
    /// 状态:`active` / `done` / `failed` / `cancelled`。
    pub status: String,
    /// 失败原因等补充说明。
    pub detail: Option<String>,
    /// 开始时间(Unix 毫秒)。
    pub started_ms: i64,
    /// 结束时间(未结束时为 None)。
    pub finished_ms: Option<i64>,
}

/// 一个会话记录(参考 box-im 的会话模型:最近消息 + 未读)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRecord {
    /// 会话键(NodeId hex 或 `group:...`)。
    pub peer: String,
    /// 最近一条消息时间(Unix 毫秒)。
    pub last_msg_ms: i64,
    /// 最近消息预览(文本截断 / `[图片]` / `[文件]`)。
    pub preview: String,
    /// 本地已读位置(Unix 毫秒)。
    pub last_read_ms: i64,
    /// 未读条数。
    pub unread: u32,
}

/// 一个群组记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRecord {
    /// 群组 ID(本地生成,形如 `group:...`)。
    pub id: String,
    /// 群名称。
    pub name: String,
    /// 成员 NodeId(hex)。
    pub members: Vec<String>,
    /// 创建时间(Unix 毫秒)。
    pub created_ms: i64,
}

/// 一条联系人记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {    /// NodeId hex。
    pub node_id: String,
    /// 展示名。
    pub display_name: String,
    /// 主机名。
    pub host_name: Option<String>,
    /// 分组。
    pub group_name: Option<String>,
    /// 首次见面(Unix 毫秒)。
    pub first_seen_ms: i64,
    /// 最近见面(Unix 毫秒)。
    pub last_seen_ms: i64,
}

/// SQLite 存储。
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// 打开(或创建)数据库并升级到当前模式。
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Corrupted(format!("创建数据目录失败: {e}")))?;
        }
        let conn = Connection::open(path)?;
        // WAL:读写并发友好,桌面单进程场景最稳
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// 内存库(测试用)。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::Corrupted(format!(
                "数据库版本 v{version} 高于本程序支持的 v{SCHEMA_VERSION},请升级程序"
            )));
        }
        if version == SCHEMA_VERSION {
            return Ok(());
        }

        // ── v0 → v1:初始三张表 ──
        if version < 1 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS messages (
                     id           TEXT PRIMARY KEY,
                     peer         TEXT NOT NULL,
                     is_outgoing  INTEGER NOT NULL,
                     from_node    TEXT NOT NULL,
                     kind         TEXT NOT NULL,
                     body         TEXT,
                     format       TEXT,
                     ts_ms        INTEGER NOT NULL,
                     created_ms   INTEGER NOT NULL,
                     delivered_ms INTEGER,
                     read_ms      INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS idx_messages_peer_ts ON messages(peer, ts_ms);

                 CREATE TABLE IF NOT EXISTS peers (
                     node_id       TEXT PRIMARY KEY,
                     display_name  TEXT NOT NULL,
                     host_name     TEXT,
                     group_name    TEXT,
                     first_seen_ms INTEGER NOT NULL,
                     last_seen_ms  INTEGER NOT NULL
                 );

                 CREATE TABLE IF NOT EXISTS pending_messages (
                     id         TEXT PRIMARY KEY,
                     peer       TEXT NOT NULL,
                     envelope   BLOB NOT NULL,
                     queued_ms  INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_pending_peer ON pending_messages(peer, queued_ms);

                 PRAGMA user_version = 1;
                 COMMIT;",
            )?;
        }

        // ── v1 → v2:联系人隐藏表(已废弃:v7 起改为"删除只是移出列表"的飞秋语义)──
        if version < 2 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS hidden_peers (
                     node_id   TEXT PRIMARY KEY,
                     hidden_ms INTEGER NOT NULL
                 );
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }

        // ── v2 → v3:群组表 ──
        if version < 3 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS groups (
                     id         TEXT PRIMARY KEY,
                     name       TEXT NOT NULL,
                     members    TEXT NOT NULL,
                     created_ms INTEGER NOT NULL
                 );
                 PRAGMA user_version = 3;
                 COMMIT;",
            )?;
        }

        // ── v3 → v4:会话表(参考 box-im 的会话模型:最近消息 + 未读数)──
        if version < 4 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS conversations (
                     peer          TEXT PRIMARY KEY,
                     last_msg_ms   INTEGER NOT NULL,
                     preview       TEXT NOT NULL DEFAULT '',
                     last_read_ms  INTEGER NOT NULL DEFAULT 0,
                     unread        INTEGER NOT NULL DEFAULT 0
                 );
                 PRAGMA user_version = 4;
                 COMMIT;",
            )?;
            // 已有消息回填会话表(每个会话取最新一条的预览 + 未读总数)
            let _ = self.conn.execute_batch(
                "INSERT OR REPLACE INTO conversations (peer, last_msg_ms, preview, last_read_ms, unread)
                 SELECT m.peer, MAX(m.ts_ms),
                        COALESCE((SELECT CASE
                            WHEN m2.kind IN ('image','file') THEN '[' || m2.kind || ']'
                            ELSE COALESCE(m2.body, '') END
                          FROM messages m2 WHERE m2.peer = m.peer
                          ORDER BY m2.ts_ms DESC LIMIT 1), ''),
                        0,
                        SUM(CASE WHEN m.is_outgoing = 0 THEN 1 ELSE 0 END)
                 FROM messages m GROUP BY m.peer;",
            );
        }

        // ── v4 → v5:传输历史表 ──
        if version < 5 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS transfers (
                     token       TEXT PRIMARY KEY,
                     peer        TEXT NOT NULL,
                     direction   TEXT NOT NULL,
                     path        TEXT NOT NULL,
                     size        INTEGER NOT NULL DEFAULT 0,
                     status      TEXT NOT NULL,
                     detail      TEXT,
                     started_ms  INTEGER NOT NULL,
                     finished_ms INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS idx_transfers_started ON transfers(started_ms DESC);
                 PRAGMA user_version = 5;
                 COMMIT;",
            )?;
        }

        // ── v5 → v6:对端头像缓存(收下后落库,离线也能显示)──
        if version < 6 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS peer_avatars (
                     node_id    TEXT PRIMARY KEY,
                     sha256     TEXT NOT NULL,
                     mime       TEXT NOT NULL,
                     data       BLOB NOT NULL,
                     updated_ms INTEGER NOT NULL
                 );
                 PRAGMA user_version = 6;
                 COMMIT;",
            )?;
        }

        // ── v6 → v7:移除"隐藏联系人"机制(改为飞秋语义:删除只是移出列表,
        // 局域网再发现就重新出现;不需要永久隐藏与恢复)──
        if version < 7 {
            self.conn.execute_batch(
                "BEGIN;
                 DROP TABLE IF EXISTS hidden_peers;
                 PRAGMA user_version = 7;
                 COMMIT;",
            )?;
        }

        Ok(())
    }

    /// 写入/更新对端头像缓存。
    pub fn put_peer_avatar(
        &self,
        node_id: &str,
        sha256: &str,
        mime: &str,
        data: &[u8],
        updated_ms: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO peer_avatars (node_id, sha256, mime, data, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![node_id, sha256, mime, data, updated_ms],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 读取对端头像(哈希 + MIME + 原始字节)。
    pub fn peer_avatar(&self, node_id: &str) -> Result<Option<(String, String, Vec<u8>)>> {
        let mut statement = self.conn.prepare(
            "SELECT sha256, mime, data FROM peer_avatars WHERE node_id = ?1",
        )?;
        let row = statement
            .query_row(params![node_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .optional()?;
        Ok(row)
    }

    /// 对端头像的哈希(判断是否需要重新拉取)。
    pub fn peer_avatar_hash(&self, node_id: &str) -> Result<Option<String>> {
        let mut statement =
            self.conn
                .prepare("SELECT sha256 FROM peer_avatars WHERE node_id = ?1")?;
        let row = statement
            .query_row(params![node_id], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(row)
    }

    /// 全部已缓存头像的 `node_id → sha256`(联系人列表一次性取,避免 N 次查询)。
    pub fn peer_avatar_hashes(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .conn
            .prepare("SELECT node_id, sha256 FROM peer_avatars")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 删除某对端的头像缓存(对端主动移除头像时用)。
    pub fn delete_peer_avatar(&self, node_id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM peer_avatars WHERE node_id = ?1", params![node_id])
            .map_err(Error::Sqlite)?;
        Ok(n > 0)
    }

    /// 写入一条历史消息(幂等:同 ID 重复写入被忽略)。
    ///
    /// 同时维护会话表:`last_msg_ms` / `preview` / `unread`(仅收到时 +1)。
    pub fn insert_message(&self, message: &NewMessage) -> Result<()> {
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO messages
                   (id, peer, is_outgoing, from_node, kind, body, format, ts_ms, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    message.id,
                    message.peer,
                    message.is_outgoing as i64,
                    message.from_node,
                    message.kind,
                    message.body,
                    message.format,
                    message.ts_ms,
                    message.created_ms,
                ],
            )
            .map_err(Error::Sqlite)?;
        if inserted == 0 {
            return Ok(()); // 重复消息不重复累计未读
        }

        let preview = match message.kind.as_str() {
            "image" => "[图片]".to_string(),
            "file" => "[文件]".to_string(),
            _ => message
                .body
                .clone()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect(),
        };
        self.conn
            .execute(
                "INSERT INTO conversations (peer, last_msg_ms, preview, last_read_ms, unread)
                 VALUES (?1, ?2, ?3, 0, ?4)
                 ON CONFLICT(peer) DO UPDATE SET
                     last_msg_ms = MAX(conversations.last_msg_ms, excluded.last_msg_ms),
                     preview     = CASE WHEN excluded.last_msg_ms >= conversations.last_msg_ms
                                        THEN excluded.preview ELSE conversations.preview END,
                     unread      = conversations.unread + ?4",
                params![
                    message.peer,
                    message.ts_ms,
                    preview,
                    if message.is_outgoing { 0i64 } else { 1i64 },
                ],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 某会话当前仍在待发队列里的消息 ID(用于推导 `pending` 状态)。
    pub fn pending_ids(&self, peer: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM pending_messages WHERE peer = ?1")?;
        let rows = statement
            .query_map(params![peer], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 列出全部会话(按最近消息时间倒序)。
    pub fn list_conversations(&self) -> Result<Vec<ConversationRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT peer, last_msg_ms, preview, last_read_ms, unread
             FROM conversations ORDER BY last_msg_ms DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(ConversationRecord {
                    peer: row.get(0)?,
                    last_msg_ms: row.get(1)?,
                    preview: row.get(2)?,
                    last_read_ms: row.get(3)?,
                    unread: row.get::<_, i64>(4)? as u32,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 标记会话已读(清零未读,记录已读时间)。
    pub fn mark_conversation_read(&self, peer: &str, read_ms: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE conversations SET unread = 0, last_read_ms = ?2 WHERE peer = ?1",
                params![peer, read_ms],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 删除会话条目(不删消息历史)。
    pub fn delete_conversation(&self, peer: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM conversations WHERE peer = ?1", params![peer])
            .map_err(Error::Sqlite)?;
        Ok(n > 0)
    }

    /// 分页取更早的历史(滚动加载):返回 `ts_ms < before_ms` 的最近 `limit` 条(升序)。
    pub fn history_before(
        &self,
        peer: &str,
        before_ms: i64,
        limit: u32,
    ) -> Result<Vec<StoredMessage>> {
        let mut statement = self.conn.prepare(
            "SELECT id, peer, is_outgoing, from_node, kind, body, format,
                    ts_ms, created_ms, delivered_ms, read_ms
             FROM messages
             WHERE peer = ?1 AND ts_ms < ?2
             ORDER BY ts_ms DESC
             LIMIT ?3",
        )?;
        let mut rows = statement
            .query_map(params![peer, before_ms, limit], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    peer: row.get(1)?,
                    is_outgoing: row.get::<_, i64>(2)? != 0,
                    from_node: row.get(3)?,
                    kind: row.get(4)?,
                    body: row.get(5)?,
                    format: row.get(6)?,
                    ts_ms: row.get(7)?,
                    created_ms: row.get(8)?,
                    delivered_ms: row.get(9)?,
                    read_ms: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// 更新回执(送达/已读)。返回是否真的更新了行。
    pub fn update_ack(
        &self,
        id: &str,
        delivered_ms: Option<i64>,
        read_ms: Option<i64>,
    ) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE messages SET
                 delivered_ms = COALESCE(?, delivered_ms),
                 read_ms      = COALESCE(?, read_ms)
             WHERE id = ?3",
            params![delivered_ms, read_ms, id],
        )?;
        Ok(updated > 0)
    }

    /// 拉取与某会话的最近 `limit` 条消息(时间升序返回)。
    pub fn history(&self, peer: &str, limit: u32) -> Result<Vec<StoredMessage>> {
        let mut statement = self.conn.prepare(
            "SELECT id, peer, is_outgoing, from_node, kind, body, format,
                    ts_ms, created_ms, delivered_ms, read_ms
             FROM messages
             WHERE peer = ?1
             ORDER BY ts_ms DESC, created_ms DESC
             LIMIT ?2",
        )?;
        let mut rows: Vec<StoredMessage> = statement
            .query_map(params![peer, limit], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    peer: row.get(1)?,
                    is_outgoing: row.get::<_, i64>(2)? != 0,
                    from_node: row.get(3)?,
                    kind: row.get(4)?,
                    body: row.get(5)?,
                    format: row.get(6)?,
                    ts_ms: row.get(7)?,
                    created_ms: row.get(8)?,
                    delivered_ms: row.get(9)?,
                    read_ms: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// 全文搜索(跨会话),按时间倒序。`LIKE` 通配符会被转义。
    pub fn search_messages(&self, query: &str, limit: u32) -> Result<Vec<StoredMessage>> {
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let mut statement = self.conn.prepare(
            "SELECT id, peer, is_outgoing, from_node, kind, body, format,
                    ts_ms, created_ms, delivered_ms, read_ms
             FROM messages
             WHERE body LIKE ?1 ESCAPE '\\'
             ORDER BY ts_ms DESC
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![pattern, limit], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    peer: row.get(1)?,
                    is_outgoing: row.get::<_, i64>(2)? != 0,
                    from_node: row.get(3)?,
                    kind: row.get(4)?,
                    body: row.get(5)?,
                    format: row.get(6)?,
                    ts_ms: row.get(7)?,
                    created_ms: row.get(8)?,
                    delivered_ms: row.get(9)?,
                    read_ms: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 入队一条待发消息(Envelope 编码字节)。
    pub fn enqueue(&self, id: &str, peer: &str, envelope: &[u8], queued_ms: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO pending_messages (id, peer, envelope, queued_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, peer, envelope, queued_ms],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 取某会话的全部待发消息(按入队时间升序),(id, envelope, queued_ms)。
    pub fn pending(&self, peer: &str) -> Result<Vec<(String, Vec<u8>, i64)>> {
        let mut statement = self.conn.prepare(
            "SELECT id, envelope, queued_ms FROM pending_messages
             WHERE peer = ?1 ORDER BY queued_ms ASC",
        )?;
        let rows = statement
            .query_map(params![peer], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 待发队列长度。
    pub fn pending_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// 移除一条已发出的待发消息。
    pub fn remove_pending(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_messages WHERE id = ?1", params![id])
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 写入/更新联系人。
    pub fn upsert_peer(&self, record: &PeerRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO peers (node_id, display_name, host_name, group_name, first_seen_ms, last_seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(node_id) DO UPDATE SET
                     display_name  = excluded.display_name,
                     host_name     = excluded.host_name,
                     group_name    = excluded.group_name,
                     last_seen_ms  = excluded.last_seen_ms",
                params![
                    record.node_id,
                    record.display_name,
                    record.host_name,
                    record.group_name,
                    record.first_seen_ms,
                    record.last_seen_ms,
                ],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 全部联系人。
    ///
    /// 说明:不区分"已删除"——删除只删联系人行,局域网里再发现到就重新入库
    /// (飞秋语义:联系人列表由发现驱动,不存在永久隐藏/恢复)。
    pub fn list_peers(&self) -> Result<Vec<PeerRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT node_id, display_name, host_name, group_name, first_seen_ms, last_seen_ms
             FROM peers
             ORDER BY display_name",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(PeerRecord {
                    node_id: row.get(0)?,
                    display_name: row.get(1)?,
                    host_name: row.get(2)?,
                    group_name: row.get(3)?,
                    first_seen_ms: row.get(4)?,
                    last_seen_ms: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 查询单个联系人。
    pub fn peer(&self, node_id: &str) -> Result<Option<PeerRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT node_id, display_name, host_name, group_name, first_seen_ms, last_seen_ms
             FROM peers WHERE node_id = ?1",
        )?;
        let record = statement
            .query_row(params![node_id], |row| {
                Ok(PeerRecord {
                    node_id: row.get(0)?,
                    display_name: row.get(1)?,
                    host_name: row.get(2)?,
                    group_name: row.get(3)?,
                    first_seen_ms: row.get(4)?,
                    last_seen_ms: row.get(5)?,
                })
            })
            .optional()?;
        Ok(record)
    }

    /// 删除联系人行(聊天记录、会话、头像缓存都保留)。
    ///
    /// 只是"暂时移出列表":局域网里再收到该对端通告就会重新入库并上屏。
    pub fn delete_peer(&self, node_id: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM peers WHERE node_id = ?1", params![node_id])
            .map_err(Error::Sqlite)?;
        Ok(deleted > 0)
    }

    /// 创建群组。
    pub fn create_group(&self, id: &str, name: &str, members: &[String]) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO groups (id, name, members, created_ms) VALUES (?1, ?2, ?3, ?4)",
                params![
                    id,
                    name,
                    members.join(","),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0)
                ],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 列出全部群组。
    pub fn list_groups(&self) -> Result<Vec<GroupRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT id, name, members, created_ms FROM groups ORDER BY created_ms",
        )?;
        let rows = statement
            .query_map([], |row| {
                let members_raw: String = row.get(2)?;
                Ok(GroupRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    members: members_raw
                        .split(',')
                        .filter(|m| !m.is_empty())
                        .map(str::to_string)
                        .collect(),
                    created_ms: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 查询单个群组。
    pub fn get_group(&self, id: &str) -> Result<Option<GroupRecord>> {
        Ok(self.list_groups()?.into_iter().find(|g| g.id == id))
    }

    /// 向群组添加成员(已存在则幂等)。
    pub fn add_group_member(&self, id: &str, member: &str) -> Result<()> {
        if let Some(mut group) = self.get_group(id)? {
            if !group.members.iter().any(|m| m == member) {
                group.members.push(member.to_string());
                self.create_group(&group.id, &group.name, &group.members)?;
            }
        }
        Ok(())
    }

    /// 删除群组(仅本地;历史消息保留)。
    pub fn delete_group(&self, id: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM groups WHERE id = ?1", params![id])
            .map_err(Error::Sqlite)?;
        Ok(deleted > 0)
    }

    /// 记录一次传输开始(`status = active`)。
    pub fn record_transfer_start(
        &self,
        token: &str,
        peer: &str,
        direction: &str,
        path: &str,
        size: u64,
        started_ms: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO transfers
                   (token, peer, direction, path, size, status, detail, started_ms, finished_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'active', NULL, ?6, NULL)",
                params![token, peer, direction, path, size as i64, started_ms],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 结束一次传输(写入最终状态与原因)。
    pub fn record_transfer_finish(
        &self,
        token: &str,
        status: &str,
        detail: Option<&str>,
        finished_ms: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE transfers SET status = ?2, detail = ?3, finished_ms = ?4 WHERE token = ?1",
                params![token, status, detail, finished_ms],
            )
            .map(|_| ())
            .map_err(Error::Sqlite)
    }

    /// 传输历史(按开始时间倒序)。
    pub fn list_transfers(&self, limit: u32) -> Result<Vec<TransferRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT token, peer, direction, path, size, status, detail, started_ms, finished_ms
             FROM transfers ORDER BY started_ms DESC LIMIT ?1",
        )?;
        let rows = statement
            .query_map(params![limit], |row| {
                Ok(TransferRecord {
                    token: row.get(0)?,
                    peer: row.get(1)?,
                    direction: row.get(2)?,
                    path: row.get(3)?,
                    size: row.get::<_, i64>(4)? as u64,
                    status: row.get(5)?,
                    detail: row.get(6)?,
                    started_ms: row.get(7)?,
                    finished_ms: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 清空传输历史(不影响进行中的传输)。
    pub fn clear_transfers(&self) -> Result<usize> {
        let n = self
            .conn
            .execute("DELETE FROM transfers WHERE status != 'active'", [])
            .map_err(Error::Sqlite)?;
        Ok(n)
    }

    /// 删除单条传输历史。
    pub fn delete_transfer(&self, token: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM transfers WHERE token = ?1", params![token])
            .map_err(Error::Sqlite)?;
        Ok(n > 0)
    }

    /// 启动时把残留的 `active` 记录标记为中断(上次进程未正常退出)。
    pub fn mark_stale_transfers(&self) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "UPDATE transfers SET status = 'failed', detail = '进程重启,传输中断'
                 WHERE status = 'active'",
                [],
            )
            .map_err(Error::Sqlite)?;
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn message(id: &str, peer: &str, outgoing: bool, ts: i64, body: &str) -> NewMessage {
        NewMessage {
            id: id.to_string(),
            peer: peer.to_string(),
            is_outgoing: outgoing,
            from_node: if outgoing { "me".into() } else { peer.to_string() },
            kind: "text".into(),
            body: Some(body.to_string()),
            format: Some("plain".into()),
            ts_ms: ts,
            created_ms: ts + 1,
        }
    }

    #[test]
    fn history_returns_chronological_tail() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..10 {
            store
                .insert_message(&message(&format!("m{i}"), "peer-a", i % 2 == 0, 1000 + i, "hi"))
                .unwrap();
        }
        let tail = store.history("peer-a", 3).unwrap();
        assert_eq!(
            tail.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["m7", "m8", "m9"],
            "应返回最近 3 条且时间升序"
        );
        // 会话隔离
        assert!(store.history("peer-b", 10).unwrap().is_empty());
    }

    /// 对端头像缓存:写入/读取/哈希 + 会话删除只影响会话表。
    #[test]
    fn peer_avatar_cache_and_conversation_delete() {
        let store = Store::open_in_memory().unwrap();

        // 头像缓存往返
        let data = vec![0x89u8, b'P', b'N', b'G', 1, 2, 3];
        store
            .put_peer_avatar("node-a", &"a".repeat(64), "image/png", &data, 111)
            .unwrap();
        let (sha, mime, bytes) = store.peer_avatar("node-a").unwrap().expect("应能读到头像");
        assert_eq!(sha, "a".repeat(64));
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, data);
        assert_eq!(
            store.peer_avatar_hash("node-a").unwrap().as_deref(),
            Some("a".repeat(64).as_str())
        );
        // 批量哈希(联系人列表一次取)
        assert_eq!(store.peer_avatar_hashes().unwrap().len(), 1);
        // 覆盖更新(换头像)
        let data2 = vec![9u8; 4];
        store
            .put_peer_avatar("node-a", &"b".repeat(64), "image/png", &data2, 222)
            .unwrap();
        let (sha2, _, bytes2) = store.peer_avatar("node-a").unwrap().unwrap();
        assert_eq!(sha2, "b".repeat(64));
        assert_eq!(bytes2, data2);
        // 未缓存的对端
        assert!(store.peer_avatar("node-b").unwrap().is_none());
        assert!(store.peer_avatar_hash("node-b").unwrap().is_none());

        // 会话删除:只掉会话行,消息与联系人保留
        store
            .insert_message(&message("m1", "node-a", false, 100, "你好"))
            .unwrap();
        assert_eq!(store.list_conversations().unwrap().len(), 1);
        assert!(store.delete_conversation("node-a").unwrap());
        assert!(store.list_conversations().unwrap().is_empty(), "会话应被移除");
        assert_eq!(store.history("node-a", 10).unwrap().len(), 1, "历史消息必须保留");
        // 重复删除返回 false
        assert!(!store.delete_conversation("node-a").unwrap());
        // 新消息到达后会重新出现(微信语义)
        store
            .insert_message(&message("m2", "node-a", false, 200, "在吗"))
            .unwrap();
        assert_eq!(store.list_conversations().unwrap().len(), 1, "新消息应重建会话");
    }

    #[test]
    fn duplicate_insert_is_ignored() {
        let store = Store::open_in_memory().unwrap();
        let msg = message("dup", "peer-a", true, 1, "只存一次");
        store.insert_message(&msg).unwrap();
        store.insert_message(&msg).unwrap();
        assert_eq!(store.history("peer-a", 10).unwrap().len(), 1);
    }

    #[test]
    fn ack_updates_only_filled_fields() {
        let store = Store::open_in_memory().unwrap();
        store.insert_message(&message("m1", "p", true, 1, "x")).unwrap();

        assert!(store.update_ack("m1", Some(111), None).unwrap());
        let msg = &store.history("p", 1).unwrap()[0];
        assert_eq!(msg.delivered_ms, Some(111));
        assert_eq!(msg.read_ms, None);

        // 再补已读,送达时间不被清空(COALESCE 语义)
        assert!(store.update_ack("m1", None, Some(222)).unwrap());
        let msg = &store.history("p", 1).unwrap()[0];
        assert_eq!(msg.delivered_ms, Some(111));
        assert_eq!(msg.read_ms, Some(222));

        // 不存在的消息
        assert!(!store.update_ack("ghost", Some(1), Some(1)).unwrap());
    }

    #[test]
    fn pending_queue_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        store.enqueue("q1", "p", b"env-1", 1).unwrap();
        store.enqueue("q2", "p", b"env-2", 2).unwrap();
        store.enqueue("q3", "other", b"env-3", 3).unwrap();

        assert_eq!(store.pending_count().unwrap(), 3);
        let queue = store.pending("p").unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0], ("q1".to_string(), b"env-1".to_vec(), 1));
        assert_eq!(queue[1], ("q2".to_string(), b"env-2".to_vec(), 2));

        store.remove_pending("q1").unwrap();
        assert_eq!(store.pending_count().unwrap(), 2);
        assert_eq!(store.pending("p").unwrap()[0].0, "q2");
    }

    #[test]
    fn peers_upsert_keeps_first_seen() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_peer(&PeerRecord {
                node_id: "n1".into(),
                display_name: "张三".into(),
                host_name: Some("PC-1".into()),
                group_name: None,
                first_seen_ms: 100,
                last_seen_ms: 100,
            })
            .unwrap();
        // 二次见面:名字变了,first_seen 保留
        store
            .upsert_peer(&PeerRecord {
                node_id: "n1".into(),
                display_name: "张三(改)".into(),
                host_name: Some("PC-1".into()),
                group_name: Some("研发".into()),
                first_seen_ms: 999,
                last_seen_ms: 500,
            })
            .unwrap();

        let peer = store.peer("n1").unwrap().unwrap();
        assert_eq!(peer.display_name, "张三(改)");
        assert_eq!(peer.group_name.as_deref(), Some("研发"));
        assert_eq!(peer.first_seen_ms, 100, "首次见面时间不得被覆盖");
        assert_eq!(peer.last_seen_ms, 500);
    }

    #[test]
    fn database_file_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data/fq.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .insert_message(&message("m1", "p", false, 42, "重启后还在"))
                .unwrap();
        }
        let reopened = Store::open(&path).unwrap();
        let history = reopened.history("p", 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].body.as_deref(), Some("重启后还在"));
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fq.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let result = Store::open(&path);
        assert!(result.is_err(), "高于程序支持的版本必须拒绝打开");
    }

    #[test]
    fn delete_peer_removes_from_list_but_rediscovery_restores() {
        let store = Store::open_in_memory().unwrap();
        let make = |id: &str, name: &str| PeerRecord {
            node_id: id.into(),
            display_name: name.into(),
            host_name: None,
            group_name: None,
            first_seen_ms: 1,
            last_seen_ms: 1,
        };
        store.upsert_peer(&make("d1", "要删的人")).unwrap();
        store.upsert_peer(&make("d2", "正常的人")).unwrap();
        assert_eq!(store.list_peers().unwrap().len(), 2);

        // 删除:行消失,但聊天记录/会话不受影响
        store
            .insert_message(&message("m1", "d1", false, 100, "之前聊过"))
            .unwrap();
        assert!(store.delete_peer("d1").unwrap());
        let left = store.list_peers().unwrap();
        assert_eq!(left.len(), 1, "删除后列表里没有它");
        assert_eq!(left[0].node_id, "d2");
        assert_eq!(store.history("d1", 10).unwrap().len(), 1, "聊天记录必须保留");
        assert!(!store.delete_peer("d1").unwrap(), "重复删除返回 false");

        // 飞秋语义:局域网里再发现到就重新入库上屏
        store.upsert_peer(&make("d1", "要删的人")).unwrap();
        assert_eq!(store.list_peers().unwrap().len(), 2, "重新发现后应回到列表");
    }

    #[test]
    fn search_messages_matches_across_conversations() {
        let store = Store::open_in_memory().unwrap();
        store.insert_message(&message("s1", "peer-a", true, 100, "项目进度汇报")).unwrap();
        store.insert_message(&message("s2", "peer-b", false, 200, "收到,项目明天交付")).unwrap();
        store.insert_message(&message("s3", "peer-a", true, 300, "无关消息")).unwrap();

        // 跨会话命中,按时间倒序
        let hits = store.search_messages("项目", 50).unwrap();
        assert_eq!(hits.len(), 2, "应命中两条");
        assert_eq!(hits[0].id, "s2", "最新的在前");
        assert_eq!(hits[1].id, "s1");

        // 无匹配
        assert!(store.search_messages("不存在的词", 50).unwrap().is_empty());
    }

    #[test]
    fn search_escapes_like_wildcards() {
        let store = Store::open_in_memory().unwrap();
        store.insert_message(&message("p1", "peer", true, 1, "完成度100%")).unwrap();
        store.insert_message(&message("p2", "peer", true, 2, "普通消息")).unwrap();

        // % 应作为字面量匹配,而不是通配符
        let hits = store.search_messages("100%", 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "p1");

        // 纯 % 只匹配"真的包含 % 字符"的消息,而不是匹配所有消息
        let pct_hits = store.search_messages("%", 50).unwrap();
        assert_eq!(pct_hits.len(), 1, "通配符必须被转义,否则会匹配全部消息");
        assert_eq!(pct_hits[0].id, "p1");
    }

    #[test]
    fn conversations_track_preview_and_unread() {
        let store = Store::open_in_memory().unwrap();
        // 收到 2 条(未读 +1),发出 1 条(不加未读)
        store.insert_message(&message("c1", "peer-a", false, 100, "第一条")).unwrap();
        store.insert_message(&message("c2", "peer-a", false, 200, "第二条")).unwrap();
        store.insert_message(&message("c3", "peer-a", true, 300, "我回的")).unwrap();

        let convs = store.list_conversations().unwrap();
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.peer, "peer-a");
        assert_eq!(conv.unread, 2, "只统计收到的");
        assert_eq!(conv.preview, "我回的", "预览应为最新一条");
        assert_eq!(conv.last_msg_ms, 300);

        // 重复插入不重复累计未读
        store.insert_message(&message("c1", "peer-a", false, 100, "第一条")).unwrap();
        assert_eq!(store.list_conversations().unwrap()[0].unread, 2);

        // 标记已读
        store.mark_conversation_read("peer-a", 400).unwrap();
        let conv = &store.list_conversations().unwrap()[0];
        assert_eq!(conv.unread, 0);
        assert_eq!(conv.last_read_ms, 400);
    }

    #[test]
    fn conversation_preview_summarizes_media() {
        let store = Store::open_in_memory().unwrap();
        let mut img = message("i1", "peer-x", false, 100, "");
        img.kind = "image".into();
        img.body = Some("{\"n\":\"a.png\"}".into());
        store.insert_message(&img).unwrap();
        assert_eq!(store.list_conversations().unwrap()[0].preview, "[图片]");

        let mut file = message("f1", "peer-y", true, 200, "");
        file.kind = "file".into();
        file.body = Some("{\"n\":\"a.zip\"}".into());
        store.insert_message(&file).unwrap();
        assert_eq!(store.list_conversations().unwrap()[0].preview, "[文件]");
    }

    #[test]
    fn history_before_paginates_backwards() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..10 {
            store
                .insert_message(&message(&format!("m{i}"), "p", true, 100 + i, "x"))
                .unwrap();
        }
        // 取早于 ts=105 的最近 3 条 → m2/m3/m4(升序)
        let page = store.history_before("p", 105, 3).unwrap();
        assert_eq!(
            page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["m2", "m3", "m4"]
        );
        // 最早一页
        let first = store.history_before("p", 102, 3).unwrap();
        assert_eq!(first.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["m0", "m1"]);
    }

    #[test]
    fn v1_database_migrates_to_latest_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fq.db");
        {
            // 模拟 v1 库:只有三张表
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (id TEXT PRIMARY KEY);
                 CREATE TABLE peers (node_id TEXT PRIMARY KEY);
                 CREATE TABLE pending_messages (id TEXT PRIMARY KEY);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        // 重新打开应逐级迁移到最新版本(含 v2 建表 → v7 删除 hidden_peers)
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION,
            "应迁移到最新版本"
        );
        // 旧库的 peers 表列不全(fixture 只建了主键),这里只验证:
        // 版本已升到最新、机制表被移除、按 node_id 的删除语句可执行
        assert!(!store.delete_peer("p1").unwrap(), "表里没有该行");
        // hidden_peers 已不再存在(机制移除)
        let legacy: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='hidden_peers'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 0, "v7 应已删除 hidden_peers 表");
    }
}
