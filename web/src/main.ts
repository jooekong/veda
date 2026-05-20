import { marked } from "marked";

// ── Auth state ────────────────────────────────────────────────────────────
type Auth = { vk: string };
const STORAGE_KEY = "veda.auth";

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
    btnJwt: "JWT 24h",
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
    jwtTitle: "JWT 已签发（24h）",
    jwtLabel: "workspace JWT",
    jwtHint: "用作数据面 endpoint 的 Bearer token。仅在内存，不会持久化。",
    expires: "过期：",
    deleteConfirm: "删除这个 workspace 和它所有数据？操作不可恢复。",
    claimTitle: "升级账号",
    claimIntro: "把当前匿名账号升级为邮箱 + 密码。现有 key 继续可用。",
    emailPlaceholder: "邮箱",
    passwordPlaceholder: "密码",
    displayNamePlaceholder: "显示名（可选）",
    claim: "升级",
    claimedAlert: (email: string) => `升级成功。现在可以在其他机器用 ${email} 登录。`,
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
    btnJwt: "JWT 24h",
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
    jwtTitle: "JWT minted (24h)",
    jwtLabel: "Workspace JWT",
    jwtHint: "Use as Bearer for data-plane endpoints. Memory-only — not stored.",
    expires: "Expires: ",
    deleteConfirm: "Delete this workspace and all its data? This cannot be undone.",
    claimTitle: "Claim account",
    claimIntro: "Upgrade this anonymous account to email + password. Your existing keys keep working.",
    emailPlaceholder: "email",
    passwordPlaceholder: "password",
    displayNamePlaceholder: "display name (optional)",
    claim: "Claim",
    claimedAlert: (email: string) => `Claimed. You can now log in with ${email} from another machine.`,
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
  created_at: string;
};

const workspaces = {
  list: (vk: string) => api<Workspace[]>("/v1/workspaces", {}, vk),
  create: (vk: string, name: string) =>
    api<Workspace>(
      "/v1/workspaces",
      { method: "POST", body: JSON.stringify({ name }) },
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
  mintToken: (vk: string, id: string) =>
    api<{ token: string; expires_at: string }>(
      `/v1/workspaces/${id}/token`,
      { method: "POST" },
      vk,
    ),
};

// ── Helpers ────────────────────────────────────────────────────────────────
function esc(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}
function attr(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/"/g, "&quot;");
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
  if (r.startsWith("/console")) await renderConsole(app);
  else if (r.startsWith("/docs")) await renderDocs(app);
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
    .map(
      (w) => `
    <div class="bg-white border border-slate-200 rounded-lg p-4 flex justify-between items-center gap-4">
      <div class="min-w-0">
        <div class="font-medium">${esc(w.name)}</div>
        <div class="text-xs text-slate-500 font-mono mt-0.5 truncate">${esc(w.id)}</div>
      </div>
      <div class="flex gap-2 shrink-0">
        <button data-act="new-key" data-id="${attr(w.id)}" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnNewKey)}</button>
        <button data-act="token" data-id="${attr(w.id)}" class="text-sm border border-slate-300 px-3 py-1.5 rounded hover:bg-slate-50">${esc(L.btnJwt)}</button>
        <button data-act="delete" data-id="${attr(w.id)}" class="text-sm border border-red-300 text-red-700 px-3 py-1.5 rounded hover:bg-red-50">${esc(L.btnDelete)}</button>
      </div>
    </div>
  `,
    )
    .join("");
  root.querySelectorAll("[data-act]").forEach((el) => {
    el.addEventListener("click", (e) => {
      const t = e.currentTarget as HTMLElement;
      const act = t.dataset.act!;
      const id = t.dataset.id!;
      if (act === "new-key") newKeyModal(vk, id);
      else if (act === "token") mintTokenModal(vk, id);
      else if (act === "delete") deleteWs(vk, id);
    });
  });
}

function createWsModal(vk: string) {
  const L = t();
  modal(
    L.newWsTitle,
    `
    <input id="ws-name" placeholder="${attr(L.wsNamePlaceholder)}" class="w-full border border-slate-300 rounded px-3 py-2 mb-4 focus:outline-none focus:border-slate-500">
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
    try {
      await workspaces.create(vk, name);
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

function mintTokenModal(vk: string, wsId: string) {
  const L = t();
  workspaces
    .mintToken(vk, wsId)
    .then((res) => {
      modal(
        L.jwtTitle,
        `
        ${kv(L.jwtLabel, res.token, L.jwtHint)}
        <p class="text-xs text-slate-500 mt-2">${esc(L.expires)}${esc(new Date(res.expires_at).toLocaleString())}</p>
        <div class="flex justify-end mt-4">
          <button data-close class="text-sm bg-slate-900 text-white px-3 py-1.5 rounded hover:bg-slate-700">${esc(L.done)}</button>
        </div>
      `,
      );
      document.querySelector("[data-close]")!.addEventListener("click", closeModal);
    })
    .catch((e) => alert(L.failed + e.message));
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
const DOCS_META: Record<Lang, { sectionLabel: string; items: { id: string; title: string }[]; loadFailed: (m: string) => string }> = {
  zh: {
    sectionLabel: "文档",
    items: [
      { id: "introduction", title: "功能与场景" },
      { id: "quickstart", title: "快速开始" },
      { id: "cli", title: "CLI 速查" },
      { id: "skill", title: "AI 助手集成" },
      { id: "fuse", title: "FUSE 挂载" },
      { id: "troubleshooting", title: "常见问题" },
    ],
    loadFailed: (m) => `加载失败：${m}`,
  },
  en: {
    sectionLabel: "Docs",
    items: [
      { id: "introduction", title: "Introduction" },
      { id: "quickstart", title: "Quickstart" },
      { id: "cli", title: "CLI reference" },
      { id: "skill", title: "AI agent skill" },
      { id: "fuse", title: "FUSE mount" },
      { id: "troubleshooting", title: "Troubleshooting" },
    ],
    loadFailed: (m) => `Failed: ${m}`,
  },
};

async function renderDocs(app: HTMLElement) {
  const lang = getLang();
  const meta = DOCS_META[lang];
  const id = currentRoute().split("/")[2] || "introduction";
  app.innerHTML = `
    <div class="grid grid-cols-1 md:grid-cols-[200px_1fr] gap-8">
      <aside class="text-sm">
        <p class="text-xs uppercase tracking-wide text-slate-500 font-semibold mb-2">${esc(meta.sectionLabel)}</p>
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
