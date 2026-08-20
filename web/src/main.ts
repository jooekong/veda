import { marked } from "marked";
import { renderAdmin } from "./admin";

// ── Auth state ────────────────────────────────────────────────────────────
type Auth = { vk: string };
const STORAGE_KEY = "veda.auth";
const FS_KEY_PREFIX = "veda.fs-key.";

function getAuth(): Auth | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as Auth) : null;
  } catch {
    return null;
  }
}
function setAuth(a: Auth) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(a));
}
function clearAuth() {
  localStorage.removeItem(STORAGE_KEY);
}

function getFsKey(workspaceId: string): string | null {
  try {
    return sessionStorage.getItem(`${FS_KEY_PREFIX}${workspaceId}`);
  } catch {
    return null;
  }
}
function setFsKey(workspaceId: string, key: string) {
  try {
    sessionStorage.setItem(`${FS_KEY_PREFIX}${workspaceId}`, key);
  } catch {
    // The file page will ask for the key again when session storage is unavailable.
  }
}
function clearFsKey(workspaceId: string) {
  try {
    sessionStorage.removeItem(`${FS_KEY_PREFIX}${workspaceId}`);
  } catch {
    // Nothing to clear when session storage is unavailable.
  }
}

// Operator identity for the memory page (`X-Veda-Operator`). One per browser
// tab and NOT per workspace — it names the person, who is the same everywhere.
const OPERATOR_KEY = "veda.operator";
function getOperator(): string | null {
  try {
    return sessionStorage.getItem(OPERATOR_KEY);
  } catch {
    return null;
  }
}
function setOperator(op: string) {
  try {
    if (op) sessionStorage.setItem(OPERATOR_KEY, op);
    else sessionStorage.removeItem(OPERATOR_KEY);
  } catch {
    // The identity bar simply stays empty when session storage is unavailable.
  }
}

// ── Language state ────────────────────────────────────────────────────────
type Lang = "zh" | "en";
const LANG_KEY = "veda.lang";

function getLang(): Lang {
  const v = localStorage.getItem(LANG_KEY);
  return v === "en" ? "en" : "zh"; // zh is default
}
function setLang(l: Lang) {
  localStorage.setItem(LANG_KEY, l);
}

// ── i18n strings ──────────────────────────────────────────────────────────
const S = {
  zh: {
    tagline: "可编程的知识存储。",
    subtitle: "文件 · 向量搜索 · SQL —— 一个 API。",
    welcomeBack: "你的账号已经在这台浏览器里。",
    goConsole: "进入 Console →",
    getStarted: "匿名开始",
    getStartedHint: "立即创建匿名账号 + workspace，无需注册。",
    creating: "创建中…",
    failed: "失败：",
    youreIn: "已注册成功",
    credIntro: "这是你的凭据。workspace key 只显示这一次，请立即保存。",
    accountKey: "账号 key (vk_)",
    accountKeyHint: "CLI 用它管理 workspace 和 key。保存在本浏览器。",
    workspaceKey: "workspace key (wk_)",
    workspaceKeyHint: "⚠ 仅显示一次。CLI / FUSE 用它做数据面操作。",
    workspaceIdLabel: "workspace id",
    nextSteps: "下一步",
    step1: "1. 安装 CLI",
    step2: "2. 连接（粘贴账号 key）",
    step3: "3. 上传第一个文件",
    manageWs: "管理 workspace →",
    readDocs: "阅读文档 →",
    noAccountHere: "这台浏览器没有账号。",
    getStartedArrow: "去注册 →",
    loading: "加载中…",
    keyInvalid: "你的账号 key 失效了。",
    getNew: "重新申请 →",
    errorPrefix: "错误：",
    consoleTitle: "Console",
    serverLabel: "服务器：",
    forgetKeys: "清除本浏览器的 key",
    workspaces: "Workspaces",
    newWorkspace: "+ 新建 workspace",
    accountSection: "账号",
    yourAccountKey: "你的账号 key (vk_)",
    bearerHint: "/v1/accounts 与 /v1/workspaces 的 Bearer token",
    claimBtn: "升级账号（加邮箱密码）",
    forgetConfirm: "清除本浏览器的 key？账号还在服务器上 —— 重新导入 vk_ 即可恢复。",
    noWs: "还没有 workspace，去上面创建一个。",
    btnNewKey: "+ Key",
    btnDelete: "删除",
    newWsTitle: "新建 workspace",
    wsNamePlaceholder: "workspace 名称（如 notes）",
    cancel: "取消",
    create: "创建",
    newKeyTitle: "新建 workspace key",
    keyNamePlaceholder: "key 名称",
    permRW: "读写",
    permR: "只读",
    keyCreated: "Workspace key 已创建",
    keyOnce: "⚠ 此 key 仅显示一次，请立即复制。",
    keyLabel: "workspace key",
    mountCmdLabel: "挂载到本地目录",
    mountCmdHint: "复制到终端执行。",
    done: "完成",
    deleteConfirm: "删除这个 workspace 和它所有数据？操作不可恢复。",
    claimTitle: "升级账号",
    claimIntro: "把当前匿名账号升级为邮箱 + 密码。现有 key 继续可用。",
    emailPlaceholder: "邮箱",
    passwordPlaceholder: "密码",
    displayNamePlaceholder: "显示名（可选）",
    claim: "升级",
    claimedAlert: (email: string) => `升级成功。现在可以在其他机器用 ${email} 登录。`,
    wsType: "类型",
    kindFile: "文件库",
    kindVector: "向量库",
    kindFileHint: "文件 + 语义搜索，CLI / FUSE 接入",
    kindVectorHint: "向量记录 + 检索，REST API 接入",
    badgeFile: "文件库",
    badgeVector: "向量库",
    btnFiles: "文件",
    btnDatasets: "数据集",
    btnApiDocs: "API 文档",
    vectorHint: "向量库用账号 key (vk_) 直接调 REST API；wk_ / JWT / FUSE 不适用于向量库。",
    datasetsTitle: "数据集",
    datasetsEmpty: "还没有额外数据集（只有默认的 default）。",
    datasetsLoadFail: "加载数据集失败：",
    btnKeys: "Keys",
    keysTitle: "Workspace Keys",
    keysEmpty: "还没有 key，点下面新建一个。",
    deleteKeyConfirm: "删除（吊销）这个 key？正在用它的客户端会立即失效。",
    wsDescPlaceholder: "描述（可选）",
    filesBack: "← 返回 Console",
    filesKeyTitle: "打开文件库",
    filesKeyHint: "粘贴这个 workspace 的 wk_。它只保存在当前浏览器标签页，用于直接请求 Veda。",
    filesKeyPlaceholder: "wk_...",
    filesOpen: "打开文件库",
    filesChangeKey: "更换 key",
    filesUpload: "上传文件",
    filesUploading: "上传中…",
    filesUploadHint: "单个文件最多 50 MB。文本和二进制文件都会原样上传。",
    filesNewDir: "新建目录",
    filesNewDirPlaceholder: "目录名，如 docs 或 docs/图片",
    filesEmpty: "空目录。",
    filesDownload: "下载",
    filesDirectory: "目录",
    filesNotFound: "这个 workspace 不存在，或不是文件库。",
    filesTooLarge: "文件超过 50 MB 限制。",
    btnMemory: "记忆",
    memTabTeam: "团队记忆",
    memTabDept: "部门记忆",
    memTabMine: "我的记忆",
    memIdentityLabel: "我的身份",
    memIdentityPlaceholder: "wecom:企微id 或 emp:工号",
    memIdentityHint: "「部门/我的」页签需要；只存在本浏览器标签页",
    memIdentityApply: "确定",
    memNeedIdentity: "先在上方填写身份，这个页签才可用。",
    memTopics: "主题",
    memTopicAll: "全部",
    memUncategorized: "未分类",
    memSearchPlaceholder: "搜索当前页签…",
    memSearchBtn: "搜索",
    memSearchResults: "搜索结果",
    memClearSearch: "× 清除",
    memAdd: "+ 添一条",
    memAddPlaceholder: "一句话一条事实，能独立看懂",
    memTopicPlaceholder: "主题（可选）",
    memSave: "保存",
    memSaved: "已保存。",
    memDup: "相同内容已存在，未新增。",
    memNeighborsHint: "已有相似记忆——考虑改旧条，别堆重复：",
    memEdit: "改",
    memDelete: "删",
    memDeleteConfirm: "删除这条记忆？不可恢复。",
    memEmpty: "这里还没有记忆。",
    memCount: (n: number) => `共 ${n} 条`,
    memPrev: "上一页",
    memNext: "下一页",
    memPortableGroup: "随身（跨项目生效）",
    memPinnedGroup: "本项目",
    memExpiry: "到期日（可选）",
    memExpiresChip: "到期",
  },
  en: {
    tagline: "A programmable knowledge store.",
    subtitle: "Files · Vector search · SQL — one API.",
    welcomeBack: "You already have an account in this browser.",
    goConsole: "Go to console →",
    getStarted: "Get started anonymously",
    getStartedHint: "Mints a fresh account + workspace. No signup required.",
    creating: "Creating…",
    failed: "Failed: ",
    youreIn: "You're in",
    credIntro: "These are your credentials. Save the workspace key — it's only shown here once.",
    accountKey: "Account key (vk_)",
    accountKeyHint: "Used by CLI to manage workspaces and keys. Stored in this browser.",
    workspaceKey: "Workspace key (wk_)",
    workspaceKeyHint: "⚠ One-time display. Used by CLI / FUSE for data-plane calls.",
    workspaceIdLabel: "Workspace id",
    nextSteps: "Next steps",
    step1: "1. Install the CLI",
    step2: "2. Connect (paste your account key)",
    step3: "3. Upload your first file",
    manageWs: "Manage workspaces →",
    readDocs: "Read the docs →",
    noAccountHere: "You don't have an account in this browser.",
    getStartedArrow: "Get started →",
    loading: "Loading…",
    keyInvalid: "Your account key is no longer valid.",
    getNew: "Get a new one →",
    errorPrefix: "Error: ",
    consoleTitle: "Console",
    serverLabel: "Server: ",
    forgetKeys: "Forget my keys in this browser",
    workspaces: "Workspaces",
    newWorkspace: "+ New workspace",
    accountSection: "Account",
    yourAccountKey: "Your account key (vk_)",
    bearerHint: "Bearer token for /v1/accounts and /v1/workspaces",
    claimBtn: "Claim account (add email + password)",
    forgetConfirm: "Forget your keys from this browser? Your account stays on the server — you can re-import the vk_ to come back.",
    noWs: "No workspaces yet. Create one above.",
    btnNewKey: "+ Key",
    btnDelete: "Delete",
    newWsTitle: "New workspace",
    wsNamePlaceholder: "workspace name (e.g. notes)",
    cancel: "Cancel",
    create: "Create",
    newKeyTitle: "New workspace key",
    keyNamePlaceholder: "key name",
    permRW: "Read & write",
    permR: "Read only",
    keyCreated: "Workspace key created",
    keyOnce: "⚠ This key is shown only once. Copy it now.",
    keyLabel: "Workspace key",
    mountCmdLabel: "Mount as a local directory",
    mountCmdHint: "Paste into a terminal.",
    done: "Done",
    deleteConfirm: "Delete this workspace and all its data? This cannot be undone.",
    claimTitle: "Claim account",
    claimIntro: "Upgrade this anonymous account to email + password. Your existing keys keep working.",
    emailPlaceholder: "email",
    passwordPlaceholder: "password",
    displayNamePlaceholder: "display name (optional)",
    claim: "Claim",
    claimedAlert: (email: string) => `Claimed. You can now log in with ${email} from another machine.`,
    wsType: "Type",
    kindFile: "File Workspace",
    kindVector: "Vector Workspace",
    kindFileHint: "Files + semantic search, via CLI / FUSE",
    kindVectorHint: "Vector records + retrieval, via REST API",
    badgeFile: "File",
    badgeVector: "Vector",
    btnFiles: "Files",
    btnDatasets: "Datasets",
    btnApiDocs: "API docs",
    vectorHint: "Vector Workspaces use the account key (vk_) with the REST API directly; wk_ / JWT / FUSE don't apply.",
    datasetsTitle: "Datasets",
    datasetsEmpty: "No extra datasets yet (only the bootstrapped default).",
    datasetsLoadFail: "Failed to load datasets: ",
    btnKeys: "Keys",
    keysTitle: "Workspace Keys",
    keysEmpty: "No keys yet — create one below.",
    deleteKeyConfirm: "Delete (revoke) this key? Clients using it stop working immediately.",
    wsDescPlaceholder: "description (optional)",
    filesBack: "← Back to Console",
    filesKeyTitle: "Open file workspace",
    filesKeyHint: "Paste this workspace's wk_. It stays only in this browser tab and is sent directly to Veda.",
    filesKeyPlaceholder: "wk_...",
    filesOpen: "Open files",
    filesChangeKey: "Change key",
    filesUpload: "Upload file",
    filesUploading: "Uploading…",
    filesUploadHint: "Up to 50 MB per file. Text and binary files are uploaded unchanged.",
    filesNewDir: "New folder",
    filesNewDirPlaceholder: "folder name, e.g. docs or docs/images",
    filesEmpty: "This directory is empty.",
    filesDownload: "Download",
    filesDirectory: "Directory",
    filesNotFound: "This workspace does not exist or is not a file workspace.",
    filesTooLarge: "This file exceeds the 50 MB limit.",
    btnMemory: "Memory",
    memTabTeam: "Team",
    memTabDept: "Department",
    memTabMine: "Mine",
    memIdentityLabel: "My identity",
    memIdentityPlaceholder: "wecom:<userid> or emp:<number>",
    memIdentityHint: "Needed for the Department/Mine tabs; kept in this browser tab only",
    memIdentityApply: "Apply",
    memNeedIdentity: "Fill in your identity above to use this tab.",
    memTopics: "Topics",
    memTopicAll: "All",
    memUncategorized: "Uncategorized",
    memSearchPlaceholder: "Search this tab…",
    memSearchBtn: "Search",
    memSearchResults: "Search results",
    memClearSearch: "× Clear",
    memAdd: "+ Add one",
    memAddPlaceholder: "One self-contained fact per memory",
    memTopicPlaceholder: "Topic (optional)",
    memSave: "Save",
    memSaved: "Saved.",
    memDup: "An identical memory already exists; nothing was added.",
    memNeighborsHint: "Similar memories exist — consider updating one instead of piling duplicates:",
    memEdit: "Edit",
    memDelete: "Del",
    memDeleteConfirm: "Delete this memory? This cannot be undone.",
    memEmpty: "No memories here yet.",
    memCount: (n: number) => `${n} total`,
    memPrev: "Prev",
    memNext: "Next",
    memPortableGroup: "Portable (all projects)",
    memPinnedGroup: "This project",
    memExpiry: "Expiry date (optional)",
    memExpiresChip: "expires",
  },
} as const;

function t(): typeof S.zh {
  return S[getLang()] as typeof S.zh;
}

// ── API client ────────────────────────────────────────────────────────────
type ApiResponse<T> = { success: boolean; data?: T; error?: string };

async function api<T = any>(
  path: string,
  opts: RequestInit = {},
  key?: string,
): Promise<T> {
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...((opts.headers as Record<string, string>) || {}),
  };
  if (key) headers["Authorization"] = `Bearer ${key}`;
  const res = await fetch(path, { ...opts, headers });
  const body = (await res.json()) as ApiResponse<T>;
  if (!res.ok || body.success === false) {
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return body.data as T;
}

const accounts = {
  anonymous: () =>
    api<{
      account_id: string;
      api_key: string;
      workspace_id: string;
      workspace_key: string;
    }>("/v1/accounts/anonymous", { method: "POST" }),
  login: (email: string, password: string) =>
    api<{ account_id: string; api_key: string }>("/v1/accounts/login", {
      method: "POST",
      body: JSON.stringify({ email, password }),
    }),
  claim: (vk: string, email: string, password: string, name?: string) =>
    api<{ account_id: string }>(
      "/v1/accounts/claim",
      {
        method: "POST",
        body: JSON.stringify({ email, password, name }),
      },
      vk,
    ),
};

type Workspace = {
  id: string;
  name: string;
  account_id: string;
  status: string;
  kind: string; // "fs" | "db"
  description?: string | null;
  created_at: string;
};

type WorkspaceKeyInfo = {
  id: string;
  name: string;
  permission: string;
  status: string;
  created_at: string;
};

type Dataset = {
  id: string;
  name: string;
  status: string;
  created_at: string;
};

// GET list endpoints return a cursor-paginated envelope. The console shows
// only the first page — workspace / dataset counts are small in alpha.
type Page<T> = { items: T[]; has_more: boolean; next_cursor?: string };

const workspaces = {
  list: (vk: string) =>
    api<Page<Workspace>>("/v1/workspaces", {}, vk).then((p) => p.items),
  create: (vk: string, name: string, kind: string, description: string) =>
    api<Workspace>(
      "/v1/workspaces",
      {
        method: "POST",
        body: JSON.stringify({ name, kind, description: description || null }),
      },
      vk,
    ),
  remove: (vk: string, id: string) =>
    api<void>(`/v1/workspaces/${id}`, { method: "DELETE" }, vk),
  createKey: (vk: string, id: string, name: string, permission: string) =>
    api<{ key: string; permission: string }>(
      `/v1/workspaces/${id}/keys`,
      { method: "POST", body: JSON.stringify({ name, permission }) },
      vk,
    ),
  listKeys: (vk: string, id: string) =>
    api<WorkspaceKeyInfo[]>(`/v1/workspaces/${id}/keys`, {}, vk),
  deleteKey: (vk: string, id: string, keyId: string) =>
    api<void>(`/v1/workspaces/${id}/keys/${keyId}`, { method: "DELETE" }, vk),
};

const datasetsApi = {
  list: (vk: string, wsId: string) =>
    api<Page<Dataset>>(`/v1/workspaces/${wsId}/datasets`, {}, vk).then(
      (p) => p.items,
    ),
};

// ── Memory API (browse page, docs/plans/agent-memory-m4a.md) ─────────────
type MemoryItem = {
  id: number;
  scope: string;
  origin_workspace_id?: string | null;
  topic?: string | null;
  kind: string;
  content: string;
  created_by: string;
  created_at: string;
  updated_by: string;
  updated_at: string;
  expires_at?: string | null;
  score?: number | null;
};
type MemoryPage = { items: MemoryItem[]; total: number; page: number; size: number };
type MemoryTopics = { topics: { topic: string | null; count: number }[] };

function opHeaders(): Record<string, string> {
  const op = getOperator();
  return op ? { "X-Veda-Operator": op } : {};
}

const memoryApi = {
  list: (wk: string, tab: string, topic: string | null, page: number) => {
    let q = `tab=${tab}&page=${page}&size=50`;
    if (topic !== null) q += `&topic=${encodeURIComponent(topic)}`;
    return api<MemoryPage>(`/v1/memory/list?${q}`, { headers: opHeaders() }, wk);
  },
  topics: (wk: string, tab: string) =>
    api<MemoryTopics>(`/v1/memory/topics?tab=${tab}`, { headers: opHeaders() }, wk),
  search: (wk: string, tab: string, query: string) =>
    api<{ items: MemoryItem[] }>(
      `/v1/memory/search?query=${encodeURIComponent(query)}&scope=${tab}`,
      { headers: opHeaders() },
      wk,
    ),
  save: (wk: string, tab: string, content: string, topic: string, expiresAt?: string) =>
    api<{ memory: MemoryItem; duplicate: boolean; neighbors: MemoryItem[] }>(
      "/v1/memory",
      {
        method: "POST",
        headers: opHeaders(),
        body: JSON.stringify({ content, scope: tab, topic: topic || null, expires_at: expiresAt }),
      },
      wk,
    ),
  update: (wk: string, id: number, patch: { content?: string; topic?: string; expires_at?: string }) =>
    api<MemoryItem>(
      `/v1/memory/${id}`,
      { method: "PATCH", headers: opHeaders(), body: JSON.stringify(patch) },
      wk,
    ),
  remove: (wk: string, id: number) =>
    api<unknown>(`/v1/memory/${id}`, { method: "DELETE", headers: opHeaders() }, wk),
};

type FsDirEntry = {
  name: string;
  path: string;
  is_dir: boolean;
  size_bytes: number | null;
  mime_type: string | null;
  updated_at: string;
};

type FsWriteResponse = {
  file_id: string;
  revision: number;
  content_unchanged: boolean;
};

const MAX_FILE_BYTES = 50 * 1024 * 1024;

function fsEndpoint(path: string): string {
  const encoded = path
    .split("/")
    .filter(Boolean)
    .map((part) => encodeURIComponent(part))
    .join("/");
  return encoded ? `/v1/fs/${encoded}` : "/v1/fs";
}

async function fsError(res: Response): Promise<string> {
  const body = (await res.json().catch(() => null)) as ApiResponse<unknown> | null;
  return body?.error || `HTTP ${res.status}`;
}

async function fsFetch(path: string, key: string, opts: RequestInit = {}): Promise<Response> {
  const headers = new Headers(opts.headers);
  headers.set("Authorization", `Bearer ${key}`);
  const res = await fetch(path, { ...opts, headers });
  if (!res.ok) throw new Error(await fsError(res));
  return res;
}

async function fsJson<T>(path: string, key: string, opts: RequestInit = {}): Promise<T> {
  const res = await fsFetch(path, key, opts);
  const body = (await res.json().catch(() => null)) as ApiResponse<T> | null;
  if (!body?.success) throw new Error(body?.error || "invalid server response");
  return body.data as T;
}

// ── Helpers ────────────────────────────────────────────────────────────────
function esc(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}
function attr(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;");
}

function fmtBytes(n: number | null): string {
  if (n == null) return "—";
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = n / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index++;
  }
  return `${value.toFixed(1)} ${units[index]}`;
}

// Raw mime strings are unreadable in a file listing (the OOXML one is 70+
// chars); map the known families to short labels, fall back to the raw mime.
// x-ole-storage stays raw: post-normalization it means "OLE but not Word"
// (xls/ppt/msi), which we can't name more precisely.
function fmtMime(mime: string): string {
  if (mime === "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    || mime === "application/msword") return "Word";
  if (mime === "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    || mime === "application/vnd.ms-excel") return "Excel";
  if (mime === "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    || mime === "application/vnd.ms-powerpoint") return "PPT";
  if (mime === "application/pdf") return "PDF";
  if (mime.startsWith("text/")) return "文本";
  if (mime.startsWith("image/")) return "图片";
  if (mime.startsWith("audio/")) return "音频";
  if (mime.startsWith("video/")) return "视频";
  return mime;
}

function fmtTime(s: string): string {
  const date = new Date(s);
  return Number.isNaN(date.getTime())
    ? s
    : date.toLocaleString(getLang() === "zh" ? "zh-CN" : "en-US", { hour12: false });
}

function kv(label: string, val: string, hint = ""): string {
  return `
    <div class="bg-white border border-slate-200 rounded-lg p-4">
      <div class="flex items-center justify-between mb-1">
        <span class="text-xs uppercase tracking-wide text-slate-500 font-semibold">${esc(label)}</span>
        <button data-copy="${attr(val)}" class="text-xs bg-slate-100 hover:bg-slate-200 px-2 py-1 rounded font-medium">Copy</button>
      </div>
      <code class="block text-sm font-mono break-all text-slate-800">${esc(val)}</code>
      ${hint ? `<p class="text-xs text-slate-500 mt-1.5">${esc(hint)}</p>` : ""}
    </div>
  `;
}

function codeblock(text: string): string {
  return `
    <div class="relative">
      <pre class="bg-slate-900 text-slate-100 p-4 rounded-lg overflow-x-auto text-sm font-mono">${esc(text)}</pre>
      <button data-copy="${attr(text)}" class="absolute top-2 right-2 text-xs bg-slate-700 hover:bg-slate-600 text-slate-100 px-2 py-1 rounded">Copy</button>
    </div>
  `;
}

// Copy-button event delegation (single listener for whole app)
document.addEventListener("click", (e) => {
  const t = e.target as HTMLElement;
  const v = t.dataset?.copy;
  if (!v) return;
  navigator.clipboard.writeText(v);
  const orig = t.textContent || "";
  t.textContent = "Copied!";
  setTimeout(() => {
    t.textContent = orig;
  }, 1200);
});

// ── Modals ────────────────────────────────────────────────────────────────
function modal(title: string, body: string): void {
  const root = document.getElementById("modal-root")!;
  root.innerHTML = `
    <div class="fixed inset-0 bg-black/40 flex items-center justify-center z-10 p-4">
      <div class="bg-white rounded-lg shadow-xl max-w-lg w-full p-6">
        <h2 class="text-lg font-semibold mb-4">${esc(title)}</h2>
        ${body}
      </div>
    </div>
  `;
}
function closeModal() {
  const root = document.getElementById("modal-root");
  if (root) root.innerHTML = "";
}

// ── Router ────────────────────────────────────────────────────────────────
function currentRoute(): string {
  return location.hash.replace(/^#/, "") || "/";
}

async function render() {
  const app = document.getElementById("app")!;
  // Ensure modal-root exists for child pages
  if (!document.getElementById("modal-root")) {
    const m = document.createElement("div");
    m.id = "modal-root";
    document.body.appendChild(m);
  }
  const r = currentRoute();
  const fsRoute = r.match(/^\/console\/fs\/([^/]+)$/);
  const memRoute = r.match(/^\/console\/memory\/([^/]+)$/);
  if (fsRoute) await renderFsWorkspace(app, decodeURIComponent(fsRoute[1]));
  else if (memRoute) await renderMemoryPage(app, decodeURIComponent(memRoute[1]));
  else if (r.startsWith("/console")) await renderConsole(app);
  else if (r.startsWith("/docs")) await renderDocs(app);
  else if (r.startsWith("/admin")) await renderAdmin(app);
  else await renderLanding(app);
}

window.addEventListener("hashchange", render);
window.addEventListener("DOMContentLoaded", render);

// ── Landing ───────────────────────────────────────────────────────────────
async function renderLanding(app: HTMLElement) {
  const a = getAuth();
  const L = t();
  app.innerHTML = `
    <section class="text-center py-12">
      <h1 class="text-5xl font-bold mb-4 tracking-tight">Veda</h1>
      <p class="text-lg text-slate-600 mb-2">${esc(L.tagline)}</p>
      <p class="text-sm text-slate-500 mb-8">${esc(L.subtitle)}</p>
      ${
        a
          ? `<div class="space-y-3">
              <p class="text-sm text-slate-600">${esc(L.welcomeBack)}</p>
              <a href="#/console" class="inline-block bg-slate-900 text-white px-6 py-3 rounded-lg font-medium hover:bg-slate-700">${esc(L.goConsole)}</a>
            </div>`
          : `<button id="get-started" class="bg-slate-900 text-white px-6 py-3 rounded-lg font-medium hover:bg-slate-700">${esc(L.getStarted)}</button>
             <p class="text-sm text-slate-500 mt-3">${esc(L.getStartedHint)}</p>`
      }
    </section>
    <section id="onboard-result" class="${a ? "" : "hidden"}"></section>
  `;
  if (!a) {
    document.getElementById("get-started")!.addEventListener("click", async () => {
      const btn = document.getElementById("get-started") as HTMLButtonElement;
      btn.disabled = true;
      btn.textContent = L.creating;
      try {
        const res = await accounts.anonymous();
        setAuth({ vk: res.api_key });
        showOnboarded(res);
      } catch (e: any) {
        alert(L.failed + e.message);
        btn.disabled = false;
        btn.textContent = L.getStarted;
      }
    });
  }
}

function showOnboarded(r: {
  api_key: string;
  workspace_id: string;
  workspace_key: string;
}) {
  const baseUrl = location.origin;
  const L = t();
  setFsKey(r.workspace_id, r.workspace_key);
  const btn = document.getElementById("get-started");
  if (btn?.parentElement) btn.parentElement.classList.add("hidden");
  const sec = document.getElementById("onboard-result")!;
  sec.classList.remove("hidden");
  sec.innerHTML = `
    <h2 class="text-2xl font-bold mb-2">${esc(L.youreIn)}</h2>
    <p class="text-slate-600 mb-6">${esc(L.credIntro)}</p>
    <div class="space-y-3 mb-8">
      ${kv(L.accountKey, r.api_key, L.accountKeyHint)}
      ${kv(L.workspaceKey, r.workspace_key, L.workspaceKeyHint)}
      ${kv(L.workspaceIdLabel, r.workspace_id, "")}
    </div>
    <h3 class="text-lg font-semibold mb-3">${esc(L.nextSteps)}</h3>
    <ol class="space-y-4 list-none">
      <li>
        <p class="text-sm font-medium mb-1.5">${esc(L.step1)}</p>
        ${codeblock(`curl -fsSL ${baseUrl}/install.sh | sh`)}
      </li>
      <li>
        <p class="text-sm font-medium mb-1.5">${esc(L.step2)}</p>
        ${codeblock(`veda init --server ${baseUrl} --import-key ${r.api_key}`)}
      </li>
      <li>
        <p class="text-sm font-medium mb-1.5">${esc(L.step3)}</p>
        ${codeblock(`echo "hello veda" > /tmp/hi.txt
veda cp /tmp/hi.txt /hi.txt
veda ls
veda search "hello"`)}
      </li>
    </ol>
    <p class="mt-8 text-sm">
      <a href="#/console" class="text-blue-600 underline">${esc(L.manageWs)}</a>
      &nbsp;·&nbsp;
      <a href="#/docs" class="text-blue-600 underline">${esc(L.readDocs)}</a>
    </p>
  `;
}

// ── Console ───────────────────────────────────────────────────────────────
async function renderConsole(app: HTMLElement) {
  const a = getAuth();
  const L = t();
  if (!a) {
    app.innerHTML = `<p class="text-slate-600">${esc(L.noAccountHere)} <a href="#/" class="text-blue-600 underline">${esc(L.getStartedArrow)}</a></p>`;
    return;
  }
  app.innerHTML = `<p class="text-slate-500">${esc(L.loading)}</p>`;
  let ws: Workspace[];
  try {
    ws = await workspaces.list(a.vk);
  } catch (e: any) {
    if (/unauthorized/i.test(e.message)) {
      clearAuth();
      app.innerHTML = `<p class="text-slate-600">${esc(L.keyInvalid)} <a href="#/" class="text-blue-600 underline">${esc(L.getNew)}</a></p>`;
      return;
    }
    app.innerHTML = `<p class="text-red-600">${esc(L.errorPrefix + e.message)}</p>`;
    return;
  }

  app.innerHTML = `
    <div class="flex justify-between items-start mb-8">
      <div>
        <h1 class="text-2xl font-bold">${esc(L.consoleTitle)}</h1>
        <p class="text-sm text-slate-500 mt-1">${esc(L.serverLabel)}<code class="text-xs">${esc(location.origin)}</code></p>
      </div>
      <button id="logout" class="text-sm text-slate-500 hover:text-red-600">${esc(L.forgetKeys)}</button>
    </div>

    <section class="mb-10">
      <div class="flex justify-between items-center mb-3">
        <h2 class="text-lg font-semibold">${esc(L.workspaces)}</h2>
        <button id="new-ws" class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.newWorkspace)}</button>
      </div>
      <div id="ws-list" class="space-y-2"></div>
    </section>

    <section>
      <h2 class="text-lg font-semibold mb-3">${esc(L.accountSection)}</h2>
      <div class="space-y-3">
        ${kv(L.yourAccountKey, a.vk, L.bearerHint)}
        <div class="flex gap-2">
          <button id="claim" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.claimBtn)}</button>
        </div>
      </div>
    </section>
  `;

  document.getElementById("logout")!.addEventListener("click", () => {
    if (confirm(L.forgetConfirm)) {
      clearAuth();
      location.hash = "#/";
    }
  });
  document.getElementById("new-ws")!.addEventListener("click", () => createWsModal(a.vk));
  document.getElementById("claim")!.addEventListener("click", () => claimModal(a.vk));
  renderWsList(ws, a.vk);
}

function renderWsList(list: Workspace[], vk: string) {
  const root = document.getElementById("ws-list")!;
  const L = t();
  if (!list.length) {
    root.innerHTML = `<p class="text-sm text-slate-500 p-4 bg-white border border-slate-200 rounded-lg">${esc(L.noWs)}</p>`;
    return;
  }
  root.innerHTML = list
    .map((w) => {
      const isVector = w.kind === "db";
      const badge = isVector
        ? `<span class="text-xs px-1.5 py-0.5 rounded bg-violet-100 text-violet-700 font-medium">${esc(L.badgeVector)}</span>`
        : `<span class="text-xs px-1.5 py-0.5 rounded bg-sky-100 text-sky-700 font-medium">${esc(L.badgeFile)}</span>`;
      // Both fs and db workspaces issue wk_ keys now (the db data plane moved
      // from vk_ to wk_). db adds dataset + API-docs shortcuts on top.
      const keyBtns = `<button data-act="new-key" data-id="${attr(w.id)}" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnNewKey)}</button>
        <button data-act="keys" data-id="${attr(w.id)}" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnKeys)}</button>`;
      const delBtn = `<button data-act="delete" data-id="${attr(w.id)}" class="text-sm border border-red-300 text-red-700 px-3 py-1.5 rounded hover:bg-red-50">${esc(L.btnDelete)}</button>`;
      const actions = isVector
        ? `${keyBtns}
        <button data-act="datasets" data-id="${attr(w.id)}" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnDatasets)}</button>
        <a href="#/docs/vectors" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnApiDocs)}</a>
        ${delBtn}`
        : `${keyBtns}
        <button data-act="files" data-id="${attr(w.id)}" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnFiles)}</button>
        <button data-act="memory" data-id="${attr(w.id)}" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnMemory)}</button>
        ${delBtn}`;
      return `
    <div class="bg-white border border-slate-200 rounded-lg p-4 flex justify-between items-center gap-4">
      <div class="min-w-0">
        <div class="font-medium flex items-center gap-2">${esc(w.name)} ${badge}</div>
        <div class="text-xs text-slate-500 font-mono mt-0.5 truncate">${esc(w.id)}</div>
      </div>
      <div class="flex gap-2 shrink-0">
        ${actions}
      </div>
    </div>
  `;
    })
    .join("");
  root.querySelectorAll("[data-act]").forEach((el) => {
    el.addEventListener("click", (e) => {
      const t = e.currentTarget as HTMLElement;
      const act = t.dataset.act!;
      const id = t.dataset.id!;
      if (act === "new-key") newKeyModal(vk, id);
      else if (act === "keys") keysModal(vk, id);
      else if (act === "datasets") datasetsModal(vk, id);
      else if (act === "files") location.hash = `#/console/fs/${encodeURIComponent(id)}`;
      else if (act === "memory") location.hash = `#/console/memory/${encodeURIComponent(id)}`;
      else if (act === "delete") deleteWs(vk, id);
    });
  });
}

async function renderFsWorkspace(app: HTMLElement, workspaceId: string) {
  const auth = getAuth();
  const L = t();
  if (!auth) {
    app.innerHTML = `<p class="text-slate-600">${esc(L.noAccountHere)} <a href="#/" class="text-blue-600 underline">${esc(L.getStartedArrow)}</a></p>`;
    return;
  }

  app.innerHTML = `<p class="text-slate-500">${esc(L.loading)}</p>`;
  let workspace: Workspace | undefined;
  try {
    workspace = (await workspaces.list(auth.vk)).find((w) => w.id === workspaceId && w.kind === "fs");
  } catch (e: any) {
    app.innerHTML = `<p class="text-red-600">${esc(L.errorPrefix + e.message)}</p>`;
    return;
  }
  if (!workspace) {
    app.innerHTML = `<p class="text-red-600">${esc(L.filesNotFound)}</p>`;
    return;
  }

  const key = getFsKey(workspaceId);
  if (!key) {
    app.innerHTML = `
      <div class="max-w-lg mx-auto mt-10">
        <a href="#/console" class="text-sm text-blue-600 hover:underline">${esc(L.filesBack)}</a>
        <h1 class="text-xl font-bold mt-5 mb-1">${esc(L.filesKeyTitle)}</h1>
        <p class="text-sm text-slate-500 mb-5">${esc(workspace.name)}</p>
        <p class="text-sm text-slate-600 mb-4">${esc(L.filesKeyHint)}</p>
        <form id="fs-key-form">
          <input id="fs-key" type="password" autocomplete="off" placeholder="${attr(L.filesKeyPlaceholder)}"
            class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500">
          <button class="w-full bg-slate-900 text-white px-4 py-2 rounded font-medium hover:bg-slate-700">${esc(L.filesOpen)}</button>
        </form>
      </div>`;
    document.getElementById("fs-key-form")!.addEventListener("submit", (event) => {
      event.preventDefault();
      const entered = (document.getElementById("fs-key") as HTMLInputElement).value.trim();
      if (!entered) return;
      setFsKey(workspaceId, entered);
      render();
    });
    return;
  }

  app.innerHTML = `
    <div class="flex flex-wrap justify-between items-start gap-3 mb-6">
      <div>
        <a href="#/console" class="text-sm text-blue-600 hover:underline">${esc(L.filesBack)}</a>
        <h1 class="text-2xl font-bold mt-3">${esc(workspace.name)}</h1>
        <p class="text-sm text-slate-500 mt-1">${esc(L.kindFile)}</p>
      </div>
      <button id="fs-change-key" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.filesChangeKey)}</button>
    </div>
    <section class="bg-white border border-slate-200 rounded-lg overflow-x-auto">
      <div id="fs-browser" class="text-sm"></div>
    </section>`;
  document.getElementById("fs-change-key")!.addEventListener("click", () => {
    clearFsKey(workspaceId);
    render();
  });
  initFsBrowser(document.getElementById("fs-browser")!, key);
}

function initFsBrowser(root: HTMLElement, key: string) {
  const L = t();

  const download = async (path: string) => {
    try {
      const res = await fsFetch(fsEndpoint(path), key);
      const blob = await res.blob();
      const link = document.createElement("a");
      const objectUrl = URL.createObjectURL(blob);
      link.href = objectUrl;
      link.download = path.split("/").filter(Boolean).pop() || "download";
      document.body.appendChild(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(objectUrl), 0);
    } catch (e: any) {
      const status = root.querySelector("#fs-status");
      if (status) {
        status.textContent = e.message;
        status.className = "text-sm text-red-600";
      }
    }
  };

  const load = async (path: string) => {
    root.innerHTML = `<div class="px-4 py-3 text-slate-500">${esc(L.loading)}</div>`;
    let entries: FsDirEntry[];
    try {
      entries = await fsJson<FsDirEntry[]>(`${fsEndpoint(path)}?list`, key);
    } catch (e: any) {
      root.innerHTML = `<div class="px-4 py-3 text-red-600">${esc(e.message)}</div>`;
      return;
    }

    entries.sort((a, b) =>
      a.is_dir === b.is_dir ? a.name.localeCompare(b.name) : a.is_dir ? -1 : 1,
    );
    const parts = path.split("/").filter(Boolean);
    let current = "";
    const crumbs = [`<button data-fs-dir="/" class="text-blue-600 hover:underline">/</button>`];
    for (const part of parts) {
      current += `/${part}`;
      crumbs.push(
        `<span class="text-slate-300"> / </span><button data-fs-dir="${attr(current)}" class="text-blue-600 hover:underline">${esc(part)}</button>`,
      );
    }
    const rows = entries
      .map((entry) => {
        const meta = entry.is_dir
          ? L.filesDirectory
          : `${fmtBytes(entry.size_bytes)}${entry.mime_type ? ` · ${esc(fmtMime(entry.mime_type))}` : ""}`;
        const name = entry.is_dir
          ? `<button data-fs-dir="${attr(entry.path)}" class="text-blue-600 hover:underline text-left">📁 ${esc(entry.name)}</button>`
          : `📄 ${esc(entry.name)}`;
        const action = entry.is_dir
          ? ""
          : `<button data-fs-download="${attr(entry.path)}" class="text-blue-600 hover:underline">${esc(L.filesDownload)}</button>`;
        return `<tr class="border-t border-slate-100">
          <td class="px-4 py-2">${name}</td>
          <td class="px-4 py-2 text-xs text-slate-500">${meta}</td>
          <td class="px-4 py-2 text-xs text-slate-500 whitespace-nowrap">${esc(fmtTime(entry.updated_at))}</td>
          <td class="px-4 py-2 text-right text-sm">${action}</td>
        </tr>`;
      })
      .join("");
    root.innerHTML = `
      <div class="px-4 py-3 border-b border-slate-100 flex flex-wrap items-center justify-between gap-3">
        <div class="text-sm">${crumbs.join("")}</div>
        <div class="flex flex-wrap items-center gap-2">
          <input id="fs-upload-input" type="file" class="hidden">
          <button id="fs-mkdir" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.filesNewDir)}</button>
          <button id="fs-upload" class="bg-slate-900 text-white px-3 py-1.5 rounded text-sm font-medium hover:bg-slate-700">${esc(L.filesUpload)}</button>
        </div>
      </div>
      <div class="px-4 py-2 text-xs text-slate-500 border-b border-slate-100 flex flex-wrap justify-between gap-2">
        <span>${esc(L.filesUploadHint)}</span><span id="fs-status"></span>
      </div>
      ${
        entries.length
          ? `<table class="w-full text-left text-sm"><tbody>${rows}</tbody></table>`
          : `<div class="px-4 py-4 text-slate-500">${esc(L.filesEmpty)}</div>`
      }`;

    root.querySelectorAll("[data-fs-dir]").forEach((element) => {
      element.addEventListener("click", () => load((element as HTMLElement).dataset.fsDir!));
    });
    root.querySelectorAll("[data-fs-download]").forEach((element) => {
      element.addEventListener("click", () => download((element as HTMLElement).dataset.fsDownload!));
    });
    document.getElementById("fs-mkdir")!.addEventListener("click", () => mkdirModal(path, key, load));
    const input = document.getElementById("fs-upload-input") as HTMLInputElement;
    const status = document.getElementById("fs-status")!;
    const button = document.getElementById("fs-upload") as HTMLButtonElement;
    const upload = async () => {
      const file = input.files?.[0];
      if (!file) {
        return;
      }
      if (file.size > MAX_FILE_BYTES) {
        status.textContent = L.filesTooLarge;
        status.className = "text-sm text-red-600";
        return;
      }
      const target = `${path === "/" ? "" : path}/${file.name}`;
      button.disabled = true;
      button.textContent = L.filesUploading;
      try {
        await fsJson<FsWriteResponse>(fsEndpoint(target), key, { method: "PUT", body: file });
        await load(path);
      } catch (e: any) {
        status.textContent = e.message;
        status.className = "text-sm text-red-600";
        button.disabled = false;
        button.textContent = L.filesUpload;
      }
    };
    button.addEventListener("click", () => {
      input.value = "";
      input.click();
    });
    input.addEventListener("change", () => void upload());
  };

  load("/");
}

/// "New folder" under the current directory. Multi-level names (a/b) are fine
/// — the server's mkdir creates parents. The browser reloads into the same
/// directory so the new folder shows up immediately.
function mkdirModal(path: string, key: string, reload: (p: string) => Promise<void>) {
  const L = t();
  modal(
    L.filesNewDir,
    `
    <input id="fs-dir-name" placeholder="${attr(L.filesNewDirPlaceholder)}" class="w-full border border-slate-300 rounded px-3 py-2 mb-4 focus:outline-none focus:border-slate-500">
    <div class="flex justify-end gap-2">
      <button data-close class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.cancel)}</button>
      <button id="fs-dir-create" class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.create)}</button>
    </div>
  `,
  );
  document.querySelector("[data-close]")!.addEventListener("click", closeModal);
  const input = document.getElementById("fs-dir-name") as HTMLInputElement;
  input.focus();
  const create = async () => {
    const name = input.value.trim().replace(/^\/+|\/+$/g, "");
    if (!name) return;
    try {
      await fsJson<unknown>("/v1/fs-mkdir", key, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ path: `${path === "/" ? "" : path}/${name}` }),
      });
      closeModal();
      await reload(path);
    } catch (e: any) {
      alert(L.failed + e.message);
    }
  };
  document.getElementById("fs-dir-create")!.addEventListener("click", () => void create());
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") void create();
  });
}

function createWsModal(vk: string) {
  const L = t();
  modal(
    L.newWsTitle,
    `
    <input id="ws-name" placeholder="${attr(L.wsNamePlaceholder)}" class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500">
    <input id="ws-desc" placeholder="${attr(L.wsDescPlaceholder)}" class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500">
    <p class="text-xs uppercase tracking-wide text-slate-500 font-semibold mb-1.5">${esc(L.wsType)}</p>
    <div class="space-y-2 mb-4">
      <label class="flex items-start gap-2 border border-slate-300 rounded px-3 py-2 cursor-pointer hover:bg-slate-50">
        <input type="radio" name="ws-kind" value="fs" checked class="mt-1">
        <span><span class="font-medium">${esc(L.kindFile)}</span><br><span class="text-xs text-slate-500">${esc(L.kindFileHint)}</span></span>
      </label>
      <label class="flex items-start gap-2 border border-slate-300 rounded px-3 py-2 cursor-pointer hover:bg-slate-50">
        <input type="radio" name="ws-kind" value="db" class="mt-1">
        <span><span class="font-medium">${esc(L.kindVector)}</span><br><span class="text-xs text-slate-500">${esc(L.kindVectorHint)}</span></span>
      </label>
    </div>
    <div class="flex justify-end gap-2">
      <button data-close class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.cancel)}</button>
      <button id="ws-create" class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.create)}</button>
    </div>
  `,
  );
  document.querySelector("[data-close]")!.addEventListener("click", closeModal);
  document.getElementById("ws-create")!.addEventListener("click", async () => {
    const name = (document.getElementById("ws-name") as HTMLInputElement).value.trim();
    if (!name) return;
    const kind = (document.querySelector('input[name="ws-kind"]:checked') as HTMLInputElement).value;
    const description = (document.getElementById("ws-desc") as HTMLInputElement).value.trim();
    try {
      await workspaces.create(vk, name, kind, description);
      closeModal();
      render();
    } catch (e: any) {
      alert(L.failed + e.message);
    }
  });
}

function newKeyModal(vk: string, wsId: string) {
  const L = t();
  modal(
    L.newKeyTitle,
    `
    <input id="key-name" value="cli" class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500" placeholder="${attr(L.keyNamePlaceholder)}">
    <select id="key-perm" class="w-full border border-slate-300 rounded px-3 py-2 mb-4 focus:outline-none focus:border-slate-500">
      <option value="readwrite">${esc(L.permRW)}</option>
      <option value="read">${esc(L.permR)}</option>
    </select>
    <div class="flex justify-end gap-2">
      <button data-close class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.cancel)}</button>
      <button id="key-create" class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.create)}</button>
    </div>
  `,
  );
  document.querySelector("[data-close]")!.addEventListener("click", closeModal);
  document.getElementById("key-create")!.addEventListener("click", async () => {
    const name =
      (document.getElementById("key-name") as HTMLInputElement).value.trim() ||
      "default";
    const perm = (document.getElementById("key-perm") as HTMLSelectElement).value;
    try {
      const res = await workspaces.createKey(vk, wsId, name, perm);
      const mountCmd = `mkdir -p ~/veda && veda-fuse mount --server ${location.origin} --key ${res.key} ~/veda`;
      modal(
        L.keyCreated,
        `
        <p class="text-sm text-amber-700 bg-amber-50 border border-amber-200 rounded p-3 mb-4">
          ${esc(L.keyOnce)}
        </p>
        ${kv(`${L.keyLabel} (${res.permission})`, res.key, "")}
        <div class="mt-4">
          <p class="text-xs uppercase tracking-wide text-slate-500 font-semibold mb-1.5">${esc(L.mountCmdLabel)}</p>
          ${codeblock(mountCmd)}
          <p class="text-xs text-slate-500 mt-1.5">${esc(L.mountCmdHint)}</p>
        </div>
        <div class="flex justify-end mt-4">
          <button data-close class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.done)}</button>
        </div>
      `,
      );
      document.querySelector("[data-close]")!.addEventListener("click", closeModal);
    } catch (e: any) {
      alert(L.failed + e.message);
    }
  });
}

function keysModal(vk: string, wsId: string) {
  const L = t();
  modal(L.keysTitle, `<p class="text-sm text-slate-500">${esc(L.loading)}</p>`);
  workspaces
    .listKeys(vk, wsId)
    .then((keys) => {
      const rows = keys.length
        ? `<div class="space-y-2 mb-4">${keys
            .map(
              (k) => `<div class="flex justify-between items-center bg-slate-50 border border-slate-200 rounded px-3 py-2 gap-2">
            <div class="min-w-0">
              <div class="text-sm font-medium truncate">${esc(k.name)} <span class="text-xs text-slate-400">(${esc(k.permission)})</span></div>
              <div class="text-xs text-slate-400 font-mono truncate">${esc(k.id)} · ${esc(k.status)}</div>
            </div>
            <button data-del-key="${attr(k.id)}" class="text-xs border border-red-300 text-red-700 px-2 py-1 rounded hover:bg-red-50 shrink-0">${esc(L.btnDelete)}</button>
          </div>`,
            )
            .join("")}</div>`
        : `<p class="text-sm text-slate-500 mb-4">${esc(L.keysEmpty)}</p>`;
      modal(
        L.keysTitle,
        `${rows}
        <div class="flex justify-between">
          <button id="keys-new" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnNewKey)}</button>
          <button data-close class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.done)}</button>
        </div>`,
      );
      document.querySelector("[data-close]")!.addEventListener("click", closeModal);
      document
        .getElementById("keys-new")!
        .addEventListener("click", () => newKeyModal(vk, wsId));
      document.querySelectorAll("[data-del-key]").forEach((el) => {
        el.addEventListener("click", async () => {
          const keyId = (el as HTMLElement).dataset.delKey!;
          if (!confirm(L.deleteKeyConfirm)) return;
          try {
            await workspaces.deleteKey(vk, wsId, keyId);
            keysModal(vk, wsId);
          } catch (e: any) {
            alert(L.failed + e.message);
          }
        });
      });
    })
    .catch((e) => {
      modal(
        L.keysTitle,
        `<p class="text-red-600 text-sm">${esc(L.failed + e.message)}</p>
        <div class="flex justify-end mt-4"><button data-close class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.done)}</button></div>`,
      );
      document.querySelector("[data-close]")!.addEventListener("click", closeModal);
    });
}

function datasetsModal(vk: string, wsId: string) {
  const L = t();
  modal(L.datasetsTitle, `<p class="text-sm text-slate-500">${esc(L.loading)}</p>`);
  datasetsApi
    .list(vk, wsId)
    .then((list) => {
      const rows = list.length
        ? `<div class="space-y-2">${list
            .map(
              (d) => `<div class="flex justify-between items-center bg-slate-50 border border-slate-200 rounded px-3 py-2">
            <span class="font-mono text-sm">${esc(d.name)}</span>
            <span class="text-xs text-slate-400">${esc(d.status)}</span>
          </div>`,
            )
            .join("")}</div>`
        : `<p class="text-sm text-slate-500">${esc(L.datasetsEmpty)}</p>`;
      modal(
        L.datasetsTitle,
        `${rows}
        <p class="text-xs text-slate-500 mt-4">${esc(L.vectorHint)}</p>
        <div class="flex justify-end mt-4">
          <button data-close class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.done)}</button>
        </div>`,
      );
      document.querySelector("[data-close]")!.addEventListener("click", closeModal);
    })
    .catch((e) => {
      modal(
        L.datasetsTitle,
        `<p class="text-red-600 text-sm">${esc(L.datasetsLoadFail + e.message)}</p>
        <div class="flex justify-end mt-4">
          <button data-close class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.done)}</button>
        </div>`,
      );
      document.querySelector("[data-close]")!.addEventListener("click", closeModal);
    });
}

async function deleteWs(vk: string, id: string) {
  const L = t();
  if (!confirm(L.deleteConfirm)) return;
  try {
    await workspaces.remove(vk, id);
    render();
  } catch (e: any) {
    alert(L.failed + e.message);
  }
}

function claimModal(vk: string) {
  const L = t();
  modal(
    L.claimTitle,
    `
    <p class="text-sm text-slate-600 mb-4">${esc(L.claimIntro)}</p>
    <input id="claim-email" placeholder="${attr(L.emailPlaceholder)}" type="email" class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500">
    <input id="claim-pw" placeholder="${attr(L.passwordPlaceholder)}" type="password" class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500">
    <input id="claim-name" placeholder="${attr(L.displayNamePlaceholder)}" class="w-full border border-slate-300 rounded px-3 py-2 mb-4 focus:outline-none focus:border-slate-500">
    <div class="flex justify-end gap-2">
      <button data-close class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.cancel)}</button>
      <button id="claim-submit" class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.claim)}</button>
    </div>
  `,
  );
  document.querySelector("[data-close]")!.addEventListener("click", closeModal);
  document.getElementById("claim-submit")!.addEventListener("click", async () => {
    const email = (document.getElementById("claim-email") as HTMLInputElement).value.trim();
    const password = (document.getElementById("claim-pw") as HTMLInputElement).value;
    const name = (document.getElementById("claim-name") as HTMLInputElement).value.trim() || undefined;
    if (!email || !password) return;
    try {
      await accounts.claim(vk, email, password, name);
      closeModal();
      alert(L.claimedAlert(email));
      render();
    } catch (e: any) {
      alert(L.failed + e.message);
    }
  });
}

// ── Docs ──────────────────────────────────────────────────────────────────
const DOCS_META: Record<Lang, { sectionLabel: string; searchPlaceholder: string; searchNoResults: string; items: { id: string; title: string }[]; loadFailed: (m: string) => string }> = {
  zh: {
    sectionLabel: "文档",
    searchPlaceholder: "搜索文档…",
    searchNoResults: "无匹配结果",
    items: [
      { id: "introduction", title: "功能与场景" },
      { id: "quickstart", title: "快速开始" },
      { id: "reference", title: "详细文档" },
      { id: "cli", title: "CLI 速查" },
      { id: "skill", title: "AI 助手集成" },
      { id: "fuse", title: "FUSE 挂载" },
      { id: "vectors", title: "向量库 API" },
      { id: "troubleshooting", title: "常见问题" },
    ],
    loadFailed: (m) => `加载失败：${m}`,
  },
  en: {
    sectionLabel: "Docs",
    searchPlaceholder: "Search docs…",
    searchNoResults: "No matches",
    items: [
      { id: "introduction", title: "Introduction" },
      { id: "quickstart", title: "Quickstart" },
      { id: "reference", title: "Reference" },
      { id: "cli", title: "CLI reference" },
      { id: "skill", title: "AI agent skill" },
      { id: "fuse", title: "FUSE mount" },
      { id: "vectors", title: "Vector Workspace API" },
      { id: "troubleshooting", title: "Troubleshooting" },
    ],
    loadFailed: (m) => `Failed: ${m}`,
  },
};

// Client-side docs search: the corpus is ~8 small markdown files per
// language, lazy-fetched once per session and substring-matched — no index
// library needed at this size.
const docsCorpusCache: Partial<Record<Lang, { id: string; title: string; text: string }[]>> = {};

async function loadDocsCorpus(lang: Lang) {
  if (!docsCorpusCache[lang]) {
    docsCorpusCache[lang] = await Promise.all(
      DOCS_META[lang].items.map(async (d) => {
        try {
          const res = await fetch(`/docs/${lang}/${d.id}.md`);
          return { id: d.id, title: d.title, text: res.ok ? await res.text() : "" };
        } catch {
          return { id: d.id, title: d.title, text: "" };
        }
      }),
    );
  }
  return docsCorpusCache[lang]!;
}

function docsSearchSnippet(text: string, idx: number, qLen: number): string {
  const start = Math.max(0, idx - 30);
  const end = Math.min(text.length, idx + qLen + 60);
  const before = esc((start > 0 ? "…" : "") + text.slice(start, idx).replace(/\s+/g, " "));
  const match = esc(text.slice(idx, idx + qLen));
  const after = esc(text.slice(idx + qLen, end).replace(/\s+/g, " ") + (end < text.length ? "…" : ""));
  return `${before}<mark>${match}</mark>${after}`;
}

function bindDocsSearch(lang: Lang, noResults: string) {
  const input = document.getElementById("docs-search") as HTMLInputElement;
  const results = document.getElementById("docs-search-results")!;
  let timer: number | undefined;
  input.addEventListener("input", () => {
    window.clearTimeout(timer);
    timer = window.setTimeout(async () => {
      const q = input.value.trim();
      if (q.length < 2) {
        results.classList.add("hidden");
        results.innerHTML = "";
        return;
      }
      const corpus = await loadDocsCorpus(lang);
      if (input.value.trim() !== q) return; // stale — a newer keystroke won
      const needle = q.toLowerCase();
      const hits = corpus
        .map((p) => {
          const lower = p.text.toLowerCase();
          const idx = lower.indexOf(needle);
          if (idx < 0) return null;
          return {
            id: p.id,
            title: p.title,
            count: lower.split(needle).length - 1,
            snippet: docsSearchSnippet(p.text, idx, q.length),
          };
        })
        .filter((h): h is NonNullable<typeof h> => h !== null)
        .sort((a, b) => b.count - a.count);
      results.innerHTML = hits.length
        ? hits
            .map(
              (h) => `<a href="#/docs/${h.id}" class="block px-3 py-2 hover:bg-slate-50 border-b border-slate-100 last:border-0">
                <span class="text-sm font-medium text-slate-900">${esc(h.title)}</span>
                <span class="ml-1 text-xs text-slate-400">${h.count}</span>
                <span class="block text-xs text-slate-500 mt-0.5">${h.snippet}</span>
              </a>`,
            )
            .join("")
        : `<p class="px-3 py-2 text-sm text-slate-500">${esc(noResults)}</p>`;
      results.classList.remove("hidden");
    }, 150);
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      input.value = "";
      results.classList.add("hidden");
    }
  });
  // Delay lets a click on a result navigate before the dropdown hides.
  input.addEventListener("blur", () => {
    window.setTimeout(() => results.classList.add("hidden"), 150);
  });
  input.addEventListener("focus", () => {
    if (results.innerHTML) results.classList.remove("hidden");
  });
}

async function renderDocs(app: HTMLElement) {
  const lang = getLang();
  const meta = DOCS_META[lang];
  const id = currentRoute().split("/")[2] || "introduction";
  app.innerHTML = `
    <div class="grid grid-cols-1 md:grid-cols-[200px_1fr] gap-8">
      <aside class="text-sm">
        <p class="text-xs uppercase tracking-wide text-slate-500 font-semibold mb-2">${esc(meta.sectionLabel)}</p>
        <div class="relative mb-3" id="docs-search-box">
          <input id="docs-search" type="search" autocomplete="off" placeholder="${esc(meta.searchPlaceholder)}"
            class="w-full border border-slate-300 rounded px-2 py-1 text-sm focus:outline-none focus:border-slate-500" />
          <div id="docs-search-results" class="absolute z-10 left-0 w-full md:w-72 mt-1 bg-white border border-slate-200 rounded shadow-lg max-h-80 overflow-auto hidden"></div>
        </div>
        ${meta.items
          .map(
            (d) => `<a href="#/docs/${d.id}" class="block py-1 ${
              d.id === id
                ? "text-slate-900 font-semibold"
                : "text-slate-600 hover:text-slate-900"
            }">${esc(d.title)}</a>`,
          )
          .join("")}
      </aside>
      <article id="md" class="prose max-w-none">…</article>
    </div>
  `;
  bindDocsSearch(lang, meta.searchNoResults);
  try {
    const res = await fetch(`/docs/${lang}/${id}.md`);
    if (!res.ok) throw new Error(`doc not found: ${lang}/${id}`);
    const md = await res.text();
    document.getElementById("md")!.innerHTML = await marked.parse(md);
  } catch (e: any) {
    document.getElementById("md")!.innerHTML =
      `<p class="text-red-600">${esc(meta.loadFailed(e.message))}</p>`;
  }
}

// ── Language toggle (top nav) ─────────────────────────────────────────────
function syncLangToggle() {
  const cur = getLang();
  document.querySelectorAll<HTMLElement>("#lang-toggle [data-lang]").forEach((el) => {
    const active = el.dataset.lang === cur;
    el.className =
      "px-1.5 py-0.5 rounded " +
      (active ? "bg-slate-900 text-white" : "text-slate-500 hover:text-slate-900");
  });
}
document.addEventListener("DOMContentLoaded", () => {
  syncLangToggle();
  document.getElementById("lang-toggle")!.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    const l = t.dataset?.lang as Lang | undefined;
    if (!l || l === getLang()) return;
    setLang(l);
    syncLangToggle();
    // Re-render current page so all UI strings flip
    render();
  });
});

// ── Memory browse page (docs/plans/agent-memory-m4a.md §1.3) ──────────────
type MemTab = "team" | "dept" | "mine";

async function renderMemoryPage(app: HTMLElement, workspaceId: string) {
  const auth = getAuth();
  const L = t();
  if (!auth) {
    app.innerHTML = `<p class="text-slate-600">${esc(L.noAccountHere)} <a href="#/" class="text-blue-600 underline">${esc(L.getStartedArrow)}</a></p>`;
    return;
  }

  app.innerHTML = `<p class="text-slate-500">${esc(L.loading)}</p>`;
  let workspace: Workspace | undefined;
  try {
    workspace = (await workspaces.list(auth.vk)).find((w) => w.id === workspaceId && w.kind === "fs");
  } catch (e: any) {
    app.innerHTML = `<p class="text-red-600">${esc(L.errorPrefix + e.message)}</p>`;
    return;
  }
  if (!workspace) {
    app.innerHTML = `<p class="text-red-600">${esc(L.filesNotFound)}</p>`;
    return;
  }

  const key = getFsKey(workspaceId);
  if (!key) {
    // Same per-tab wk_ prompt as the files page — one key opens both.
    app.innerHTML = `
      <div class="max-w-lg mx-auto mt-10">
        <a href="#/console" class="text-sm text-blue-600 hover:underline">${esc(L.filesBack)}</a>
        <h1 class="text-xl font-bold mt-5 mb-1">${esc(L.filesKeyTitle)}</h1>
        <p class="text-sm text-slate-500 mb-5">${esc(workspace.name)} · ${esc(L.btnMemory)}</p>
        <p class="text-sm text-slate-600 mb-4">${esc(L.filesKeyHint)}</p>
        <form id="mem-key-form">
          <input id="mem-key" type="password" autocomplete="off" placeholder="${attr(L.filesKeyPlaceholder)}"
            class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500">
          <button class="w-full bg-slate-900 text-white px-4 py-2 rounded font-medium hover:bg-slate-700">${esc(L.filesOpen)}</button>
        </form>
      </div>`;
    document.getElementById("mem-key-form")!.addEventListener("submit", (event) => {
      event.preventDefault();
      const entered = (document.getElementById("mem-key") as HTMLInputElement).value.trim();
      if (!entered) return;
      setFsKey(workspaceId, entered);
      render();
    });
    return;
  }

  // Page state. topic: null = all, "" = the uncategorized bucket.
  let tab: MemTab = "team";
  let topic: string | null = null;
  let page = 1;
  let searchQuery: string | null = null;
  // Close-neighbor hint from the last save, rendered inside the (rebuilt)
  // add form so a successful save can still refresh the list (Codex F2).
  let addHint = "";

  app.innerHTML = `
    <div class="flex flex-wrap justify-between items-start gap-3 mb-4">
      <div>
        <a href="#/console" class="text-sm text-blue-600 hover:underline">${esc(L.filesBack)}</a>
        <h1 class="text-2xl font-bold mt-3">${esc(workspace.name)} · ${esc(L.btnMemory)}</h1>
      </div>
      <button id="mem-change-key" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.filesChangeKey)}</button>
    </div>
    <div class="flex flex-wrap items-center gap-2 mb-4 text-sm">
      <label class="text-slate-600">${esc(L.memIdentityLabel)}</label>
      <input id="mem-op" value="${attr(getOperator() || "")}" placeholder="${attr(L.memIdentityPlaceholder)}"
        class="border border-slate-300 rounded px-2 py-1 w-64 font-mono text-xs focus:outline-none focus:border-slate-500">
      <button id="mem-op-apply" class="border border-slate-300 px-2 py-1 rounded hover:bg-slate-50">${esc(L.memIdentityApply)}</button>
      <span class="text-xs text-slate-400">${esc(L.memIdentityHint)}</span>
    </div>
    <div class="flex flex-wrap items-center gap-2 mb-4">
      <div id="mem-tabs" class="flex gap-1"></div>
      <div class="grow"></div>
      <form id="mem-search-form" class="flex gap-1">
        <input id="mem-search" placeholder="${attr(L.memSearchPlaceholder)}"
          class="border border-slate-300 rounded px-2 py-1 text-sm w-56 focus:outline-none focus:border-slate-500">
        <button class="text-sm border border-slate-300 px-3 py-1 rounded hover:bg-slate-50">${esc(L.memSearchBtn)}</button>
      </form>
    </div>
    <div class="flex gap-4 items-start">
      <nav id="mem-topics" class="w-44 shrink-0 bg-white border border-slate-200 rounded-lg p-2 text-sm"></nav>
      <section class="grow bg-white border border-slate-200 rounded-lg min-w-0">
        <div id="mem-body" class="text-sm"></div>
      </section>
    </div>
  `;

  document.getElementById("mem-change-key")!.addEventListener("click", () => {
    clearFsKey(workspaceId);
    render();
  });
  document.getElementById("mem-op-apply")!.addEventListener("click", () => {
    setOperator((document.getElementById("mem-op") as HTMLInputElement).value.trim());
    topic = null;
    page = 1;
    searchQuery = null;
    addHint = "";
    refresh();
  });
  document.getElementById("mem-search-form")!.addEventListener("submit", (e) => {
    e.preventDefault();
    const q = (document.getElementById("mem-search") as HTMLInputElement).value.trim();
    if (!q) return;
    searchQuery = q;
    refresh();
  });

  const tabLabel = (x: MemTab) =>
    x === "team" ? L.memTabTeam : x === "dept" ? L.memTabDept : L.memTabMine;

  function renderTabs() {
    const root = document.getElementById("mem-tabs")!;
    root.innerHTML = (["team", "dept", "mine"] as MemTab[])
      .map((x) => {
        const needsOp = x !== "team" && !getOperator();
        const active = x === tab;
        const cls = active
          ? "bg-slate-900 text-white"
          : needsOp
            ? "text-slate-400 border border-slate-200"
            : "border border-slate-300 hover:bg-slate-50";
        return `<button data-tab="${x}" class="text-sm px-3 py-1 rounded ${cls}">${esc(tabLabel(x))}</button>`;
      })
      .join("");
    root.querySelectorAll("[data-tab]").forEach((el) =>
      el.addEventListener("click", (e) => {
        tab = (e.currentTarget as HTMLElement).dataset.tab as MemTab;
        topic = null;
        page = 1;
        searchQuery = null;
        addHint = "";
        refresh();
      }),
    );
  }

  function fmtMeta(m: MemoryItem): string {
    const by = m.updated_by.length > 12 ? m.updated_by.slice(0, 8) : m.updated_by;
    const extra: string[] = [];
    if (m.topic) extra.push(`<span class="px-1 rounded bg-slate-100">${esc(m.topic)}</span>`);
    if (m.expires_at)
      extra.push(
        `<span class="px-1 rounded bg-amber-50 text-amber-700">${esc(L.memExpiresChip)} ${esc(m.expires_at.slice(0, 10))}</span>`,
      );
    if (typeof m.score === "number") extra.push(`<span>${m.score.toFixed(2)}</span>`);
    return `<span class="font-mono">[mem:${m.id}]</span>
      <span>${esc((m.updated_at || "").slice(0, 10))}</span>
      <span>${esc(by)}</span>
      <span class="px-1 rounded bg-slate-100">${esc(m.kind)}</span>
      ${extra.join("\n")}`;
  }

  function rowHtml(m: MemoryItem): string {
    return `
      <div class="border-b border-slate-100 last:border-0 py-2 px-3" data-mid="${m.id}">
        <div class="mem-view">
          <div class="whitespace-pre-wrap break-words">${esc(m.content)}</div>
          <div class="text-xs text-slate-400 mt-1 flex flex-wrap gap-x-2 items-center">
            ${fmtMeta(m)}
            <button data-act="edit" class="text-blue-600 hover:underline">${esc(L.memEdit)}</button>
            <button data-act="del" class="text-red-600 hover:underline">${esc(L.memDelete)}</button>
          </div>
        </div>
      </div>`;
  }

  function wireRows(container: HTMLElement, byId: Map<number, MemoryItem>) {
    container.querySelectorAll("[data-mid]").forEach((rowEl) => {
      const id = Number((rowEl as HTMLElement).dataset.mid);
      const m = byId.get(id)!;
      rowEl.querySelector('[data-act="del"]')?.addEventListener("click", async () => {
        if (!confirm(L.memDeleteConfirm)) return;
        try {
          await memoryApi.remove(key!, id);
          refresh();
        } catch (e: any) {
          alert(L.errorPrefix + e.message);
        }
      });
      rowEl.querySelector('[data-act="edit"]')?.addEventListener("click", () => {
        const view = rowEl.querySelector(".mem-view") as HTMLElement;
        view.innerHTML = `
          <textarea class="mem-edit-content w-full border border-slate-300 rounded px-2 py-1 text-sm" rows="2">${esc(m.content)}</textarea>
          <div class="flex gap-2 mt-1 items-center">
            <input class="mem-edit-topic border border-slate-300 rounded px-2 py-1 text-xs w-40" placeholder="${attr(L.memTopicPlaceholder)}" value="${attr(m.topic || "")}">
            <input type="date" class="mem-edit-expiry border border-slate-300 rounded px-2 py-1 text-xs" title="${attr(L.memExpiry)}" value="${attr((m.expires_at || "").slice(0, 10))}">
            <button class="mem-edit-save text-xs bg-slate-900 text-white px-2 py-1 rounded">${esc(L.memSave)}</button>
            <button class="mem-edit-cancel text-xs border border-slate-300 px-2 py-1 rounded">${esc(L.cancel)}</button>
          </div>`;
        view.querySelector(".mem-edit-cancel")!.addEventListener("click", () => refresh());
        view.querySelector(".mem-edit-save")!.addEventListener("click", async () => {
          const content = (view.querySelector(".mem-edit-content") as HTMLTextAreaElement).value.trim();
          const newTopic = (view.querySelector(".mem-edit-topic") as HTMLInputElement).value.trim();
          const newExpiry = (view.querySelector(".mem-edit-expiry") as HTMLInputElement).value;
          const patch: { content?: string; topic?: string; expires_at?: string } = {};
          if (content && content !== m.content) patch.content = content;
          if (newTopic && newTopic !== (m.topic || "")) patch.topic = newTopic;
          // Set/change only — clearing expiry via PATCH is out by M1 decision.
          if (newExpiry && newExpiry !== (m.expires_at || "").slice(0, 10))
            patch.expires_at = `${newExpiry}T00:00:00Z`;
          if (!Object.keys(patch).length) return refresh();
          try {
            await memoryApi.update(key!, id, patch);
            refresh();
          } catch (e: any) {
            alert(L.errorPrefix + e.message);
          }
        });
      });
    });
  }

  function addFormHtml(): string {
    return `
      <form id="mem-add-form" class="border-b border-slate-200 p-3 bg-slate-50 rounded-t-lg">
        <textarea id="mem-add-content" rows="2" placeholder="${attr(L.memAddPlaceholder)}"
          class="w-full border border-slate-300 rounded px-2 py-1 text-sm focus:outline-none focus:border-slate-500"></textarea>
        <div class="flex gap-2 mt-1 items-center">
          <input id="mem-add-topic" placeholder="${attr(L.memTopicPlaceholder)}"
            class="border border-slate-300 rounded px-2 py-1 text-xs w-40">
          <input type="date" id="mem-add-expiry" title="${attr(L.memExpiry)}"
            class="border border-slate-300 rounded px-2 py-1 text-xs">
          <button class="text-xs bg-slate-900 text-white px-3 py-1 rounded">${esc(L.memAdd)}</button>
          <span id="mem-add-msg" class="text-xs text-slate-500"></span>
        </div>
        <div id="mem-add-neighbors" class="text-xs text-amber-700 mt-1">${addHint}</div>
      </form>`;
  }

  function wireAddForm() {
    const form = document.getElementById("mem-add-form");
    form?.addEventListener("submit", async (e) => {
      e.preventDefault();
      const contentEl = document.getElementById("mem-add-content") as HTMLTextAreaElement;
      const topicEl = document.getElementById("mem-add-topic") as HTMLInputElement;
      const msg = document.getElementById("mem-add-msg")!;
      const content = contentEl.value.trim();
      if (!content) return;
      const expiryEl = document.getElementById("mem-add-expiry") as HTMLInputElement;
      try {
        const out = await memoryApi.save(
          key!,
          tab,
          content,
          topicEl.value.trim(),
          expiryEl.value ? `${expiryEl.value}T00:00:00Z` : undefined,
        );
        if (out.duplicate) {
          // Nothing changed — keep the typed text so the writer can rework it.
          msg.textContent = L.memDup;
          return;
        }
        // Always refresh so the new row is visible (save returns top-3
        // neighbors unconditionally — suppressing the refresh on any
        // neighbor would hide almost every save). Only CLOSE neighbors are
        // worth the "update the old row instead" hint; the rebuilt form
        // renders it from `addHint`.
        const close = out.neighbors.filter((n) => (n.score ?? 0) >= 0.85);
        addHint = close.length
          ? `${esc(L.memNeighborsHint)}<br>` +
            close
              .map((n) => `<span class="font-mono">[mem:${n.id}]</span> ${esc(n.content)}`)
              .join("<br>")
          : "";
        refresh();
      } catch (e: any) {
        msg.textContent = L.errorPrefix + e.message;
      }
    });
  }

  async function refreshTopics() {
    const root = document.getElementById("mem-topics")!;
    try {
      const { topics } = await memoryApi.topics(key!, tab);
      const item = (label: string, value: string | null, count: number | null) => {
        const active = value === topic;
        return `<button data-topic="${value === null ? "*all*" : attr(value)}"
          class="block w-full text-left px-2 py-1 rounded ${active ? "bg-slate-900 text-white" : "hover:bg-slate-50"}">
          ${esc(label)}${count === null ? "" : ` <span class="text-xs opacity-60">(${count})</span>`}</button>`;
      };
      root.innerHTML =
        `<div class="text-xs text-slate-400 px-2 pb-1">${esc(L.memTopics)}</div>` +
        item(L.memTopicAll, null, null) +
        topics
          .map((tc) => item(tc.topic === null ? L.memUncategorized : tc.topic, tc.topic === null ? "" : tc.topic, tc.count))
          .join("");
      root.querySelectorAll("[data-topic]").forEach((el) =>
        el.addEventListener("click", (e) => {
          const v = (e.currentTarget as HTMLElement).dataset.topic!;
          topic = v === "*all*" ? null : v;
          page = 1;
          searchQuery = null;
          refresh();
        }),
      );
    } catch (e: any) {
      root.innerHTML = `<p class="text-xs text-red-600 px-2">${esc(e.message)}</p>`;
    }
  }

  function renderList(body: HTMLElement, resp: MemoryPage) {
    const byId = new Map(resp.items.map((m) => [m.id, m] as [number, MemoryItem]));
    let rows: string;
    if (tab === "mine") {
      // §15.2: portable prefs vs project-pinned notes, grouped by origin.
      const portable = resp.items.filter((m) => !m.origin_workspace_id);
      const pinned = resp.items.filter((m) => !!m.origin_workspace_id);
      const group = (label: string, xs: MemoryItem[]) =>
        xs.length
          ? `<div class="text-xs text-slate-400 px-3 pt-2">${esc(label)}</div>` + xs.map(rowHtml).join("")
          : "";
      rows = group(L.memPortableGroup, portable) + group(L.memPinnedGroup, pinned);
    } else {
      rows = resp.items.map(rowHtml).join("");
    }
    if (!resp.items.length) rows = `<p class="text-slate-500 p-4">${esc(L.memEmpty)}</p>`;
    const lastPage = Math.max(1, Math.ceil(resp.total / resp.size));
    body.innerHTML =
      addFormHtml() +
      rows +
      `<div class="flex items-center gap-3 p-3 text-xs text-slate-500 border-t border-slate-100">
        <span>${esc(L.memCount(resp.total))}</span>
        <div class="grow"></div>
        ${resp.page > 1 ? `<button id="mem-prev" class="border border-slate-300 px-2 py-1 rounded hover:bg-slate-50">${esc(L.memPrev)}</button>` : ""}
        <span>${resp.page}/${lastPage}</span>
        ${resp.page < lastPage ? `<button id="mem-next" class="border border-slate-300 px-2 py-1 rounded hover:bg-slate-50">${esc(L.memNext)}</button>` : ""}
      </div>`;
    wireAddForm();
    wireRows(body, byId);
    document.getElementById("mem-prev")?.addEventListener("click", () => {
      page -= 1;
      refresh();
    });
    document.getElementById("mem-next")?.addEventListener("click", () => {
      page += 1;
      refresh();
    });
  }

  function renderSearch(body: HTMLElement, items: MemoryItem[]) {
    const byId = new Map(items.map((m) => [m.id, m] as [number, MemoryItem]));
    body.innerHTML =
      `<div class="flex items-center gap-2 p-3 border-b border-slate-200 text-xs text-slate-500">
        <span>${esc(L.memSearchResults)}: ${esc(searchQuery!)}</span>
        <button id="mem-clear-search" class="text-blue-600 hover:underline">${esc(L.memClearSearch)}</button>
      </div>` +
      (items.length ? items.map(rowHtml).join("") : `<p class="text-slate-500 p-4">${esc(L.memEmpty)}</p>`);
    document.getElementById("mem-clear-search")!.addEventListener("click", () => {
      searchQuery = null;
      (document.getElementById("mem-search") as HTMLInputElement).value = "";
      refresh();
    });
    wireRows(body, byId);
  }

  async function refresh() {
    renderTabs();
    const body = document.getElementById("mem-body")!;
    if (tab !== "team" && !getOperator()) {
      document.getElementById("mem-topics")!.innerHTML = "";
      body.innerHTML = `<p class="text-slate-500 p-4">${esc(L.memNeedIdentity)}</p>`;
      return;
    }
    body.innerHTML = `<p class="text-slate-500 p-4">${esc(L.loading)}</p>`;
    refreshTopics();
    try {
      if (searchQuery !== null) {
        let items = (await memoryApi.search(key!, tab, searchQuery)).items;
        // `scope=mine` search is the full personal domain by design (agent
        // semantics, M1). This page's mine tab is a this-workspace view, so
        // keep search consistent with the list: drop foreign-origin rows.
        // Own-data filtering only — not a security boundary.
        if (tab === "mine")
          items = items.filter(
            (m) => !m.origin_workspace_id || m.origin_workspace_id === workspaceId,
          );
        renderSearch(body, items);
      } else {
        renderList(body, await memoryApi.list(key!, tab, topic, page));
      }
    } catch (e: any) {
      body.innerHTML = `<p class="text-red-600 p-4">${esc(L.errorPrefix + e.message)}</p>`;
    }
  }

  refresh();
}
