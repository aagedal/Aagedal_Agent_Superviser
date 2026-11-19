const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const GIB = 1024 ** 3;
const FAST_ACTIVE_PARAMS = 12; // as in crates/core/src/catalog.rs
const gib = (bytes) => `${(bytes / GIB).toFixed(1)} GB`;

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (key === "class") node.className = value;
    else if (value !== undefined && value !== null && value !== false) node.setAttribute(key, value);
  }
  for (const child of children.flat()) {
    if (child !== null && child !== undefined) node.append(child instanceof Node ? child : String(child));
  }
  return node;
}

function chip(text, kind = "") {
  return el("span", { class: `chip ${kind}` }, text);
}

// --- Tabs ---
function showTab(name) {
  if (name === "installed") {
    // The old tab's name, from links made before Models and Installed were one tab.
    modelFilter = "local";
    name = "models";
  }
  for (const button of document.querySelectorAll("nav button")) button.classList.toggle("active", button.dataset.tab === name);
  for (const section of document.querySelectorAll("main section")) section.hidden = section.id !== name;
  if (name === "models") renderModelsTab();
  if (name === "projects") renderProjects();
  if (name === "guide") renderGuide();
  if (name === "connect") renderConnect();
  if (name === "safety") {
    renderJudge();
    renderCalls();
  }
}
for (const button of document.querySelectorAll("nav button")) button.addEventListener("click", () => showTab(button.dataset.tab));

// --- State shared by the tabs ---
let overview = null;
let installedIds = new Set();
let localByRepo = new Map(); // lowercased repo -> installed models (any source) that fit
let modelFilter = "suggested"; // or "local"
const downloads = new Map(); // id -> {done, total}
const downloadButtons = new Map(); // id -> Map(box -> render function); a model can show in the picks and the full list

function rerenderAllDownloads() {
  for (const id of downloadButtons.keys()) rerenderDownload(id);
}

function rerenderDownload(id) {
  for (const [box, render] of downloadButtons.get(id) || []) {
    if (box.isConnected) render();
    else downloadButtons.get(id).delete(box);
  }
}

async function refreshOverview() {
  overview = await invoke("overview");
  const hw = overview.hardware;
  const kind = hw.unified_memory ? "This Mac" : "This computer";
  const judge = overview.model_budget < hw.model_budget ? ` (${gib(hw.model_budget - overview.model_budget)} is kept for the judge, see Safety)` : "";
  $("machine").textContent = `${kind} has ${wholeGib(hw.total_memory)} of memory. Each choice below fits; they differ in how well they code, how quickly they answer and how much memory they leave for your other apps.`;
  $("budget").textContent = `Up to ${gib(overview.model_budget)} can go to a model${judge}. Only models that fit are shown.`;
}

const wholeGib = (bytes) => `${Math.round(bytes / GIB)} GB`;

async function refreshInstalledIds() {
  const installed = await invoke("installed_models");
  installedIds = new Set(installed.map((m) => m.id));
  localByRepo = new Map();
  for (const m of installed) {
    if (!m.model.repo || !m.fit) continue;
    const key = m.model.repo.toLowerCase();
    localByRepo.set(key, [...(localByRepo.get(key) || []), m]);
  }
  return installed;
}

// --- Models tab ---

// The same model already on this machine, possibly another quant from another app.
function localCopy(model, id) {
  const local = localByRepo.get(model.repo.toLowerCase()) || [];
  return local.find((m) => m.id === id) || local[0];
}

// `compact` leaves out the second download button for this model's own quant; the full list keeps it.
function downloadControl(model, { compact = false } = {}) {
  const id = `${model.repo}:${model.quant.label}`;
  const box = el("div", { class: "actions" });
  const render = () => {
    box.replaceChildren();
    const progress = downloads.get(id);
    const local = localCopy(model, id);
    if (local && isLoaded(local.id)) {
      box.append(chip("Loaded", "good"));
    } else if (installedIds.has(id) && local?.source === "app") {
      box.append(chip("Installed", "good"), el("button", { class: compact ? "primary" : "", onclick: () => loadModel(id) }, "Load"));
    } else if (local && !progress) {
      box.append(chip("On this Mac", "good"), el("button", { class: "primary", onclick: () => loadModel(local.id) }, "Load"));
      if (local.id !== id && !compact) box.append(el("button", { onclick: () => download(model, id) }, `Download ${model.quant.label}`));
    } else if (progress) {
      const bar = el("progress", { max: progress.total || 1, value: progress.done });
      const pct = progress.total ? Math.floor((progress.done / progress.total) * 100) : 0;
      box.append(bar, el("span", { class: "small muted" }, `${pct}%`), el("button", { onclick: () => invoke("cancel_download", { id }) }, "Cancel"));
    } else {
      box.append(el("button", { class: "primary", onclick: () => download(model, id) }, `Download (${wholeGib(model.quant.size)})`));
    }
  };
  if (!downloadButtons.has(id)) downloadButtons.set(id, new Map());
  downloadButtons.get(id).set(box, render);
  render();
  return box;
}

async function download(model, id) {
  downloads.set(id, { done: 0, total: model.quant.size });
  rerenderDownload(id);
  try {
    await invoke("download_model", { model });
  } catch (e) {
    downloads.delete(id);
    rerenderDownload(id);
    alert(e);
  }
}

listen("download", async ({ payload }) => {
  if (payload.finished || payload.error) {
    downloads.delete(payload.id);
    if (payload.error && !payload.error.includes("cancelled")) alert(`Download failed: ${payload.error}`);
    await refreshInstalledIds();
  } else {
    downloads.set(payload.id, { done: payload.done, total: payload.total });
  }
  rerenderDownload(payload.id);
  if (payload.id === judgeDownload) renderJudge();
});

// An (i) that shows `content` on hover, or keeps it open after a click.
function infoButton(...content) {
  const wrap = el("span", { class: "info-wrap" });
  const button = el("button", {
    class: "info",
    title: "Details",
    "aria-label": "Details",
    onclick: (event) => {
      event.stopPropagation();
      const open = !wrap.classList.contains("open");
      closePopovers();
      wrap.classList.toggle("open", open);
    },
  }, "i");
  wrap.append(button, el("div", { class: "popover" }, ...content));
  return wrap;
}

function closePopovers() {
  for (const open of document.querySelectorAll(".info-wrap.open")) open.classList.remove("open");
}
document.addEventListener("click", (event) => {
  if (!event.target.closest(".popover")) closePopovers();
});

function modelDetails(model, fit) {
  const id = `${model.repo}:${model.quant.label}`;
  const local = localCopy(model, id);
  const rows = [
    ["File", `${model.quant.label}, ${gib(model.quant.size)} download`],
    ["Context", `${Math.round(fit.context / 1024)}k tokens`],
    ["Memory", `about ${gib(fit.required)} while running`],
  ];
  if (model.active_params) rows.push(["Speed", `reads ${model.active_params}B parameters per token${model.active_params <= FAST_ACTIVE_PARAMS ? ", so it answers quickly" : ""}`]);
  if (local) rows.push(["On this Mac", `${local.model.quant.label} from ${local.source_label}`]);
  return [
    el("div", { class: "title" }, model.name),
    model.description ? el("p", { class: "muted" }, model.description) : null,
    el("dl", {}, rows.flatMap(([term, text]) => [el("dt", {}, term), el("dd", {}, text)])),
  ];
}

function modelCard(model, fit, recommended) {
  return el(
    "div",
    { class: "card" },
    el(
      "div",
      { class: "body" },
      el("div", { class: "title" }, model.name),
      model.description ? el("p", { class: "muted small" }, model.description) : null,
      recommended ? el("div", { class: "chips" }, chip("Recommended for this machine", "accent")) : null,
    ),
    infoButton(...modelDetails(model, fit)),
    downloadControl(model),
  );
}

const ROLES = {
  most_capable: "Most capable",
  fast: "Capable and fast",
  light: "Lighter",
  basic: "For basic tasks",
};

function skill(capability) {
  if (capability >= 8) return "Handles bigger coding tasks";
  if (capability >= 6) return "Good for everyday coding";
  if (capability >= 4) return "Fine for smaller changes";
  return "Basic tasks only";
}

function pickCard(pick) {
  const left = overview.hardware.total_memory - pick.fit.required;
  const summary = [skill(pick.capability || 0), pick.fast ? "Quick replies" : "Slower replies", `Leaves ${wholeGib(left)} for your other apps`].join(" · ");
  return el(
    "div",
    { class: "card pick" },
    el("div", { class: "body" }, el("div", { class: "title" }, ROLES[pick.role], el("span", { class: "muted" }, pick.name)), el("p", { class: "muted" }, summary)),
    infoButton(...modelDetails(pick, pick.fit)),
    downloadControl(pick, { compact: true }),
  );
}

async function renderOffers() {
  const { picks, offers, hidden } = await invoke("catalog_offers");
  $("picks").replaceChildren(...picks.map(pickCard));
  const list = $("offers");
  list.replaceChildren();
  if (offers.length === 0) {
    $("picks").append(el("div", { class: "banner" }, "None of the suggested models fit in this machine's memory. Coding agents need a model with a long context, which takes more memory than this machine can spare."));
  }
  for (const offer of offers) list.append(modelCard(offer, offer.fit, offer.recommended));
  $("hidden-note").textContent = hidden ? `${hidden} larger suggested model${hidden === 1 ? " is" : "s are"} hidden because ${hidden === 1 ? "it needs" : "they need"} more memory than this machine has.` : "";
}

$("search-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const query = $("search").value.trim();
  if (!query) return;
  const results = $("results");
  results.replaceChildren(el("p", { class: "muted" }, "Searching…"));
  try {
    const hits = await invoke("search_models", { query });
    results.replaceChildren();
    if (hits.length === 0) results.append(el("p", { class: "muted" }, "No GGUF models found."));
    for (const hit of hits) results.append(searchCard(hit));
  } catch (e) {
    results.replaceChildren(el("div", { class: "banner bad" }, String(e)));
  }
});

function searchCard(hit) {
  const actions = el("div", { class: "actions" });
  const card = el(
    "div",
    { class: "card" },
    el("div", { class: "body" }, el("div", { class: "title selectable" }, hit.id), el("div", { class: "chips" }, chip(`${hit.downloads.toLocaleString()} downloads`), chip(`${hit.likes} likes`))),
    actions,
  );
  const check = el("button", {}, "Check");
  check.addEventListener("click", async () => {
    check.disabled = true;
    check.textContent = "Checking…";
    try {
      const { model, fit } = await invoke("check_model", { repo: hit.id });
      if (fit) {
        card.replaceWith(modelCard(model, fit, false));
      } else {
        actions.replaceChildren(chip(`${model.quant.label} needs more memory than this machine has`));
      }
    } catch (e) {
      actions.replaceChildren(chip(String(e).slice(0, 80)));
    }
  });
  actions.append(check);
  return card;
}

// --- Models tab: status and what's on this Mac ---
function statusText(status) {
  switch (status.state) {
    case "running": return `Running ${status.name} with a ${Math.round(status.context / 1024)}k context.`;
    case "loading": return `Loading ${status.name}… big models can take a minute.`;
    case "failed": return `Could not start the model: ${status.error}`;
    default: return "No model is loaded.";
  }
}

function renderModelsTab() {
  renderStatus();
  setModelFilter(modelFilter);
}

function setModelFilter(filter) {
  modelFilter = filter;
  for (const button of document.querySelectorAll("#model-filter button")) button.classList.toggle("active", button.dataset.filter === filter);
  $("models-suggested").hidden = filter !== "suggested";
  $("models-local").hidden = filter !== "local";
  if (filter === "local") {
    renderInstalled();
    invoke("other_apps").then(renderOtherApps);
  }
}
for (const button of document.querySelectorAll("#model-filter button")) button.addEventListener("click", () => setModelFilter(button.dataset.filter));

// Loaded (or loading) models: in memory, ready for a chat.
const isLoaded = (id) => overview?.models.some((m) => m.id === id && m.state !== "failed");

function modelLine(m) {
  switch (m.state) {
    case "running": return `${m.name} is loaded, with a ${Math.round(m.context / 1024)}k context (${gib(m.memory)}).`;
    case "loading": return `Loading ${m.name}… big models can take a minute.`;
    default: return `Could not load ${m.name}: ${m.error}`;
  }
}

// The loaded models, or why none can load, above both filters. Hidden when there's nothing to say.
async function renderStatus() {
  await refreshOverview();
  const banner = $("status-banner");
  banner.replaceChildren();
  banner.classList.toggle("bad", !overview.llama_server);
  if (!overview.llama_server) {
    banner.append("llama-server was not found. Install llama.cpp (brew install llama.cpp) to run models.");
  } else {
    for (const m of overview.models) {
      banner.append(
        el(
          "div",
          { class: `row model-line ${m.state}` },
          el("span", { class: "spacer" }, modelLine(m)),
          el("button", { onclick: () => unloadModel(m.id), title: m.state === "failed" ? "" : "Frees the model's memory" }, m.state === "failed" ? "Dismiss" : "Unload"),
        ),
      );
    }
  }
  banner.hidden = banner.childNodes.length === 0;
  rerenderAllDownloads();
}

async function unloadModel(id) {
  await invoke("unload_model", { id });
  renderStatus();
  if (modelFilter === "local") renderInstalled();
}

async function renderInstalled() {
  await refreshOverview();
  const status = overview.status;
  const installed = await refreshInstalledIds();
  $("model-filter").querySelector('[data-filter="local"]').textContent = installed.length ? `On this Mac (${installed.length})` : "On this Mac";
  const list = $("installed-list");
  list.replaceChildren();
  if (installed.length === 0) list.append(el("p", { class: "muted" }, "Nothing here yet. Pick a model under Suggested."));
  for (const m of installed) {
    const active = isLoaded(m.id);
    const loading = overview.models.some((l) => l.id === m.id && l.state === "loading");
    const actions = el("div", { class: "actions" });
    if (active) {
      actions.append(el("button", { onclick: () => unloadModel(m.id), title: "Frees the model's memory", disabled: loading }, "Unload"));
    } else {
      actions.append(el("button", { class: "primary", disabled: !m.fit, onclick: () => loadModel(m.id) }, "Load"));
    }
    if (m.removable) {
      actions.append(el("button", { class: "danger", disabled: active, onclick: () => deleteModel(m) }, "Delete"));
    }
    const details = [
      ["File", `${m.model.quant.label}, ${gib(m.model.quant.size)}`],
      ["Context", m.fit ? `${Math.round(m.fit.context / 1024)}k tokens` : "too large for this machine"],
      ["From", m.source === "app" ? "downloaded here" : m.source_label],
      ["Where", m.source === "app" ? m.model.repo : m.path],
    ];
    list.append(
      el(
        "div",
        { class: "card" },
        el(
          "div",
          { class: "body" },
          el("div", { class: "title" }, m.model.name, active ? " ●" : ""),
          m.fit ? null : el("p", { class: "muted small" }, "Too large for this machine's memory."),
          m.source === "app" ? null : el("div", { class: "chips" }, chip(`from ${m.source_label}`, "good")),
        ),
        infoButton(el("div", { class: "title" }, m.model.name), el("dl", {}, details.flatMap(([term, text]) => [el("dt", {}, term), el("dd", {}, text)]))),
        actions,
      ),
    );
  }
}

function renderOtherApps(info) {
  $("use-other-apps").checked = info.enabled;
  const mlx = info.mlx.length;
  const parts = [];
  if (!info.enabled) parts.push("Off.");
  else {
    parts.push(`Found ${info.found} usable model${info.found === 1 ? "" : "s"} from other apps.`);
    if (mlx) parts.push(`${mlx} MLX model${mlx === 1 ? "" : "s"} found too; MLX support is planned.`);
    if (info.skipped.length) parts.push(`${info.skipped.length} file${info.skipped.length === 1 ? "" : "s"} left out (see details).`);
  }
  $("other-apps-summary").textContent = parts.join(" ");
  const list = $("other-apps-list");
  list.replaceChildren();
  const where = Object.entries(info.locations).flatMap(([app, dirs]) => dirs.map((d) => `${app}: ${d}`));
  list.append(el("p", { class: "muted small" }, where.length ? `Looked in ${where.join(" · ")}` : "No other apps' model folders found."));
  for (const s of info.skipped) {
    list.append(el("div", { class: "call" }, el("span", { class: "muted small" }, s.source), el("span", { class: "verdict" }, "SKIPPED"), el("div", {}, el("code", { class: "selectable" }, s.name), el("div", { class: "reason" }, s.reason))));
  }
  for (const m of info.mlx) {
    list.append(el("div", { class: "call" }, el("span", { class: "muted small" }, m.source), el("span", { class: "verdict" }, "MLX"), el("div", {}, el("code", { class: "selectable" }, m.name), el("div", { class: "reason" }, "MLX support is planned"))));
  }
  $("other-apps-details").hidden = !info.enabled;
}

$("use-other-apps").addEventListener("change", async (event) => {
  renderOtherApps(await invoke("set_use_other_apps", { enabled: event.target.checked }));
  await renderInstalled();
  renderOffers();
});
$("rescan").addEventListener("click", async () => {
  $("rescan").disabled = true;
  renderOtherApps(await invoke("rescan_models"));
  $("rescan").disabled = false;
  await renderInstalled();
  renderOffers();
});

// Loads a model beside the loaded ones; when memory is short, offers to unload the ones used longest ago.
async function loadModel(id) {
  try {
    const room = await invoke("room_for", { id });
    if (!room.fits) {
      const names = room.unload.map((u) => overview.models.find((m) => m.id === u)?.name || u);
      if (!confirm(`There isn't room for this model beside ${names.join(" and ")}. Unload ${names.length === 1 ? "it" : "them"} first?`)) return;
      for (const u of room.unload) await invoke("unload_model", { id: u });
    }
  } catch (e) {
    alert(e);
    return;
  }
  const loading = invoke("load_model", { id });
  renderStatus();
  try {
    await loading;
  } catch (e) {
    // The status area shows the error.
  }
  renderStatus();
  if (modelFilter === "local") renderInstalled();
}

async function deleteModel(model) {
  if (!confirm(`Delete ${model.model.name} (${gib(model.model.quant.size)})?`)) return;
  try {
    await invoke("delete_model", { id: model.id });
  } catch (e) {
    alert(e);
  }
  await renderInstalled();
  renderOffers();
}

listen("status", () => {
  renderStatus();
  if (!$("models").hidden && modelFilter === "local") renderInstalled();
  if (!$("projects").hidden && !editingProject) renderProjects();
  if (!$("guide").hidden) renderGuide();
});

// --- Connect tab ---
async function renderConnect() {
  await refreshOverview();
  const info = await invoke("api_info");
  $("base-url").value = info.base_url || "";
  $("api-key").value = info.api_key;
  $("model-name").value = info.model || "(load a model first)";
  $("opencode-version").textContent = overview.opencode_version ? `OpenCode ${overview.opencode_version} found.` : "OpenCode was not found on your PATH. Install it from opencode.ai, then come back.";
  $("setup-opencode").disabled = overview.status.state !== "running";
  $("hf-status").textContent = overview.hf_token_set ? "A token is saved." : "No token saved.";
}

for (const button of document.querySelectorAll("[data-copy]")) {
  button.addEventListener("click", async () => {
    await navigator.clipboard.writeText($(button.dataset.copy).value);
    button.textContent = "Copied";
    setTimeout(() => (button.textContent = "Copy"), 1200);
  });
}
$("reveal-key").addEventListener("click", () => {
  const input = $("api-key");
  input.type = input.type === "password" ? "text" : "password";
  $("reveal-key").textContent = input.type === "password" ? "Show" : "Hide";
});
$("regenerate-key").addEventListener("click", async () => {
  if (!confirm("Make a new API key? Tools set up with the old key will need to be set up again.")) return;
  await invoke("regenerate_api_key");
  renderConnect();
});
$("setup-opencode").addEventListener("click", async () => {
  const result = $("opencode-result");
  try {
    const done = await invoke("setup_opencode");
    result.textContent = `Updated ${done.path}${done.restarted_service ? " and restarted the OpenCode service" : ""}.`;
  } catch (e) {
    result.textContent = String(e);
  }
});
$("hf-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  await invoke("set_hf_token", { token: $("hf-token").value });
  $("hf-token").value = "";
  renderConnect();
});
$("hf-clear").addEventListener("click", async () => {
  await invoke("set_hf_token", { token: null });
  renderConnect();
});

// --- Sandboxed projects ---
// `text` with its `backticked` parts as code.
function withCode(text) {
  return text.split("`").map((part, i) => (i % 2 ? el("code", { class: "selectable" }, part) : part));
}

// Isolation options one install away: what they need, a download button and the steps.
function renderInstallable(choices) {
  const box = $("sandbox-installable");
  box.replaceChildren();
  for (const choice of choices) {
    const { needs, steps, url } = choice.setup;
    const status = el("span", { class: "small muted" });
    const download = el("button", {}, "Download the installer");
    download.addEventListener("click", async () => {
      download.disabled = true;
      status.textContent = "Finding the latest version…";
      try {
        const opened = await invoke("get_installer", { isolation: choice.isolation });
        status.textContent = `Opened ${opened.split("/").pop()} in your browser.`;
      } catch (e) {
        status.textContent = String(e);
      }
      download.disabled = false;
    });
    box.append(
      el(
        "div",
        { class: "banner" },
        el("p", {}, el("strong", {}, `${choice.label}: `), `available after installing ${needs}.`),
        el("ol", { class: "small" }, steps.map((step) => el("li", {}, withCode(step)))),
        el("div", { class: "row" }, download, el("span", { class: "small muted selectable" }, url), status),
        el("div", { class: "row small" }, el("span", { class: "muted" }, "Already installed it?"), el("button", { onclick: renderProjects }, "Check again")),
      ),
    );
  }
}

// --- Projects tab ---
let projectsInfo = null;
let editingProject = null; // id of the project open for editing

async function renderProjects() {
  await refreshOverview();
  projectsInfo = await invoke("projects_info");
  const running = overview.status.state === "running";
  $("sandbox-unavailable").hidden = projectsInfo.available;
  $("new-project").disabled = !projectsInfo.available;
  $("projects-note").textContent = running ? "" : "Load a model to open a project in OpenCode.";
  renderInstallable(projectsInfo.installable);
  const list = $("project-list");
  list.replaceChildren();
  if (projectsInfo.projects.length === 0) list.append(el("p", { class: "muted" }, "No projects yet. Make one for the folder you code in."));
  for (const project of projectsInfo.projects) {
    list.append(project.id === editingProject ? projectEditor(project) : projectCard(project, running));
  }
}

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

function isolationLabel(project) {
  return project.choices.find((c) => c.isolation === project.isolation)?.label || project.isolation;
}

function projectCard(project, running) {
  const chips = [chip(isolationLabel(project)), chip(project.network === "internet" ? "Internet on" : "Internet off")];
  if (project.folders.length > 1) chips.push(chip(`+ ${plural(project.folders.length - 1, "folder")}`));
  if (project.allowed_hosts.length) chips.push(chip(plural(project.allowed_hosts.length, "allowed site")));
  const limits = project.tools_off.length + project.blocked_programs.length + project.mcp_off.length;
  if (limits) chips.push(chip(plural(limits, "limit"), "warn"));
  return el(
    "div",
    { class: "card" },
    el("div", { class: "body" }, el("div", { class: "title" }, project.name), el("p", { class: "muted small selectable" }, project.folders[0].path), el("div", { class: "chips" }, chips)),
    el(
      "div",
      { class: "actions" },
      el("button", { onclick: () => { editingProject = project.id; renderProjects(); } }, "Edit"),
      el("button", { disabled: !projectsInfo.available || !running, title: running ? "Open in OpenCode, in a Terminal window" : "Load a model first", onclick: () => invoke("open_project", { id: project.id }) }, "OpenCode"),
      el("button", { class: "primary", onclick: () => invoke("open_chat", { project: project.id }) }, "Chat"),
    ),
  );
}

function field(label, help, ...content) {
  return el("div", { class: "field" }, el("div", { class: "field-label" }, label), help ? el("p", { class: "muted small" }, help) : null, ...content);
}

// Removable chips for `items`, and a box to add one; `onChange` after every change.
function listEditor(items, placeholder, onChange) {
  const box = el("div");
  const render = () => {
    const input = el("input", { type: "text", placeholder });
    const add = () => {
      const value = input.value.trim();
      if (value && !items.includes(value)) items.push(value);
      onChange();
      render();
      box.querySelector("input").focus();
    };
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        add();
      }
    });
    box.replaceChildren(
      items.length ? el("div", { class: "chips" }, items.map((item, i) => el("span", { class: "chip removable" }, item, el("button", { title: "Remove", onclick: () => { items.splice(i, 1); onChange(); render(); } }, "×")))) : null,
      el("div", { class: "row" }, input, el("button", { onclick: add }, "Add")),
    );
  };
  render();
  return box;
}

function projectEditor(project) {
  const { choices, ...rest } = project;
  const draft = structuredClone(rest);
  const box = el("div", { class: "card editor" });
  const render = () => {
    const folders = draft.folders.map((folder, i) => {
      const access = i === 0
        ? chip("Main folder · can change")
        : (() => {
            const select = el("select", { class: "small" }, el("option", { value: "read" }, "Read only"), el("option", { value: "write" }, "Can change"));
            select.value = folder.writable ? "write" : "read";
            select.addEventListener("change", () => (folder.writable = select.value === "write"));
            return select;
          })();
      const remove = i === 0 ? null : el("button", { class: "small", onclick: () => { draft.folders.splice(i, 1); render(); } }, "Remove");
      return el("div", { class: "row folder" }, el("span", { class: "selectable path" }, folder.path), access, remove);
    });
    const addFolder = el("button", {
      onclick: async () => {
        const path = await invoke("choose_folder", { title: `Add a folder to ${draft.name}` });
        if (path && !draft.folders.some((f) => f.path === path)) draft.folders.push({ path, writable: false });
        render();
      },
    }, "Add folder…");

    const isolation = el("select", {});
    for (const choice of choices) {
      const option = el("option", { value: choice.isolation, disabled: Boolean(choice.unavailable) }, choice.unavailable ? `${choice.label} (${choice.unavailable})` : choice.label);
      option.selected = choice.isolation === draft.isolation;
      isolation.append(option);
    }
    isolation.addEventListener("change", () => (draft.isolation = isolation.value));

    const internet = el("input", { type: "checkbox" });
    internet.checked = draft.network === "internet";
    internet.addEventListener("change", () => (draft.network = internet.checked ? "internet" : "local-only"));

    const tools = projectsInfo.tool_groups.map((group) => {
      const box = el("input", { type: "checkbox" });
      box.checked = !draft.tools_off.includes(group.key);
      box.addEventListener("change", () => {
        draft.tools_off = draft.tools_off.filter((k) => k !== group.key);
        if (!box.checked) draft.tools_off.push(group.key);
      });
      return el("label", { class: "check" }, box, group.label);
    });

    const suggestions = el("div", { class: "chips" });
    const renderSuggestions = () => {
      const missing = projectsInfo.suggested_programs.filter((p) => !draft.blocked_programs.includes(p));
      suggestions.replaceChildren(
        ...(missing.length ? [el("span", { class: "small muted" }, "Suggestions:")] : []),
        ...missing.map((program) => el("button", { class: "chip-button", onclick: () => { draft.blocked_programs.push(program); render(); } }, `+ ${program}`)),
      );
    };
    renderSuggestions();

    const servers = projectsInfo.mcp_servers.length
      ? projectsInfo.mcp_servers.map((name) => {
          const box = el("input", { type: "checkbox" });
          box.checked = !draft.mcp_off.includes(name);
          box.addEventListener("change", () => {
            draft.mcp_off = draft.mcp_off.filter((n) => n !== name);
            if (!box.checked) draft.mcp_off.push(name);
          });
          return el("label", { class: "check" }, box, name);
        })
      : [el("p", { class: "muted small" }, "No MCP servers in your OpenCode config.")];

    const approvals = el("select", {}, el("option", { value: "commands" }, "Before running commands"), el("option", { value: "everything" }, "Before commands and file changes"), el("option", { value: "never" }, "Never"));
    approvals.value = draft.approvals || "commands";
    approvals.addEventListener("change", () => (draft.approvals = approvals.value));

    const name = el("input", { type: "text", value: draft.name });
    name.addEventListener("input", () => (draft.name = name.value));

    box.replaceChildren(
      el("div", { class: "body" },
        field("Name", null, name),
        field("Folders", "OpenCode starts in the main folder. Add related folders the agent should read, or change too.", ...folders, el("div", { class: "row" }, addFolder)),
        field("Runs in", null, isolation),
        field("The agent asks", "In the chat and in OpenCode. Either way, the guard and the sandbox check every call.", approvals),
        field("Internet", null, el("label", { class: "check" }, internet, "Allow internet (package installs, git push); the guard still checks every call")),
        field("Allowed websites", "Calls may always reach these. Other sites still pass when your conversation mentions them.", listEditor(draft.allowed_hosts, "docs.example.com", () => {})),
        field("The agent may", "What's unticked is turned off in OpenCode, and the guard blocks it too.", el("div", { class: "checks" }, tools)),
        field("Blocked programs", "Commands that run these are blocked by the guard, and in the macOS sandbox they can't start at all.", listEditor(draft.blocked_programs, "e.g. ssh", renderSuggestions), suggestions),
        field("MCP servers", "From your OpenCode config. Unticked ones don't start for this project.", el("div", { class: "checks" }, servers)),
        el("div", { class: "row editor-actions" },
          el("button", { class: "danger", onclick: () => deleteProject(draft) }, "Delete project"),
          el("span", { class: "spacer" }),
          el("button", { onclick: () => { editingProject = null; renderProjects(); } }, "Cancel"),
          el("button", { class: "primary", onclick: () => saveProject(draft) }, "Save"),
        ),
      ),
    );
  };
  render();
  return box;
}

async function saveProject(draft) {
  try {
    await invoke("save_project", { project: draft });
    editingProject = null;
  } catch (e) {
    alert(e);
  }
  renderProjects();
}

async function deleteProject(project) {
  if (!confirm(`Remove the project ${project.name}? Its folders and files stay as they are.`)) return;
  await invoke("delete_project", { id: project.id });
  editingProject = null;
  renderProjects();
}

// Asks for a folder and makes a project of it (or finds the one it has); null if cancelled.
async function chooseProjectFolder() {
  const folder = await invoke("choose_folder", { title: "Choose your project's main folder" });
  if (!folder) return null;
  try {
    return await invoke("new_project", { folder });
  } catch (e) {
    alert(e);
    return null;
  }
}

$("new-project").addEventListener("click", async () => {
  const project = await chooseProjectFolder();
  if (!project) return;
  editingProject = project.id;
  renderProjects();
});
listen("projects", () => {
  if (!$("projects").hidden && !editingProject) renderProjects();
});

// --- Safety tab ---
const VERDICT_LABELS = { passed: "PASS", suspicious: "WARN", flagged: "FLAG", blocked: "BLOCK" };
function callRow(call) {
  const time = new Date(call.time * 1000).toLocaleTimeString();
  return el(
    "div",
    { class: "call" },
    el("span", { class: "muted small" }, time),
    el("span", { class: `verdict ${call.verdict}` }, VERDICT_LABELS[call.verdict] || call.verdict.toUpperCase()),
    el(
      "div",
      {},
      el("code", { class: "selectable" }, `${call.tool}: ${call.summary}`),
      call.reason ? el("div", { class: "reason" }, call.reason) : call.signals?.length ? el("div", { class: "reason" }, call.signals.join(", ")) : null,
      judgeNote(call),
    ),
  );
}

// The second check's answer, unless the reason already gives it.
function judgeNote(call) {
  if (!call.judge) return null;
  const [kind, ...rest] = call.judge.split(": ");
  const why = rest.join(": ");
  if (kind === "block") return null;
  const text = kind === "allow" ? `Judge allowed it: ${why}` : `Judge gave no answer: ${why}`;
  return el("div", { class: "reason" }, text);
}

const JUDGE_STATUS = {
  idle: () => "It starts with your next model.",
  loading: () => "Starting…",
  running: () => "Running: WARN calls are checked.",
  failed: (s) => `Couldn't start: ${s.error}`,
};
let judgeDownload = null;
async function renderJudge() {
  const judge = await invoke("judge_settings");
  judgeDownload = judge.download;
  if (!judge.model) {
    $("judge-enabled").disabled = true;
    return;
  }
  $("judge-name").textContent = judge.model.name;
  $("judge-about").textContent = `${judge.model.name}: ${judge.model.description} A ${gib(judge.model.quant.size)} download; it takes about ${gib(judge.memory)} of memory while a model runs, which models then can't use.`;
  $("judge-enabled").checked = judge.enabled;
  // Too little memory: it can only be turned off.
  $("judge-enabled").disabled = !judge.fits && !judge.enabled;
  $("judge-fail-closed").checked = judge.fail_closed;
  $("judge-fail-closed").disabled = !judge.enabled;

  const box = $("judge-download");
  box.replaceChildren();
  const progress = downloads.get(judge.download);
  if (progress) {
    const pct = progress.total ? Math.floor((progress.done / progress.total) * 100) : 0;
    box.append(
      el("progress", { max: progress.total || 1, value: progress.done }),
      el("span", { class: "small muted" }, `${pct}%`),
      el("button", { onclick: () => invoke("cancel_download", { id: judge.download }) }, "Cancel"),
    );
  } else if (judge.enabled && !judge.installed) {
    box.append(el("button", { class: "primary", onclick: downloadJudge }, `Download ${judge.model.name} (${gib(judge.model.quant.size)})`));
  }

  const status = $("judge-status");
  status.className = "small";
  if (!judge.fits && !judge.enabled) {
    status.textContent = "This machine doesn't have the memory to run it beside a model.";
    status.classList.add("muted");
  } else if (!judge.enabled) {
    status.textContent = "";
  } else if (!judge.installed) {
    status.textContent = progress ? "Downloading…" : "Download it to start checking.";
    status.classList.add("muted");
  } else {
    status.textContent = JUDGE_STATUS[judge.status.state](judge.status);
    if (judge.status.state === "failed") status.style.color = "var(--bad)";
    else status.style.color = "";
  }
}
async function downloadJudge() {
  downloads.set(judgeDownload, { done: 0, total: 0 });
  await renderJudge();
  try {
    await invoke("download_judge");
  } catch (e) {
    downloads.delete(judgeDownload);
    alert(e);
  }
  await renderJudge();
}
async function saveJudge() {
  const enabled = $("judge-enabled").checked;
  try {
    await invoke("set_judge", { enabled, failClosed: $("judge-fail-closed").checked });
  } catch (e) {
    alert(e);
  }
  await renderJudge();
  // Start the download on first use.
  const judge = await invoke("judge_settings");
  if (enabled && judge.enabled && !judge.installed && !downloads.has(judge.download)) await downloadJudge();
}
// What fits changed, or the judge started or stopped.
listen("judge", async () => {
  await refreshOverview();
  renderOffers();
  renderJudge();
});
listen("status", () => {
  if (!$("safety").hidden) renderJudge();
});
$("judge-enabled").addEventListener("change", saveJudge);
$("judge-fail-closed").addEventListener("change", saveJudge);

async function renderCalls() {
  const calls = await invoke("recent_calls");
  const list = $("calls");
  list.replaceChildren();
  if (calls.length === 0) list.append(el("p", { class: "muted" }, "No tool calls yet."));
  for (const call of calls) list.append(callRow(call));
}

listen("guard-call", ({ payload }) => {
  const list = $("calls");
  if (list.querySelector("p")) list.replaceChildren();
  list.prepend(callRow(payload));
});

// --- Setup guide ---
const GUIDE_STEPS = ["Model", "Project", "Safety", "Connect"];
let guideStep = 0;
let guideProject = null; // id of the project made or picked in step 2

async function renderGuide() {
  await refreshOverview();
  $("guide-steps").replaceChildren(
    ...GUIDE_STEPS.map((name, i) => el("li", { class: i === guideStep ? "current" : i < guideStep ? "done" : "", onclick: () => goToStep(i) }, el("span", { class: "num" }, i < guideStep ? "✓" : i + 1), name)),
  );
  const content = await [guideModel, guideProjectStep, guideSafety, guideConnect][guideStep]();
  $("guide-body").replaceChildren(...content.filter(Boolean));
  $("guide-back").hidden = guideStep === 0;
  $("guide-next").textContent = guideStep === GUIDE_STEPS.length - 1 ? "Finish" : "Next";
}

function goToStep(step) {
  guideStep = Math.max(0, Math.min(GUIDE_STEPS.length - 1, step));
  renderGuide();
}

function runningModelName() {
  return overview.status.state === "running" ? overview.status.name : null;
}

async function guideModel() {
  const { picks } = await invoke("catalog_offers");
  const running = runningModelName();
  const loading = overview.status.state === "loading";
  return [
    el("h2", {}, "Pick a model"),
    el("p", { class: "muted" }, "The model writes the code. Bigger ones code better but answer more slowly and leave less memory for your other apps. Load one that's already on this Mac, or download one: downloads keep going while you carry on."),
    running ? el("div", { class: "banner good" }, `✓ ${running} is loaded.`) : loading ? el("div", { class: "banner" }, statusText(overview.status)) : null,
    ...picks.map(pickCard),
    picks.length ? null : el("div", { class: "banner" }, "None of the suggested models fit in this machine's memory."),
    el("p", { class: "small muted" }, "More models, and models from other apps, are on the Models tab."),
  ];
}

async function guideProjectStep() {
  const { projects } = await invoke("projects_info");
  const project = projects.find((p) => p.id === guideProject) || projects[0];
  if (project) guideProject = project.id;
  const choose = el("button", { class: project ? "" : "primary", onclick: async () => {
    const made = await chooseProjectFolder();
    if (made) {
      guideProject = made.id;
      renderGuide();
    }
  } }, project ? "Choose another folder…" : "Choose folder…");
  const parts = [
    el("h2", {}, "Your first project"),
    el("p", { class: "muted" }, "Pick the folder you'll code in. The agent works in it in a sandbox: it can change files only there, and can't read your passwords, keys or other private files."),
  ];
  if (project) {
    const internet = el("input", { type: "checkbox" });
    internet.checked = project.network === "internet";
    internet.addEventListener("change", async () => {
      const { choices, ...rest } = project;
      await invoke("save_project", { project: { ...rest, network: internet.checked ? "internet" : "local-only" } });
    });
    parts.push(
      el("div", { class: "card" }, el("div", { class: "body" }, el("div", { class: "title" }, project.name), el("p", { class: "muted small selectable" }, project.folders[0].path))),
      el("label", { class: "check" }, internet, "Allow internet (package installs, git push); the guard still checks every call"),
    );
  }
  parts.push(el("div", { class: "row" }, choose), el("p", { class: "small muted" }, "Related folders, allowed websites and limits on tools are in Projects whenever you want them."));
  return parts;
}

async function guideSafety() {
  const judge = await invoke("judge_settings");
  judgeDownload = judge.download;
  const parts = [
    el("h2", {}, "Safety"),
    el("p", { class: "muted" }, "The guard is always on. It checks every command and file edit the model asks for before it runs, and blocks the ones that reach for your credentials, other agents' data or websites nobody mentioned."),
  ];
  if (judge.model) {
    const box = el("input", { type: "checkbox" });
    box.checked = judge.enabled;
    box.disabled = !judge.fits && !judge.enabled;
    box.addEventListener("change", async () => {
      try {
        await invoke("set_judge", { enabled: box.checked, failClosed: judge.fail_closed });
        const now = await invoke("judge_settings");
        if (now.enabled && !now.installed && !downloads.has(now.download)) downloadJudge();
      } catch (e) {
        alert(e);
      }
      renderGuide();
    });
    const note = !judge.fits && !judge.enabled
      ? "This Mac doesn't have the memory to run it beside a model."
      : `A ${gib(judge.model.quant.size)} download that uses about ${gib(judge.memory)} of memory while a model runs. Recommended if you can spare it.`;
    parts.push(
      el("label", { class: "check" }, box, `Double-check borderline calls with ${judge.model.name}, a small decision model`),
      el("p", { class: "small muted indent" }, note, downloads.has(judge.download) ? " Downloading now; progress is on the Safety tab." : ""),
    );
  }
  parts.push(el("p", { class: "small muted" }, "Every call the guard looked at is listed on the Safety tab."));
  return parts;
}

async function guideConnect() {
  const { projects } = await invoke("projects_info");
  const project = projects.find((p) => p.id === guideProject) || projects[0];
  const running = runningModelName();
  const parts = [el("h2", {}, "Start coding"), el("p", { class: "muted" }, "Tell the agent what to do in the chat window. It works in the project's sandbox, talking to your model through the guard, and asks before it runs commands.")];
  if (!running) {
    parts.push(el("div", { class: "banner" }, "Load a model first. ", el("button", { onclick: () => goToStep(0) }, "Back to models")));
  } else if (!project) {
    parts.push(el("div", { class: "banner" }, "Choose a project folder first. ", el("button", { onclick: () => goToStep(1) }, "Back to projects")));
  } else {
    parts.push(
      el("div", { class: "row" }, el("button", { class: "primary", onclick: () => invoke("open_chat", { project: project.id }) }, `Chat about ${project.name}`), el("span", { class: "small muted" }, `with ${running}`)),
      el("p", { class: "small muted" }, "Prefer OpenCode? ", el("button", { class: "link", onclick: () => invoke("open_project", { id: project.id }) }, "Open it in OpenCode"), " instead."),
    );
  }
  parts.push(el("p", { class: "small muted" }, "Later, open the chat from the menu bar icon or the Projects tab. Other tools that speak the OpenAI API can use the guard too: see Connect."));
  return parts;
}

async function finishGuide() {
  await invoke("set_setup_done", { done: true });
  showTab("models");
}

$("guide-back").addEventListener("click", () => goToStep(guideStep - 1));
$("guide-next").addEventListener("click", () => (guideStep === GUIDE_STEPS.length - 1 ? finishGuide() : goToStep(guideStep + 1)));
$("guide-skip").addEventListener("click", finishGuide);

// --- Start ---
(async () => {
  await refreshOverview();
  await refreshInstalledIds();
  await renderOffers();
  renderStatus();
  const tab = location.hash.slice(1);
  if (!(await invoke("setup_done"))) showTab("guide");
  else if (tab) showTab(tab);
})();
