import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import * as api from "./api";
import type {
  ChatMessage,
  FqEvent,
  Peer,
  PendingOffer,
  SelfInfo,
  Toast,
  TransferItem,
} from "./types";
import { avatarColor, fmtBytes, initials, transferSpeed } from "./types";
import EMOJI_GROUPS from "./emoji";
import { applyTheme, watchSystemTheme, type ThemeMode } from "./theme";

function formatTime(tsMs: number): string {
  const d = new Date(tsMs);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 微信风格时间分割线:超过 5 分钟的间隔才显示。 */
function formatDivider(tsMs: number): string {
  const d = new Date(tsMs);
  const now = new Date();
  const pad = (n: number) => String(n).padStart(2, "0");
  const isToday = d.toDateString() === now.toDateString();
  const isYesterday = new Date(now.getTime() - 86400000).toDateString() === d.toDateString();
  if (isToday) return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  if (isYesterday) return `昨天 ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return `${d.getMonth() + 1}月${d.getDate()}日 ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function shouldShowDivider(prev: ChatMessage | null, current: ChatMessage): boolean {
  if (!prev) return true;
  return current.ts_ms - prev.ts_ms > 5 * 60 * 1000;
}

function shortId(id: string): string {
  return id.slice(0, 8);
}

/** 联系人面板里的分组结构。 */
interface PeerGroup {
  label: string;
  peers: Peer[];
}

function buildGroups(peers: Peer[]): { online: PeerGroup[]; offline: PeerGroup[] } {
  const onlineMap = new Map<string, Peer[]>();
  const offlineMap = new Map<string, Peer[]>();
  for (const p of peers) {
    const label = p.group?.trim() || "未分组";
    const target = p.online ? onlineMap : offlineMap;
    if (!target.has(label)) target.set(label, []);
    target.get(label)!.push(p);
  }
  const toGroups = (m: Map<string, Peer[]>) =>
    [...m.entries()]
      .sort((a, b) => a[0].localeCompare(b[0], "zh-CN"))
      .map(([label, list]) => ({
        label,
        peers: list.sort((a, b) => a.name.localeCompare(b.name, "zh-CN")),
      }));
  return { online: toGroups(onlineMap), offline: toGroups(offlineMap) };
}

/** 图片缩略图组件:后端读文件转 base64,懒加载 + 缓存。 */
const imageCache = new Map<string, string>();

function ImageThumb({ path, name }: { path: string; name: string }) {
  const [src, setSrc] = useState<string | null>(imageCache.get(path) ?? null);

  useEffect(() => {
    if (src) return;
    let disposed = false;
    api
      .readImageBase64(path)
      .then((base64) => {
        if (disposed) return;
        const dataUrl = `data:image/png;base64,${base64}`;
        imageCache.set(path, dataUrl);
        setSrc(dataUrl);
      })
      .catch(() => {
        if (!disposed) setSrc(null);
      });
    return () => {
      disposed = true;
    };
  }, [path, src]);

  if (!src) {
    return (
      <div className="img-loading" title={name}>
        🖼️ {name}
      </div>
    );
  }
  return <img src={src} alt={name} className="chat-image" loading="lazy" />;
}

/** 灯箱里的大图:读原图 base64,支持滚轮缩放与拖动查看。 */
function LightboxImage({ path }: { path: string }) {
  const [src, setSrc] = useState<string | null>(imageCache.get(path) ?? null);
  const [zoom, setZoom] = useState(1);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  const drag = useRef<{ x: number; y: number; ox: number; oy: number } | null>(null);

  useEffect(() => {
    if (src) return;
    let disposed = false;
    api
      .readImageBase64(path)
      .then((base64) => {
        if (disposed) return;
        const dataUrl = `data:image/png;base64,${base64}`;
        imageCache.set(path, dataUrl);
        setSrc(dataUrl);
      })
      .catch(() => {
        if (!disposed) setSrc(null);
      });
    return () => {
      disposed = true;
    };
  }, [path, src]);

  if (!src) return <div className="lightbox-loading">图片读取中…</div>;
  return (
    <img
      src={src}
      alt=""
      draggable={false}
      className="lightbox-image"
      style={{ transform: `translate(${offset.x}px, ${offset.y}px) scale(${zoom})` }}
      onWheel={(e) => {
        e.preventDefault();
        setZoom((z) => Math.min(6, Math.max(1, z - e.deltaY * 0.0015)));
      }}
      onMouseDown={(e) => {
        e.preventDefault();
        drag.current = { x: e.clientX, y: e.clientY, ox: offset.x, oy: offset.y };
      }}
      onMouseMove={(e) => {
        const d = drag.current;
        if (!d) return;
        setOffset({ x: d.ox + (e.clientX - d.x), y: d.oy + (e.clientY - d.y) });
      }}
      onMouseUp={() => (drag.current = null)}
      onMouseLeave={() => (drag.current = null)}
      onDoubleClick={() => {
        setZoom((z) => (z > 1 ? 1 : 2));
        setOffset({ x: 0, y: 0 });
      }}
      onClick={(e) => e.stopPropagation()}
    />
  );
}

/** 版本号比较(数字分段;段数不足补 0)。 */
function versionCompare(a: string, b: string): number {
  const parse = (s: string) => s.split(/[.\-+]/).map((p) => parseInt(p, 10) || 0);
  const va = parse(a);
  const vb = parse(b);
  for (let i = 0; i < Math.max(va.length, vb.length); i++) {
    const x = va[i] ?? 0;
    const y = vb[i] ?? 0;
    if (x !== y) return x > y ? 1 : -1;
  }
  return 0;
}

/** 本机版本(由构建时注入,与 Rust 端 env!("CARGO_PKG_VERSION") 一致)。 */
const currentVersion = __APP_VERSION__;

/** 头像气泡:有图显示图,否则显示名字首字(离线加灰)。 */
function AvatarBubble({
  url,
  fallback,
  seed,
  base = "avatar",
  className = "",
  size,
  round = false,
  onClick,
  title,
}: {
  url?: string;
  fallback: string;
  /** 兜底配色的种子(一般传 NodeId,保证同一人颜色稳定)。 */
  seed?: string;
  /** 基础类名(列表用 `avatar`,消息气泡用 `msg-avatar`)。 */
  base?: string;
  className?: string;
  size?: number;
  round?: boolean;
  onClick?: () => void;
  title?: string;
}) {
  const style: CSSProperties = size
    ? { width: size, height: size, fontSize: Math.max(10, Math.round(size * 0.42)) }
    : {};
  if (!url) {
    // 没有头像图片时,用"按种子派生的颜色 + 首字"兜底(参考 whisper 的 UserAvatar)
    style.background = avatarColor(seed || fallback);
  }
  return (
    <div
      className={`avatar ${base} ${round ? "round" : ""} ${className}`.trim()}
      style={style}
      onClick={onClick}
      title={title}
      role={onClick ? "button" : undefined}
    >
      {url ? <img src={url} alt="" draggable={false} /> : initials(fallback)}
    </div>
  );
}

/** 搜索命中的关键词高亮。 */
function Highlighted({ text, query }: { text: string; query: string }) {
  const q = query.trim();
  if (!q) return <>{text}</>;
  const lower = text.toLowerCase();
  const lowerQ = q.toLowerCase();
  const parts: React.ReactNode[] = [];
  let cursor = 0;
  let index = lower.indexOf(lowerQ);
  let key = 0;
  while (index >= 0) {
    if (index > cursor) parts.push(<span key={key++}>{text.slice(cursor, index)}</span>);
    parts.push(
      <mark key={key++} className="search-mark">
        {text.slice(index, index + q.length)}
      </mark>,
    );
    cursor = index + q.length;
    index = lower.indexOf(lowerQ, cursor);
  }
  if (cursor < text.length) parts.push(<span key={key++}>{text.slice(cursor)}</span>);
  return <>{parts}</>;
}

export default function App() {
  const [self, setSelf] = useState<SelfInfo | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [messages, setMessages] = useState<Record<string, ChatMessage[]>>({});
  const [unread, setUnread] = useState<Record<string, number>>({});
  const [draft, setDraft] = useState("");
  const [transfers, setTransfers] = useState<Record<string, TransferItem>>({});
  const [trustWarning, setTrustWarning] = useState<string | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [offers, setOffers] = useState<PendingOffer[]>([]);
  const [offerDirChoice, setOfferDirChoice] = useState<Record<string, string>>({});
  const [searchText, setSearchText] = useState("");
  const [collapsedGroups, setCollapsedGroups] = useState<Record<string, boolean>>({});
  const [showOffline, setShowOffline] = useState(false);
  const [showEmoji, setShowEmoji] = useState(false);
  const [emojiTab, setEmojiTab] = useState(0);
  const [showSettings, setShowSettings] = useState(false);
  const [settingsTab, setSettingsTab] = useState<
    "general" | "profile" | "files" | "identity" | "about"
  >("general");
  const [versionReport, setVersionReport] = useState<api.VersionReport | null>(null);
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [transferPanelOpen, setTransferPanelOpen] = useState(true);
  const [showTransferHistory, setShowTransferHistory] = useState(false);
  const [transferHistory, setTransferHistory] = useState<api.TransferHistoryItem[]>([]);
  /** 自动更新:已下载并校验通过、等待重启安装的更新包。 */
  const [pendingInstall, setPendingInstall] = useState<{
    version: string;
    from_name: string;
    path: string;
  } | null>(null);
  /** 是否显示"更新就绪"弹窗(关掉不影响已下载的包)。 */
  const [showUpdatePrompt, setShowUpdatePrompt] = useState(false);
  /** 自动模式下的重启倒计时(秒);null = 未倒计时。 */
  const [installCountdown, setInstallCountdown] = useState<number | null>(null);
  /** 自动更新开关(设置页可改,持久化在 profile.json)。 */
  const [autoUpdate, setAutoUpdate] = useState(false);
  /** 对端头像缓存(node_id → data URL)。 */
  const [avatars, setAvatars] = useState<Record<string, string>>({});
  /** 资料卡:点联系人/消息头像弹出(参考 whisper 的 ProfileCard)。 */
  const [profileCard, setProfileCard] = useState<{ self: boolean; nodeId?: string } | null>(null);
  /** 图片灯箱(点图片放大查看)。 */
  const [lightbox, setLightbox] = useState<{ path: string; name: string; title: string } | null>(
    null,
  );
  /** 消息右键菜单。 */
  const [msgMenu, setMsgMenu] = useState<{ x: number; y: number; msg: ChatMessage } | null>(null);
  /** 会话右键菜单(置顶/免打扰/标为已读)。 */
  const [convMenu, setConvMenu] = useState<{
    x: number;
    y: number;
    peer: string;
    label: string;
    pinned: boolean;
    muted: boolean;
  } | null>(null);
  /** 免打扰会话集合(ref:事件回调在挂载时捕获,读 state 会过期)。 */
  const mutedPeersRef = useRef<Set<string>>(new Set());
  /** 正在扫网段(按钮防抖)。 */
  const [scanning, setScanning] = useState(false);
  /** 当前主题模式(供"跟随系统"监听使用;展示用 prefs.theme)。 */
  const themeModeRef = useRef<ThemeMode>("system");
  /** 待转发的消息(弹出目标选择)。 */
  const [forwarding, setForwarding] = useState<ChatMessage | null>(null);
  /** 正在引用的消息(输入区上方显示引用条)。 */
  const [quoting, setQuoting] = useState<ChatMessage | null>(null);
  /** 收到抖动时给窗口加抖动动画 + 抖动节流(同会话 1.5s 一次,防"抖动炸弹")。 */
  const [shaking, setShaking] = useState(false);
  const shakeCooldownRef = useRef<Record<string, number>>({});
  /** 通用设置(在线状态 / 限速 / 目录等)。 */
  const [prefs, setPrefs] = useState<api.Preferences | null>(null);
  const [probeInput, setProbeInput] = useState("");
  const [probeHint, setProbeHint] = useState("");
  /** 对端"正在输入"时间戳(node_id → ms;4 秒无更新自动消失)。 */
  const [typingPeers, setTypingPeers] = useState<Record<string, number>>({});
  /** 群聊 @ 选择器与已选中的被 @ 成员。 */
  const [showMentions, setShowMentions] = useState(false);
  const [mentionIds, setMentionIds] = useState<string[]>([]);
  /** 我上报"正在输入"的节流状态与停止定时器。 */
  const typingSentRef = useRef<{ peer: string; sentAt: number } | null>(null);
  const typingStopTimerRef = useRef<number | null>(null);

  /** 打开资料卡:`nodeId` 为空表示自己。 */
  const openProfileCard = useCallback((nodeId?: string) => {
    setProfileCard(nodeId ? { self: false, nodeId } : { self: true });
  }, []);
  const avatarLoadingRef = useRef<Set<string>>(new Set());
  /** 折叠分区(最近会话 / 群聊),持久化到 localStorage。 */
  const [collapsedSections, setCollapsedSections] = useState<Record<string, boolean>>(() => {
    try {
      return JSON.parse(localStorage.getItem("fq.collapsedSections") ?? "{}") as Record<
        string,
        boolean
      >;
    } catch {
      return {};
    }
  });
  /** 已发起过更新请求的 `版本@节点`,避免重复索取。 */
  const requestedUpdateRef = useRef<string | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [settingsName, setSettingsName] = useState("");
  const [settingsGroup, setSettingsGroup] = useState("");
  const [showSearch, setShowSearch] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchHits, setSearchHits] = useState<api.SearchHit[]>([]);
  const [groups, setGroups] = useState<api.GroupInfo[]>([]);
  const [conversations, setConversations] = useState<api.ConversationInfo[]>([]);
  const [showCreateGroup, setShowCreateGroup] = useState(false);
  const [newGroupName, setNewGroupName] = useState("");
  const [newGroupMembers, setNewGroupMembers] = useState<Record<string, boolean>>({});
  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    peer: Peer;
  } | null>(null);
  const listBottomRef = useRef<HTMLDivElement>(null);
  const toastSeq = useRef(0);

  const pushToast = useCallback((kind: Toast["kind"], text: string) => {
    const id = ++toastSeq.current;
    setToasts((list) => [...list.slice(-4), { id, kind, text }]);
    if (kind === "info") {
      window.setTimeout(() => {
        setToasts((list) => list.filter((t) => t.id !== id));
      }, 4000);
    }
  }, []);

  const dismissToast = useCallback((id: number) => {
    setToasts((list) => list.filter((t) => t.id !== id));
  }, []);

  const finishTransfer = useCallback((token: string, failed: string | null) => {
    setTransfers((all) => {
      const cur = all[token];
      if (!cur) return all;
      return { ...all, [token]: { ...cur, failed, done: true, finished_at: Date.now() } };
    });
    if (!failed) {
      window.setTimeout(() => {
        setTransfers((all) => {
          const next = { ...all };
          delete next[token];
          return next;
        });
      }, 3000);
    }
  }, []);

  const removeTransfer = useCallback((token: string) => {
    setTransfers((all) => {
      const next = { ...all };
      delete next[token];
      return next;
    });
  }, []);

  const refreshPeers = useCallback(() => {
    api
      .listPeers()
      .then(setPeers)
      .catch((e) => console.error("拉取成员失败", e));
  }, []);

  const refreshGroups = useCallback(() => {
    api
      .listGroups()
      .then(setGroups)
      .catch((e) => console.error("拉取群组失败", e));
  }, []);

  const refreshConversations = useCallback(() => {
    api
      .listConversations()
      .then(setConversations)
      .catch((e) => console.error("拉取会话失败", e));
  }, []);

  // 免打扰名单给事件回调用(回调在挂载时捕获,不能读最新 state)
  useEffect(() => {
    mutedPeersRef.current = new Set(
      conversations.filter((c) => c.muted).map((c) => c.peer),
    );
  }, [conversations]);

  const refreshTransferHistory = useCallback(() => {
    api
      .listTransferHistory()
      .then(setTransferHistory)
      .catch((e) => console.error("拉取传输历史失败", e));
  }, []);

  /** 取消进行中的传输(后端中断分块流并通知对端)。 */
  const doCancelTransfer = useCallback(
    async (token: string) => {
      try {
        const ok = await api.cancelTransfer(token);
        if (!ok) {
          // 会话已经结束:只需从面板移除
          removeTransfer(token);
          pushToast("info", "该传输已经结束");
          return;
        }
        pushToast("info", "已取消传输");
        window.setTimeout(() => removeTransfer(token), 800);
      } catch (e) {
        pushToast("error", `取消失败:${String(e)}`);
      }
    },
    [removeTransfer, pushToast],
  );

  const doOpenTransferHistory = useCallback(() => {
    setShowTransferHistory(true);
    refreshTransferHistory();
  }, [refreshTransferHistory]);

  /** 向某个新版本对端索取更新包(对端会自动回发,接收后触发 update_ready)。 */
  const doRequestUpdate = useCallback(
    async (nodeId: string, version: string) => {
      const key = `${version}@${nodeId}`;
      if (requestedUpdateRef.current === key) return;
      requestedUpdateRef.current = key;
      setUpdateBusy(true);
      try {
        await api.requestUpdate(nodeId);
        pushToast("info", `正在从对端获取 v${version} 更新包…`);
      } catch (e) {
        requestedUpdateRef.current = null;
        pushToast("error", `获取更新失败:${String(e)}`);
      } finally {
        setUpdateBusy(false);
      }
    },
    [pushToast],
  );

  /** 安装已下载并校验通过的更新包(应用会退出,由更新脚本覆盖并重启)。 */
  const doInstallUpdate = useCallback(async () => {
    if (!pendingInstall) return;
    try {
      setInstallCountdown(null);
      await api.installUpdate(pendingInstall.path);
    } catch (e) {
      pushToast("error", `安装失败:${String(e)}`);
    }
  }, [pendingInstall, pushToast]);

  /** 读取偏好设置(自动更新开关 + 通用设置)。 */
  const refreshPreferences = useCallback(() => {
    api
      .getPreferences()
      .then((p) => {
        setPrefs(p);
        setAutoUpdate(p.auto_update);
        // 主题以后端 profile.json 为准(启动时先用本机缓存挡闪屏)
        const mode: ThemeMode =
          p.theme === "light" || p.theme === "dark" ? p.theme : "system";
        themeModeRef.current = mode;
        applyTheme(mode);
      })
      .catch((e) => console.error("读取偏好设置失败", e));
  }, []);

  // 跟随系统:系统深浅色变化时重新计算生效主题
  useEffect(
    () => watchSystemTheme(() => applyTheme(themeModeRef.current)),
    [],
  );

  /** 设置界面主题(system / light / dark)。 */
  const doSetTheme = useCallback(
    async (mode: ThemeMode) => {
      applyTheme(mode); // 立即生效,不等后端往返
      themeModeRef.current = mode;
      try {
        await api.setTheme(mode);
        await refreshPreferences();
      } catch (e) {
        pushToast("error", `主题设置失败:${String(e)}`);
      }
    },
    [pushToast, refreshPreferences],
  );

  /** 切换在线状态(立即广播)。 */
  const doSetStatus = useCallback(
    async (status: string) => {
      try {
        await api.setStatus(status);
        setPrefs((p) => (p ? { ...p, status } : p));
        const label =
          status === "online"
            ? "在线"
            : status === "busy"
              ? "忙碌"
              : status === "dnd"
                ? "勿扰"
                : "离开";
        pushToast("info", `在线状态已切换为「${label}」`);
      } catch (e) {
        pushToast("error", `设置状态失败:${String(e)}`);
      }
    },
    [pushToast],
  );

  /** 设置发送限速。 */
  const doSetLimit = useCallback(
    async (bytes: number) => {
      try {
        await api.setTransferLimit(bytes);
        setPrefs((p) => (p ? { ...p, send_limit_bytes: bytes } : p));
        pushToast(
          "info",
          bytes > 0 ? `发送限速已设为 ${Math.round(bytes / 1024 / 1024)} MB/s` : "已取消发送限速",
        );
      } catch (e) {
        pushToast("error", `设置限速失败:${String(e)}`);
      }
    },
    [pushToast],
  );

  /** 手动探测对方 IP(定向握手,解决"搜不到同伴")。 */
  const doProbe = useCallback(async () => {
    try {
      const message = await api.probePeer(probeInput);
      setProbeHint(message);
      pushToast("info", message);
    } catch (e) {
      pushToast("error", `探测失败:${String(e)}`);
    }
  }, [probeInput, pushToast]);

  /** 一键放行 Windows 防火墙(会弹 UAC)。 */
  const doFirewall = useCallback(async () => {
    try {
      pushToast("info", await api.addFirewallRules());
    } catch (e) {
      pushToast("error", `添加放行规则失败:${String(e)}`);
    }
  }, [pushToast]);

  /** 静默版本检查(后台轮询用,不弹提示)。 */
  const silentCheckUpdate = useCallback(async () => {
    try {
      setVersionReport(await api.checkUpdate());
    } catch {
      /* 后台检查失败不打扰用户 */
    }
  }, []);

  /** 拉取某对端头像并缓存(失败静默,展示兜底首字母);`refresh` 时同步清掉旧图。 */
  const loadAvatar = useCallback((nodeId: string, refresh = false) => {
    if (avatarLoadingRef.current.has(nodeId)) return;
    avatarLoadingRef.current.add(nodeId);
    api
      .getPeerAvatar(nodeId)
      .then((url) => {
        setAvatars((prev) => {
          if (url) return { ...prev, [nodeId]: url };
          if (!refresh) return prev;
          // 已移除头像:清掉本地展示
          const next = { ...prev };
          delete next[nodeId];
          return next;
        });
      })
      .catch(() => undefined)
      .finally(() => avatarLoadingRef.current.delete(nodeId));
  }, []);

  /** 切换分区折叠(持久化)。 */
  const toggleSection = useCallback((key: string) => {
    setCollapsedSections((prev) => {
      const next = { ...prev, [key]: !prev[key] };
      try {
        localStorage.setItem("fq.collapsedSections", JSON.stringify(next));
      } catch {
        /* 隐私模式等场景忽略 */
      }
      return next;
    });
  }, []);

  /** 从最近会话中移除(聊天记录保留;有未读时先提示)。 */
  const doDeleteConversation = useCallback(
    async (peer: string, label: string, unread: number) => {
      const tip =
        unread > 0
          ? `「${label}」有 ${unread} 条未读,仍要从最近会话移除?(聊天记录会保留)`
          : `从最近会话移除「${label}」?(聊天记录会保留)`;
      if (!window.confirm(tip)) return;
      try {
        await api.deleteConversation(peer);
        setConversations((list) => list.filter((c) => c.peer !== peer));
        pushToast("info", `已从最近会话移除「${label}」`);
      } catch (e) {
        pushToast("error", `移除失败:${String(e)}`);
      }
    },
    [pushToast],
  );

  /** 设置会话置顶/免打扰(右键菜单)。 */
  const doSetConvFlags = useCallback(
    async (peer: string, label: string, flags: { pinned?: boolean; muted?: boolean }) => {
      try {
        await api.setConversationFlags(peer, flags);
        await refreshConversations();
        if (flags.pinned !== undefined) {
          pushToast("info", flags.pinned ? `已置顶「${label}」` : `已取消置顶「${label}」`);
        }
        if (flags.muted !== undefined) {
          pushToast(
            "info",
            flags.muted ? `已对「${label}」开启免打扰` : `已关闭「${label}」的免打扰`,
          );
        }
      } catch (e) {
        pushToast("error", `设置失败:${String(e)}`);
      }
    },
    [pushToast, refreshConversations],
  );

  /** 扫一遍本网段(广播被拦时的发现兜底)。 */
  const doScanSubnet = useCallback(async () => {
    setScanning(true);
    try {
      const n = await api.scanSubnet();
      pushToast("info", `已向本网段 ${n} 个地址发出探测,稍等片刻看联系人列表`);
    } catch (e) {
      pushToast("error", `扫描失败:${String(e)}`);
    } finally {
      setScanning(false);
    }
  }, [pushToast]);

  /** 一键全部已读。 */
  const doMarkAllRead = useCallback(async () => {    try {
      const n = await api.markAllConversationsRead();
      setUnread({});
      await refreshConversations();
      pushToast("info", n > 0 ? `已把 ${n} 个会话标为已读` : "没有未读会话");
    } catch (e) {
      pushToast("error", `操作失败:${String(e)}`);
    }
  }, [pushToast, refreshConversations]);

  /** 更换头像:选择图片 → 后端压缩成 256×256 → 广播。 */
  const doChooseAvatar = useCallback(async () => {
    try {
      const url = await api.chooseAvatar();
      if (url === null) return; // 用户取消
      setSelf((s) => (s ? { ...s, avatar: url } : s));
      pushToast("info", "头像已更新并广播给局域网成员");
    } catch (e) {
      pushToast("error", `设置头像失败:${String(e)}`);
    }
  }, [pushToast]);

  /** 移除头像。 */
  const doClearAvatar = useCallback(async () => {
    try {
      await api.clearAvatar();
      setSelf((s) => (s ? { ...s, avatar: null } : s));
      pushToast("info", "头像已移除");
    } catch (e) {
      pushToast("error", `移除头像失败:${String(e)}`);
    }
  }, [pushToast]);

  const openPeer = useCallback((nodeId: string) => {
    setSelected(nodeId);
    setQuoting(null);
    setUnread((u) => ({ ...u, [nodeId]: 0 }));
    api
      .history(nodeId)
      .then((rows) => setMessages((m) => ({ ...m, [nodeId]: rows })))
      .catch((e) => console.error("拉取历史失败", e));
    // 会话级已读(持久化,重启后未读不复活)
    api.markConversationRead(nodeId).then(refreshConversations).catch(() => undefined);
    if (!nodeId.startsWith("group:")) {
      api.markRead(nodeId).catch(() => undefined);
    }
  }, [refreshConversations]);

  /** 向上滚动加载更早的历史。 */
  const loadEarlier = useCallback(
    async (nodeId: string) => {
      const list = messages[nodeId] ?? [];
      const oldest = list[0];
      if (!oldest) return;
      try {
        const older = await api.historyBefore(nodeId, oldest.ts_ms, 50);
        if (older.length > 0) {
          setMessages((m) => ({ ...m, [nodeId]: [...older, ...(m[nodeId] ?? [])] }));
        }
      } catch (e) {
        console.error("加载更早历史失败", e);
      }
    },
    [messages],
  );

  const doRemovePeer = useCallback(
    async (peer: Peer) => {
      try {
        await api.removePeer(peer.node_id);
        if (selected === peer.node_id) setSelected(null);
        // 本地先摘掉,界面立即有反馈(对方再上线/点刷新会自动回来)
        setPeers((list) => list.filter((p) => p.node_id !== peer.node_id));
        pushToast(
          "info",
          `已移出列表:${peer.name}(聊天记录保留;对方在线时点"刷新"会自动回来)`,
        );
      } catch (e) {
        pushToast("error", `删除联系人失败:${String(e)}`);
      }
    },
    [selected, pushToast],
  );

  /** 抖一抖(单聊或群聊;飞秋经典功能)。 */
  const doSendShake = useCallback(
    async (target: string) => {
      // 节流:同一会话 1.5s 内只发一次,避免"抖动炸弹"
      const now = Date.now();
      if (now - (shakeCooldownRef.current[target] ?? 0) < 1500) {
        pushToast("info", "别抖了,稍等一下下 😄");
        return;
      }
      shakeCooldownRef.current[target] = now;
      try {
        await api.sendShake(target);
        // 本地也留一条提示(历史由后端落库)
        setMessages((all) => {
          const list = all[target] ?? [];
          const hint: ChatMessage = {
            id: `shake-local-${Date.now()}`,
            outgoing: true,
            kind: "shake",
            body: null,
            ts_ms: Date.now(),
            delivered: false,
            read: false,
            from_node: self?.node_id,
          };
          return { ...all, [target]: [...list, hint] };
        });
        pushToast("info", "已发送窗口抖动");
      } catch (e) {
        pushToast("error", `抖动发送失败:${String(e)}`);
      }
    },
    [pushToast, self?.node_id],
  );

  /** 复制消息文本。 */
  const doCopyMessage = useCallback(
    async (m: ChatMessage) => {
      const text = m.body ?? "";
      if (!text.trim()) {
        pushToast("info", "这条消息没有可复制的文本");
        return;
      }
      try {
        await navigator.clipboard.writeText(text);
        pushToast("info", "已复制到剪贴板");
      } catch {
        pushToast("error", "复制失败(剪贴板不可用)");
      }
    },
    [pushToast],
  );

  /** 转发消息:文本重发文本,图片/文件重发本地文件。 */
  const doForwardTo = useCallback(
    async (m: ChatMessage, target: string) => {
      setForwarding(null);
      try {
        if (m.kind === "text") {
          if (target.startsWith("group:")) await api.sendGroupText(target, m.body ?? "");
          else await api.sendText(target, m.body ?? "");
        } else {
          const info = JSON.parse(m.body ?? "{}") as { p?: string };
          if (!info.p) throw new Error("找不到本地文件路径(可能已被删除)");
          await api.sendFileTo(target, info.p);
        }
        pushToast("info", "已转发");
      } catch (e) {
        pushToast("error", `转发失败:${String(e)}`);
      }
    },
    [pushToast],
  );

  /** 删除一条消息(仅本机)。 */
  const doDeleteMessage = useCallback(
    async (m: ChatMessage) => {
      try {
        await api.deleteMessage(m.id);
        setMessages((all) => {
          const key = selected ?? "";
          const list = all[key] ?? [];
          return { ...all, [key]: list.filter((x) => x.id !== m.id) };
        });
        pushToast("info", "已删除本条消息(仅本机)");
      } catch (e) {
        pushToast("error", `删除失败:${String(e)}`);
      }
    },
    [selected, pushToast],
  );

  const doScreenshot = useCallback(async () => {
    if (!selected) {
      pushToast("info", "请先选择要发送给的成员或群聊");
      return;
    }
    try {
      await api.takeScreenshot(selected);
      pushToast("info", "截图已发送");
    } catch (e) {
      pushToast("error", `截图失败:${String(e)}`);
    }
  }, [selected, pushToast]);

  const doSendImage = useCallback(async () => {
    if (!selected) {
      pushToast("info", "请先选择要发送给的成员或群聊");
      return;
    }
    try {
      const path = await api.pickImage();
      if (!path) return;
      const tokens = await api.sendFileTo(selected, path);
      for (const token of tokens) {
        setTransfers((t) => ({
          ...t,
          [token]: {
            token,
            direction: "send",
            path,
            transferred: 0,
            total: 0,
            done: false,
            failed: null,
            finished_at: null,
            samples: [],
          },
        }));
      }
      // 立即在聊天中显示图片气泡
      const fileName = path.split(/[\\/]/).pop() ?? path;
      const msg: ChatMessage = {
        id: `img-${Date.now()}`,
        outgoing: true,
        kind: "image",
        body: JSON.stringify({ n: fileName, s: 0, p: path, i: true }),
        ts_ms: Date.now(),
        delivered: false,
        read: false,
      };
      setMessages((m) => ({ ...m, [selected]: [...(m[selected] ?? []), msg] }));
    } catch (e) {
      pushToast("error", `发送图片失败:${String(e)}`);
    }
  }, [selected, pushToast]);

  const handleImageClick = useCallback(async (path: string) => {
    try {
      await api.openFile(path);
    } catch (e) {
      pushToast("error", `打开图片失败:${String(e)}`);
    }
  }, [pushToast]);

  const insertEmoji = useCallback((emoji: string) => {
    setDraft((d) => d + emoji);
  }, []);

  /** 粘贴图片:剪贴板有图则直接发送。 */
  const doPasteImage = useCallback(async () => {
    if (!selected) {
      pushToast("info", "请先选择要发送给的成员或群聊");
      return;
    }
    try {
      const path = await api.pasteImage();
      if (!path) {
        pushToast("info", "剪贴板里没有图片");
        return;
      }
      const tokens = await api.sendFileTo(selected, path);
      for (const token of tokens) {
        setTransfers((t) => ({
          ...t,
          [token]: {
            token,
            direction: "send",
            path,
            transferred: 0,
            total: 0,
            done: false,
            failed: null,
            finished_at: null,
            samples: [],
          },
        }));
      }
      const fileName = path.split(/[\\/]/).pop() ?? path;
      const msg: ChatMessage = {
        id: `paste-${Date.now()}`,
        outgoing: true,
        kind: "image",
        body: JSON.stringify({ n: fileName, s: 0, p: path, i: true }),
        ts_ms: Date.now(),
        delivered: false,
        read: false,
      };
      setMessages((m) => ({ ...m, [selected]: [...(m[selected] ?? []), msg] }));
      pushToast("info", "已发送剪贴板图片");
    } catch (e) {
      pushToast("error", `粘贴图片失败:${String(e)}`);
    }
  }, [selected, pushToast]);

  /** 历史搜索:输入防抖 300ms。 */
  useEffect(() => {
    if (!showSearch) return;
    const q = searchQuery.trim();
    if (!q) {
      setSearchHits([]);
      return;
    }
    const timer = window.setTimeout(() => {
      api.searchMessages(q).then(setSearchHits).catch(() => setSearchHits([]));
    }, 300);
    return () => window.clearTimeout(timer);
  }, [searchQuery, showSearch]);

  /** 打开一条搜索命中所在的会话。 */
  const openSearchHit = useCallback(
    (hit: api.SearchHit) => {
      const known = peers.some((p) => p.node_id === hit.peer);
      if (!known) {
        // 历史联系人(可能已离线/已删除):合成一个临时条目以便查看历史
        setPeers((list) => [
          ...list,
          {
            node_id: hit.peer,
            name: `${hit.peer.slice(0, 8)}…`,
            group: null,
            online: false,
            last_seen_ms: hit.ts_ms,
            ips: [],
            app_version: null,
            avatar_sha256: null,
          },
        ]);
      }
      openPeer(hit.peer);
      setShowSearch(false);
    },
    [peers, openPeer],
  );

  const doCreateGroup = useCallback(async () => {
    const name = newGroupName.trim();
    const members = Object.entries(newGroupMembers)
      .filter(([, checked]) => checked)
      .map(([id]) => id);
    if (!name) {
      pushToast("info", "请填写群名称");
      return;
    }
    if (members.length === 0) {
      pushToast("info", "请至少勾选一位成员");
      return;
    }
    try {
      const groupId = await api.createGroup(name, members);
      refreshGroups();
      setShowCreateGroup(false);
      setNewGroupName("");
      setNewGroupMembers({});
      openPeer(groupId);
      pushToast("info", `群聊「${name}」已创建(${members.length} 位成员)`);
    } catch (e) {
      pushToast("error", `创建群聊失败:${String(e)}`);
    }
  }, [newGroupName, newGroupMembers, refreshGroups, openPeer, pushToast]);

  const doDeleteGroup = useCallback(
    async (group: api.GroupInfo) => {
      try {
        await api.deleteGroup(group.id);
        if (selected === group.id) setSelected(null);
        refreshGroups();
        pushToast("info", `已删除群聊「${group.name}」(历史保留)`);
      } catch (e) {
        pushToast("error", `删除群聊失败:${String(e)}`);
      }
    },
    [selected, refreshGroups, pushToast],
  );

  const doCheckUpdate = useCallback(async () => {
    setCheckingUpdate(true);
    try {
      const report = await api.checkUpdate();
      setVersionReport(report);
      if (report.update_available) {
        pushToast(
          "info",
          `发现更新版本 v${report.latest_version}(${report.latest_from}),可向对方索取安装包`,
        );
      } else {
        pushToast("info", "已是最新版本");
      }
    } catch (e) {
      pushToast("error", `检查更新失败:${String(e)}`);
    } finally {
      setCheckingUpdate(false);
    }
  }, [pushToast]);

  const openSettings = useCallback(() => {
    setSettingsName(self?.name ?? "");
    setSettingsGroup(self?.group ?? "");
    setSettingsTab("profile");
    setShowSettings(true);
  }, [self]);

  const doSaveSettings = useCallback(async () => {
    try {
      await api.setProfile(settingsName, settingsGroup);
      setSelf((s) => (s ? { ...s, name: settingsName, group: settingsGroup || null } : s));
      setShowSettings(false);
      pushToast("info", "已保存并广播新资料,对方几秒内更新");
    } catch (e) {
      pushToast("error", `保存失败:${String(e)}`);
    }
  }, [settingsName, settingsGroup, pushToast]);

  const doRefresh = useCallback(() => {
    // 刷新 = 重新广播 + 把局域网里已知的成员全部写回联系人表
    //(之前删掉的人只要还在线就会重新出现 —— 飞秋语义)
    api
      .refreshPeers()
      .then((count) => {
        api.listPeers().then(setPeers).catch(() => undefined);
        pushToast("info", `已刷新:局域网内 ${count} 位成员已重新获取`);
      })
      .catch((e) => pushToast("error", `刷新失败:${String(e)}`));
  }, [pushToast]);

  useEffect(() => {
    api.getSelfInfo().then(setSelf).catch((e) => console.error(e));
    refreshPeers();
    refreshGroups();
    refreshConversations();
    const timer = window.setInterval(refreshPeers, 5000);
    const groupTimer = window.setInterval(refreshGroups, 5000);
    const convTimer = window.setInterval(refreshConversations, 3000);

    let disposed = false;
    let unlisten: (() => void) | null = null;
    api
      .onFqEvent((event) => handleEvent(event))
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch((e) => console.error("事件订阅失败", e));

    return () => {
      disposed = true;
      unlisten?.();
      window.clearInterval(timer);
      window.clearInterval(groupTimer);
      window.clearInterval(convTimer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    listBottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages, selected]);

  // 头像懒加载:联系人(含离线)声明了头像哈希且本地未缓存时拉取
  useEffect(() => {
    peers.forEach((p) => {
      if (p.avatar_sha256 && !avatars[p.node_id]) loadAvatar(p.node_id);
    });
  }, [peers, avatars, loadAvatar]);

  // 自动更新:开机读设置 + 后台定期做版本发现(无需更新服务器)
  useEffect(() => {
    refreshPreferences();
  }, [refreshPreferences]);

  useEffect(() => {
    void silentCheckUpdate();
    const timer = window.setInterval(() => void silentCheckUpdate(), 30_000);
    return () => window.clearInterval(timer);
  }, [silentCheckUpdate]);

  // 自动模式:发现更高版本即自动向对方索取更新包(同一版本只索取一次)
  useEffect(() => {
    if (!autoUpdate || pendingInstall) return;
    const newer = (versionReport?.peers ?? []).filter((p) => p.relation === "newer");
    if (newer.length === 0) return;
    const best = newer.reduce((a, b) =>
      versionCompare(b.version, a.version) > 0 ? b : a,
    );
    void doRequestUpdate(best.node_id, best.version);
  }, [autoUpdate, versionReport, pendingInstall, doRequestUpdate]);

  // 自动模式:更新包就绪后倒计时重启安装(点"稍后"即取消倒计时)
  useEffect(() => {
    if (!autoUpdate || !pendingInstall || !showUpdatePrompt) {
      setInstallCountdown(null);
      return;
    }
    let left = 10;
    setInstallCountdown(left);
    const timer = window.setInterval(() => {
      left -= 1;
      if (left <= 0) {
        window.clearInterval(timer);
        void doInstallUpdate();
      } else {
        setInstallCountdown(left);
      }
    }, 1000);
    return () => window.clearInterval(timer);
  }, [autoUpdate, pendingInstall, showUpdatePrompt, doInstallUpdate]);

  // "正在输入"提示 4 秒后自动消失(对方停止输入或已发送消息)
  useEffect(() => {
    const timer = window.setInterval(() => {
      setTypingPeers((prev) => {
        const now = Date.now();
        const next: Record<string, number> = {};
        let changed = false;
        for (const [id, ts] of Object.entries(prev)) {
          if (now - ts < 4000) next[id] = ts;
          else changed = true;
        }
        return changed ? next : prev;
      });
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);

  const handleEvent = (event: FqEvent) => {
    switch (event.type) {
      case "peer_up":
      case "peer_down":
        refreshPeers();
        break;
      case "message": {
        const mentions = Array.isArray(event.mentions) ? event.mentions : [];
        const mentionedMe = Boolean(self?.node_id) && mentions.includes(self!.node_id);
        const msg: ChatMessage = {
          id: event.id,
          outgoing: false,
          kind: "text",
          body: event.body,
          ts_ms: event.ts_ms,
          delivered: false,
          read: false,
          mentions,
          reply_to: event.reply_to ?? null,
        };
        setMessages((m) => ({
          ...m,
          [event.from]: [...(m[event.from] ?? []), msg],
        }));
        setUnread((u) =>
          selected === event.from ? u : { ...u, [event.from]: (u[event.from] ?? 0) + 1 },
        );
        // 被打断输入状态:收到消息即清除"正在输入"
        setTypingPeers((prev) => {
          if (!prev[event.from]) return prev;
          const next = { ...prev };
          delete next[event.from];
          return next;
        });
        if (mentionedMe && !mutedPeersRef.current.has(event.from)) {
          pushToast("info", `${event.from_name} 在群里 @ 了你`);
        }
        refreshConversations();
        if (selected === event.from) {
          api.markRead(event.from).catch(() => undefined);
        }
        break;
      }
      case "typing": {
        setTypingPeers((prev) => {
          const next = { ...prev };
          if (event.started) next[event.from] = Date.now();
          else delete next[event.from];
          return next;
        });
        break;
      }
      case "delivered":
      case "read": {
        const flag = event.type === "delivered" ? "delivered" : "read";
        setMessages((all) => {
          const next: Record<string, ChatMessage[]> = {};
          for (const [peer, list] of Object.entries(all)) {
            next[peer] = list.map((m) =>
              m.id === event.id
                ? { ...m, delivered: true, read: m.read || flag === "read" }
                : m,
            );
          }
          return next;
        });
        break;
      }
      case "queued_flushed":
        if (selected) {
          api
            .history(selected)
            .then((rows) => setMessages((m) => ({ ...m, [selected]: rows })))
            .catch(() => undefined);
        }
        break;
      case "file_offer":
        setOffers((list) => [
          ...list,
          {
            token: event.token,
            from_name: event.from_name,
            entries: event.entries,
            total_bytes: event.total_bytes,
          },
        ]);
        break;
      case "file_progress": {
        // 有进度说明对方已接受:把可能还挂着的确认弹窗收掉
        setOffers((list) => (list.some((o) => o.token === event.token) ? list.filter((o) => o.token !== event.token) : list));
        const now = Date.now();
        setTransfers((t) => {
          const cur = t[event.token] ?? {
            token: event.token,
            direction: event.direction,
            path: event.path,
            transferred: 0,
            total: event.total,
            done: false,
            failed: null,
            finished_at: null,
            samples: [],
          };
          const samples = [
            ...cur.samples.filter((s) => now - s.t <= 1500),
            { t: now, bytes: event.transferred },
          ];
          return {
            ...t,
            [event.token]: {
              ...cur,
              direction: event.direction,
              path: event.path,
              transferred: event.transferred,
              total: event.total,
              failed: null,
              samples,
            },
          };
        });
        break;
      }
      case "file_done":
        break;
      case "file_completed":
        setOffers((list) => list.filter((o) => o.token !== event.token));
        finishTransfer(event.token, null);
        break;
      case "file_failed":
        setOffers((list) => list.filter((o) => o.token !== event.token));
        finishTransfer(event.token, event.reason);
        pushToast("error", `文件传输失败:${event.reason}`);
        break;
      case "transfer_retrying":
        // 自动重试:撤掉旧的失败行(新会话的进度会另起一行),提示重试进度
        setTransfers((t) => {
          if (!t[event.token]) return t;
          const next = { ...t };
          delete next[event.token];
          return next;
        });
        pushToast(
          "info",
          `${event.name} 传输中断,正在自动重试(${event.attempt}/${event.max})…`,
        );
        break;
      case "transfer_retry_gave_up":
        pushToast("error", `${event.name} 自动重试失败,已放弃(可重新发送)`);
        break;
      case "update_incoming": {
        // 更新包自动接收:面板里显示进度(无需用户点"接收")
        pushToast("info", `正在从 ${event.from_name} 获取 v${event.version} 更新包…`);
        setTransferPanelOpen(true);
        setTransfers((t) => ({
          ...t,
          [event.token]: {
            token: event.token,
            direction: "recv",
            path: event.file_name,
            transferred: 0,
            total: event.total_bytes,
            done: false,
            failed: null,
            finished_at: null,
            samples: [],
          },
        }));
        break;
      }
      case "update_ready": {
        setPendingInstall({
          version: event.version,
          from_name: event.from_name,
          path: event.path,
        });
        setShowUpdatePrompt(true);
        pushToast("info", `v${event.version} 更新包已下载并校验通过`);
        break;
      }
      case "peer_avatar": {
        // 对端头像有更新:清掉缓存重新拉取(拿不到就保持首字母兜底)
        setAvatars((prev) => {
          const next = { ...prev };
          delete next[event.node_id];
          return next;
        });
        loadAvatar(event.node_id, true);
        break;
      }
      case "peer_avatar_removed": {
        setAvatars((prev) => {
          const next = { ...prev };
          delete next[event.node_id];
          return next;
        });
        break;
      }
      case "shaken": {
        // 对端抖了我一下:晃窗口 + 留下一条提示(历史由后端落库)
        setShaking(true);
        window.setTimeout(() => setShaking(false), 700);
        pushToast("info", `${event.from_name} 抖了你一下`);
        setMessages((all) => {
          const list = all[event.from] ?? [];
          const hint: ChatMessage = {
            id: `shake-local-${Date.now()}`,
            outgoing: false,
            kind: "shake",
            body: null,
            ts_ms: Date.now(),
            delivered: false,
            read: false,
            from_node: event.from,
          };
          return { ...all, [event.from]: [...list, hint] };
        });
        refreshConversations();
        break;
      }
      case "trust_warning":
        setTrustWarning(
          `节点 ${shortId(event.node_id)}… 的静态密钥发生变化!\n原指纹 ${event.pinned}\n新指纹 ${event.presented}\n可能是对端重装,也可能是中间人攻击 —— 请当面核实后再继续通信。`,
        );
        break;
    }
  };

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    api
      .onFileDrop(async (paths) => {
        if (paths.length === 0) return;
        if (!selected) {
          pushToast("info", "请先在左侧选择要发送给的成员或群聊,再拖入文件");
          return;
        }
        for (const path of paths) {
          try {
            const tokens = await api.sendFileTo(selected, path);
            for (const token of tokens) {
              setTransfers((t) => ({
                ...t,
                [token]: {
                  token,
                  direction: "send",
                  path,
                  transferred: 0,
                  total: 0,
                  done: false,
                  failed: null,
                  finished_at: null,
                  samples: [],
                },
              }));
            }
          } catch (e) {
            pushToast("error", `拖拽发送失败(${path}):${String(e)}`);
          }
        }
      })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch((e) => pushToast("error", `拖拽功能初始化失败:${String(e)}`));
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [selected, pushToast]);

  const doSend = async () => {
    const body = draft.trim();
    if (!body || !selected) return;
    setDraft("");
    const isGroup = selected.startsWith("group:");
    // @ 提醒:只保留正文里确实还带着"@名字"的成员(用户可能删掉了)
    const mentions = mentionIds.filter((id) => {
      const name = peers.find((p) => p.node_id === id)?.name;
      return name ? body.includes(`@${name}`) : false;
    });
    setMentionIds([]);
    const replyTo = quoting?.id ?? null;
    try {
      if (isGroup) {
        const [sent, queued] = await api.sendGroupText(selected, body, mentions, replyTo);
        const msg: ChatMessage = {
          id: `grp-${Date.now()}`,
          outgoing: true,
          kind: "text",
          body,
          ts_ms: Date.now(),
          delivered: false,
          read: false,
          mentions,
          reply_to: replyTo,
        };
        setMessages((m) => ({ ...m, [selected]: [...(m[selected] ?? []), msg] }));
        if (queued > 0) {
          pushToast("info", `已送达 ${sent} 人,${queued} 人离线(消息已入队,上线后自动补发)`);
        }
      } else {
        const result = await api.sendText(selected, body, [], replyTo);
        const msg: ChatMessage = {
          id: result.id,
          outgoing: true,
          kind: "text",
          body,
          ts_ms: Date.now(),
          delivered: false,
          read: false,
          reply_to: replyTo,
        };
        setMessages((m) => ({ ...m, [selected]: [...(m[selected] ?? []), msg] }));
        if (result.queued) {
          pushToast("info", "对方当前不在线,消息已入队,对方上线后自动送达");
        }
      }
      setQuoting(null);
    } catch (e) {
      console.error("发送失败", e);
      pushToast("error", `消息发送失败:${String(e)}`);
      setDraft(body);
    }
  };

  /** 输入变化:单聊时按 3 秒节流上报"正在输入",停手 3 秒后发停止。 */
  const onDraftChange = (value: string) => {
    setDraft(value);
    if (!selected || selected.startsWith("group:")) return;
    const now = Date.now();
    const sent = typingSentRef.current;
    if (!sent || sent.peer !== selected || now - sent.sentAt > 3000) {
      typingSentRef.current = { peer: selected, sentAt: now };
      void api.sendTyping(selected, true).catch(() => undefined);
    }
    if (typingStopTimerRef.current) window.clearTimeout(typingStopTimerRef.current);
    typingStopTimerRef.current = window.setTimeout(() => {
      typingSentRef.current = null;
      void api.sendTyping(selected, false).catch(() => undefined);
    }, 3000);
  };

  /** 发送文件的目标可能是单聊或群聊;返回的每个 token 都是独立传输。 */
  const addTransferItems = useCallback((tokens: string[], path: string) => {
    setTransfers((t) => {
      const next = { ...t };
      for (const token of tokens) {
        next[token] = {
          token,
          direction: "send",
          path,
          transferred: 0,
          total: 0,
          done: false,
          failed: null,
          finished_at: null,
          samples: [],
        };
      }
      return next;
    });
  }, []);

  const doAttach = async () => {
    if (!selected) {
      pushToast("info", "请先在左侧选择要发送给的成员或群聊");
      return;
    }
    let path: string | null;
    try {
      path = await api.pickFile();
    } catch (e) {
      pushToast("error", `打开文件选择框失败:${String(e)}`);
      return;
    }
    if (!path) return;
    try {
      const tokens = await api.sendFileTo(selected, path);
      addTransferItems(tokens, path);
      pushToast("info", `已发出文件,等待对方确认接收:${path.split(/[\\/]/).pop()}`);
    } catch (e) {
      console.error("发送文件失败", e);
      pushToast("error", `文件发送失败:${String(e)}`);
    }
  };

  const doAcceptOffer = async (token: string) => {
    setOffers((list) => list.filter((o) => o.token !== token));
    const dir = offerDirChoice[token];
    try {
      const ok = await api.acceptFileOffer(token, dir);
      if (!ok) {
        // 要约已超时/已取消(会话没了):明确告知但不吓人
        pushToast("info", "该文件要约已失效(可能超时或被取消),请让对方重发");
      }
    } catch (e) {
      pushToast("error", `同意接收失败:${String(e)}`);
    }
    setOfferDirChoice((c) => {
      const next = { ...c };
      delete next[token];
      return next;
    });
  };

  const doRejectOffer = async (token: string) => {
    setOffers((list) => list.filter((o) => o.token !== token));
    try {
      await api.rejectFileOffer(token);
    } catch (e) {
      pushToast("error", `拒绝失败:${String(e)}`);
    }
  };

  const doPickOfferDir = async (token: string) => {
    try {
      const dir = await api.pickDir();
      if (dir) setOfferDirChoice((c) => ({ ...c, [token]: dir }));
    } catch (e) {
      pushToast("error", `打开目录选择框失败:${String(e)}`);
    }
  };

  const doChangeDownloadDir = async () => {
    try {
      const dir = await api.pickDir();
      if (!dir) return;
      await api.setDownloadDir(dir);
      setSelf((s) => (s ? { ...s, download_dir: dir } : s));
      pushToast("info", `接收文件将保存到:${dir}`);
    } catch (e) {
      pushToast("error", `修改保存目录失败:${String(e)}`);
    }
  };

  const selectedPeer = useMemo(
    () => peers.find((p) => p.node_id === selected) ?? null,
    [peers, selected],
  );
  /** 当前选中项的统一描述(单聊=peer,群聊=group)。 */
  const selectedEntity = useMemo(() => {
    if (!selected) return null;
    const group = groups.find((g) => g.id === selected);
    if (group) {
      return {
        isGroup: true,
        name: group.name,
        sub: `${group.member_count} 位成员`,
        online: true,
      };
    }
    if (selectedPeer) {
      return {
        isGroup: false,
        name: selectedPeer.name,
        sub:
          selectedPeer.ips.length > 0
            ? selectedPeer.ips[0]
            : shortId(selectedPeer.node_id),
        online: selectedPeer.online,
      };
    }
    // 历史联系人(不在当前列表里):用 ID 兜底展示
    return {
      isGroup: false,
      name: `${selected.slice(0, 8)}…`,
      sub: shortId(selected),
      online: false,
    };
  }, [selected, groups, selectedPeer]);
  const currentMessages = selected ? messages[selected] ?? [] : [];

  /** 消息发送者展示名(群聊里用于引用条与"谁引用了谁")。 */
  const nameOfMsg = (msg: ChatMessage): string =>
    msg.outgoing
      ? self?.name ?? "我"
      : peers.find((p) => p.node_id === msg.from_node)?.name ?? "对方";

  /** 引用条里引用内容的摘要(图片/文件/抖动没有正文,给出占位)。 */
  const quotePreview = (msg: ChatMessage): string => {
    if (msg.kind === "image") return "[图片]";
    if (msg.kind === "file") {
      try {
        return `[文件] ${JSON.parse(msg.body ?? "{}").n ?? ""}`.trim();
      } catch {
        return "[文件]";
      }
    }
    if (msg.kind === "shake") return "[窗口抖动]";
    return msg.body ?? "";
  };
  const transferList = useMemo(
    () => Object.values(transfers).sort((a, b) => a.token.localeCompare(b.token)),
    [transfers],
  );

  const filteredPeers = useMemo(() => {
    if (!searchText.trim()) return peers;
    const q = searchText.trim().toLowerCase();
    return peers.filter(
      (p) =>
        p.name.toLowerCase().includes(q) ||
        p.ips.some((ip) => ip.includes(q)) ||
        p.node_id.toLowerCase().startsWith(q),
    );
  }, [peers, searchText]);

  const { online: onlineGroups, offline: offlineGroups } = useMemo(
    () => buildGroups(filteredPeers),
    [filteredPeers],
  );
  const onlineCount = filteredPeers.filter((p) => p.online).length;
  const offlineCount = filteredPeers.length - onlineCount;

  const toggleGroup = (label: string) => {
    setCollapsedGroups((c) => ({ ...c, [label]: !c[label] }));
  };

  const renderPeerItem = (peer: Peer) => (
    <div
      key={peer.node_id}
      className={`peer-item ${selected === peer.node_id ? "active" : ""} ${peer.online ? "" : "offline-peer"}`}
      onClick={() => openPeer(peer.node_id)}
      onContextMenu={(e) => {
        e.preventDefault();
        setContextMenu({ x: e.clientX, y: e.clientY, peer });
      }}
    >
      <AvatarBubble
        url={avatars[peer.node_id]}
        fallback={peer.name}
        seed={peer.node_id}
        className={peer.online ? "" : "offline"}
        title="查看资料"
        onClick={() => openProfileCard(peer.node_id)}
      />
      <div className="peer-meta">
        <div className="peer-name">
          {peer.name}
          {peer.online &&
            peer.app_version &&
            versionCompare(peer.app_version, currentVersion) > 0 && (
              <span className="peer-ver-tag" title={`对方运行更新版本 v${peer.app_version}`}>
                NEW
              </span>
            )}
        </div>
        <div className="peer-ips" title={peer.ips.join("\n")}>
          {peer.ips.length > 0 ? peer.ips[0] : "…"}
        </div>
      </div>
      {unread[peer.node_id] ? (
        <span className="badge">{unread[peer.node_id]}</span>
      ) : null}
    </div>
  );

  const renderGroupSection = (groups: PeerGroup[], kind: "online" | "offline") =>
    groups.map((group) => {
      const key = `${kind}:${group.label}`;
      const collapsed = collapsedGroups[key] ?? (kind === "offline");
      return (
        <div key={key} className="peer-group-section">
          <div className="group-header" onClick={() => toggleGroup(key)}>
            <span className={`group-arrow ${collapsed ? "" : "open"}`}>▸</span>
            <span className="group-label">{group.label}</span>
            <span className="group-count">
              {group.peers.filter((p) => p.online).length}/{group.peers.length}
            </span>
          </div>
          {!collapsed && <div className="group-body">{group.peers.map(renderPeerItem)}</div>}
        </div>
      );
    });

  return (
    <div className={`app ${shaking ? "shaking" : ""}`}>
      {/* ── 图标栏(QQ 三栏的第一栏)── */}
      <nav className="rail">
        <div
          className="rail-avatar"
          title={`${self?.name ?? ""}\n${self?.fingerprint ?? ""}\n点击修改个人资料与头像`}
          onClick={openSettings}
        >
          {self?.avatar ? <img src={self.avatar} alt="" draggable={false} /> : (self?.name ?? "?").slice(0, 1)}
        </div>
        <div className="rail-icons">
          <button className="rail-btn active" title="聊天">
            💬
          </button>
          <button
            className={`rail-btn ${showTransferHistory ? "active" : ""}`}
            title="文件传输记录"
            onClick={() => (showTransferHistory ? setShowTransferHistory(false) : doOpenTransferHistory())}
          >
            📁
          </button>
        </div>
        <div className="rail-bottom">
          <div
            className="rail-btn rail-dir"
            title={`接收目录:${self?.download_dir ?? ""}(点击更改)`}
            onClick={() => void doChangeDownloadDir()}
          >
            📂
          </div>
          <button className="rail-btn" title="刷新成员" onClick={doRefresh}>
            🔄
          </button>
          <button className="rail-btn" title="设置" onClick={openSettings}>
            ⚙️
          </button>
        </div>
      </nav>

      {/* ── 联系人面板(QQ 三栏的第二栏)── */}
      <aside className="sidebar">
        <div className="search-bar">
          <input
            type="text"
            placeholder="搜索昵称 / IP / ID"
            value={searchText}
            onChange={(e) => setSearchText(e.target.value)}
          />
        </div>
        <div className="peer-list">
          {filteredPeers.length === 0 && groups.length === 0 && conversations.length === 0 && (
            <div className="empty-hint">
              {searchText ? "无匹配结果" : "局域网内暂未发现同伴"}
            </div>
          )}

          {/* ── 最近会话(参考 box-im/微信:按活跃排序 + 未读角标,可折叠)── */}
          {conversations.length > 0 && !searchText && (
            <div className="peer-group-section">
              <div
                className="group-header static"
                onClick={() => toggleSection("conversations")}
                title={collapsedSections.conversations ? "展开最近会话" : "收起最近会话"}
              >
                <span className={`group-arrow ${collapsedSections.conversations ? "" : "open"}`}>
                  ▸
                </span>
                <span className="group-label">💬 最近会话</span>
                <span className="group-count">{conversations.length}</span>
              </div>
              {!collapsedSections.conversations && (
                <div className="group-body">
                  {conversations.slice(0, 8).map((conv) => {
                    const label = conv.peer.startsWith("group:")
                      ? groups.find((g) => g.id === conv.peer)?.name ?? "群聊"
                      : peers.find((p) => p.node_id === conv.peer)?.name ??
                        `${conv.peer.slice(0, 8)}…`;
                    const isGroupConv = conv.peer.startsWith("group:");
                    const online =
                      !isGroupConv &&
                      (peers.find((p) => p.node_id === conv.peer)?.online ?? false);
                    return (
                      <div
                        key={conv.peer}
                        className={`peer-item ${selected === conv.peer ? "active" : ""}`}
                        onClick={() => openPeer(conv.peer)}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          setConvMenu({
                            x: e.clientX,
                            y: e.clientY,
                            peer: conv.peer,
                            label,
                            pinned: conv.pinned,
                            muted: conv.muted,
                          });
                        }}
                      >
                        <AvatarBubble
                          url={isGroupConv ? undefined : avatars[conv.peer]}
                          fallback={isGroupConv ? "群" : label}
                          seed={conv.peer}
                          className={isGroupConv ? "group-avatar" : online ? "" : "offline"}
                        />
                        <div className="peer-meta">
                          <div className="peer-name">
                            {conv.pinned && <span className="conv-pin" title="已置顶">📌</span>}
                            {label}
                          </div>
                          <div className="conv-preview">{conv.preview}</div>
                        </div>
                        <div className="conv-side">
                          <div className="conv-time">{formatDivider(conv.last_msg_ms)}</div>
                          {conv.unread > 0 &&
                            (conv.muted ? (
                              <span className="badge-dot" title="免打扰:有新消息" />
                            ) : (
                              <span className="badge">{conv.unread}</span>
                            ))}
                        </div>
                        <button
                          className="conv-remove"
                          title="从最近会话中移除(聊天记录保留)"
                          onClick={(e) => {
                            e.stopPropagation();
                            void doDeleteConversation(conv.peer, label, conv.unread);
                          }}
                        >
                          ×
                        </button>
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          )}

          {/* ── 群聊区块(可折叠)── */}
          <div className="peer-group-section">
            <div
              className="group-header static"
              onClick={() => toggleSection("groups")}
              title={collapsedSections.groups ? "展开群聊" : "收起群聊"}
            >
              <span className={`group-arrow ${collapsedSections.groups ? "" : "open"}`}>▸</span>
              <span className="group-label">👥 群聊</span>
              <span className="group-count">{groups.length}</span>
              <button
                className="group-add-btn"
                title="新建群聊"
                onClick={(e) => {
                  e.stopPropagation();
                  setShowCreateGroup(true);
                }}
              >
                +
              </button>
            </div>
            {!collapsedSections.groups && (
              <div className="group-body">
                {groups.map((group) => (
                  <div
                    key={group.id}
                    className={`peer-item ${selected === group.id ? "active" : ""}`}
                    onClick={() => openPeer(group.id)}
                    onContextMenu={(e) => {
                      e.preventDefault();
                      if (window.confirm(`删除群聊「${group.name}」?(历史消息会保留)`)) {
                        void doDeleteGroup(group);
                      }
                    }}
                  >
                    <div className="avatar group-avatar">群</div>
                    <div className="peer-meta">
                      <div className="peer-name">{group.name}</div>
                      <div className="peer-ips">{group.member_count} 位成员</div>
                    </div>
                  </div>
                ))}
                {groups.length === 0 && (
                  <div className="empty-hint small">点上方 + 创建群聊</div>
                )}
              </div>
            )}
          </div>

          {renderGroupSection(onlineGroups, "online")}
          {offlineGroups.length > 0 && (
            <div className="offline-toggle" onClick={() => setShowOffline(!showOffline)}>
              <span className={`group-arrow ${showOffline ? "open" : ""}`}>▸</span>
              离线联系人 ({offlineCount})
            </div>
          )}
          {showOffline && renderGroupSection(offlineGroups, "offline")}
        </div>
        <div className="sidebar-footer">
          <span className="status-dot on" /> {onlineCount} 在线
          {offlineCount > 0 && <span className="offline-count"> · {offlineCount} 离线</span>}
          {conversations.some((c) => c.unread > 0) && (
            <button
              className="footer-action"
              title="把所有会话的未读清零"
              onClick={() => void doMarkAllRead()}
            >
              全部已读
            </button>
          )}
        </div>
      </aside>

      {/* ── 聊天区(QQ 三栏的第三栏)── */}
      <main className="chat">
        {!selectedEntity ? (
          <div className="chat-placeholder">
            <div className="logo">FQ</div>
            <div>选择左侧成员或群聊开始聊天</div>
            <div className="hint">支持直接拖拽文件到窗口发送</div>
          </div>
        ) : (
          <>
            <header className="chat-header">
              <div className={`dot ${selectedEntity.online ? "on" : "off"}`} />
              {!selectedEntity.isGroup && selectedPeer && (
                <AvatarBubble
                  url={avatars[selectedPeer.node_id]}
                  fallback={selectedPeer.name}
                  seed={selectedPeer.node_id}
                  className="header-avatar"
                  title="查看资料"
                  onClick={() => openProfileCard(selectedPeer.node_id)}
                />
              )}
              <span className="chat-title">
                {selectedEntity.isGroup ? "👥 " : ""}
                {selectedEntity.name}
              </span>
              <span className="chat-sub">{selectedEntity.sub}</span>
              <span className="chat-status">
                {typingPeers[selected ?? ""] ? (
                  <span className="typing-hint">
                    对方正在输入
                    <span className="typing-dots">
                      <i />
                      <i />
                      <i />
                    </span>
                  </span>
                ) : selectedEntity.isGroup ? (
                  "群聊(消息扇出到各成员)"
                ) : selectedEntity.online ? (
                  "在线"
                ) : (
                  "离线(消息将入队)"
                )}
              </span>
              <button
                className="header-btn"
                title="抖一抖(飞秋经典:提醒对方注意)"
                onClick={() => selected && void doSendShake(selected)}
              >
                👋
              </button>
              <button
                className={`header-btn ${showSearch ? "active" : ""}`}
                title="搜索历史消息"
                onClick={() => setShowSearch(!showSearch)}
              >
                🔍
              </button>
            </header>
            {showSearch && (
              <div className="search-panel">
                <div className="search-panel-input">
                  <input
                    type="text"
                    autoFocus
                    placeholder="搜索全部会话的历史消息…"
                    value={searchQuery}
                    onChange={(e) => setSearchQuery(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Escape") setShowSearch(false);
                    }}
                  />
                  <span className="search-count">
                    {searchQuery.trim() ? `${searchHits.length} 条` : ""}
                  </span>
                </div>
                <div className="search-results">
                  {searchHits.length === 0 && searchQuery.trim() && (
                    <div className="search-empty">没有匹配的消息</div>
                  )}
                  {searchHits.map((hit) => {
                    const peerName =
                      peers.find((p) => p.node_id === hit.peer)?.name ??
                      `${hit.peer.slice(0, 8)}…`;
                    return (
                      <div
                        key={hit.id}
                        className="search-hit"
                        onClick={() => openSearchHit(hit)}
                      >
                        <div className="search-hit-head">
                          <span className="search-hit-peer">{peerName}</span>
                          <span className="search-hit-time">{formatDivider(hit.ts_ms)}</span>
                        </div>
                        <div className="search-hit-body">
                          {hit.outgoing ? "我: " : ""}
                          <Highlighted text={hit.body ?? `<${hit.kind}>`} query={searchQuery} />
                        </div>
                      </div>
                    );
                  })}
                </div>
              </div>
            )}
            {!showSearch && (
              <div className="message-list">
              {currentMessages.length > 0 && (
                <button
                  className="load-earlier"
                  onClick={() => void loadEarlier(selected!)}
                >
                  ↑ 加载更早的消息
                </button>
              )}
              {currentMessages.map((m, i) => {
                const prev = i > 0 ? currentMessages[i - 1] : null;
                const showDivider = shouldShowDivider(prev, m);
                const isMine = m.outgoing;
                // 头像:自己用本机头像;对端用缓存头像;都没有则按 NodeId 配色首字
                const avatarId = isMine ? self?.node_id : m.from_node ?? selected ?? undefined;
                const avatarName = isMine
                  ? self?.name ?? "我"
                  : peers.find((p) => p.node_id === m.from_node)?.name ??
                    selectedPeer?.name ??
                    selectedEntity?.name ??
                    "对方";
                const avatarUrl = isMine ? self?.avatar ?? undefined : avatarId ? avatars[avatarId] : undefined;
                const showSender = Boolean(selectedEntity?.isGroup) && !isMine;

                // 文件/图片消息:特殊气泡
                let fileInfo: { n: string; s: number; p: string; i: boolean } | null = null;
                if (m.kind === "image" || m.kind === "file") {
                  try {
                    fileInfo = JSON.parse(m.body ?? "{}");
                  } catch {
                    fileInfo = null;
                  }
                }

                return (
                  <div key={m.id} id={`msg-${m.id}`}>
                    {showDivider && (
                      <div className="time-divider">{formatDivider(m.ts_ms)}</div>
                    )}

                    {m.kind === "shake" ? (
                      /* ── 窗口抖动提示(参考 whisper:居中一行小字)── */
                      <div className="msg-shake">
                        <span>{isMine ? "👋 你抖了对方一下" : `👋 ${avatarName} 抖了你一下`}</span>
                      </div>
                    ) : (
                      <div
                        className={`msg-row ${isMine ? "mine" : "theirs"}`}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          setMsgMenu({ x: e.clientX, y: e.clientY, msg: m });
                        }}
                      >
                        <AvatarBubble
                          base="msg-avatar"
                          className={isMine ? "mine" : "theirs"}
                          url={avatarUrl}
                          fallback={avatarName}
                          seed={avatarId}
                          title="查看资料"
                          onClick={() => openProfileCard(isMine ? undefined : avatarId)}
                        />
                        <div className="msg-col">
                          {showSender && <div className="msg-sender">{avatarName}</div>}
                          {!isMine && m.mentions && self?.node_id && m.mentions.includes(self.node_id) && (
                            <div className="mention-tag">有人@我</div>
                          )}
                          {fileInfo ? (
                            fileInfo.i ? (
                              /* 图片:缩略图预览 → 点击放大(灯箱) */
                              <div
                                className="msg-image-bubble"
                                onClick={() =>
                                  setLightbox({
                                    path: fileInfo!.p,
                                    name: fileInfo!.n,
                                    title: `${avatarName} · ${formatTime(m.ts_ms)}`,
                                  })
                                }
                                title={`${fileInfo.n}(点击放大)`}
                              >
                                <ImageThumb path={fileInfo.p} name={fileInfo.n} />
                              </div>
                            ) : (
                              /* 文件:文件信息卡片 */
                              <div
                                className="msg-file-bubble"
                                onClick={() => void handleImageClick(fileInfo!.p)}
                                title="点击打开文件"
                              >
                                <div className="file-icon-large">📄</div>
                                <div className="file-info">
                                  <div className="file-name-text">{fileInfo.n}</div>
                                  <div className="file-size-text">{fmtBytes(fileInfo.s)}</div>
                                </div>
                                <div className="file-open">📂</div>
                              </div>
                            )
                          ) : (
                            <div className="msg-bubble" title={formatTime(m.ts_ms)}>
                              {m.reply_to &&
                                (() => {
                                  const quoted = currentMessages.find(
                                    (x) => x.id === m.reply_to,
                                  );
                                  return (
                                    <div
                                      className="quote-block"
                                      title={quoted ? "点击定位原消息" : "原消息不在本地"}
                                      onClick={(e) => {
                                        e.stopPropagation();
                                        if (!quoted) return;
                                        const el = document.getElementById(
                                          `msg-${quoted.id}`,
                                        );
                                        el?.scrollIntoView({
                                          behavior: "smooth",
                                          block: "center",
                                        });
                                        el?.classList.add("flash");
                                        window.setTimeout(
                                          () => el?.classList.remove("flash"),
                                          1200,
                                        );
                                      }}
                                    >
                                      <div className="quote-name">
                                        {quoted ? nameOfMsg(quoted) : "原消息"}
                                      </div>
                                      <div className="quote-text">
                                        {quoted ? quotePreview(quoted) : "原消息不在本地"}
                                      </div>
                                    </div>
                                  );
                                })()}
                              <div className="msg-body">{m.body ?? `<${m.kind}>`}</div>
                              {isMine && (
                                <span
                                  className={`msg-status ${m.status === "pending" ? "pending" : ""}`}
                                  title={
                                    m.status === "pending"
                                      ? "对方离线,消息已入队待补发"
                                      : m.read
                                        ? "已读"
                                        : m.delivered
                                          ? "已送达"
                                          : "已发送"
                                  }
                                >
                                  {m.status === "pending"
                                    ? "🕓"
                                    : m.read
                                      ? "✓✓"
                                      : m.delivered
                                        ? "✓"
                                        : ""}
                                </span>
                              )}
                            </div>
                          )}
                        </div>
                      </div>
                    )}
                  </div>
                );
              })}
              <div ref={listBottomRef} />
            </div>
            )}
            <footer className="composer">
              <div className="composer-toolbar">
                <button
                  className={`tool-btn ${showEmoji ? "active" : ""}`}
                  title="表情"
                  onClick={() => setShowEmoji(!showEmoji)}
                >
                  😊
                </button>
                <button className="tool-btn" title="发送图片" onClick={() => void doSendImage()}>
                  🖼️
                </button>
                <button className="tool-btn" title="发送文件" onClick={doAttach}>
                  📁
                </button>
                <button className="tool-btn" title="截屏发送" onClick={() => void doScreenshot()}>
                  ✂️
                </button>
                {selectedEntity?.isGroup && (
                  <button
                    className={`tool-btn ${showMentions ? "active" : ""}`}
                    title="@ 群成员(被 @ 的人会看到提醒)"
                    onClick={() => setShowMentions(!showMentions)}
                  >
                    @
                  </button>
                )}
              </div>

              {showMentions && selectedEntity?.isGroup && (
                <div className="mention-panel">
                  <div className="mention-title">
                    选择要 @ 的成员
                    <button className="mention-close" onClick={() => setShowMentions(false)}>
                      ×
                    </button>
                  </div>
                  {(groups.find((g) => g.id === selected)?.members ?? []).map((memberId) => {
                    const member = peers.find((p) => p.node_id === memberId);
                    const name = member?.name ?? `${memberId.slice(0, 8)}…`;
                    return (
                      <button
                        key={memberId}
                        className="mention-item"
                        onClick={() => {
                          setDraft((d) => `${d}${d && !d.endsWith(" ") ? " " : ""}@${name} `);
                          setMentionIds((ids) =>
                            ids.includes(memberId) ? ids : [...ids, memberId],
                          );
                          setShowMentions(false);
                        }}
                      >
                        <AvatarBubble
                          url={avatars[memberId]}
                          fallback={name}
                          seed={memberId}
                          size={24}
                          round
                        />
                        <span>{name}</span>
                      </button>
                    );
                  })}
                </div>
              )}

              {showEmoji && (
                <div className="emoji-panel">
                  <div className="emoji-tabs">
                    {EMOJI_GROUPS.map((group, i) => (
                      <button
                        key={group.label}
                        className={`emoji-tab ${emojiTab === i ? "active" : ""}`}
                        onClick={() => setEmojiTab(i)}
                      >
                        {group.label}
                      </button>
                    ))}
                    <button className="emoji-close" onClick={() => setShowEmoji(false)}>
                      ×
                    </button>
                  </div>
                  <div className="emoji-grid">
                    {EMOJI_GROUPS[emojiTab].emojis.map((emoji, i) => (
                      <button
                        key={`${emoji}-${i}`}
                        className="emoji-item"
                        onClick={() => insertEmoji(emoji)}
                      >
                        {emoji}
                      </button>
                    ))}
                  </div>
                </div>
              )}

              {quoting && (
                <div className="quote-bar">
                  <div className="quote-bar-body">
                    <span className="quote-bar-name">{nameOfMsg(quoting)}</span>
                    <span className="quote-bar-text">{quotePreview(quoting)}</span>
                  </div>
                  <button
                    className="quote-bar-close"
                    title="取消引用"
                    onClick={() => setQuoting(null)}
                  >
                    ×
                  </button>
                </div>
              )}

              <div className="composer-input-row">
                <textarea
                  value={draft}
                  placeholder=""
                  onChange={(e) => onDraftChange(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      void doSend();
                    }
                    if (e.key === "Escape") {
                      setShowEmoji(false);
                    }
                  }}
                  onPaste={(e) => {
                    // 剪贴板含图片 → 拦截并发送;纯文本则放行走默认粘贴
                    const items = e.clipboardData?.items ?? [];
                    const hasImage = Array.from(items).some((item) =>
                      item.type.startsWith("image/"),
                    );
                    if (hasImage) {
                      e.preventDefault();
                      void doPasteImage();
                    }
                  }}
                />
                <button
                  className="send-btn"
                  onClick={() => void doSend()}
                  disabled={!draft.trim()}
                >
                  发送
                </button>
              </div>
            </footer>
          </>
        )}
      </main>

      {transferList.length > 0 && transferPanelOpen && (
        <div className="transfer-panel">
          <div className="transfer-title">
            <span>
              文件传输
              <span className="transfer-count">{transferList.length}</span>
            </span>
            <button
              className="transfer-collapse"
              title="收起面板"
              onClick={() => setTransferPanelOpen(false)}
            >
              ▾
            </button>
          </div>
          <div className="transfer-scroll">
            {transferList.map((t) => {
            const pct = t.total > 0 ? Math.round((t.transferred / t.total) * 100) : 0;
            const speed = transferSpeed(t);
            const fileName = t.path.split(/[\\/]/).pop() ?? t.path;
            const status = t.failed
              ? "失败"
              : t.done
                ? "完成 ✓"
                : t.total === 0
                  ? "等待接收方响应…"
                  : `${fmtBytes(t.transferred)} / ${fmtBytes(t.total)} · ${speed.toFixed(1)} MB/s`;
            return (
              <div key={t.token} className={`transfer-item ${t.failed ? "err" : ""}`}>
                <span className="dir">{t.direction === "send" ? "↑" : "↓"}</span>
                <div className="transfer-main">
                  <div className="transfer-name" title={t.path}>
                    {fileName}
                  </div>
                  <div className="bar">
                    <div
                      className={`fill ${t.failed ? "err" : t.done ? "ok" : ""}`}
                      style={{ width: `${t.failed ? 100 : pct}%` }}
                    />
                  </div>
                  <div className="transfer-status">
                    {status}
                    {t.total > 0 && !t.failed && !t.done && <span className="pct">{pct}%</span>}
                  </div>
                </div>
                {t.failed ? (
                  <button
                    className="transfer-close"
                    title="从面板移除"
                    onClick={() => removeTransfer(t.token)}
                  >
                    ×
                  </button>
                ) : t.done ? (
                  <button
                    className="transfer-close"
                    title="从面板移除"
                    onClick={() => removeTransfer(t.token)}
                  >
                    ×
                  </button>
                ) : (
                  <button
                    className="transfer-cancel"
                    title="取消传输"
                    onClick={() => void doCancelTransfer(t.token)}
                  >
                    ✕
                  </button>
                )}
              </div>
            );
          })}
          </div>
        </div>
      )}

      {/* 面板收起后的悬浮小胶囊:显示传输数量,点击重新展开 */}
      {transferList.length > 0 && !transferPanelOpen && (
        <button
          className="transfer-pill"
          title="展开文件传输面板"
          onClick={() => setTransferPanelOpen(true)}
        >
          ⇅ 文件传输
          <span className="transfer-count">{transferList.length}</span>
        </button>
      )}

      {/* ── 传输历史抽屉(图标栏 📁 打开)── */}
      {showTransferHistory && (
        <div className="history-drawer">
          <div className="history-head">
            <span>📁 文件传输记录</span>
            <div className="history-head-actions">
              <button
                className="history-clear"
                title="清空历史(不影响进行中的传输)"
                onClick={() => {
                  void api.clearTransferHistory().then((n) => {
                    refreshTransferHistory();
                    pushToast("info", n > 0 ? `已清空 ${n} 条记录` : "没有可清空的记录");
                  });
                }}
              >
                清空
              </button>
              <button className="history-close" onClick={() => setShowTransferHistory(false)}>
                ×
              </button>
            </div>
          </div>
          <div className="history-list">
            {transferHistory.length === 0 && (
              <div className="empty-hint small">暂无传输记录</div>
            )}
            {transferHistory.map((h) => {
              const statusText: Record<string, string> = {
                active: "传输中",
                done: "已完成",
                failed: "失败",
                cancelled: "已取消",
              };
              const canOpen = h.status === "done" && h.direction === "recv";
              return (
                <div key={h.token} className="history-item">
                  <span className={`history-dir ${h.direction}`}>
                    {h.direction === "send" ? "↑" : "↓"}
                  </span>
                  <div className="history-meta">
                    <div className="history-name" title={h.path}>
                      {h.path.split(/[\\/]/).pop()}
                    </div>
                    <div className="history-sub">
                      <span>{h.peer_name}</span>
                      <span> · {fmtBytes(h.size)}</span>
                      <span> · {formatDivider(h.started_ms)}</span>
                    </div>
                    {h.detail && <div className="history-detail">{h.detail}</div>}
                  </div>
                  <span className={`history-status ${h.status}`}>
                    {statusText[h.status] ?? h.status}
                  </span>
                  <div className="history-actions">
                    {canOpen && (
                      <button
                        className="history-action"
                        title="打开文件"
                        onClick={() =>
                          void api
                            .openFile(h.path)
                            .catch((e) => pushToast("error", `打开失败:${String(e)}`))
                        }
                      >
                        📂
                      </button>
                    )}
                    <button
                      className="history-action"
                      title="删除这条记录"
                      onClick={() => {
                        void api.deleteTransferHistory(h.token).then(refreshTransferHistory);
                      }}
                    >
                      ✕
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      )}

      {/* ── 图片灯箱(点图片放大查看)── */}
      {lightbox && (
        <div className="lightbox-mask" onClick={() => setLightbox(null)}>
          <div className="lightbox-head">
            <span className="lightbox-title">{lightbox.title}</span>
            <div className="lightbox-actions">
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  void api.openFile(lightbox.path);
                }}
              >
                用系统程序打开
              </button>
              <button className="lightbox-close" onClick={() => setLightbox(null)}>
                ×
              </button>
            </div>
          </div>
          <LightboxImage path={lightbox.path} />
          <div className="lightbox-hint">滚轮缩放 · 双击 1x/2x · 拖动查看 · 点击空白关闭</div>
        </div>
      )}

      {/* ── 资料卡(点联系人或消息头像弹出,参考 whisper 的 ProfileCard)── */}
      {profileCard &&
        (() => {
          const peer = profileCard.self
            ? null
            : peers.find((p) => p.node_id === profileCard.nodeId) ?? null;
          const nodeId = profileCard.self ? self?.node_id : profileCard.nodeId;
          const name = profileCard.self
            ? self?.name ?? "我"
            : peer?.name ?? `${shortId(profileCard.nodeId ?? "")}…`;
          const avatarUrl = profileCard.self
            ? self?.avatar ?? undefined
            : nodeId
              ? avatars[nodeId]
              : undefined;
          const group = profileCard.self ? self?.group : peer?.group;
          const ips = profileCard.self ? self?.local_ips ?? [] : peer?.ips ?? [];
          const appVersion = profileCard.self ? currentVersion : peer?.app_version ?? null;
          const online = profileCard.self ? true : peer?.online ?? false;
          return (
            <>
              <div className="pc-mask" onClick={() => setProfileCard(null)} />
              <div className="profile-card">
                <div className="pc-head">
                  <AvatarBubble
                    url={avatarUrl}
                    fallback={name}
                    seed={nodeId}
                    size={56}
                    round
                    className={online ? "" : "offline"}
                  />
                  <div className="pc-meta">
                    <div className="pc-name">{name}</div>
                    <div className={`pc-sub ${online ? "online" : ""}`}>
                      {profileCard.self ? "本机" : online ? "在线" : "离线"}
                      {group ? ` · ${group}` : ""}
                    </div>
                    <div className="pc-sub mono" title={ips.join("\n")}>
                      {ips.length ? ips.join("  ·  ") : "IP 未知"}
                    </div>
                    {appVersion && <div className="pc-sub">版本 v{appVersion}</div>}
                  </div>
                </div>
                <div className="pc-id mono" title={nodeId}>
                  {nodeId}
                </div>
                <div className="pc-actions">
                  {profileCard.self ? (
                    <>
                      <button className="pc-primary" onClick={() => void doChooseAvatar()}>
                        {self?.avatar ? "更换头像" : "上传头像"}
                      </button>
                      {self?.avatar && <button onClick={() => void doClearAvatar()}>移除</button>}
                      <button
                        onClick={() => {
                          setProfileCard(null);
                          openSettings();
                        }}
                      >
                        打开设置
                      </button>
                    </>
                  ) : (
                    <>
                      <button
                        className="pc-primary"
                        onClick={() => {
                          if (nodeId) openPeer(nodeId);
                          setProfileCard(null);
                        }}
                      >
                        发消息
                      </button>
                      <button
                        onClick={() => {
                          if (nodeId) void doSendShake(nodeId);
                          setProfileCard(null);
                        }}
                      >
                        👋 抖一抖
                      </button>
                      <button
                        onClick={() => {
                          void navigator.clipboard.writeText(nodeId ?? "");
                          pushToast("info", "已复制 Node ID");
                        }}
                      >
                        复制 ID
                      </button>
                    </>
                  )}
                </div>
              </div>
            </>
          );
        })()}

      {/* ── 会话右键菜单(置顶 / 免打扰 / 标为已读)── */}
      {convMenu && (
        <>
          <div
            className="context-mask"
            onClick={() => setConvMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setConvMenu(null);
            }}
          />
          <div className="context-menu" style={{ left: convMenu.x, top: convMenu.y }}>
            <div
              className="context-item"
              onClick={() => {
                void doSetConvFlags(convMenu.peer, convMenu.label, { pinned: !convMenu.pinned });
                setConvMenu(null);
              }}
            >
              {convMenu.pinned ? "📍 取消置顶" : "📌 置顶会话"}
            </div>
            <div
              className="context-item"
              onClick={() => {
                void doSetConvFlags(convMenu.peer, convMenu.label, { muted: !convMenu.muted });
                setConvMenu(null);
              }}
            >
              {convMenu.muted ? "🔔 关闭免打扰" : "🔕 消息免打扰"}
            </div>
            <div
              className="context-item"
              onClick={() => {
                void api
                  .markConversationRead(convMenu.peer)
                  .then(() => {
                    setUnread((u) => ({ ...u, [convMenu.peer]: 0 }));
                    refreshConversations();
                  })
                  .catch(() => undefined);
                setConvMenu(null);
              }}
            >
              ✓ 标为已读
            </div>
            <div
              className="context-item danger"
              onClick={() => {
                void doDeleteConversation(convMenu.peer, convMenu.label, 0);
                setConvMenu(null);
              }}
            >
              🗑 从最近会话移除
            </div>
          </div>
        </>
      )}

      {/* ── 消息右键菜单 ── */}
      {msgMenu && (
        <>
          <div
            className="context-mask"
            onClick={() => setMsgMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setMsgMenu(null);
            }}
          />
          <div className="context-menu" style={{ left: msgMenu.x, top: msgMenu.y }}>
            <div
              className="context-item"
              onClick={() => {
                setQuoting(msgMenu.msg);
                setMsgMenu(null);
              }}
            >
              💬 引用回复
            </div>
            {msgMenu.msg.kind === "text" && (
              <div
                className="context-item"
                onClick={() => {
                  void doCopyMessage(msgMenu.msg);
                  setMsgMenu(null);
                }}
              >
                📋 复制文本
              </div>
            )}
            <div
              className="context-item"
              onClick={() => {
                setForwarding(msgMenu.msg);
                setMsgMenu(null);
              }}
            >
              ↗️ 转发到…
            </div>
            {selected && (
              <div
                className="context-item"
                onClick={() => {
                  void doSendShake(selected);
                  setMsgMenu(null);
                }}
              >
                👋 抖一抖
              </div>
            )}
            <div className="context-sep" />
            <div
              className="context-item danger"
              onClick={() => {
                void doDeleteMessage(msgMenu.msg);
                setMsgMenu(null);
              }}
            >
              🗑️ 删除(仅本机)
            </div>
          </div>
        </>
      )}

      {/* ── 转发目标选择 ── */}
      {forwarding && (
        <div className="modal-mask" onClick={() => setForwarding(null)}>
          <div className="modal forward" onClick={(e) => e.stopPropagation()}>
            <h2>↗️ 转发消息</h2>
            <div className="forward-preview">
              {(forwarding.body ?? "").slice(0, 100) || "(非文本消息)"}
            </div>
            <div className="forward-list">
              {groups.map((g) => (
                <div
                  key={g.id}
                  className="forward-item"
                  onClick={() => void doForwardTo(forwarding, g.id)}
                >
                  <div className="avatar group-avatar">群</div>
                  <div className="forward-name">{g.name}</div>
                  <div className="forward-sub">{g.member_count} 位成员</div>
                </div>
              ))}
              {peers.map((p) => (
                <div
                  key={p.node_id}
                  className="forward-item"
                  onClick={() => void doForwardTo(forwarding, p.node_id)}
                >
                  <AvatarBubble
                    url={avatars[p.node_id]}
                    fallback={p.name}
                    seed={p.node_id}
                    className={p.online ? "" : "offline"}
                  />
                  <div className="forward-name">{p.name}</div>
                  <div className="forward-sub">{p.online ? "在线" : "离线(入队)"}</div>
                </div>
              ))}
            </div>
          </div>
        </div>
      )}

      {offers.length > 0 &&
        offers.map((offer) => (
          <div className="modal-mask" key={offer.token}>
            <div className="modal offer">
              <h2>📥 文件接收请求</h2>
              <p className="offer-line">
                <b>{offer.from_name}</b> 要给你发送 <b>{offer.entries}</b> 个文件
                (共 {fmtBytes(offer.total_bytes)})
              </p>
              <div className="offer-dir" title={offerDirChoice[offer.token] ?? self?.download_dir}>
                保存到:{offerDirChoice[offer.token] ?? self?.download_dir ?? "默认目录"}
              </div>
              <div className="offer-actions">
                <button className="offer-secondary" onClick={() => void doPickOfferDir(offer.token)}>
                  选择位置…
                </button>
                <button className="offer-reject" onClick={() => void doRejectOffer(offer.token)}>
                  拒绝
                </button>
                <button className="offer-accept" onClick={() => void doAcceptOffer(offer.token)}>
                  接收
                </button>
              </div>
            </div>
          </div>
        ))}

      {showUpdatePrompt && pendingInstall && (
        <div className="modal-mask" onClick={() => setShowUpdatePrompt(false)}>
          <div className="modal update" onClick={(e) => e.stopPropagation()}>
            <h2>🎉 新版本 v{pendingInstall.version} 已就绪</h2>
            <div className="update-body">
              <div className="update-line">
                安装包已从 <b>{pendingInstall.from_name}</b> 自动获取,并通过 SHA-256 校验。
              </div>
              <div className="update-line dim">
                更新会重启 feiqiu-r;身份密钥、聊天记录与设置全部保留。
              </div>
              <div className="update-path" title={pendingInstall.path}>
                {pendingInstall.path}
              </div>
              {installCountdown !== null && (
                <div className="update-countdown">
                  自动模式:{installCountdown} 秒后自动重启并完成更新
                </div>
              )}
            </div>
            <div className="update-actions">
              <button
                className="update-later"
                onClick={() => {
                  setShowUpdatePrompt(false);
                  setInstallCountdown(null);
                }}
              >
                稍后
              </button>
              <button className="update-now" onClick={() => void doInstallUpdate()}>
                立即重启并更新
              </button>
            </div>
          </div>
        </div>
      )}

      {trustWarning && (
        <div className="modal-mask" onClick={() => setTrustWarning(null)}>
          <div className="modal trust" onClick={(e) => e.stopPropagation()}>
            <h2>⚠️ 信任告警</h2>
            <pre>{trustWarning}</pre>
            <button onClick={() => setTrustWarning(null)}>我已知晓</button>
          </div>
        </div>
      )}

      {/* ── 右键菜单 ── */}
      {contextMenu && (
        <>
          <div className="context-mask" onClick={() => setContextMenu(null)} onContextMenu={(e) => { e.preventDefault(); setContextMenu(null); }} />
          <div
            className="context-menu"
            style={{ left: contextMenu.x, top: contextMenu.y }}
          >
            <div
              className="context-item danger"
              onClick={() => {
                void doRemovePeer(contextMenu.peer);
                setContextMenu(null);
              }}
              title="只从列表移出,聊天记录保留;对方在线时点刷新会自动回来"
            >
              🗑️ 从列表移出
            </div>
            <div className="context-sep" />
            <div
              className="context-item"
              onClick={() => {
                void api.markRead(contextMenu.peer.node_id);
                setContextMenu(null);
              }}
            >
              ✓ 标为已读
            </div>
            <div
              className="context-item"
              onClick={() => {
                openPeer(contextMenu.peer.node_id);
                setContextMenu(null);
              }}
            >
              💬 发消息
            </div>
          </div>
        </>
      )}

      {showCreateGroup && (
        <div className="modal-mask" onClick={() => setShowCreateGroup(false)}>
          <div className="modal settings" onClick={(e) => e.stopPropagation()}>
            <h2>👥 新建群聊</h2>
            <div className="settings-body">
              <label className="settings-field">
                <span>群名称</span>
                <input
                  type="text"
                  autoFocus
                  value={newGroupName}
                  placeholder="如:项目组"
                  onChange={(e) => setNewGroupName(e.target.value)}
                />
              </label>
              <div className="settings-field">
                <span>选择成员({Object.values(newGroupMembers).filter(Boolean).length} 已选)</span>
                <div className="member-picker">
                  {peers.length === 0 && (
                    <div className="empty-hint small">暂无可选成员(等待发现同伴)</div>
                  )}
                  {peers.map((peer) => (
                    <label key={peer.node_id} className="member-option">
                      <input
                        type="checkbox"
                        checked={newGroupMembers[peer.node_id] ?? false}
                        onChange={(e) =>
                          setNewGroupMembers((m) => ({
                            ...m,
                            [peer.node_id]: e.target.checked,
                          }))
                        }
                      />
                      <span className={`dot ${peer.online ? "on" : "off"}`} />
                      <span className="member-name">{peer.name}</span>
                      <span className="member-ip">
                        {peer.ips[0] ?? shortId(peer.node_id)}
                      </span>
                    </label>
                  ))}
                </div>
              </div>
              <div className="settings-note">
                群聊为局域网扇出:每人各自收到一条带群标识的消息;离线成员的消息会入队,上线后补发。
              </div>
            </div>
            <div className="settings-actions">
              <button className="offer-secondary" onClick={() => setShowCreateGroup(false)}>
                取消
              </button>
              <button className="offer-accept" onClick={() => void doCreateGroup()}>
                创建群聊
              </button>
            </div>
          </div>
        </div>
      )}

      {showSettings && (
        <div className="modal-mask" onClick={() => setShowSettings(false)}>
          <div className="modal wx-settings" onClick={(e) => e.stopPropagation()}>
            {/* ── 左栏:账号 + 导航(微信样式)── */}
            <aside className="wx-settings-side">
              <div className="wx-account">
                <div className="wx-account-avatar">{(self?.name ?? "?").slice(0, 1)}</div>
                <div className="wx-account-meta">
                  <div className="wx-account-name">{self?.name ?? "…"}</div>
                  <div className="wx-account-sub" title={self?.node_id}>
                    {self ? `${self.node_id.slice(0, 12)}…` : ""}
                  </div>
                </div>
              </div>
              <nav className="wx-nav">
                {(
                  [
                    ["general", "通用"],
                    ["profile", "个人资料"],
                    ["files", "文件管理"],
                    ["identity", "身份信息"],
                    ["about", "关于与更新"],
                  ] as const
                ).map(([key, label]) => (
                  <button
                    key={key}
                    className={`wx-nav-item ${settingsTab === key ? "active" : ""}`}
                    onClick={() => {
                      setSettingsTab(key);
                      if (key === "about") void doCheckUpdate();
                    }}
                  >
                    {label}
                  </button>
                ))}
              </nav>
            </aside>

            {/* ── 右栏:分组行 ── */}
            <section className="wx-settings-main">
              <header className="wx-main-head">
                <span>
                  {settingsTab === "general"
                    ? "通用"
                    : settingsTab === "profile"
                      ? "个人资料"
                      : settingsTab === "files"
                        ? "文件管理"
                        : settingsTab === "identity"
                          ? "身份信息"
                          : "关于与更新"}
                </span>
                <button className="wx-close" onClick={() => setShowSettings(false)}>
                  ×
                </button>
              </header>

              {settingsTab === "general" && (
                <div className="wx-rows">
                  <div className="wx-block">
                    <div className="set-label">
                      <span>主题</span>
                      <span className="set-note">跟随系统,或手动指定浅色 / 深色</span>
                    </div>
                    <div className="wx-pills">
                      {(
                        [
                          ["system", "跟随系统"],
                          ["light", "浅色"],
                          ["dark", "深色"],
                        ] as const
                      ).map(([value, label]) => (
                        <button
                          key={value}
                          className={`wx-pill ${(prefs?.theme ?? "system") === value ? "active" : ""}`}
                          onClick={() => void doSetTheme(value)}
                        >
                          {label}
                        </button>
                      ))}
                    </div>
                  </div>

                  <div className="wx-block">
                    <div className="set-label">
                      <span>在线状态</span>
                      <span className="set-note">对方的联系人列表里会显示这个状态(切立即广播)</span>
                    </div>
                    <div className="wx-pills">
                      {(
                        [
                          ["online", "在线"],
                          ["busy", "忙碌"],
                          ["dnd", "勿扰"],
                          ["away", "离开"],
                        ] as const
                      ).map(([value, label]) => (
                        <button
                          key={value}
                          className={`wx-pill ${prefs?.status === value ? "active" : ""}`}
                          onClick={() => void doSetStatus(value)}
                        >
                          {label}
                        </button>
                      ))}
                    </div>
                  </div>

                  <div className="wx-block">
                    <div className="set-label">
                      <span>发现同伴</span>
                      <span className="set-note">
                        广播被交换机/安全软件拦截时,可扫一遍本网段逐个探测(已按 /24 自动执行)
                      </span>
                    </div>
                    <div className="wx-pills">
                      <button
                        className="wx-pill"
                        disabled={scanning}
                        onClick={() => void doScanSubnet()}
                      >
                        {scanning ? "扫描中…" : "扫描本网段"}
                      </button>
                    </div>
                  </div>

                  <div className="wx-block">
                    <div className="set-label">
                      <span>文件传输限速</span>
                      <span className="set-note">
                        只作用于<b>发送</b>方向;接收速度由对端决定
                      </span>
                    </div>
                    <div className="wx-pills">
                      {(
                        [
                          [0, "不限速"],
                          [1048576, "1 MB/s"],
                          [5242880, "5 MB/s"],
                          [10485760, "10 MB/s"],
                        ] as const
                      ).map(([bytes, label]) => (
                        <button
                          key={label}
                          className={`wx-pill ${prefs?.send_limit_bytes === bytes ? "active" : ""}`}
                          onClick={() => void doSetLimit(bytes)}
                        >
                          {label}
                        </button>
                      ))}
                    </div>
                  </div>

                  <div className="wx-block">
                    <div className="set-label">
                      <span>网络发现</span>
                      <span className="set-note">
                        默认通过广播 + 每网卡定向自动发现。若对方搜不到你,通常是 Windows
                        防火墙拦截入站 —— 可一键放行;也可以直接填对方 IP 定向探测。
                      </span>
                    </div>
                    <div className="probe-row">
                      <input
                        className="wx-row-input"
                        placeholder="对方 IP(可带端口,如 192.168.1.8)"
                        value={probeInput}
                        onChange={(e) => setProbeInput(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") void doProbe();
                        }}
                      />
                      <button className="wx-btn-small" onClick={() => void doProbe()}>
                        探测
                      </button>
                      <button
                        className="wx-btn-small"
                        title="需要管理员权限,会弹出 UAC 让确认"
                        onClick={() => void doFirewall()}
                      >
                        放行防火墙
                      </button>
                    </div>
                    {probeHint && <div className="set-note probe-hint">{probeHint}</div>}
                  </div>

                  <div className="wx-block">
                    <div className="set-label">
                      <span>数据与日志</span>
                      <span className="set-note">
                        聊天记录、身份密钥都在数据目录;出问题时日志是唯一线索
                      </span>
                    </div>
                    <div className="probe-row">
                      <button
                        className="wx-btn-small"
                        onClick={() => prefs && void api.openFile(prefs.data_dir)}
                      >
                        打开数据目录
                      </button>
                      <button
                        className="wx-btn-small"
                        onClick={() => prefs && void api.openFile(prefs.log_dir)}
                      >
                        打开日志目录
                      </button>
                    </div>
                    {prefs && <div className="set-note mono path-note">{prefs.data_dir}</div>}
                  </div>

                  <div className="wx-hint">
                    当前端口:UDP(发现)默认 24250,TCP 监听 {prefs?.listen_port ?? "—"}。
                    同机运行两个实例时需在 profile.json 里错开端口。
                  </div>
                </div>
              )}

              {settingsTab === "profile" && (
                <div className="wx-rows">
                  <div className="wx-row avatar-row">
                    <span className="wx-row-label">头像</span>
                    <div className="avatar-editor">
                      <div className="avatar-preview">
                        {self?.avatar ? (
                          <img src={self.avatar} alt="" draggable={false} />
                        ) : (
                          <span>{(self?.name ?? "?").slice(0, 1)}</span>
                        )}
                      </div>
                      <div className="avatar-actions">
                        <button className="wx-btn-small" onClick={() => void doChooseAvatar()}>
                          更换头像
                        </button>
                        {self?.avatar && (
                          <button
                            className="wx-btn-small ghost"
                            onClick={() => void doClearAvatar()}
                          >
                            移除
                          </button>
                        )}
                      </div>
                      <div className="avatar-hint">支持 PNG/JPG,自动裁成 256×256 方图</div>
                    </div>
                  </div>
                  <div className="wx-row">
                    <span className="wx-row-label">昵称</span>
                    <input
                      className="wx-row-input"
                      type="text"
                      value={settingsName}
                      placeholder="别人看到的名字"
                      onChange={(e) => setSettingsName(e.target.value)}
                    />
                  </div>
                  <div className="wx-row">
                    <span className="wx-row-label">分组</span>
                    <input
                      className="wx-row-input"
                      type="text"
                      value={settingsGroup}
                      placeholder="如:研发部"
                      onChange={(e) => setSettingsGroup(e.target.value)}
                    />
                  </div>
                  <div className="wx-hint">
                    保存后立即广播给局域网成员;对方的联系人列表会在几秒内更新你的昵称、分组与头像。
                  </div>
                  <div className="wx-actions">
                    <button className="wx-btn-primary" onClick={() => void doSaveSettings()}>
                      保存
                    </button>
                  </div>
                </div>
              )}

              {settingsTab === "files" && (
                <div className="wx-rows">
                  <div className="wx-row">
                    <span className="wx-row-label">文件默认保存位置</span>
                    <span className="wx-row-value" title={self?.download_dir}>
                      {self?.download_dir}
                    </span>
                    <button
                      className="wx-link"
                      onClick={() => void doChangeDownloadDir()}
                    >
                      更改
                    </button>
                  </div>
                  <div className="wx-hint">
                    收到文件时会弹窗询问(发送方 / 文件数 / 总大小),可当场指定「本次保存位置」或拒绝;
                    此处为默认目录。中断的传输会保留 .part,对方重发时自动从断点续传。
                  </div>
                </div>
              )}

              {settingsTab === "about" && (
                <div className="wx-rows">
                  <div className="about-head">
                    <div className="about-logo">FQ</div>
                    <div className="about-title">
                      <div className="about-name">feiqiu-r</div>
                      <div className="about-version">
                        v{versionReport?.local_version ?? "…"}
                        <span className="about-proto">
                          协议 v{versionReport?.protocol_version ?? "…"}
                        </span>
                      </div>
                      <div className="about-desc">局域网即时通讯(飞秋现代化重构)</div>
                    </div>
                  </div>

                  <div className="wx-row">
                    <span className="wx-row-label">版本状态</span>
                    <span className="wx-row-value" style={{ direction: "ltr" }}>
                      {versionReport
                        ? versionReport.update_available
                          ? `发现更新 v${versionReport.latest_version}`
                          : "已是最新"
                        : "未检查"}
                    </span>
                    <button
                      className="wx-link"
                      disabled={checkingUpdate}
                      onClick={() => void doCheckUpdate()}
                    >
                      {checkingUpdate ? "检查中…" : "检查更新"}
                    </button>
                  </div>

                  {versionReport?.update_available && (
                    <div className="about-update">
                      <div>
                        局域网内的 <b>{versionReport.latest_from}</b> 运行着更新的版本
                        <b> v{versionReport.latest_version}</b>。
                      </div>
                      <button
                        className="about-update-btn"
                        disabled={updateBusy}
                        onClick={() => {
                          const newer = (versionReport?.peers ?? []).filter(
                            (p) => p.relation === "newer",
                          );
                          if (newer.length === 0) return;
                          const best = newer.reduce((a, b) =>
                            versionCompare(b.version, a.version) > 0 ? b : a,
                          );
                          void doRequestUpdate(best.node_id, best.version);
                        }}
                      >
                        {updateBusy ? "获取中…" : "自动获取并安装"}
                      </button>
                    </div>
                  )}

                  {pendingInstall && (
                    <div className="about-update ready">
                      <div>
                        ✅ 更新包 v{pendingInstall.version} 已下载并校验通过
                        <span className="about-update-from">(来自 {pendingInstall.from_name})</span>
                      </div>
                      <button
                        className="about-update-btn"
                        onClick={() => void doInstallUpdate()}
                      >
                        重启完成更新
                      </button>
                    </div>
                  )}

                  <div className="wx-row">
                    <span className="wx-row-label">自动更新</span>
                    <span className="wx-row-value" style={{ direction: "ltr" }}>
                      {autoUpdate ? "发现新版本自动下载并重启" : "发现后先询问(推荐)"}
                    </span>
                    <button
                      className={`fq-switch ${autoUpdate ? "on" : ""}`}
                      role="switch"
                      aria-checked={autoUpdate}
                      onClick={() => {
                        const next = !autoUpdate;
                        setAutoUpdate(next);
                        api
                          .setAutoUpdate(next)
                          .catch((e) => pushToast("error", `保存设置失败:${String(e)}`));
                        if (!next) setInstallCountdown(null);
                      }}
                    >
                      <span className="fq-switch-knob" />
                    </button>
                  </div>

                  {versionReport && versionReport.peers.length > 0 && (
                    <div className="about-peers">
                      <div className="about-peers-title">局域网版本分布</div>
                      {versionReport.peers.map((p) => (
                        <div key={p.node_id} className="about-peer-row">
                          <span className="about-peer-name">{p.name}</span>
                          <span className={`about-peer-ver ${p.relation}`}>
                            v{p.version}
                            {p.relation === "newer"
                              ? " ↑ 更新"
                              : p.relation === "older"
                                ? " (较旧)"
                                : " (相同)"}
                          </span>
                        </div>
                      ))}
                    </div>
                  )}

                  <div className="wx-row">
                    <span className="wx-row-label">更新方式</span>
                    <span className="wx-row-value" style={{ direction: "ltr" }}>
                      局域网 P2P 版本发现(无需更新服务器)
                    </span>
                  </div>

                  <div className="wx-hint">
                    更新采用局域网 P2P:各节点在在线通告里携带版本号,发现新版本后可直接
                    向对方<b>自动获取安装包</b>(Noise 加密传输 + SHA-256 校验),重启即完成更新,
                    无需人工拷贝安装文件。打开上方「自动更新」则全程无需操作。
                  </div>
                </div>
              )}

              {settingsTab === "identity" && (
                <div className="wx-rows">
                  <div className="wx-row">
                    <span className="wx-row-label">安全指纹</span>
                    <span className="wx-row-value mono" title={self?.fingerprint}>
                      {self?.fingerprint}
                    </span>
                    <button
                      className="wx-link"
                      onClick={() => {
                        if (self?.fingerprint) {
                          void navigator.clipboard.writeText(self.fingerprint);
                          pushToast("info", "指纹已复制");
                        }
                      }}
                    >
                      复制
                    </button>
                  </div>
                  <div className="wx-row">
                    <span className="wx-row-label">Node ID</span>
                    <span className="wx-row-value mono" title={self?.node_id}>
                      {self?.node_id}
                    </span>
                    <button
                      className="wx-link"
                      onClick={() => {
                        if (self?.node_id) {
                          void navigator.clipboard.writeText(self.node_id);
                          pushToast("info", "Node ID 已复制");
                        }
                      }}
                    >
                      复制
                    </button>
                  </div>
                  <div className="wx-row">
                    <span className="wx-row-label">本机 IP</span>
                    <span className="wx-row-value mono">
                      {(self?.local_ips ?? []).join("  ·  ")}
                    </span>
                  </div>
                  <div className="wx-hint">
                    指纹用于当面核对身份(防中间人替换密钥);对方可在「接收文件」时与你核对,
                    也可用本机 IP 作为 bootstrap 地址直连。
                  </div>
                </div>
              )}
            </section>
          </div>
        </div>
      )}

      {toasts.length > 0 && (
        <div className="toast-stack">
          {toasts.map((t) => (
            <div key={t.id} className={`toast ${t.kind}`} onClick={() => dismissToast(t.id)} title="点击关闭">
              {t.text}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
