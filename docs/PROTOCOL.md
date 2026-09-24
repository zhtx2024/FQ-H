# feiqiu-r 局域网通信协议 v1

> 状态:**已实现并测试**(`crates/fq-proto`,56 项测试全绿)
> 权威实现:本仓库 `crates/fq-proto`。本文档与实现不一致时,**以测试为准**。

---

## 1. 范围与定位

本协议是 feiqiu-r 自有的局域网即时通信协议,**刻意不与 IPMSG / 飞秋 / 飞鸽传书互通**。

为什么不用 IPMSG:

| 维度 | IPMSG(飞秋所基于的协议) | 本协议 v1 |
|---|---|---|
| 编码 | 默认 CP932,中文实现用 GBK,UTF-8 需能力位协商 | **仅 UTF-8** |
| 选项位 | 同名位在不同命令族里含义不同(`0x100` 既是 `ABSENCEOPT` 又是 `SENDCHECKOPT`) | 能力位集独立,**不复用** |
| 扩展性 | 定长数组式的字符串拼接,"加字段即破坏兼容" | MessagePack map,**未知字段/未知类型/未知枚举值全部安全降级** |
| 加密 | RSA + RC2/Blowfish 等历史算法 | **Noise IK**(Curve25519 + ChaChaPoly + BLAKE2s) |
| 身份 | 用户名字符串,可随意冒充 | Ed25519 公钥派生 `NodeId`,与握手密钥绑定 |
| 文件传输 | 无断点续传 | **按 offset 续传 + SHA-256 校验** |

**兼容性对冲(架构约束)**:`fq-net` 只依赖 `Codec` / `Discovery` 抽象,不直接依赖本协议。
未来若需接入 IPMSG 或逆向飞秋私有扩展,新增 adapter crate 即可,网络层与 UI 无需改动。

---

## 2. 传输层

| 用途 | 传输 | 默认端口 |
|---|---|---|
| 发现 / 在线心跳 | UDP 广播 `255.255.255.255` + 各网卡定向广播 + IPv6 组播 | `24250` |
| 控制报文 / 文本 / 文件 | TCP(长连接) | `24250` |

端口**刻意避开 IPMSG 的 2425**:同机若同时运行飞秋等老客户端,监听同一端口会让双方都收到无法解析的报文。

心跳间隔 20s,超过 60s 未收到判定离线。所有监听失败、广播被企业网络阻断等场景,支持手动填写 IP 直连作为退路。

---

## 3. 分帧

TCP 上所有报文都用 **4 字节大端长度前缀**:

```
+----------------+---------------------------+
| u32 BE length  | payload(length 字节)      |
+----------------+---------------------------+
```

| 参数 | 值 | 说明 |
|---|---|---|
| 长度前缀 | 4 字节,大端 | |
| 最大帧 | **1 MiB** | 文件分块默认 256 KiB,留足余量 |
| 超限处理 | **读到长度前缀即刻断开并报错** | 绝不先缓冲再判断 |

**为什么必须立即拒绝**:若先缓冲再判断,一个恶意声明的 4 GiB 长度就能让接收方 OOM。
同理,空帧(长度 0)合法,但必须在应用层被解码器拒绝(不是合法 MessagePack 报文)。

---

## 4. 编码规则

### 4.1 强制使用 map 编码

报文必须是 MessagePack 的 **map**,**禁止**使用数组编码(即 Rust 侧禁止 `rmp_serde::to_vec`,
必须用 `to_vec_named`)。原因:

1. 内部标签枚举无法向定长数组注入 `kind` 标签,序列化直接失败。
2. 数组是定长的 —— **加一个字段就破坏与老版本的兼容**。
3. `#[serde(default)]` 在数组编码下永远不会生效。

### 4.2 扁平信封

`kind` 标签与载荷字段**内联**在同一层 map 中:

```jsonc
{
  "v": 1, "id": "<uuidv7>", "from": "<node_id>", "to": null, "ts_ms": 1750000000000,
  "kind": "text",              // ← 标签在顶层
  "body": "你好", "format": "plain", "reply_to": null, "mentions": [], "group_id": null
}
```

**禁止**嵌套形式 `{"kind": {"kind": "text", …}}` —— 同名键嵌套既浪费字节又易误实现,
已有回归测试拒绝该输入(`nested_kind_map_is_rejected_because_wire_format_must_be_flat`)。

**字段命名空间**:载荷字段名不得使用 `v` / `id` / `from` / `to` / `ts_ms` / `kind`。

### 4.3 键的省略策略(明确决策)

可选字段在**值为 null 时仍然输出**(`to: null` 而不是省略该键)。理由:格式统一,
跨语言手写解析器与抓包排查更简单;代价是每个空可选键约 3 字节,而传输大头(文件分块)
本来就没有可选字段。

### 4.4 二进制字段

`public_key`、`FileChunk.data` 必须使用 MessagePack **bin** 类型(`0xc4` / `0xc5` / `0xc6`)。
若误用默认的 `Vec<u8>` 编码(每字节编成一个整数),体积会膨胀 **3~5 倍**。
已有测试断言 64 KiB 分块编码后不超过 `数据长度 + 1024` 字节。

### 4.5 解码安全契约(强制)

解码路径**只允许返回错误,不允许 panic / abort**。交给 serde 之前先做结构预校验
(`msgpack_guard`):

| 校验 | 阈值 | 防的是什么 |
|---|---|---|
| 容器声明长度 ≤ 帧内剩余字节 | — | 声明 4 GiB 长度导致的超大内存分配 |
| 嵌套深度 | 64 层 | 超深嵌套导致的栈溢出 |
| 恰好消费完,无尾部残留 | — | 报文走私 / 解析歧义 |
| 保留标记 `0xc1` | 拒绝 | 非法类型标记 |

回归测试:4000 轮随机字节、4000 轮位翻转、全长度截断、20000 层嵌套、伪造超大长度。

---

## 5. 信封字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `v` | u16 | ✅ | 协议版本,本端当前为 `1` |
| `id` | str(36) | ✅ | 消息 ID,UUIDv7(自带毫秒时间戳,可按时间排序) |
| `from` | str(32) | ✅ | 发送方 `NodeId`,32 字符小写 hex |
| `to` | str(32) / null | ✅(可 null) | 目标 `NodeId`;`null` = 广播/群组 |
| `ts_ms` | i64 | ✅ | 发送时刻 Unix 毫秒 |
| `kind` | str | ✅ | 载荷类型标签 |

### NodeId 的派生

```
NodeId = SHA-256(Ed25519 公钥)[0..16]        // 16 字节 → 32 字符 hex
```

用公钥派生而非随机数,是为了让"身份"与"Noise 握手的静态密钥"天然绑定 ——
攻击者无法在不伪造签名的前提下冒充某个 `NodeId`。

---

## 6. 报文类型

| `kind` | 载荷 | 瞬态 | 说明 |
|---|---|---|---|
| `presence` | `PresenceInfo` | ✅ | 上线/更新/下线/心跳 |
| `text` | `TextBody` | | 文本消息(Markdown 可选) |
| `ack` | `AckBody` | | 送达 / 已读回执 |
| `typing` | `TypingBody` | ✅ | 正在输入 |
| `shake` | `ShakeBody` | | 窗口抖动(老版本忽略,不影响兼容) |
| `recall` | `RecallBody` | | 消息撤回(按消息 ID 标记) |
| `file_offer` | `FileOffer` | | 文件/目录要约(含清单) |
| `file_request` | `FileRequest` | | 请求数据(带 offset,支持续传) |
| `file_chunk` | `FileChunk` | | 数据分块(bin 编码) |
| `file_done` | `FileDone` | | 单文件完成(带 SHA-256) |
| `file_abort` | `FileAbort` | | 中止传输 |
| `update_offer` | `UpdateOffer` | | 更新包要约(接收方**自动接收**,不弹确认框) |
| `update_request` | `UpdateRequest` | | 索取更新包(对端版本更高时以 `update_offer` 回发) |
| `avatar_request` | `AvatarRequest` | | 索取头像(带已知哈希;相同则可不回发) |
| `avatar_reply` | `AvatarReply` | | 头像应答(整图一次发完,`data` 为 bin) |
| `ping` / `pong` | `PingBody` | ✅ | 保活探测/应答 |
| 其它任意值 | — | | 降级为 `Unknown`,**解码不失败** |

### 6.1 PresenceInfo

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `event` | str | — | `announce` / `update` / `leave` / `heartbeat` |
| `display_name` | str | — | 昵称 |
| `host_name` | str | — | 主机名 |
| `status` | str | `online` | `online` / `away` / `busy` / `dnd` / `offline` / `unknown` |
| `group` | str / null | `null` | 分组 |
| `port` | u16 | `24250` | 监听端口 |
| `endpoints` | [str] | `[]` | 可用地址 `ip:port`,多网卡直连用 |
| `public_key` | bin(32) | `[]` | Ed25519 身份公钥,NodeId 派生依据 |
| `noise_static` | bin(32) | `[]` | X25519 Noise IK 静态公钥,发起握手必需 |
| `binding_signature` | bin(64) | `[]` | 身份对 `noise_static` 的绑定签名;接收方必须验证,失败即整条丢弃 |
| `capabilities` | u64 | `0` | 能力位集 |
| `avatar_sha256` | str / null | `null` | 头像内容哈希(hex) |
| `app_version` | str / null | `null` | 软件版本(局域网更新发现用) |

### 6.1.1 头像同步(0.5.0)

头像**不放进通告**(UDP 数据报会被 IP 分片,且浪费广播带宽),只放 64 字节哈希;
接收方发现哈希与本地缓存不同时,用 TCP 加密通道按需拉取整图:

```
A(NodeId 较小,发起方)                B
 │ presence(avatar_sha256=h2) ──────►│   (B 改了头像)
 │                                   │
 │ ◄──────── avatar_request(known=h1)│   A 缓存是 h1 ≠ h2 → 索取
 │ avatar_reply(h2 + 图) ───────────►│
 │                                   │
 │  ⚠ 只有一方发起,避免"互拨替换连接 → RST 丢在途帧"
```

**一次往返双向同步**:`AvatarRequest` 里可以顺带捎上自己的头像(`mine`),
对端收到即缓存 —— 于是"我改头像"不需要对端也来拨号:

```
A 改头像 → presence(h3) + avatar_request(known=缓存, mine=A的图) ──► B
                                                                    ↓ 直接缓存 A 的图
                                                    (若 known≠h3 才回发 B 的图)
```

| 字段 | 说明 |
|---|---|
| `AvatarRequest.known_sha256` | 请求方缓存的**对方**头像哈希;相同则对端不回发(省流量) |
| `AvatarRequest.mine` | 请求方**自己的**头像(`{sha256, mime, data}`,≤256 KiB);可为 null |
| `AvatarReply.avatar` | 对端头像(同上结构) |

约定:

- 能力位 `AVATAR`(位 8)声明支持;**仅对声明该能力的对端**发起。
- 接收方**必须**校验 `sha256(data) == sha256`,不一致即丢弃(防篡改/损坏)。
- **发起方确定**:NodeId 较小的一方主动拉取(全序,永不互撞);较大一方在 15s
  兜底后也允许发起(应对对方不支持/一直不拉)。
- 失败重试:退避 1s→2s→3s→6s(最多 12 次)+ 按 NodeId 派生的固定抖动。
- 对端通告 `avatar_sha256 = null` 表示**已移除头像**,接收方清掉本地缓存。

### 6.2 文本 / 回执 / 输入中

| 类型 | 字段 |
|---|---|
| `TextBody` | `body`(str)、`format`(`plain`/`markdown`)、`reply_to`(MsgId/null)、`mentions`([NodeId])、`group_id`(str/null) |
| `AckBody` | `ack_id`(MsgId)、`status`(`delivered`/`read`) |
| `TypingBody` | `state`(`started`/`stopped`)、`thread`(MsgId/null) |
| `ShakeBody` | `reason`(str/null,附带说明) |
| `RecallBody` | `message_id`(MsgId,被撤回的消息) |

**引用与撤回只传 ID(0.9.0 / 0.10.0)**

`reply_to` 与 `RecallBody.message_id` 都**不带正文**:消息 ID 是全局唯一 UUIDv7,
两端各自在自己的历史里查原文 / 标记已撤回。好处是协议不膨胀、历史不被远端文本污染。

撤回的两条硬性约定:

1. **只有发送方能撤回自己的消息**,且默认要求 2 分钟窗口内(`RECALL_WINDOW_MS`);
   接收方按「这条消息的 `from_node` 是否等于撤回报文的来源」做越权校验,不通过即忽略。
2. **群消息是「一个逻辑消息、N 份投递」**:`send_group_text` 扇出时所有成员的副本
   **共用同一个消息 ID 与时间戳**,否则每端各存各的 ID,撤回无法定位到对方的副本。

老版本收到 `recall` 会走 `Unknown` 分支忽略(解码不失败),只是不会隐藏那条消息。

### 6.3 文件与目录

| 类型 | 字段 |
|---|---|
| `FileOffer` | `token`(传输令牌)、`manifest`、`message`(str/null) |
| `FileManifest` | `root_name`、`total_bytes`、`entries`([FileEntry]) |
| `FileEntry` | `path`(相对路径,统一 `/` 分隔)、`size`、`mtime_ms`、`kind`(`file`/`dir`/`symlink`)、`sha256`(str/null) |
| `FileRequest` | `token`、`path`、`offset`、`chunk_size`(u32/null) |
| `FileChunk` | `token`、`path`、`offset`、`data`(bin) |
| `FileDone` | `token`、`path`、`sha256`(str/null) |
| `FileAbort` | `token`、`path`、`reason` |

**传输流程**

```
发送方                                          接收方
   │  file_offer(token, manifest)  ───────────────▶
   │                                ◀─────────────  file_request(token, path, offset=已落盘字节数)
   │  file_chunk(offset, data) × N  ───────────────▶
   │  file_done(sha256)             ───────────────▶   校验 SHA-256
```

* **断点续传**:接收方在 `file_request` 里带上已正确落盘的字节数作为 `offset`,
  发送方从该偏移继续。中断后重连即可续传,无需重传已完成部分。
* **完整性**:逐文件 SHA-256,接收方校验不通过则丢弃并回 `file_abort`。
* **目录**:清单里的 `dir` 条目用于在接收端还原目录树(含空目录)。
* **解耦**:同一 TCP 连接上按 `token` 复用,支持多文件/多会话并发。

---

## 7. 能力协商

`capabilities` 是 u64 位集,双方按 `本端 ∩ 对端` 决定实际启用哪些特性。

| 位 | 名称 | 含义 |
|---|---|---|
| 0 | `TEXT` | 纯文本 |
| 1 | `MARKDOWN` | Markdown 富文本 |
| 2 | `FILE_TRANSFER` | 单文件传输 |
| 3 | `FILE_RESUME` | 断点续传 |
| 4 | `DIRECTORY` | 目录传输 |
| 5 | `TYPING` | 正在输入 |
| 6 | `READ_RECEIPT` | 送达/已读回执 |
| 7 | `GROUP_CHAT` | 群聊 |
| 8 | `AVATAR` | 头像同步 |
| 9 | `PRESENCE_STATUS` | 在线状态 |
| 10 | `NOISE_IK` | 安全通道(**v1 必选**) |
| 11 | `IPV6` | IPv6 组播发现 |
| 32–63 | — | **预留给实验特性,任何实现不得占用** |

**未知位必须原样保留**(`from_bits_retain`),不得因为"本端不认识"而报错或丢弃 ——
否则未来版本的新能力会在老版本节点上被静默清除。

v1 的最低要求:`TEXT | NOISE_IK`。

---

## 8. 安全模型

| 目标 | 手段 |
|---|---|
| 机密性 + 前向保密 | Noise `IK`(Curve25519 / ChaChaPoly / BLAKE2s),1-RTT |
| 双向认证 | 双方 Ed25519 静态公钥;`NodeId` 由公钥派生并校验 |
| 首次信任 | **TOFU**:首次见面展示指纹供用户确认,之后锁定 |
| 防重放 | Noise 握手 nonce + 消息 ID 去重窗口 |
| 传输完整性 | 文件 SHA-256;分块偏移校验 |

Noise `IK` 模式的前提是发起方已从发现报文拿到响应方的静态公钥 ——
发现报文里的公钥是**自签名声明**,因此必须用 `NodeId == SHA-256(pubkey)[0..16]`
做一致性校验,不匹配直接丢弃(防止伪造发现报文做中间人)。

---

## 9. 前向兼容策略(版本演进)

| 变更类型 | 是否安全 | 机制 |
|---|---|---|
| 新增可选字段 | ✅ | `#[serde(default)]` |
| 新增报文类型 | ✅ | 未知 `kind` → `Unknown`,不报错 |
| 新增枚举取值 | ✅ | 未知取值 → 各自 `Unknown` 兜底 |
| 新增能力位 | ✅ | 未知位原样保留 |
| **删除 / 改名现有字段** | ❌ | 破坏性变更,必须提升 `v` |
| **改变字段类型或语义** | ❌ | 破坏性变更,必须提升 `v` |
| **改变分帧或编码规则** | ❌ | 破坏性变更,必须提升 `v` |

版本策略:接收方用 `is_protocol_compatible()`(`v <= 本端`)判断。
`decode()` 会**宽松解码**不合版本报文(便于回一条"请升级"提示),
`decode_checked()` **严格拒绝**(返回 `UnsupportedVersion`)。两者的选择权留给上层。

---

## 10. 实测字节样例

以下由 `cargo run -p fq-proto --example wire_dump` 生成并自校验。

### 最小报文:Ping

```
Envelope::broadcast(me, Kind::Ping(PingBody { nonce: 1 }))
```

```
87 a1 76 01
a2 69 64 d9 24 30 31 61 30 63 32 62 37 2d 32 37 66 34 2d 37 36 64 64 2d 61 31 63 38 2d 32 37 66 38 62 64 39 38 39 30 34 32
a4 66 72 6f 6d d9 20 30 30 31 31 32 32 33 33 34 34 35 35 36 36 37 37 38 38 39 39 61 61 62 62 63 63 64 64 65 65 66 66
a2 74 6f c0
a5 74 73 5f 6d 73 cf 00 00 01 a0 c2 b7 27 f4
a4 6b 69 6e 64 a4 70 69 6e 67
a5 6e 6f 6e 63 65 01
```

逐段解读:

| 字节 | 含义 |
|---|---|
| `87` | fixmap,7 个键值对 |
| `a1 76` `01` | `"v"` → `1` |
| `a2 69 64` `d9 24 …` | `"id"` → str8(36):UUIDv7 字符串 |
| `a4 66 72 6f 6d` `d9 20 …` | `"from"` → str8(32):32 字符 hex 的 NodeId |
| `a2 74 6f` `c0` | `"to"` → `null`(广播) |
| `a5 74 73 5f 6d 73` `cf …` | `"ts_ms"` → uint64 毫秒时间戳 |
| `a4 6b 69 6e 64` `a4 70 69 6e 67` | `"kind"` → `"ping"` |
| `a5 6e 6f 6e 63 65` `01` | `"nonce"` → `1` |

合计 120 字节。

### 文件分块的数据字段

`FileChunk.data` 为 32 字节时:

```
… a4 64 61 74 61 c4 20 ab ab ab ab …（32 个 0xab）
```

`c4 20` = bin8,长度 32。**若这里出现 32 个独立的整数元素,说明对端实现有误。**

---

## 11. 已知权衡与后续优化

| 项 | 现状 | 后续方向 |
|---|---|---|
| 信封开销 | 约 120~190 字节(UUID 36 字符 + NodeId 各 32 字符 + 字段名) | 对高频小报文可引入会话内短 ID;文件大数据量走专用裸流通道 |
| 大分块解码 | `flatten` 会让 serde 先缓冲整个 map | 文件分块改走裸流后此开销自然消失 |
| 群聊 | 仅协议占位(`group_id` + `GROUP_CHAT` 能力位) | 网格群发或主机中继 |
| 多播发现 | 未实现 | IPv6 组播 `IPV6` 能力位已预留 |
| 中继/跨网段 | 未实现 | 需要中继节点设计(涉及信任模型) |
| 同连接队头阻塞 | 大文件传输期间文本消息排队等待(<1s @LAN) | 传输走专用裸流通道 |

---

## 12. 传输层分段(fq-net 实现细节,2026-09-21 勘误)

Noise 协议规定单条传输消息总长 ≤ 65535 字节,且实现(snow)要求
`明文 + 16 字节 AEAD 标签 ≤ 65535` —— **明文实际可用上限是 65519 字节**
(源码 snow/transportstate.rs:60,漏算这 16 字节会得到极隐蔽的故障)。

因此 TCP 上的实际线缆格式为**两层**:

```text
u32 长度前缀 | Noise密文( 1 字节控制头 + ≤65518 字节明文段 )
```

* 控制头 `0x01` = 还有后续分段;`0x00` = 本报文结束
* 大于单段上限的应用报文(如 256 KiB 文件分块)自动拆为多段,接收端重组
* 重组缓冲以 1 MiB 封顶(防恶意对端用无限分段撑爆内存)
* 该机制对协议层完全透明:任何 ≤ 1 MiB 的 Envelope 都可直接发送

## 13. 实现与测试索引

| 关注点 | 文件 |
|---|---|
| 报文类型 | `crates/fq-proto/src/message.rs` |
| 编解码(强制 map) | `crates/fq-proto/src/codec.rs` |
| 分帧 | `crates/fq-proto/src/frame.rs` |
| 解码安全预校验 | `crates/fq-proto/src/msgpack_guard.rs` |
| 身份 / 消息 ID | `crates/fq-proto/src/ids.rs` |
| 能力位 | `crates/fq-proto/src/capability.rs` |
| 往返测试(全类型) | `crates/fq-proto/tests/protocol.rs` |
| 前向兼容测试 | `crates/fq-proto/tests/forward_compat.rs` |
| 恶意输入测试 | `crates/fq-proto/tests/hostile_input.rs` |
| 线缆转储工具 | `crates/fq-proto/examples/wire_dump.rs` |
