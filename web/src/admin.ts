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
  if (route === "/admin/tunnel") {
    await renderTunnel(app);
    return;
  }
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
    <div class="flex items-center justify-between mb-4 gap-4">
      <p class="text-sm text-slate-500">本节点共 ${list.length} 个 workspace（跨所有账号/租户）。点击查看详情。</p>
      <a href="#/admin/tunnel" class="text-sm text-blue-600 hover:underline whitespace-nowrap">企微机器人管理 →</a>
    </div>
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

  // fs heat board (search_hits / reads per doc).
  const statsSection =
    w.kind === "fs" ? section("文档热度", `<div id="adm-stats" class="text-sm">加载中…</div>`) : "";

  // Team-memory cleanup view (M4a). Team domain only — personal/dept
  // memories stay owner-visible even for the admin.
  const memSection =
    w.kind === "fs" ? section("团队记忆", `<div id="adm-memories" class="text-sm">加载中…</div>`) : "";

  app.innerHTML = `${header("#/admin", w.name)}
    ${info}
    ${datasetsSection}
    ${statsSection}
    ${memSection}
    ${toolSection}
    ${keysSection}`;
  bindLogout();

  if (w.kind === "fs") {
    initDocStats(id);
    initMemoryAdmin(id);
    initFilesBrowser(id);
  } else initVectorConsole(id);
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

// ── fs doc heat board ───────────────────────────────────
interface DocAccessEntry {
  path: string;
  search_hits: number;
  reads: number;
}

function initDocStats(wsId: string) {
  const root = document.getElementById("adm-stats");
  if (!root) return;
  let days = 30;
  let orderBy: "reads" | "search_hits" = "reads";

  async function load() {
    root!.innerHTML = `<div class="px-3 py-2 text-slate-500">加载中…</div>`;
    let data: { days: number; items: DocAccessEntry[] };
    try {
      data = await adminApi(
        `/admin/v1/workspaces/${encodeURIComponent(wsId)}/stats/docs?days=${days}&order_by=${orderBy}&limit=50`,
      );
    } catch (e: any) {
      root!.innerHTML = `<div class="px-3 py-2 text-red-600">${esc(e.message)}</div>`;
      return;
    }

    const sortBtn = (key: "reads" | "search_hits", label: string) =>
      `<button data-order="${key}" class="adm-stats-order px-2 py-0.5 rounded text-xs ${
        orderBy === key
          ? "bg-slate-700 text-white"
          : "bg-slate-100 text-slate-600 hover:bg-slate-200"
      }">${label}</button>`;
    const daysBtn = (n: number) =>
      `<button data-days="${n}" class="adm-stats-days px-2 py-0.5 rounded text-xs ${
        days === n ? "bg-slate-700 text-white" : "bg-slate-100 text-slate-600 hover:bg-slate-200"
      }">${n} 天</button>`;

    const controls = `
      <div class="flex items-center gap-2 px-3 py-2 border-b border-slate-100 flex-wrap">
        <span class="text-xs text-slate-400">窗口</span>${[7, 30, 90].map(daysBtn).join("")}
        <span class="text-xs text-slate-400 ml-3">排序</span>
        ${sortBtn("reads", "按读取")}${sortBtn("search_hits", "按命中")}
        <span class="text-xs text-slate-400 ml-auto" title="命中=出现在搜索结果里(top-k 曝光,含 agent 检索);读取=内容被实际取出。grep/SQL 扫描不计;~30s 落库一次">ⓘ 口径</span>
      </div>`;

    const table = data.items.length
      ? `<table class="w-full text-left text-sm">
          <thead class="text-xs uppercase tracking-wide text-slate-500">
            <tr><th class="px-3 py-2">路径</th><th class="px-3 py-2 text-right">搜索命中</th><th class="px-3 py-2 text-right">读取</th></tr>
          </thead>
          <tbody>${data.items
            .map(
              (it) => `<tr class="border-t border-slate-100">
                <td class="px-3 py-2 font-mono text-xs break-all">${esc(it.path)}</td>
                <td class="px-3 py-2 text-right">${it.search_hits}</td>
                <td class="px-3 py-2 text-right">${it.reads}</td>
              </tr>`,
            )
            .join("")}</tbody>
        </table>`
      : `<p class="text-sm text-slate-500 px-3 py-2">近 ${days} 天没有访问记录。计数约 30 秒落库一次,刚发生的访问稍等再刷。</p>`;

    root!.innerHTML = controls + table;
    root!.querySelectorAll(".adm-stats-days").forEach((el) =>
      el.addEventListener("click", () => {
        days = Number((el as HTMLElement).dataset.days);
        load();
      }),
    );
    root!.querySelectorAll(".adm-stats-order").forEach((el) =>
      el.addEventListener("click", () => {
        orderBy = (el as HTMLElement).dataset.order as "reads" | "search_hits";
        load();
      }),
    );
  }

  load();
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
        const sizeCell =
          e.size_bytes == null ? "" : fmtBytes(e.size_bytes);
        return `<tr class="border-t border-slate-100">
          <td class="px-3 py-1.5">${icon} ${nameCell}</td>
          <td class="px-3 py-1.5 text-xs text-slate-500 whitespace-nowrap text-right">${sizeCell}</td>
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

// ── 企微机器人 (veda-tunnel) ─────────────────────────────
// Manages WeCom bots on the standalone veda-tunnel service. tunnel's admin
// API is a separate process (:9100), reached same-origin via an nginx reverse
// proxy: /tunnel/v1/* → 127.0.0.1:9100/admin/*. Reuses the admin token
// (configure tunnel's [admin].token = VEDA_ADMIN_TOKEN). Unlike veda-server,
// tunnel returns bare JSON (no {success,data} envelope), so it has its own
// fetch wrapper.

const TUNNEL_BASE = "/tunnel/v1";

type TunnelBot = {
  name: string;
  bot_id: string;
  workspace: string;
  project?: string;
  mode: string;
  limit: number;
  prompt?: string;
  veda_key_masked: string;
  conn_state?: "connecting" | "subscribed" | "reconnecting" | "down";
  connected_since?: string;
  last_msg_at?: string;
  msg_count: number;
  error_count: number;
  last_error?: string;
};

async function tunnelApi<T = any>(path: string, opts: RequestInit = {}): Promise<T> {
  const token = getAdminToken();
  const res = await fetch(TUNNEL_BASE + path, {
    ...opts,
    headers: {
      "Content-Type": "application/json",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...((opts.headers as Record<string, string>) || {}),
    },
  });
  if (res.status === 401) throw new Error("UNAUTHORIZED");
  const body = await res.json().catch(() => ({}) as any);
  // 404 with no error body = surface disabled (token unset) or proxy missing.
  if (res.status === 404 && !body.error) throw new Error("DISABLED");
  if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
  return body as T;
}

function tunnelError(app: HTMLElement, e: Error) {
  // Unlike the console-wide 401 handling, a tunnel 401 must NOT clear the
  // stored token: the console and the tunnel are different backends, and if
  // their tokens ever diverge, clearing would trap the user in a
  // login-kick loop. Show an inline hint instead.
  const msg =
    e.message === "UNAUTHORIZED"
      ? "当前登录的 admin token 对企微管理面无效（tunnel 使用生产 admin token）。请退出后用生产 token 重新登录。"
      : e.message === "DISABLED"
        ? "企微机器人管理未接通：确认 veda-tunnel 在运行、nginx 已反代 /tunnel/v1/，且 tunnel 的 admin token 与此处一致。"
        : `错误：${e.message}`;
  app.innerHTML = `${header("#/admin", "企微机器人")}<p class="text-red-600 text-sm">${esc(msg)}</p>`;
  bindLogout();
}

function connBadge(s?: string): string {
  const m: Record<string, [string, string]> = {
    subscribed: ["在线", "bg-emerald-100 text-emerald-700"],
    connecting: ["连接中", "bg-amber-100 text-amber-700"],
    reconnecting: ["重连中", "bg-amber-100 text-amber-700"],
    down: ["离线", "bg-rose-100 text-rose-700"],
  };
  const [label, cls] = m[s ?? ""] ?? ["未知", "bg-slate-100 text-slate-500"];
  return `<span class="text-xs px-1.5 py-0.5 rounded font-medium ${cls}">${label}</span>`;
}

// ── 问答统计（qa_log，docs/archive/plans/veda-tunnel-qa-log.md）──

type QaStats = {
  days: number;
  total: number;
  outcomes: Record<string, number>;
  feedback_up: number;
  feedback_down: number;
};
type QaRow = {
  id: number;
  ts: string;
  user_id: string;
  query: string;
  outcome: string;
  hit_count: number;
  citation_count: number;
  latency_ms: number;
  answer_text: string | null;
  /** JSON array `[{tool, detail}]` of the retrieval steps behind the answer. */
  tool_trace: string | null;
  up_count: number;
  down_count: number;
};

const OUTCOME_BADGE: Record<string, [string, string]> = {
  answered: ["已回答", "bg-emerald-100 text-emerald-700"],
  no_context: ["无内容", "bg-amber-100 text-amber-800"],
  ungrounded: ["无引用", "bg-orange-100 text-orange-700"],
  raw_search: ["纯检索", "bg-sky-100 text-sky-700"],
  error: ["错误", "bg-rose-100 text-rose-700"],
  upstream_error: ["上游故障", "bg-purple-100 text-purple-700"],
  throttled: ["限流", "bg-slate-100 text-slate-500"],
  disabled: ["未启用", "bg-slate-100 text-slate-500"],
};

function outcomeBadge(o: string): string {
  const [label, cls] = OUTCOME_BADGE[o] ?? [o, "bg-slate-100 text-slate-500"];
  return `<span class="text-xs px-1.5 py-0.5 rounded font-medium ${cls}">${esc(label)}</span>`;
}

/** Collapsible retrieval story: one line per tool step (search / read_file). */
function traceDetails(raw: string | null): string {
  if (!raw) return "";
  let steps: { tool?: string; detail?: string }[];
  try {
    steps = JSON.parse(raw);
  } catch {
    return "";
  }
  if (!Array.isArray(steps) || !steps.length) return "";
  const lines = steps
    .map((s) => {
      const label = s.tool === "read_file" ? "📄 查阅" : "🔍 检索";
      return `<div>${label} ${esc(s.detail ?? "")}</div>`;
    })
    .join("");
  return `<details class="mt-1"><summary class="text-xs text-slate-400 cursor-pointer">过程（${steps.length} 步）</summary><div class="text-xs text-slate-600 mt-1 space-y-0.5">${lines}</div></details>`;
}

function statCard(label: string, value: string, sub = ""): string {
  return `<div class="bg-white border border-slate-200 rounded-lg p-4">
    <div class="text-xs text-slate-500 mb-1">${esc(label)}</div>
    <div class="text-2xl font-bold">${value}</div>
    ${sub ? `<div class="text-xs text-slate-400 mt-1">${sub}</div>` : ""}
  </div>`;
}

async function renderQaStats(root: HTMLElement) {
  root.innerHTML = `<p class="text-sm text-slate-400">统计加载中…</p>`;
  let stats: QaStats;
  try {
    stats = await tunnelApi<QaStats>("/stats?days=7");
  } catch {
    root.innerHTML = ""; // 老版本 tunnel 无此端点时静默隐藏
    return;
  }
  const noCtx = stats.outcomes["no_context"] ?? 0;
  const pct = (n: number) => (stats.total ? `${((n / stats.total) * 100).toFixed(0)}%` : "—");
  root.innerHTML = `
    <h2 class="text-lg font-semibold mb-3">问答统计（近 7 天）</h2>
    <div class="grid grid-cols-1 sm:grid-cols-3 gap-3 mb-4">
      ${statCard("问答总量", String(stats.total), Object.entries(stats.outcomes).map(([k, v]) => `${esc(k)} ${v}`).join(" · "))}
      ${statCard("知识库无内容", `${noCtx} <span class="text-sm font-normal text-slate-400">(${pct(noCtx)})</span>`, "这些问题就是「缺什么文档」清单")}
      ${statCard("用户反馈", `👍 ${stats.feedback_up} · 👎 ${stats.feedback_down}`, "点踩的进下方 bad case")}
    </div>
    <div class="flex items-center gap-3 mb-2">
      <h3 class="text-sm font-semibold">问答明细</h3>
      <select id="qa-outcome" class="text-xs border border-slate-300 rounded px-2 py-1">
        <option value="">全部结果</option>
        <option value="no_context">无内容</option>
        <option value="ungrounded">无引用</option>
        <option value="answered">已回答</option>
        <option value="error">错误</option>
        <option value="upstream_error">上游故障</option>
      </select>
      <label class="text-xs text-slate-600 flex items-center gap-1">
        <input type="checkbox" id="qa-down"> 仅被踩
      </label>
    </div>
    <div id="qa-rows"></div>`;
  const refresh = () => {
    const outcome = (document.getElementById("qa-outcome") as HTMLSelectElement).value;
    const down = (document.getElementById("qa-down") as HTMLInputElement).checked;
    void loadQaRows(document.getElementById("qa-rows")!, outcome, down);
  };
  document.getElementById("qa-outcome")!.addEventListener("change", refresh);
  document.getElementById("qa-down")!.addEventListener("change", refresh);
  refresh();
}

async function loadQaRows(box: HTMLElement, outcome: string, downVoted: boolean) {
  box.innerHTML = `<p class="text-sm text-slate-400">加载中…</p>`;
  let rows: QaRow[];
  try {
    const q = new URLSearchParams({ size: "20" });
    if (outcome) q.set("outcome", outcome);
    if (downVoted) q.set("down_voted", "true");
    rows = await tunnelApi<QaRow[]>(`/qa-log?${q}`);
  } catch (e: any) {
    box.innerHTML = `<p class="text-sm text-rose-600">加载失败：${esc(e.message)}</p>`;
    return;
  }
  if (!rows.length) {
    box.innerHTML = `<p class="text-sm text-slate-500 p-3 bg-white border border-slate-200 rounded-lg">没有匹配的记录。</p>`;
    return;
  }
  const tr = rows
    .map(
      (r) => `
    <tr class="border-t border-slate-100 align-top">
      <td class="px-3 py-2 text-xs text-slate-400 whitespace-nowrap">
        ${esc(new Date(r.ts).toLocaleString("zh-CN", { hour12: false }))}
        ${r.user_id ? `<div class="mt-0.5 text-slate-500">${esc(r.user_id)}</div>` : ""}
      </td>
      <td class="px-3 py-2 text-sm max-w-md">
        <div class="font-medium">${esc(r.query)}</div>
        ${r.answer_text ? `<details class="mt-1"><summary class="text-xs text-slate-400 cursor-pointer">答案</summary><div class="text-xs text-slate-600 whitespace-pre-wrap mt-1">${esc(r.answer_text)}</div></details>` : ""}
        ${traceDetails(r.tool_trace)}
      </td>
      <td class="px-3 py-2">${outcomeBadge(r.outcome)}</td>
      <td class="px-3 py-2 text-xs text-slate-500 whitespace-nowrap">${r.citation_count} 引用 · ${(r.latency_ms / 1000).toFixed(1)}s</td>
      <td class="px-3 py-2 text-xs whitespace-nowrap">${r.up_count ? `👍${r.up_count} ` : ""}${r.down_count ? `<span class="text-rose-600">👎${r.down_count}</span>` : ""}</td>
    </tr>`,
    )
    .join("");
  box.innerHTML = `<div class="bg-white border border-slate-200 rounded-lg overflow-x-auto">
    <table class="w-full text-left">
      <thead class="text-xs uppercase tracking-wide text-slate-500">
        <tr><th class="px-3 py-2 font-semibold">时间 / 提问人</th><th class="px-3 py-2 font-semibold">问题 / 答案 / 过程</th><th class="px-3 py-2 font-semibold">结果</th><th class="px-3 py-2 font-semibold">质量</th><th class="px-3 py-2 font-semibold">反馈</th></tr>
      </thead>
      <tbody>${tr}</tbody>
    </table>
  </div>`;
}

async function renderTunnel(app: HTMLElement) {
  app.innerHTML = `${header("#/admin", "企微机器人")}<p class="text-slate-500 text-sm">加载中…</p>`;
  bindLogout();
  let bots: TunnelBot[];
  try {
    bots = await tunnelApi<TunnelBot[]>("/bots");
  } catch (e: any) {
    tunnelError(app, e);
    return;
  }
  const rows = bots
    .map((b) => {
      // Lightweight badge + title preview when a custom persona is configured.
      const promptPreview = b.prompt ? b.prompt.slice(0, 100) + (b.prompt.length > 100 ? "…" : "") : "";
      const promptBadge = b.prompt
        ? ` <span class="text-[11px] px-1.5 py-0.5 rounded bg-slate-100 text-slate-500" title="${attr(promptPreview)}">自定义 prompt</span>`
        : "";
      return `
      <tr class="border-t border-slate-100">
        <td class="px-3 py-2">
          <div class="font-medium flex items-center gap-2">${esc(b.name)} ${connBadge(b.conn_state)}</div>
          <div class="text-xs text-slate-400 font-mono">${esc(b.bot_id)}</div>
        </td>
        <td class="px-3 py-2 text-sm">${esc(b.workspace)}${b.project ? ` <span class="text-slate-400">/ ${esc(b.project)}</span>` : ""}</td>
        <td class="px-3 py-2 text-xs font-mono text-slate-500">${esc(b.veda_key_masked)}</td>
        <td class="px-3 py-2 text-sm whitespace-nowrap">${esc(b.mode)} · ${b.limit}${promptBadge}</td>
        <td class="px-3 py-2 text-sm text-center">${b.msg_count}${b.error_count ? ` <span class="text-rose-500">/${b.error_count}</span>` : ""}</td>
        <td class="px-3 py-2 text-right whitespace-nowrap">
          <button data-act="edit" data-id="${attr(b.bot_id)}" class="text-xs text-blue-600 hover:underline">编辑</button>
          <button data-act="reconnect" data-id="${attr(b.bot_id)}" class="text-xs text-slate-500 hover:underline ml-2">重连</button>
          <button data-act="delete" data-id="${attr(b.bot_id)}" data-name="${attr(b.name)}" class="text-xs text-rose-600 hover:underline ml-2">删除</button>
        </td>
      </tr>`;
    })
    .join("");
  const table = bots.length
    ? `<div class="bg-white border border-slate-200 rounded-lg overflow-x-auto">
        <table class="w-full text-left">
          <thead class="text-xs uppercase tracking-wide text-slate-500">
            <tr>
              <th class="px-3 py-2 font-semibold">机器人</th>
              <th class="px-3 py-2 font-semibold">workspace</th>
              <th class="px-3 py-2 font-semibold">veda key</th>
              <th class="px-3 py-2 font-semibold">检索</th>
              <th class="px-3 py-2 font-semibold text-center">消息/错误</th>
              <th class="px-3 py-2 font-semibold text-right">操作</th>
            </tr>
          </thead>
          <tbody>${rows}</tbody>
        </table>
      </div>`
    : `<p class="text-sm text-slate-500 p-4 bg-white border border-slate-200 rounded-lg">还没有机器人，点右上「+ 新增」添加。</p>`;
  const lastErr = bots.find((b) => b.last_error);
  app.innerHTML = `${header("#/admin", "企微机器人")}
    <div class="flex items-center justify-between mb-4">
      <p class="text-sm text-slate-500">共 ${bots.length} 个企微机器人（veda-tunnel 长连接）。</p>
      <button id="tn-add" class="bg-slate-900 text-white px-3 py-1.5 rounded text-sm font-medium hover:bg-slate-700">+ 新增</button>
    </div>
    ${table}
    ${lastErr ? `<p class="text-xs text-rose-500 mt-3">最近错误（${esc(lastErr.name)}）：${esc(lastErr.last_error)}</p>` : ""}
    <div id="qa-stats" class="mt-8"></div>`;
  bindLogout();
  void renderQaStats(document.getElementById("qa-stats")!);
  document.getElementById("tn-add")!.addEventListener("click", () => openBotForm(app));
  app.querySelectorAll("button[data-act]").forEach((el) => {
    const btn = el as HTMLElement;
    const id = btn.dataset.id!;
    const act = btn.dataset.act!;
    btn.addEventListener("click", async () => {
      if (act === "edit") {
        const bot = bots.find((b) => b.bot_id === id);
        if (bot) openBotForm(app, bot);
      } else if (act === "reconnect") {
        try {
          await tunnelApi(`/bots/${encodeURIComponent(id)}/reconnect`, { method: "POST" });
          renderTunnel(app);
        } catch (e: any) {
          alert(`重连失败：${e.message}`);
        }
      } else if (act === "delete") {
        if (!confirm(`删除机器人「${btn.dataset.name}」？连接会立即断开。`)) return;
        try {
          await tunnelApi(`/bots/${encodeURIComponent(id)}`, { method: "DELETE" });
          renderTunnel(app);
        } catch (e: any) {
          alert(`删除失败：${e.message}`);
        }
      }
    });
  });
}

function tnField(
  name: string,
  label: string,
  value: string,
  placeholder: string,
  opts: { readonly?: boolean; type?: string } = {},
): string {
  const ro = opts.readonly ? "readonly" : "";
  const roCls = opts.readonly ? "bg-slate-100 text-slate-500" : "";
  return `<div>
    <label class="block text-xs text-slate-500 mb-1">${esc(label)}</label>
    <input name="${name}" type="${opts.type || "text"}" value="${attr(value)}" placeholder="${attr(placeholder)}" ${ro}
      class="w-full border border-slate-300 rounded px-2 py-1.5 text-sm ${roCls}">
  </div>`;
}

function openBotForm(app: HTMLElement, bot?: TunnelBot) {
  const editing = !!bot;
  const title = editing ? `编辑：${bot!.name}` : "新增企微机器人";
  const body = `
    <form id="tn-form" class="space-y-3">
      ${tnField("name", "名称", editing ? bot!.name : "", "hr-helper")}
      ${tnField("bot_id", "bot_id", editing ? bot!.bot_id : "", "企微机器人 id", { readonly: editing })}
      ${tnField("secret", "secret", "", editing ? "留空 = 不修改" : "长连接密钥", { type: "password" })}
      ${tnField("veda_key", "veda key (wk_)", "", editing ? `当前 ${bot!.veda_key_masked}，留空 = 不修改` : "wk_...", { type: "password" })}
      ${tnField("workspace", "workspace 标注", editing ? bot!.workspace : "", "hr-kb")}
      ${tnField("project", "project 标注（可选）", editing && bot!.project ? bot!.project : "", "可选")}
      <div class="flex gap-3">
        <div class="flex-1">
          <label class="block text-xs text-slate-500 mb-1">检索模式</label>
          <select name="mode" class="w-full border border-slate-300 rounded px-2 py-1.5 text-sm">
            ${["hybrid", "semantic", "fulltext"].map((mo) => `<option value="${mo}" ${editing && bot!.mode === mo ? "selected" : ""}>${mo}</option>`).join("")}
          </select>
        </div>
        <div class="w-24">
          <label class="block text-xs text-slate-500 mb-1">limit</label>
          <input name="limit" type="number" min="1" max="100" value="${editing ? bot!.limit : 8}" class="w-full border border-slate-300 rounded px-2 py-1.5 text-sm">
        </div>
      </div>
      <div>
        <label class="block text-xs text-slate-500 mb-1">bot prompt（角色/风格，留空 = 服务端默认）</label>
        <textarea name="prompt" rows="5" maxlength="4000" placeholder="# 角色&#10;DAL 答疑机器人。回答简洁,操作类问题给编号步骤;涉及工单引导到 OnePaaS 平台。"
          class="w-full border border-slate-300 rounded px-2 py-1.5 text-sm font-mono">${editing && bot!.prompt ? esc(bot!.prompt) : ""}</textarea>
        <div class="flex justify-between items-baseline mt-1 gap-3">
          <p class="text-[11px] text-slate-400">追加在内置知识库协议(检索策略/引用/拒答规则)之后,只定义角色与风格,不会覆盖协议。</p>
          <span id="tn-prompt-count" class="text-[11px] text-slate-400 font-mono shrink-0"></span>
        </div>
      </div>
      <div class="flex items-center gap-3 pt-2">
        <button type="submit" class="bg-slate-900 text-white px-4 py-1.5 rounded text-sm font-medium hover:bg-slate-700">${editing ? "保存" : "创建"}</button>
        <span id="tn-form-msg" class="text-xs"></span>
      </div>
    </form>`;
  showModal(title, body);
  const form = document.getElementById("tn-form") as HTMLFormElement;
  const msg = document.getElementById("tn-form-msg")!;
  // Live prompt char counter; warns in amber as it nears the 4000 cap.
  const promptEl = form.querySelector<HTMLTextAreaElement>('[name="prompt"]')!;
  const promptCount = document.getElementById("tn-prompt-count")!;
  const syncPromptCount = () => {
    const n = promptEl.value.length;
    promptCount.textContent = `${n} / 4000`;
    promptCount.className = `text-[11px] font-mono shrink-0 ${n > 3800 ? "text-amber-600" : "text-slate-400"}`;
  };
  promptEl.addEventListener("input", syncPromptCount);
  syncPromptCount();
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const fd = new FormData(form);
    const get = (k: string) => ((fd.get(k) as string) || "").trim();
    const payload: any = {
      name: get("name"),
      bot_id: editing ? bot!.bot_id : get("bot_id"),
      secret: get("secret"),
      veda_key: get("veda_key"),
      workspace: get("workspace"),
      project: get("project") || undefined,
      mode: fd.get("mode") as string,
      limit: parseInt(get("limit"), 10) || 8,
      // Whole-config semantics (admin body = BotConfig): empty textarea
      // clears the persona back to the server default.
      prompt: ((fd.get("prompt") as string) || "").trim() || undefined,
    };
    if (!payload.name || !payload.bot_id || !payload.workspace) {
      msg.textContent = "name / bot_id / workspace 必填";
      msg.className = "text-xs text-amber-600";
      return;
    }
    if (!editing && (!payload.secret || !payload.veda_key)) {
      msg.textContent = "新增时 secret / veda_key 必填";
      msg.className = "text-xs text-amber-600";
      return;
    }
    msg.textContent = "提交中…";
    msg.className = "text-xs text-slate-500";
    try {
      if (editing) {
        await tunnelApi(`/bots/${encodeURIComponent(bot!.bot_id)}`, {
          method: "PUT",
          body: JSON.stringify(payload),
        });
      } else {
        await tunnelApi("/bots", { method: "POST", body: JSON.stringify(payload) });
      }
      closeModal();
      renderTunnel(app);
    } catch (err: any) {
      msg.textContent = `失败：${err.message}`;
      msg.className = "text-xs text-red-600";
    }
  });
}

// ── team-memory cleanup (M4a) ───────────────────────────
interface AdminMemoryItem {
  id: number;
  topic?: string | null;
  kind: string;
  content: string;
  updated_by: string;
  updated_at: string;
}

function initMemoryAdmin(wsId: string) {
  const root = document.getElementById("adm-memories");
  if (!root) return;
  let order: "updated_at" | "last_used_at" = "updated_at";
  let kind = "";
  let page = 1;

  async function load() {
    root!.innerHTML = `<div class="px-3 py-2 text-slate-500">加载中…</div>`;
    let data: { items: AdminMemoryItem[]; total: number; page: number; size: number };
    try {
      let q = `workspace=${encodeURIComponent(wsId)}&order=${order}&page=${page}&size=50`;
      if (kind) q += `&kind=${kind}`;
      data = await adminApi(`/admin/v1/memories?${q}`);
    } catch (e: any) {
      root!.innerHTML = `<div class="px-3 py-2 text-red-600">${esc(e.message)}</div>`;
      return;
    }
    const lastPage = Math.max(1, Math.ceil(data.total / data.size));
    const rows = data.items.length
      ? `<table class="w-full text-left text-sm">
          <thead class="text-xs uppercase tracking-wide text-slate-500">
            <tr><th class="px-3 py-2">内容</th><th class="px-3 py-2">类型</th><th class="px-3 py-2">主题</th><th class="px-3 py-2">署名 · 时间</th><th class="px-3 py-2"></th></tr>
          </thead>
          <tbody>${data.items
            .map(
              (m) => `<tr class="border-t border-slate-100">
                <td class="px-3 py-2 max-w-md"><span class="font-mono text-xs text-slate-400">[mem:${m.id}]</span> ${esc(m.content)}</td>
                <td class="px-3 py-2 text-xs">${esc(m.kind)}</td>
                <td class="px-3 py-2 text-xs">${esc(m.topic) || "—"}</td>
                <td class="px-3 py-2 text-xs text-slate-500 whitespace-nowrap">${esc(m.updated_by.slice(0, 8))} · ${fmtTime(m.updated_at)}</td>
                <td class="px-3 py-2 text-right"><button data-mem-del="${m.id}" class="text-xs text-red-600 hover:underline">删除</button></td>
              </tr>`,
            )
            .join("")}</tbody>
        </table>`
      : `<p class="text-sm text-slate-500 px-3 py-2">没有团队记忆。</p>`;
    root!.innerHTML = `
      <div class="flex items-center gap-2 px-3 py-2 border-b border-slate-100 text-xs">
        <select id="adm-mem-order" class="border border-slate-300 rounded px-1 py-0.5">
          <option value="updated_at"${order === "updated_at" ? " selected" : ""}>最近编辑</option>
          <option value="last_used_at"${order === "last_used_at" ? " selected" : ""}>最近被检索（热度）</option>
        </select>
        <select id="adm-mem-kind" class="border border-slate-300 rounded px-1 py-0.5">
          <option value="">全部类型</option>
          ${["fact", "preference", "decision", "procedure", "derived"]
            .map((k) => `<option value="${k}"${kind === k ? " selected" : ""}>${k}</option>`)
            .join("")}
        </select>
        <span class="text-slate-500">共 ${data.total} 条</span>
        <div class="grow"></div>
        ${page > 1 ? `<button id="adm-mem-prev" class="border border-slate-300 px-2 py-0.5 rounded">上一页</button>` : ""}
        <span class="text-slate-500">${data.page}/${lastPage}</span>
        ${page < lastPage ? `<button id="adm-mem-next" class="border border-slate-300 px-2 py-0.5 rounded">下一页</button>` : ""}
      </div>
      ${rows}`;
    document.getElementById("adm-mem-order")!.addEventListener("change", (e) => {
      order = (e.target as HTMLSelectElement).value as typeof order;
      page = 1;
      load();
    });
    document.getElementById("adm-mem-kind")!.addEventListener("change", (e) => {
      kind = (e.target as HTMLSelectElement).value;
      page = 1;
      load();
    });
    document.getElementById("adm-mem-prev")?.addEventListener("click", () => {
      page -= 1;
      load();
    });
    document.getElementById("adm-mem-next")?.addEventListener("click", () => {
      page += 1;
      load();
    });
    root!.querySelectorAll("[data-mem-del]").forEach((el) =>
      el.addEventListener("click", async (e) => {
        const id = (e.currentTarget as HTMLElement).dataset.memDel!;
        if (!confirm("删除这条团队记忆？不可恢复，全组检索立即消失。")) return;
        try {
          await adminApi(`/admin/v1/memories/${id}?workspace=${encodeURIComponent(wsId)}`, {
            method: "DELETE",
          });
          load();
        } catch (err: any) {
          alert(`删除失败：${err.message}`);
        }
      }),
    );
  }

  load();
}
