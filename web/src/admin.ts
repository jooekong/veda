// Admin dashboard — cross-tenant ops view (`#/admin`).
//
// Separate from the account Console: auth is a single deploy-wide admin token
// (server `VEDA_ADMIN_TOKEN`), entered once and kept in localStorage. The
// server gates every /admin/v1 route on it and 404s the whole surface when
// it's unset. Chinese-only — this is an internal operations surface.

const ADMIN_KEY = "veda.admin";

function getAdminToken(): string | null {
  return localStorage.getItem(ADMIN_KEY);
}
function setAdminToken(t: string) {
  localStorage.setItem(ADMIN_KEY, t);
}
function clearAdminToken() {
  localStorage.removeItem(ADMIN_KEY);
}

// ── helpers ─────────────────────────────────────────────
function esc(s: string | null | undefined): string {
  const d = document.createElement("div");
  d.textContent = s ?? "";
  return d.innerHTML;
}
function attr(s: string): string {
  return (s ?? "").replace(/&/g, "&amp;").replace(/"/g, "&quot;");
}
function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const u = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${u[i]}`;
}
function fmtTime(s: string): string {
  const d = new Date(s);
  return isNaN(d.getTime()) ? s : d.toLocaleString("zh-CN", { hour12: false });
}
function kindBadge(kind: string): string {
  return kind === "db"
    ? `<span class="text-xs px-1.5 py-0.5 rounded bg-violet-100 text-violet-700 font-medium">向量库</span>`
    : `<span class="text-xs px-1.5 py-0.5 rounded bg-sky-100 text-sky-700 font-medium">文件库</span>`;
}

// ── API client ──────────────────────────────────────────
async function adminApi<T = any>(path: string, opts: RequestInit = {}): Promise<T> {
  const token = getAdminToken();
  const res = await fetch(path, {
    ...opts,
    headers: {
      "Content-Type": "application/json",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...((opts.headers as Record<string, string>) || {}),
    },
  });
  if (res.status === 404) {
    throw new Error("DISABLED");
  }
  if (res.status === 401) {
    throw new Error("UNAUTHORIZED");
  }
  const body = await res.json().catch(() => ({}) as any);
  if (!res.ok || body.success === false) {
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return body.data as T;
}

// ── types ───────────────────────────────────────────────
type Kind = "fs" | "db";
type FsStats = { total_files: number; total_directories: number; total_bytes: number };
type AdminWorkspace = {
  id: string;
  name: string;
  kind: Kind;
  app_id: string | null;
  account_id: string;
  description: string | null;
  creator: string | null;
  creator_name: string | null;
  dataset_count: number;
  key_count: number;
  files: FsStats | null;
  created_at: string;
  updated_at: string;
};
type AdminDataset = {
  id: string;
  name: string;
  description: string | null;
  vector_count: number | null;
  created_at: string;
};
type AdminKey = {
  id: string;
  name: string;
  permission: string;
  status: string;
  created_at: string;
};
type AdminDetail = { workspace: AdminWorkspace; datasets: AdminDataset[]; keys: AdminKey[] };
type DirEntry = {
  name: string;
  path: string;
  is_dir: boolean;
  size_bytes: number | null;
  mime_type: string | null;
  created_at: string;
  updated_at: string;
};
type VectorHit = {
  id: string;
  dataset: string | null;
  category: string | null;
  tags: string[] | null;
  text: string | null;
  meta: any;
  score: number;
  score_type: string;
};

// ── header / shell ──────────────────────────────────────
function header(backHref: string | null, title: string): string {
  const back = backHref
    ? `<a href="${attr(backHref)}" class="text-sm text-blue-600 hover:underline">← 返回</a>`
    : `<span></span>`;
  return `
    <div class="flex justify-between items-center mb-6">
      <div class="flex items-center gap-3">
        ${back}
        <h1 class="text-xl font-bold">${esc(title)}</h1>
        <span class="text-xs px-1.5 py-0.5 rounded bg-amber-100 text-amber-700 font-medium">管理后台</span>
      </div>
      <button id="adm-logout" class="text-sm text-slate-500 hover:text-red-600">退出</button>
    </div>`;
}
function bindLogout() {
  const b = document.getElementById("adm-logout");
  if (b)
    b.addEventListener("click", () => {
      clearAdminToken();
      location.hash = "#/admin";
      renderAdminRoot();
    });
}

// ── router entry ────────────────────────────────────────
export async function renderAdmin(app: HTMLElement) {
  if (!getAdminToken()) {
    renderLogin(app);
    return;
  }
  const route = location.hash.replace(/^#/, "");
  const m = route.match(/^\/admin\/ws\/(.+)$/);
  if (m) await renderDetail(app, decodeURIComponent(m[1]));
  else await renderList(app);
}

// Re-render the current admin view (used after login/logout).
function renderAdminRoot() {
  const app = document.getElementById("app");
  if (app) renderAdmin(app);
}

// ── login ───────────────────────────────────────────────
function renderLogin(app: HTMLElement) {
  app.innerHTML = `
    <div class="max-w-md mx-auto mt-10">
      <h1 class="text-xl font-bold mb-1">管理后台</h1>
      <p class="text-sm text-slate-500 mb-6">输入服务端配置的 admin token（<code>VEDA_ADMIN_TOKEN</code>）。仅保存在本浏览器。</p>
      <input id="adm-token" type="password" placeholder="admin token"
        class="w-full border border-slate-300 rounded px-3 py-2 mb-3 focus:outline-none focus:border-slate-500">
      <button id="adm-login" class="w-full bg-slate-900 text-white px-4 py-2 rounded font-medium hover:bg-slate-700">进入</button>
      <p id="adm-err" class="text-sm text-red-600 mt-3 hidden"></p>
    </div>`;
  const submit = () => {
    const v = (document.getElementById("adm-token") as HTMLInputElement).value.trim();
    if (!v) return;
    setAdminToken(v);
    renderAdminRoot();
  };
  document.getElementById("adm-login")!.addEventListener("click", submit);
  document.getElementById("adm-token")!.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") submit();
  });
}

// Map an adminApi error to a user message; on auth failure, drop the token so
// the next render shows the login form.
function handleError(app: HTMLElement, e: Error) {
  if (e.message === "UNAUTHORIZED") {
    clearAdminToken();
    renderLogin(app);
    const err = document.getElementById("adm-err");
    if (err) {
      err.textContent = "token 无效，请重新输入。";
      err.classList.remove("hidden");
    }
    return;
  }
  const msg =
    e.message === "DISABLED"
      ? "管理后台未启用（服务端未配置 VEDA_ADMIN_TOKEN）。"
      : `错误：${e.message}`;
  app.innerHTML = `${header(null, "管理后台")}<p class="text-red-600 text-sm">${esc(msg)}</p>`;
  bindLogout();
}

// ── workspace list ──────────────────────────────────────
function volumeCell(w: AdminWorkspace): string {
  if (w.kind === "fs") {
    if (!w.files) return `<span class="text-slate-400">—</span>`;
    return `${w.files.total_files} 文件 · <span class="text-slate-500">${fmtBytes(w.files.total_bytes)}</span>`;
  }
  return `${w.dataset_count} 数据集`;
}

async function renderList(app: HTMLElement) {
  app.innerHTML = `${header(null, "Workspaces / Projects")}<p class="text-slate-500 text-sm">加载中…</p>`;
  bindLogout();
  let list: AdminWorkspace[];
  try {
    list = await adminApi<AdminWorkspace[]>("/admin/v1/workspaces");
  } catch (e: any) {
    handleError(app, e);
    return;
  }
  const rows = list
    .map(
      (w) => `
      <tr class="border-t border-slate-100 hover:bg-slate-50 cursor-pointer" data-id="${attr(w.id)}">
        <td class="px-3 py-2">
          <div class="font-medium flex items-center gap-2">${esc(w.name)} ${kindBadge(w.kind)}</div>
          <div class="text-xs text-slate-400 font-mono">${esc(w.id)}</div>
        </td>
        <td class="px-3 py-2 text-sm">${esc(w.app_id) || '<span class="text-slate-400">—</span>'}</td>
        <td class="px-3 py-2 text-sm">${esc(w.creator_name || w.creator) || '<span class="text-slate-400">—</span>'}</td>
        <td class="px-3 py-2 text-sm whitespace-nowrap">${volumeCell(w)}</td>
        <td class="px-3 py-2 text-sm text-center">${w.key_count}</td>
        <td class="px-3 py-2 text-xs text-slate-500 whitespace-nowrap">${fmtTime(w.created_at)}</td>
      </tr>`,
    )
    .join("");
  const table = list.length
    ? `
    <div class="bg-white border border-slate-200 rounded-lg overflow-x-auto">
      <table class="w-full text-left">
        <thead class="text-xs uppercase tracking-wide text-slate-500">
          <tr>
            <th class="px-3 py-2 font-semibold">名称</th>
            <th class="px-3 py-2 font-semibold">租户(app_id)</th>
            <th class="px-3 py-2 font-semibold">创建者</th>
            <th class="px-3 py-2 font-semibold">数据量</th>
            <th class="px-3 py-2 font-semibold text-center">Keys</th>
            <th class="px-3 py-2 font-semibold">创建时间</th>
          </tr>
        </thead>
        <tbody>${rows}</tbody>
      </table>
    </div>`
    : `<p class="text-sm text-slate-500 p-4 bg-white border border-slate-200 rounded-lg">没有 workspace。</p>`;
  app.innerHTML = `${header(null, "Workspaces / Projects")}
    <p class="text-sm text-slate-500 mb-4">本节点共 ${list.length} 个 workspace（跨所有账号/租户）。点击查看详情。</p>
    ${table}`;
  bindLogout();
  app.querySelectorAll("tr[data-id]").forEach((el) => {
    el.addEventListener("click", () => {
      const id = (el as HTMLElement).dataset.id!;
      location.hash = `#/admin/ws/${encodeURIComponent(id)}`;
    });
  });
}

// ── workspace detail ────────────────────────────────────
async function renderDetail(app: HTMLElement, id: string) {
  app.innerHTML = `${header("#/admin", "详情")}<p class="text-slate-500 text-sm">加载中…</p>`;
  bindLogout();
  let d: AdminDetail;
  try {
    d = await adminApi<AdminDetail>(`/admin/v1/workspaces/${encodeURIComponent(id)}`);
  } catch (e: any) {
    handleError(app, e);
    return;
  }
  const w = d.workspace;

  // Summary card.
  const totalVectors = d.datasets.reduce((s, ds) => s + (ds.vector_count ?? 0), 0);
  const volume =
    w.kind === "fs"
      ? w.files
        ? `${w.files.total_files} 文件 · ${w.files.total_directories} 目录 · ${fmtBytes(w.files.total_bytes)}`
        : "—"
      : `${d.datasets.length} 数据集 · ${totalVectors} 向量`;
  const info = `
    <div class="bg-white border border-slate-200 rounded-lg p-4 mb-6 grid grid-cols-2 md:grid-cols-3 gap-x-6 gap-y-3 text-sm">
      ${infoItem("ID", `<span class="font-mono text-xs">${esc(w.id)}</span>`)}
      ${infoItem("租户 app_id", esc(w.app_id) || "—")}
      ${infoItem("账号", `<span class="font-mono text-xs">${esc(w.account_id)}</span>`)}
      ${infoItem("创建者", esc(w.creator_name || w.creator) || "—")}
      ${infoItem("数据量", volume)}
      ${infoItem("创建时间", fmtTime(w.created_at))}
      ${w.description ? infoItem("描述", esc(w.description)) : ""}
    </div>`;

  // Datasets (db).
  const datasetsSection =
    w.kind === "db"
      ? section(
          "数据集",
          d.datasets.length
            ? `<table class="w-full text-left text-sm">
                <thead class="text-xs uppercase tracking-wide text-slate-500">
                  <tr><th class="px-3 py-2">名称</th><th class="px-3 py-2 text-right">向量数</th><th class="px-3 py-2">描述</th><th class="px-3 py-2">创建时间</th></tr>
                </thead>
                <tbody>${d.datasets
                  .map(
                    (ds) => `<tr class="border-t border-slate-100">
                      <td class="px-3 py-2 font-mono">${esc(ds.name)}</td>
                      <td class="px-3 py-2 text-right">${ds.vector_count == null ? '<span class="text-slate-400">—</span>' : ds.vector_count}</td>
                      <td class="px-3 py-2 text-slate-500">${esc(ds.description) || ""}</td>
                      <td class="px-3 py-2 text-xs text-slate-500 whitespace-nowrap">${fmtTime(ds.created_at)}</td>
                    </tr>`,
                  )
                  .join("")}</tbody>
              </table>`
            : `<p class="text-sm text-slate-500 px-3 py-2">没有数据集。</p>`,
        )
      : "";

  // Keys.
  const keysSection = section(
    "Keys",
    d.keys.length
      ? `<table class="w-full text-left text-sm">
          <thead class="text-xs uppercase tracking-wide text-slate-500">
            <tr><th class="px-3 py-2">名称</th><th class="px-3 py-2">权限</th><th class="px-3 py-2">状态</th><th class="px-3 py-2">创建时间</th></tr>
          </thead>
          <tbody>${d.keys
            .map(
              (k) => `<tr class="border-t border-slate-100">
                <td class="px-3 py-2">${esc(k.name)}</td>
                <td class="px-3 py-2">${esc(k.permission)}</td>
                <td class="px-3 py-2">${k.status === "active" ? '<span class="text-emerald-600">active</span>' : `<span class="text-slate-400">${esc(k.status)}</span>`}</td>
                <td class="px-3 py-2 text-xs text-slate-500 whitespace-nowrap">${fmtTime(k.created_at)}</td>
              </tr>`,
            )
            .join("")}</tbody>
        </table>`
      : `<p class="text-sm text-slate-500 px-3 py-2">没有 key。</p>`,
  );

  // fs documents browser / db vector console.
  const toolSection =
    w.kind === "fs"
      ? section("文档", `<div id="adm-files" class="text-sm">加载中…</div>`)
      : section("向量查询", vectorConsoleHtml(d.datasets));

  app.innerHTML = `${header("#/admin", w.name)}
    ${info}
    ${datasetsSection}
    ${toolSection}
    ${keysSection}`;
  bindLogout();

  if (w.kind === "fs") initFilesBrowser(id);
  else initVectorConsole(id);
}

function infoItem(label: string, val: string): string {
  return `<div><div class="text-xs uppercase tracking-wide text-slate-400 font-semibold mb-0.5">${esc(label)}</div><div>${val}</div></div>`;
}
function section(title: string, bodyHtml: string): string {
  return `
    <section class="mb-6">
      <h2 class="text-sm font-semibold text-slate-700 mb-2">${esc(title)}</h2>
      <div class="bg-white border border-slate-200 rounded-lg overflow-x-auto">${bodyHtml}</div>
    </section>`;
}

// ── fs documents browser ────────────────────────────────
function initFilesBrowser(wsId: string) {
  const root = document.getElementById("adm-files");
  if (!root) return;

  const load = async (path: string) => {
    root.innerHTML = `<div class="px-3 py-2 text-slate-500">加载中…</div>`;
    let entries: DirEntry[];
    try {
      entries = await adminApi<DirEntry[]>(
        `/admin/v1/workspaces/${encodeURIComponent(wsId)}/files?path=${encodeURIComponent(path)}`,
      );
    } catch (e: any) {
      root.innerHTML = `<div class="px-3 py-2 text-red-600">${esc(e.message)}</div>`;
      return;
    }
    // Breadcrumb.
    const parts = path.split("/").filter(Boolean);
    let acc = "";
    const crumbs = [`<a data-path="/" class="text-blue-600 hover:underline cursor-pointer">/</a>`];
    for (const p of parts) {
      acc += "/" + p;
      crumbs.push(
        `<span class="text-slate-300"> / </span><a data-path="${attr(acc)}" class="text-blue-600 hover:underline cursor-pointer">${esc(p)}</a>`,
      );
    }
    // Directories first, then files; both alphabetical.
    entries.sort((a, b) =>
      a.is_dir === b.is_dir ? a.name.localeCompare(b.name) : a.is_dir ? -1 : 1,
    );
    const rows = entries
      .map((e) => {
        const icon = e.is_dir ? "📁" : "📄";
        const nameCell = e.is_dir
          ? `<a data-path="${attr(e.path)}" class="text-blue-600 hover:underline cursor-pointer">${esc(e.name)}</a>`
          : `<a data-file="${attr(e.path)}" class="text-slate-700 hover:text-blue-600 hover:underline cursor-pointer">${esc(e.name)}</a>`;
        return `<tr class="border-t border-slate-100">
          <td class="px-3 py-1.5">${icon} ${nameCell}</td>
          <td class="px-3 py-1.5 text-xs text-slate-500 whitespace-nowrap">${fmtTime(e.updated_at)}</td>
        </tr>`;
      })
      .join("");
    root.innerHTML = `
      <div class="px-3 py-2 text-sm border-b border-slate-100">${crumbs.join("")}</div>
      ${
        entries.length
          ? `<table class="w-full text-left text-sm"><tbody>${rows}</tbody></table>`
          : `<div class="px-3 py-2 text-slate-500">空目录。</div>`
      }`;
    root.querySelectorAll("[data-path]").forEach((el) => {
      el.addEventListener("click", () => load((el as HTMLElement).dataset.path!));
    });
    root.querySelectorAll("[data-file]").forEach((el) => {
      el.addEventListener("click", () => openFilePreview(wsId, (el as HTMLElement).dataset.file!));
    });
  };

  load("/");
}

// ── file preview modal ──────────────────────────────────
type FilePreview = { path: string; size: number; truncated: boolean; content: string };

async function openFilePreview(wsId: string, path: string) {
  showModal(path, `<p class="text-slate-500 text-sm">加载中…</p>`);
  let pv: FilePreview;
  try {
    pv = await adminApi<FilePreview>(
      `/admin/v1/workspaces/${encodeURIComponent(wsId)}/file?path=${encodeURIComponent(path)}`,
    );
  } catch (e: any) {
    showModal(path, `<p class="text-red-600 text-sm">${esc(e.message)}</p>`);
    return;
  }
  const note = pv.truncated
    ? `<p class="text-xs text-amber-600 mb-2">⚠ 文件 ${fmtBytes(pv.size)}，仅预览前 256 KB</p>`
    : `<p class="text-xs text-slate-400 mb-2">${fmtBytes(pv.size)}</p>`;
  const body = pv.content
    ? `<pre class="text-xs bg-slate-50 border border-slate-200 rounded p-3 overflow-auto whitespace-pre-wrap break-words" style="max-height:60vh">${esc(pv.content)}</pre>`
    : `<p class="text-sm text-slate-500">（空文件）</p>`;
  showModal(path, `${note}${body}`);
}

function showModal(title: string, bodyHtml: string) {
  let root = document.getElementById("adm-modal");
  if (!root) {
    root = document.createElement("div");
    root.id = "adm-modal";
    document.body.appendChild(root);
  }
  root.innerHTML = `
    <div class="fixed inset-0 bg-black/40 flex items-center justify-center z-20 p-4" data-modal-bg>
      <div class="bg-white rounded-lg shadow-xl max-w-3xl w-full p-5 flex flex-col" style="max-height:85vh">
        <div class="flex justify-between items-center mb-3 gap-4">
          <h3 class="font-mono text-sm truncate">${esc(title)}</h3>
          <button data-modal-close class="text-slate-400 hover:text-slate-700 text-2xl leading-none shrink-0">×</button>
        </div>
        <div class="overflow-auto">${bodyHtml}</div>
      </div>
    </div>`;
  root.querySelector("[data-modal-close]")!.addEventListener("click", closeModal);
  root.querySelector("[data-modal-bg]")!.addEventListener("click", (e) => {
    if (e.target === (e.currentTarget as HTMLElement)) closeModal();
  });
}

function closeModal() {
  const root = document.getElementById("adm-modal");
  if (root) root.innerHTML = "";
}

// ── db vector query console ─────────────────────────────
function vectorConsoleHtml(datasets: AdminDataset[]): string {
  const opts = datasets.length
    ? datasets.map((d) => `<option value="${attr(d.name)}">${esc(d.name)}</option>`).join("")
    : `<option value="default">default</option>`;
  return `
    <div class="p-4 space-y-3">
      <div class="flex flex-wrap gap-2 items-end">
        <div>
          <label class="block text-xs text-slate-500 mb-1">数据集</label>
          <select id="adm-ds" class="border border-slate-300 rounded px-2 py-1.5 text-sm">${opts}</select>
        </div>
        <div>
          <label class="block text-xs text-slate-500 mb-1">模式</label>
          <select id="adm-mode" class="border border-slate-300 rounded px-2 py-1.5 text-sm">
            <option value="hybrid">hybrid</option>
            <option value="semantic">semantic</option>
            <option value="fulltext">fulltext</option>
          </select>
        </div>
        <div>
          <label class="block text-xs text-slate-500 mb-1">top_k</label>
          <input id="adm-topk" type="number" value="10" min="1" max="100" class="w-16 border border-slate-300 rounded px-2 py-1.5 text-sm">
        </div>
        <div>
          <label class="block text-xs text-slate-500 mb-1">category</label>
          <input id="adm-cat" placeholder="可选" class="w-24 border border-slate-300 rounded px-2 py-1.5 text-sm">
        </div>
        <div>
          <label class="block text-xs text-slate-500 mb-1">tags(逗号分隔)</label>
          <input id="adm-tags" placeholder="可选" class="w-32 border border-slate-300 rounded px-2 py-1.5 text-sm">
        </div>
        <div class="flex-1 min-w-[180px]">
          <label class="block text-xs text-slate-500 mb-1">查询文本</label>
          <input id="adm-q" placeholder="输入查询文本…" class="w-full border border-slate-300 rounded px-2 py-1.5 text-sm">
        </div>
        <button id="adm-search" class="bg-slate-900 text-white px-4 py-1.5 rounded text-sm font-medium hover:bg-slate-700">搜索</button>
      </div>
      <div id="adm-results"></div>

      <details class="border-t border-slate-100 pt-3">
        <summary class="text-sm font-medium text-slate-700 cursor-pointer select-none">＋ 写入向量</summary>
        <div class="mt-3 space-y-2">
          <div class="flex flex-wrap gap-2 items-end">
            <div>
              <label class="block text-xs text-slate-500 mb-1">数据集</label>
              <select id="adm-w-ds" class="border border-slate-300 rounded px-2 py-1.5 text-sm">${opts}</select>
            </div>
            <div>
              <label class="block text-xs text-slate-500 mb-1">category</label>
              <input id="adm-w-cat" placeholder="可选" class="w-24 border border-slate-300 rounded px-2 py-1.5 text-sm">
            </div>
            <div class="flex-1 min-w-[160px]">
              <label class="block text-xs text-slate-500 mb-1">tags(逗号分隔)</label>
              <input id="adm-w-tags" placeholder="可选" class="w-full border border-slate-300 rounded px-2 py-1.5 text-sm">
            </div>
          </div>
          <div>
            <label class="block text-xs text-slate-500 mb-1">文本</label>
            <textarea id="adm-w-text" rows="3" placeholder="输入要写入的文本…" class="w-full border border-slate-300 rounded px-2 py-1.5 text-sm"></textarea>
          </div>
          <div class="flex items-center gap-3">
            <button id="adm-write" class="bg-emerald-600 text-white px-4 py-1.5 rounded text-sm font-medium hover:bg-emerald-700">写入</button>
            <span id="adm-write-msg" class="text-xs"></span>
          </div>
        </div>
      </details>
    </div>`;
}

function parseTags(s: string): string[] {
  return s
    .split(",")
    .map((t) => t.trim())
    .filter((t) => t.length > 0);
}

function initVectorConsole(wsId: string) {
  // ── 查询 ──
  const btn = document.getElementById("adm-search");
  const qInput = document.getElementById("adm-q") as HTMLInputElement | null;
  const results = document.getElementById("adm-results");
  if (btn && qInput && results) {
    const run = async () => {
      const query = qInput.value.trim();
      if (!query) return;
      const dataset = (document.getElementById("adm-ds") as HTMLSelectElement).value;
      const mode = (document.getElementById("adm-mode") as HTMLSelectElement).value;
      const top_k = parseInt((document.getElementById("adm-topk") as HTMLInputElement).value, 10) || 10;
      const category = (document.getElementById("adm-cat") as HTMLInputElement).value.trim() || undefined;
      const tags = parseTags((document.getElementById("adm-tags") as HTMLInputElement).value);
      results.innerHTML = `<p class="text-slate-500 text-sm">查询中…</p>`;
      let hits: VectorHit[];
      try {
        hits = await adminApi<VectorHit[]>(
          `/admin/v1/workspaces/${encodeURIComponent(wsId)}/vectors/search`,
          {
            method: "POST",
            body: JSON.stringify({
              dataset,
              query,
              mode,
              top_k,
              category,
              tags: tags.length ? tags : undefined,
            }),
          },
        );
      } catch (e: any) {
        results.innerHTML = `<p class="text-red-600 text-sm">${esc(e.message)}</p>`;
        return;
      }
      if (!hits.length) {
        results.innerHTML = `<p class="text-slate-500 text-sm">无结果。</p>`;
        return;
      }
      results.innerHTML = hits
        .map((h) => {
          const metaStr = h.meta && Object.keys(h.meta).length ? JSON.stringify(h.meta) : "";
          const tagStr = h.tags && h.tags.length ? h.tags.join(", ") : "";
          const catStr = h.category ? ` · ${esc(h.category)}` : "";
          return `
          <div class="border border-slate-200 rounded p-3 mb-2">
            <div class="flex justify-between items-center mb-1">
              <span class="text-xs font-mono text-slate-400">${esc(h.id)}${catStr}</span>
              <span class="text-xs px-1.5 py-0.5 rounded bg-emerald-100 text-emerald-700">${h.score.toFixed(4)} <span class="text-emerald-500">${esc(h.score_type)}</span></span>
            </div>
            <div class="text-sm whitespace-pre-wrap">${esc(h.text)}</div>
            ${tagStr ? `<div class="text-xs text-slate-500 mt-1">tags: ${esc(tagStr)}</div>` : ""}
            ${metaStr ? `<pre class="text-xs text-slate-500 mt-1 overflow-x-auto">${esc(metaStr)}</pre>` : ""}
          </div>`;
        })
        .join("");
    };
    btn.addEventListener("click", run);
    qInput.addEventListener("keydown", (e) => {
      if ((e as KeyboardEvent).key === "Enter") run();
    });
  }

  // ── 写入 ──
  const wbtn = document.getElementById("adm-write");
  const wtext = document.getElementById("adm-w-text") as HTMLTextAreaElement | null;
  const wmsg = document.getElementById("adm-write-msg");
  if (wbtn && wtext && wmsg) {
    wbtn.addEventListener("click", async () => {
      const text = wtext.value.trim();
      if (!text) {
        wmsg.textContent = "请输入文本";
        wmsg.className = "text-xs text-amber-600";
        return;
      }
      const dataset = (document.getElementById("adm-w-ds") as HTMLSelectElement).value;
      const category = (document.getElementById("adm-w-cat") as HTMLInputElement).value.trim() || undefined;
      const tags = parseTags((document.getElementById("adm-w-tags") as HTMLInputElement).value);
      wmsg.textContent = "写入中…";
      wmsg.className = "text-xs text-slate-500";
      try {
        const res = await adminApi<{ id: string; commit_ts: number }>(
          `/admin/v1/workspaces/${encodeURIComponent(wsId)}/vectors/upsert`,
          {
            method: "POST",
            body: JSON.stringify({ dataset, text, category, tags: tags.length ? tags : undefined }),
          },
        );
        wmsg.textContent = `✓ 已写入 id=${res.id.slice(0, 8)}…`;
        wmsg.className = "text-xs text-emerald-600";
        wtext.value = "";
      } catch (e: any) {
        wmsg.textContent = `失败：${e.message}`;
        wmsg.className = "text-xs text-red-600";
      }
    });
  }
}
