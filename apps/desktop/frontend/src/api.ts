/** 后端 commands 的类型安全封装。 */
import type { ChatMessage, FqEvent, Peer, SelfInfo, SendResult } from "./types";

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

export const getSelfInfo = () => invoke<SelfInfo>("get_self_info");

export const listPeers = () => invoke<Peer[]>("list_peers");

export const sendText = (
  nodeId: string,
  body: string,
  mentions?: string[],
  replyTo?: string | null,
) =>
  invoke<SendResult>("send_text", {
    nodeId,
    body,
    mentions: mentions ?? null,
    replyTo: replyTo ?? null,
  });

/** 发送"正在输入"状态(提示类,单聊用)。 */
export const sendTyping = (nodeId: string, started: boolean) =>
  invoke<void>("send_typing", { nodeId, started });

export const sendFileTo = (target: string, path: string) =>
  invoke<string[]>("send_file_to", { target, path });
/** 兼容别名:目标可为单聊 NodeId 或 `group:...` 群 ID。 */
export const sendFile = (target: string, path: string) => sendFileTo(target, path);

export const history = (nodeId: string, limit = 100) =>
  invoke<ChatMessage[]>("history", { nodeId, limit });

export const markRead = (nodeId: string) => invoke<void>("mark_read", { nodeId });

/** 订阅后端统一事件流,返回取消函数。 */
export async function onFqEvent(cb: (event: FqEvent) => void): Promise<() => void> {
  const { getCurrentWebview } = await import("@tauri-apps/api/webview");
  const unlisten = await getCurrentWebview().listen<FqEvent>("fq://event", (e) => cb(e.payload));
  return unlisten;
}

/** 文件拖拽事件(Tauri webview 内建)。 */
export async function onFileDrop(cb: (paths: string[]) => void): Promise<() => void> {
  const { getCurrentWebview } = await import("@tauri-apps/api/webview");
  const unlisten = await getCurrentWebview().listen<{ paths: string[] }>(
    "tauri://drag-drop",
    (e) => cb(e.payload.paths),
  );
  return unlisten;
}

/** 系统文件选择对话框。 */
export async function pickFile(): Promise<string | null> {
  const dialog = await import("@tauri-apps/plugin-dialog");
  const picked = await dialog.open({ multiple: false, directory: false });
  return typeof picked === "string" ? picked : null;
}

/** 系统目录选择对话框(选接收文件保存位置)。 */
export async function pickDir(): Promise<string | null> {
  const dialog = await import("@tauri-apps/plugin-dialog");
  const picked = await dialog.open({ multiple: false, directory: true });
  return typeof picked === "string" ? picked : null;
}

export const acceptFileOffer = (token: string, dir?: string) =>
  invoke<boolean>("accept_file_offer", { token, dir: dir ?? null });

export const rejectFileOffer = (token: string) =>
  invoke<boolean>("reject_file_offer", { token });

export const setDownloadDir = (path: string) =>
  invoke<void>("set_download_dir", { path });

/** 刷新联系人:重新广播 + 把局域网内已知成员写回列表(含之前删掉的人);返回写回数量。 */
export const refreshPeers = () => invoke<number>("refresh_peers");

export const removePeer = (nodeId: string) =>
  invoke<boolean>("remove_peer", { nodeId });

export const takeScreenshot = (target: string) =>
  invoke<void>("take_screenshot", { target });

export const openFile = (path: string) =>
  invoke<void>("open_file", { path });

export const pickImage = () =>
  invoke<string | null>("pick_image");

export const readImageBase64 = (path: string) =>
  invoke<string>("read_image_base64", { path });

export const pasteImage = () =>
  invoke<string | null>("paste_image");

export const setProfile = (name: string | null, group: string | null) =>
  invoke<void>("set_profile", { name, group });

export interface SearchHit {
  peer: string;
  id: string;
  outgoing: boolean;
  kind: string;
  body: string | null;
  ts_ms: number;
}

export const searchMessages = (query: string, limit = 80) =>
  invoke<SearchHit[]>("search_messages", { query, limit });

export interface GroupInfo {
  id: string;
  name: string;
  members: string[];
  member_count: number;
}

export const listGroups = () => invoke<GroupInfo[]>("list_groups");

export const createGroup = (name: string, members: string[]) =>
  invoke<string>("create_group", { name, members });

export const deleteGroup = (groupId: string) =>
  invoke<boolean>("delete_group", { groupId });

export const sendGroupText = (
  groupId: string,
  body: string,
  mentions?: string[],
  replyTo?: string | null,
) =>
  invoke<[number, number]>("send_group_text", {
    groupId,
    body,
    mentions: mentions ?? null,
    replyTo: replyTo ?? null,
  });

export interface ConversationInfo {
  peer: string;
  last_msg_ms: number;
  preview: string;
  unread: number;
  /** 是否置顶(置顶排最前) */
  pinned: boolean;
  /** 是否免打扰(未读只显示小点,不弹提示) */
  muted: boolean;
}

export const listConversations = () =>
  invoke<ConversationInfo[]>("list_conversations");

export const markConversationRead = (peer: string) =>
  invoke<void>("mark_conversation_read", { peer });

/** 扫一遍本网段(广播不可达时的发现兜底),返回探测地址数。 */
export const scanSubnet = () => invoke<number>("scan_subnet");

/** 设置会话置顶/免打扰(只传要改的那一项)。 */
export const setConversationFlags = (
  peer: string,
  flags: { pinned?: boolean; muted?: boolean },
) => invoke<void>("set_conversation_flags", { peer, ...flags });

/** 一键全部已读,返回受影响的会话数。 */
export const markAllConversationsRead = () =>
  invoke<number>("mark_all_conversations_read");

export const historyBefore = (peer: string, beforeMs: number, limit = 50) =>
  invoke<import("./types").ChatMessage[]>("history_before", { peer, beforeMs, limit });

export interface PeerVersion {
  node_id: string;
  name: string;
  version: string;
  relation: "newer" | "same" | "older";
}

export interface VersionReport {
  local_version: string;
  protocol_version: number;
  latest_version: string | null;
  latest_from: string | null;
  update_available: boolean;
  peers: PeerVersion[];
}

export const checkUpdate = () => invoke<VersionReport>("check_update");

export interface TransferHistoryItem {
  token: string;
  peer: string;
  peer_name: string;
  direction: "send" | "recv";
  path: string;
  size: number;
  status: "active" | "done" | "failed" | "cancelled";
  detail: string | null;
  started_ms: number;
  finished_ms: number | null;
}

export const listTransferHistory = (limit = 200) =>
  invoke<TransferHistoryItem[]>("list_transfer_history", { limit });

export const clearTransferHistory = () =>
  invoke<number>("clear_transfer_history");

export const deleteTransferHistory = (token: string) =>
  invoke<boolean>("delete_transfer_history", { token });

export const cancelTransfer = (token: string) =>
  invoke<boolean>("cancel_transfer", { token });

/** 偏好设置(通用设置页需要)。 */
export interface Preferences {
  auto_update: boolean;
  local_version: string;
  /** 当前在线状态:online / away / busy / dnd */
  status: string;
  /** 发送方向限速(字节/秒;0 = 不限速) */
  send_limit_bytes: number;
  /** 数据目录(可一键打开) */
  data_dir: string;
  /** 日志目录(排查问题时用) */
  log_dir: string;
  /** 当前 TCP 监听端口 */
  listen_port: number;
  /** 界面主题:system / light / dark */
  theme: string;
}

export const getPreferences = () => invoke<Preferences>("get_preferences");

export const setAutoUpdate = (enabled: boolean) =>
  invoke<void>("set_auto_update", { enabled });

/** 设置界面主题(system / light / dark)。 */
export const setTheme = (theme: string) => invoke<void>("set_theme", { theme });

/** 设置在线状态(online / away / busy / dnd)并立即广播。 */
export const setStatus = (status: string) => invoke<void>("set_status", { status });

/** 设置发送方向限速(字节/秒;0 = 不限速)。 */
export const setTransferLimit = (bytesPerSec: number) =>
  invoke<void>("set_transfer_limit", { bytesPerSec });

/** 手动探测:向指定 IP(或 ip:端口)定向发一次通告。 */
export const probePeer = (target: string) => invoke<string>("probe_peer", { target });

/** 添加 Windows 防火墙入站放行规则(会弹 UAC)。 */
export const addFirewallRules = () => invoke<string>("add_firewall_rules");

/** 向对端索取更新包(对端版本更高时会自动回发,接收后触发 update_ready)。 */
export const requestUpdate = (nodeId: string) =>
  invoke<void>("request_update", { nodeId });

/** 安装已下载并校验通过的更新包(应用会退出并由更新脚本重启)。 */
export const installUpdate = (path: string) =>
  invoke<void>("install_update", { path });

/** 选择并设置头像(后端压缩成 256×256 PNG 并广播);返回新头像 data URL。 */
export const chooseAvatar = () => invoke<string | null>("choose_avatar");

/** 移除头像。 */
export const clearAvatar = () => invoke<void>("clear_avatar");

/** 本机头像 data URL。 */
export const getSelfAvatar = () => invoke<string | null>("get_self_avatar");

/** 对端头像 data URL(未缓存为 null)。 */
export const getPeerAvatar = (nodeId: string) =>
  invoke<string | null>("get_peer_avatar", { nodeId });

/** 从"最近会话"移除一条(聊天记录与联系人保留)。 */
export const deleteConversation = (peer: string) =>
  invoke<boolean>("delete_conversation", { peer });

/** 发送窗口抖动(飞秋经典功能;target 可为 NodeId 或 `group:...`)。 */
export const sendShake = (target: string) => invoke<void>("send_shake", { target });

/** 本地删除一条历史消息(只删本机)。 */
export const deleteMessage = (id: string) => invoke<boolean>("delete_message", { id });
