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
    if (child !== null && child !== undefined && child !== false) node.append(child instanceof Node ? child : String(child));
  }
  return node;
}

function chip(text, kind = "") {
  return el("span", { class: `chip ${kind}` }, text);
}

// What each page does when these happen; settings.js and setup.js add their own.
const hooks = {
  overview: [], // the overview was fetched again
  started: [], // a download started (its id)
  progress: [], // a download reported progress (the event)
  finished: [], // a download finished, failed or was cancelled (the event)
  models: [], // a model started loading, or finished
  judge: [], // the judge's download started or stopped
};
const run = (name, ...args) => hooks[name].forEach((hook) => hook(...args));

// --- State shared by the windows' pages ---
let overview = null;
let installedIds = new Set();
let localByRepo = new Map(); // lowercased repo -> installed models (any source) that fit
const downloads = new Map(); // id -> {done, total, started, startDone}
const downloadNames = new Map(); // id -> the model's name, for the activity bar
const downloadNeeds = new Map(); // id -> memory the model takes while running, when known
const downloadButtons = new Map(); // id -> Map(box -> render function); a model can show in the picks and the full list

// Progress, keeping when the download (or this window's view of it) started, for a time estimate.
function trackDownload(id, done, total) {
  const prev = downloads.get(id);
  downloads.set(id, prev?.started ? { ...prev, done, total } : { done, total, started: Date.now(), startDone: done });
}

// "about 6 min left", once there's enough to go on; "" until then.
function timeLeft(progress) {
  if (!progress?.started || !progress.total) return "";
  const seconds = (Date.now() - progress.started) / 1000;
  const rate = (progress.done - progress.startDone) / seconds;
  if (seconds < 3 || rate <= 0) return "";
  const left = (progress.total - progress.done) / rate;
  if (left < 60) return "less than a minute left";
  if (left < 90 * 60) return `about ${Math.round(left / 60)} min left`;
  return `about ${(left / 3600).toFixed(1)} hours left`;
}

const percent = (progress) => (progress?.total ? Math.floor((progress.done / progress.total) * 100) : 0);

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
  run("overview");
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

// --- Models and downloads ---

// The same model already on this machine, possibly another quant from another app.
function localCopy(model, id) {
  const local = localByRepo.get(model.repo.toLowerCase()) || [];
  return local.find((m) => m.id === id) || local[0];
}

// `compact` leaves out the second download button for this model's own quant; the full list keeps it.
// `quiet` (or a function saying so) draws the download button plainly, so one suggestion can stand out.
// Without `load`, a model already here just says so: the setup guide starts it with the chat.
function downloadControl(model, { compact = false, quiet = false, load = true } = {}) {
  const id = `${model.repo}:${model.quant.label}`;
  const box = el("div", { class: "actions" });
  const render = () => {
    const progress = downloads.get(id);
    // Progress only: move the bar, keeping Cancel where the pointer is.
    const bar = box.querySelector("progress");
    if (progress && bar && !localCopy(model, id)) {
      bar.max = progress.total || 1;
      bar.value = progress.done;
      box.querySelector(".pct").textContent = `${percent(progress)}%`;
      return;
    }
    box.replaceChildren();
    const local = localCopy(model, id);
    if (local && isLoading(local.id)) {
      box.append(chip("Loading…"));
    } else if (local && isLoaded(local.id)) {
      box.append(chip("Loaded", "good"));
    } else if (local && !load) {
      box.append(chip(local.source === "app" ? "Downloaded" : "On this Mac", "good"));
    } else if (installedIds.has(id) && local?.source === "app") {
      box.append(chip("Installed", "good"), el("button", { class: compact ? "primary" : "", onclick: () => loadModel(id) }, "Load"));
    } else if (local && !progress) {
      box.append(chip("On this Mac", "good"), el("button", { class: "primary", onclick: () => loadModel(local.id) }, "Load"));
      if (local.id !== id && !compact) box.append(el("button", { onclick: () => download(model, id) }, `Download ${model.quant.label}`));
    } else if (progress) {
      const bar = el("progress", { max: progress.total || 1, value: progress.done });
      box.append(bar, el("span", { class: "small muted pct" }, `${percent(progress)}%`), el("button", { onclick: () => invoke("cancel_download", { id }) }, "Cancel"));
    } else {
      const plain = typeof quiet === "function" ? quiet() : quiet;
      box.append(el("button", { class: plain ? "" : "primary", onclick: () => download(model, id) }, `Download (${wholeGib(model.quant.size)})`));
    }
  };
  if (!downloadButtons.has(id)) downloadButtons.set(id, new Map());
  downloadButtons.get(id).set(box, render);
  render();
  return box;
}

async function download(model, id) {
  downloads.set(id, { done: 0, total: model.quant.size });
  downloadNames.set(id, model.name);
  if (model.fit) downloadNeeds.set(id, model.fit.required);
  rerenderDownload(id);
  renderActivity();
  run("started", id);
  try {
    await invoke("download_model", { model });
  } catch (e) {
    downloads.delete(id);
    rerenderDownload(id);
    renderActivity();
    run("finished", { id, error: String(e) });
    alert(e);
  }
}

listen("download", async ({ payload }) => {
  if (payload.name) downloadNames.set(payload.id, payload.name);
  if (payload.needs) downloadNeeds.set(payload.id, payload.needs);
  const ended = payload.finished || payload.error;
  if (!ended && !downloads.has(payload.id)) {
    trackDownload(payload.id, payload.done, payload.total);
    rerenderDownload(payload.id);
    renderActivity();
    run("started", payload.id);
    return;
  }
  if (ended) {
    downloads.delete(payload.id);
    if (payload.error && !payload.error.includes("cancelled")) alert(`The download of ${downloadNames.get(payload.id) || "the model"} stopped: ${payload.error}\n\nIt picks up where it left off when you try again.`);
    await refreshInstalledIds();
  } else {
    trackDownload(payload.id, payload.done, payload.total);
  }
  // Ended either way: which suggestion stands out may change.
  if (ended) rerenderAllDownloads();
  else rerenderDownload(payload.id);
  renderActivity();
  run(ended ? "finished" : "progress", payload);
});

// Downloads at the top of the window, wherever you are: how far, and how long is left. Rows are
// updated in place, so Cancel stays clickable while progress comes in.
const activityRows = new Map(); // id -> {row, bar, text}
function renderActivity() {
  const box = $("activity");
  for (const [id, entry] of activityRows) {
    if (!downloads.has(id)) {
      entry.row.remove();
      activityRows.delete(id);
    }
  }
  for (const [id, progress] of downloads) {
    let entry = activityRows.get(id);
    if (!entry) {
      entry = { bar: el("progress"), text: el("span", { class: "spacer" }) };
      entry.row = el("div", { class: "row" }, entry.bar, entry.text, el("span", { class: "small muted" }, "You can keep going meanwhile."), el("button", { class: "small", onclick: () => invoke("cancel_download", { id }) }, "Cancel"));
      activityRows.set(id, entry);
      box.append(entry.row);
    }
    const name = id === judgeDownload ? "the safety check model" : downloadNames.get(id) || id.split("/").pop().split(":")[0];
    entry.bar.max = progress.total || 1;
    entry.bar.value = progress.done;
    const left = timeLeft(progress);
    entry.text.replaceChildren(`Downloading ${name}: ${percent(progress)}%`, left ? el("span", { class: "muted" }, ` · ${left}`) : "");
  }
  box.hidden = downloads.size === 0;
}

// An (i) that shows `content` on hover, or keeps it open after a click.
function infoButton(...content) {
  const wrap = el("span", { class: "info-wrap" });
  const button = el("button", {
    type: "button",
    class: "info",
    title: "Details",
    "aria-label": "Details",
    "aria-expanded": "false",
    onclick: (event) => {
      event.stopPropagation();
      const open = !wrap.classList.contains("open");
      closePopovers();
      wrap.classList.toggle("open", open);
      button.setAttribute("aria-expanded", String(open));
    },
  }, "i");
  wrap.append(button, el("div", { class: "popover" }, ...content));
  return wrap;
}

function closePopovers(restoreFocus = false) {
  for (const open of document.querySelectorAll(".info-wrap.open")) {
    const hadFocus = open.contains(document.activeElement);
    open.classList.remove("open");
    const btn = open.querySelector("button.info");
    if (btn) {
      btn.setAttribute("aria-expanded", "false");
      if (restoreFocus && hadFocus) btn.focus();
    }
  }
}
document.addEventListener("click", (event) => {
  if (!event.target.closest(".popover")) closePopovers();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closePopovers(true);
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

// The one pick to suggest first: capable and quick if there's one, else the most capable.
function startingPick(picks) {
  return picks.find((p) => p.role === "fast") || picks.find((p) => p.role === "most_capable") || picks[0];
}

// Whether a model is on this Mac or on its way: then no download needs to stand out.
const haveModel = () => installedIds.size > 0 || [...downloads.keys()].some((id) => id !== judgeDownload);

function pickCard(pick, suggested = false, { load = true } = {}) {
  const left = overview.hardware.total_memory - pick.fit.required;
  const summary = [skill(pick.capability || 0), pick.fast ? "Quick replies" : "Slower replies", `Leaves ${wholeGib(left)} for your other apps`].join(" · ");
  return el(
    "div",
    { class: `card pick${suggested ? " suggested" : ""}` },
    el(
      "div",
      { class: "body" },
      el("div", { class: "title" }, ROLES[pick.role], el("span", { class: "muted" }, pick.name)),
      el("p", { class: "muted" }, summary),
      suggested ? el("div", { class: "chips" }, chip("Good place to start", "accent")) : null,
    ),
    infoButton(...modelDetails(pick, pick.fit)),
    downloadControl(pick, { compact: true, quiet: () => !suggested || haveModel(), load }),
  );
}

function pickCards(picks, options = {}) {
  const first = startingPick(picks);
  return picks.map((pick) => pickCard(pick, pick === first, options));
}

function statusText(status) {
  switch (status.state) {
    case "running": return `Running ${status.name} with a ${Math.round(status.context / 1024)}k context.`;
    case "loading": return `Loading ${status.name}… big models can take a minute.`;
    case "failed": return `Could not start the model: ${status.error}`;
    default: return "No model is loaded.";
  }
}

// Loaded (or loading) models: in memory, ready for a chat.
const isLoaded = (id) => overview?.models.some((m) => m.id === id && m.state !== "failed");
const isLoading = (id) => overview?.models.some((m) => m.id === id && m.state === "loading");


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
  run("models");
  let error = null;
  try {
    await loading;
  } catch (e) {
    error = e;
  }
  await refreshOverview().catch(() => {});
  run("models");
  // Startup failures show in the model status banner; surface pre-flight errors that never reached it.
  if (error && !overview?.models.some((m) => m.id === id && m.state === "failed")) {
    alert(error);
  }
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

let judgeDownload = "judge"; // the judge's download id, as judge_settings reports it

async function downloadJudge() {
  downloads.set(judgeDownload, { done: 0, total: 0 });
  renderActivity();
  run("judge");
  try {
    await invoke("download_judge");
  } catch (e) {
    downloads.delete(judgeDownload);
    renderActivity();
    alert(e);
  }
  run("judge");
}
