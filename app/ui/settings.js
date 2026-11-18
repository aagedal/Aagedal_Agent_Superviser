const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const GIB = 1024 ** 3;
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
  for (const button of document.querySelectorAll("nav button")) button.classList.toggle("active", button.dataset.tab === name);
  for (const section of document.querySelectorAll("main section")) section.hidden = section.id !== name;
  if (name === "installed") {
    renderInstalled();
    invoke("other_apps").then(renderOtherApps);
  }
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
const downloads = new Map(); // id -> {done, total}
const downloadButtons = new Map(); // id -> render function

async function refreshOverview() {
  overview = await invoke("overview");
  const hw = overview.hardware;
  const kind = hw.unified_memory ? "This Mac" : "This computer";
  $("machine").textContent = `${kind} has ${gib(hw.total_memory)} of memory; up to ${gib(hw.model_budget)} can go to a model. Only models that fit are shown.`;
}

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
function downloadControl(model) {
  const id = `${model.repo}:${model.quant.label}`;
  const box = el("div", { class: "actions" });
  const render = () => {
    box.replaceChildren();
    const progress = downloads.get(id);
    // The same model already on this machine, possibly another quant from another app.
    const local = (localByRepo.get(model.repo.toLowerCase()) || []).find((m) => m.id === id) || (localByRepo.get(model.repo.toLowerCase()) || [])[0];
    if (installedIds.has(id) && local?.source === "app") {
      box.append(chip("Installed", "good"), el("button", { onclick: () => startModel(id) }, "Start"));
    } else if (local && !progress) {
      box.append(
        chip(`On this Mac: ${local.model.quant.label} from ${local.source_label}`, "good"),
        el("button", { class: "primary", onclick: () => startModel(local.id) }, "Start"),
      );
      if (local.id !== id) box.append(el("button", { onclick: () => download(model, id) }, `Download ${model.quant.label}`));
    } else if (progress) {
      const bar = el("progress", { max: progress.total || 1, value: progress.done });
      const pct = progress.total ? Math.floor((progress.done / progress.total) * 100) : 0;
      box.append(bar, el("span", { class: "small muted" }, `${pct}%`), el("button", { onclick: () => invoke("cancel_download", { id }) }, "Cancel"));
    } else {
      box.append(el("button", { class: "primary", onclick: () => download(model, id) }, "Download"));
    }
  };
  downloadButtons.set(id, render);
  render();
  return box;
}

async function download(model, id) {
  downloads.set(id, { done: 0, total: model.quant.size });
  downloadButtons.get(id)?.();
  try {
    await invoke("download_model", { model });
  } catch (e) {
    downloads.delete(id);
    downloadButtons.get(id)?.();
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
  downloadButtons.get(payload.id)?.();
});

function modelCard(model, fit, recommended) {
  const chips = [
    chip(model.quant.label),
    chip(`${gib(model.quant.size)} download`),
    chip(`${Math.round(fit.context / 1024)}k context`),
    chip(`needs ~${gib(fit.required)}`),
  ];
  if (recommended) chips.unshift(chip("Recommended for this machine", "accent"));
  return el(
    "div",
    { class: "card" },
    el("div", { class: "body" }, el("div", { class: "title" }, model.name), model.description ? el("p", { class: "muted small" }, model.description) : null, el("div", { class: "chips" }, chips)),
    downloadControl(model),
  );
}

async function renderOffers() {
  const { offers, hidden } = await invoke("catalog_offers");
  const list = $("offers");
  list.replaceChildren();
  if (offers.length === 0) {
    list.append(el("div", { class: "banner" }, "None of the suggested models fit in this machine's memory. Coding agents need a model with a long context, which takes more memory than this machine can spare."));
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

// --- Installed tab ---
function statusText(status) {
  switch (status.state) {
    case "running": return `Running ${status.name} with a ${Math.round(status.context / 1024)}k context.`;
    case "loading": return `Loading ${status.name}… big models can take a minute.`;
    case "failed": return `Could not start the model: ${status.error}`;
    default: return "No model is running.";
  }
}

async function renderInstalled() {
  await refreshOverview();
  const status = overview.status;
  const banner = $("status-banner");
  banner.textContent = statusText(status);
  banner.classList.toggle("bad", status.state === "failed");
  if (!overview.llama_server) {
    banner.textContent = "llama-server was not found. Install llama.cpp (brew install llama.cpp) to run models.";
    banner.classList.add("bad");
  }
  const installed = await refreshInstalledIds();
  const list = $("installed-list");
  list.replaceChildren();
  if (installed.length === 0) list.append(el("p", { class: "muted" }, "Nothing installed yet. Pick a model on the Models tab."));
  for (const m of installed) {
    const active = (status.state === "running" || status.state === "loading") && status.model === m.id;
    const actions = el("div", { class: "actions" });
    if (active) {
      actions.append(el("button", { onclick: async () => { await invoke("stop_model"); renderInstalled(); } }, "Stop"));
    } else {
      actions.append(el("button", { class: "primary", disabled: !m.fit || status.state === "loading", onclick: () => startModel(m.id) }, "Start"));
    }
    if (m.removable) {
      actions.append(el("button", { class: "danger", disabled: active, onclick: () => deleteModel(m) }, "Delete"));
    }
    list.append(
      el(
        "div",
        { class: "card" },
        el(
          "div",
          { class: "body" },
          el("div", { class: "title" }, m.model.name, active ? " ●" : ""),
          el(
            "div",
            { class: "chips" },
            chip(m.model.quant.label),
            chip(gib(m.model.quant.size)),
            m.fit ? chip(`${Math.round(m.fit.context / 1024)}k context`) : chip("too large for this machine"),
            m.source === "app" ? null : chip(`from ${m.source_label}`, "good"),
          ),
          el("p", { class: "muted small selectable" }, m.source === "app" ? m.model.repo : m.path),
        ),
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

async function startModel(id) {
  showTab("installed");
  try {
    await invoke("start_model", { id });
  } catch (e) {
    // The status banner shows the error.
  }
  renderInstalled();
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
  if (!$("installed").hidden) renderInstalled();
});

// --- Connect tab ---
async function renderConnect() {
  await refreshOverview();
  const info = await invoke("api_info");
  $("base-url").value = info.base_url || "";
  $("api-key").value = info.api_key;
  $("model-name").value = info.model || "(start a model first)";
  $("opencode-version").textContent = overview.opencode_version ? `OpenCode ${overview.opencode_version} found.` : "OpenCode was not found on your PATH. Install it from opencode.ai, then come back.";
  $("setup-opencode").disabled = overview.status.state !== "running";
  $("hf-status").textContent = overview.hf_token_set ? "A token is saved." : "No token saved.";
  await renderProjects();
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
async function renderProjects() {
  const info = await invoke("sandbox_info");
  const running = overview?.status.state === "running";
  $("sandbox-unavailable").hidden = info.available;
  $("choose-project").disabled = !info.available || !running;
  $("choose-project").title = running ? "" : "Start a model first";
  $("sandbox-internet").checked = info.network === "internet";
  renderInstallable(info.installable);
  const list = $("recent-projects");
  list.replaceChildren();
  for (const path of info.recent_projects) {
    const name = path.split("/").filter(Boolean).pop();
    const isolation = await invoke("project_isolation", { path });
    list.append(
      el(
        "div",
        { class: "card" },
        el(
          "div",
          { class: "body" },
          el("div", { class: "title" }, name),
          el("p", { class: "muted small selectable" }, path),
          isolationPicker(path, isolation),
        ),
        el("div", { class: "actions" }, el("button", { disabled: !info.available || !running, onclick: () => invoke("open_project", { path }) }, "Open")),
      ),
    );
  }
}

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

// Every way a project can be isolated; ones that can't be used say why.
function isolationPicker(path, { current, choices }) {
  const select = el("select", { class: "small" });
  for (const choice of choices) {
    const label = choice.unavailable ? `${choice.label} (${choice.unavailable})` : choice.label;
    const option = el("option", { value: choice.isolation, disabled: Boolean(choice.unavailable) }, label);
    option.selected = choice.isolation === current;
    select.append(option);
  }
  select.addEventListener("change", async () => {
    try {
      await invoke("set_project_isolation", { path, isolation: select.value });
    } catch (e) {
      alert(e);
      select.value = current;
    }
  });
  return el("div", { class: "row small" }, el("span", { class: "muted" }, "Runs in"), select);
}
$("choose-project").addEventListener("click", () => invoke("open_project", { path: null }));
$("sandbox-internet").addEventListener("change", (event) =>
  invoke("set_sandbox_network", { network: event.target.checked ? "internet" : "local-only" }),
);
listen("projects", renderProjects);

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
  const text = kind === "allow" ? `Model allowed it: ${why}` : `Model gave no answer: ${why}`;
  return el("div", { class: "reason" }, text);
}

async function renderJudge() {
  const judge = await invoke("judge_settings");
  $("judge-enabled").checked = judge.enabled;
  $("judge-fail-closed").checked = judge.fail_closed;
  $("judge-fail-closed").disabled = !judge.enabled;
}
async function saveJudge() {
  await invoke("set_judge", { enabled: $("judge-enabled").checked, failClosed: $("judge-fail-closed").checked });
  await renderJudge();
}
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

// --- Start ---
(async () => {
  await refreshOverview();
  await refreshInstalledIds();
  await renderOffers();
  const tab = location.hash.slice(1);
  if (tab) showTab(tab);
})();
