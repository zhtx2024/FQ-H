/** 后端(IPC)与前端共享的数据类型。 */

export interface SelfInfo {
  node_id: string;
  name: string;
  fingerprint: string;
  /** 本机全部可用 IPv4(多网卡会有多个) */
  local_ips: string[];
  /** 当前接收文件保存目录 */
  download_dir: string;
  /** 本机头像 data URL(未设置为 null) */
  avatar: string | null;
  /** 当前分组名 */
  group: string | null;
}

export interface Peer {
  node_id: string;
  name: string;
  group: string | null;
  online: boolean;
  last_seen_ms: number;
  /** 对端可达 IP(展示用) */
  ips: string[];
  /** 对端软件版本 */
  app_version: string | null;
  /** 对端头像哈希(有头像时非空;离线也会带缓存哈希) */
  avatar_sha256: string | null;
}

export interface ChatMessage {
  id: string;
  outgoing: boolean;
  kind: string;
  body: string | null;
  ts_ms: number;
  delivered: boolean;
  read: boolean;
  /** 发送状态:pending / sent / delivered / read */
  status?: string;
  /** 发送方 NodeId(群聊里显示是谁发的) */
  from_node?: string;
  /** 被 @ 的节点(群聊 @提醒;仅内存态,不回读) */
  mentions?: string[];
  /** 被引用的消息 ID(引用回复;渲染时本地查原文) */
  reply_to?: string | null;
}

export interface SendResult {
  id: string;
  queued: boolean;
}

/** 后端统一事件(fq://event 的载荷)。 */
export type FqEvent =
  | { type: "peer_up"; node_id: string; name: string; group: string | null }
  | { type: "peer_down"; node_id: string }
  | { type: "message"; from: string; from_name: string; id: string; body: string; ts_ms: number; mentions: string[]; reply_to: string | null }
  | { type: "delivered"; id: string }
  | { type: "read"; id: string }
  | { type: "queued_flushed"; to: string; count: number }
  | { type: "file_offer"; from_name: string; token: string; entries: number; total_bytes: number }
  | {
      type: "file_progress";
      direction: "send" | "recv";
      token: string;
      path: string;
      transferred: number;
      total: number;
    }
  | { type: "file_done"; direction: "send" | "recv"; token: string; path: string; verified: boolean }
  | { type: "file_completed"; direction: "send" | "recv"; token: string }
  | { type: "file_failed"; direction: "send" | "recv"; token: string; path: string | null; reason: string }
  | { type: "trust_warning"; node_id: string; pinned: string; presented: string }
  // 自动更新:更新包正在接收 / 已就绪(可直接安装)
  | {
      type: "update_incoming";
      from_name: string;
      version: string;
      token: string;
      file_name: string;
      total_bytes: number;
    }
  | { type: "update_ready"; from: string; from_name: string; version: string; path: string }
  // 头像:某对端头像已更新(前端应重新拉取)
  | { type: "peer_avatar"; node_id: string }
  // 头像:某对端已移除头像(前端清掉展示)
  | { type: "peer_avatar_removed"; node_id: string }
  // 窗口抖动:对端抖了我一下(前端晃动窗口 + 留一条提示)
  | { type: "shaken"; from: string; from_name: string }
  // 对端正在输入(提示类,不落库)
  | { type: "typing"; from: string; from_name: string; started: boolean }
  // 发送中断,后端正在自动重试(第 attempt 次,共 max 次)
  | { type: "transfer_retrying"; token: string; attempt: number; max: number; name: string }
  // 自动重试次数用尽,已放弃本次发送
  | { type: "transfer_retry_gave_up"; token: string; name: string };

export interface SpeedSample {
  t: number;
  bytes: number;
}

export interface TransferItem {
  token: string;
  direction: "send" | "recv";
  path: string;
  transferred: number;
  total: number;
  done: boolean;
  failed: string | null;
  /** 已完成/失败的时间戳(用于延迟清理) */
  finished_at: number | null;
  /** 速度采样窗口(滑窗计算 MB/s) */
  samples: SpeedSample[];
}

/** 头像兜底首字(参考 whisper:取首个字符)。 */
export function initials(name: string): string {
  return name.trim().slice(0, 1).toUpperCase() || "?";
}

/** 按稳定种子(如 NodeId)取一个头像底色,便于在列表里区分不同联系人。 */
const AVATAR_COLORS = [
  "#00A4FF",
  "#36CFC9",
  "#597EF7",
  "#9254DE",
  "#F759AB",
  "#FF7A45",
  "#73D13D",
];

export function avatarColor(seed: string): string {
  let hash = 0;
  for (let i = 0; i < seed.length; i += 1) {
    hash = (hash * 31 + seed.charCodeAt(i)) >>> 0;
  }
  return AVATAR_COLORS[hash % AVATAR_COLORS.length];
}

/** 速度(MB/s):取窗口内首尾差分。 */
export function transferSpeed(item: TransferItem): number {
  const now = Date.now();
  const window = item.samples.filter((s) => now - s.t <= 1500);
  if (window.length < 2) return 0;
  const first = window[0];
  const last = window[window.length - 1];
  const dt = (last.t - first.t) / 1000;
  if (dt <= 0) return 0;
  return (last.bytes - first.bytes) / 1024 / 1024 / dt;
}

export function fmtBytes(n: number): string {
  if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${n} B`;
}

/** 待确认的接收要约。 */
export interface PendingOffer {
  token: string;
  from_name: string;
  entries: number;
  total_bytes: number;
}

export interface Toast {
  id: number;
  kind: "error" | "info";
  text: string;
}
