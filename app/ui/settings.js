// --- Tabs ---
const TAB_ORDER = ["models", "projects", "safety", "connect"];
let activeTab = "models";

function showTab(name) {
  // The setup guide has a window of its own.
  if (name === "guide") return invoke("open_window", { name: "setup" });
  if (name === "installed") {
    // The old tab's name, from links made before Models and Installed were one tab.
    modelFilter = "local";
    name = "models";
  }
  activeTab = name;
  for (const button of document.querySelectorAll("nav button[data-tab]")) {
    const selected = button.dataset.tab === name;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-selected", String(selected));
    button.tabIndex = selected ? 0 : -1;
  }
  for (const section of document.querySelectorAll("main section")) section.hidden = section.id !== name;
  if (name === "models") renderModelsTab();
  if (name === "projects") renderProjects();
  if (name === "connect") renderConnect();
  if (name === "safety") {
    renderJudge();
    renderCalls();
  }
}
const navTabs = [...document.querySelectorAll("nav button[data-tab]")];
for (const button of navTabs) {
  button.addEventListener("click", () => showTab(button.dataset.tab));
  button.addEventListener("keydown", (event) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const idx = navTabs.indexOf(button);
    let next = -1;
    if (event.key === "ArrowRight") next = (idx + 1) % navTabs.length;
    else if (event.key === "ArrowLeft") next = (idx - 1 + navTabs.length) % navTabs.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = navTabs.length - 1;
    if (next >= 0) {
      event.preventDefault();
      showTab(navTabs[next].dataset.tab);
      navTabs[next].focus();
    }
  });
}
$("open-setup").addEventListener("click", () => invoke("open_window", { name: "setup" }));

let modelFilter = "suggested"; // or "local"

// What this Mac has, above the suggestions.
hooks.overview.push(() => {
  const hw = overview.hardware;
  const kind = hw.unified_memory ? "This Mac" : "This computer";
  const judge = overview.model_budget < hw.model_budget ? ` (${gib(hw.model_budget - overview.model_budget)} is kept for the judge, see Safety)` : "";
  $("machine").textContent = `${kind} has ${wholeGib(hw.total_memory)} of memory. Each choice below fits; they differ in how well they code, how quickly they answer and how much memory they leave for your other apps.`;
  $("budget").textContent = `Up to ${gib(overview.model_budget)} can go to a model${judge}. Only models that fit are shown.`;
});
hooks.models.push(() => {
  renderStatus();
  if (modelFilter === "local") renderInstalled();
});
hooks.progress.push((event) => event.id === judgeDownload && renderJudge());
hooks.finished.push((event) => {
  if (event.id === judgeDownload) renderJudge();
  renderLocalCount();
  if (modelFilter === "local") renderInstalled();
});

// How many models are on this Mac, on the filter, without opening it.
function renderLocalCount() {
  $("model-filter").querySelector('[data-filter="local"]').textContent = installedIds.size ? `On this Mac (${installedIds.size})` : "On this Mac";
}
hooks.judge.push(renderJudge);
// Started here or in the setup window: the other suggestions stop standing out.
hooks.started.push((id) => (id === judgeDownload ? renderJudge() : rerenderAllDownloads()));

// --- Models tab ---

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

async function renderOffers() {
  const { picks, offers, hidden } = await invoke("catalog_offers");
  $("picks").replaceChildren(...pickCards(picks));
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
function renderModelsTab() {
  renderStatus();
  setModelFilter(modelFilter);
}

function setModelFilter(filter) {
  modelFilter = filter;
  for (const button of document.querySelectorAll("#model-filter button")) {
    const selected = button.dataset.filter === filter;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-selected", String(selected));
    button.tabIndex = selected ? 0 : -1;
  }
  $("models-suggested").hidden = filter !== "suggested";
  $("models-local").hidden = filter !== "local";
  if (filter === "local") {
    renderInstalled();
    invoke("other_apps").then(renderOtherApps);
    invoke("saved_conversations").then(renderConversations);
  }
}
const filterButtons = [...document.querySelectorAll("#model-filter button")];
for (const button of filterButtons) {
  button.addEventListener("click", () => setModelFilter(button.dataset.filter));
  button.addEventListener("keydown", (event) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const idx = filterButtons.indexOf(button);
    let next = -1;
    if (event.key === "ArrowRight") next = (idx + 1) % filterButtons.length;
    else if (event.key === "ArrowLeft") next = (idx - 1 + filterButtons.length) % filterButtons.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = filterButtons.length - 1;
    if (next >= 0) {
      event.preventDefault();
      setModelFilter(filterButtons[next].dataset.filter);
      filterButtons[next].focus();
    }
  });
}

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
  try {
    renderOtherApps(await invoke("rescan_models"));
    await renderInstalled();
    renderOffers();
  } catch (e) {
    alert(e);
  } finally {
    $("rescan").disabled = false;
  }
});

const diskGb = (bytes) => `${(bytes / 1e9).toFixed(1)} GB`;

function renderConversations(info) {
  $("save-conversations").checked = info.enabled;
  $("clear-conversations").disabled = !info.count;
  $("conversations-summary").textContent = !info.enabled
    ? "Off."
    : info.count
      ? `${info.count} saved, ${diskGb(info.bytes)} of ${diskGb(info.limit)} allowed.`
      : `None saved yet. Up to ${diskGb(info.limit)} allowed.`;
}

$("save-conversations").addEventListener("change", async (event) => {
  renderConversations(await invoke("set_save_conversations", { enabled: event.target.checked }));
});
$("clear-conversations").addEventListener("click", async () => {
  renderConversations(await invoke("clear_saved_conversations"));
});

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
});

// --- Connect tab ---
async function renderConnect() {
  await refreshOverview();
  const [info, guides] = await Promise.all([invoke("api_info"), invoke("connect_guides")]);
  $("base-url").value = info.base_url || "";
  $("api-key").value = info.api_key;
  $("model-name").value = info.model || "(load a model first)";
  $("hf-status").textContent = overview.hf_token_set ? "A token is saved." : "No token saved.";
  const box = $("harness-guides");
  if (box) {
    box.replaceChildren(
      ...guides.map((g) => {
        const copyBtn = el("button", {}, "Copy config");
        copyBtn.addEventListener("click", async () => {
          await navigator.clipboard.writeText(g.snippet);
          copyBtn.textContent = "Copied";
          setTimeout(() => (copyBtn.textContent = "Copy config"), 1200);
        });
        return el(
          "details",
          { open: g.id === "pi", class: "guide" },
          el("summary", {}, el("strong", {}, g.name), " — ", el("span", { class: "muted small" }, g.subtitle)),
          el("div", { class: "row" }, el("code", { class: "small muted selectable" }, g.file_path), copyBtn),
          el("pre", { class: "selectable" }, g.snippet),
          el("p", { class: "muted small" }, g.notes),
        );
      }),
    );
  }
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
  if (!confirm("Make a new API key? External tools using the old key will need to be updated.")) return;
  await invoke("regenerate_api_key");
  renderConnect();
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
      ...(items.length ? [el("div", { class: "chips" }, items.map((item, i) => el("span", { class: "chip removable" }, item, el("button", { type: "button", title: "Remove", "aria-label": `Remove ${item}`, onclick: () => { items.splice(i, 1); onChange(); render(); } }, "×"))))] : []),
      el("div", { class: "row" }, input, el("button", { type: "button", onclick: add }, "Add")),
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
  box.addEventListener("keydown", (event) => {
    const mod = event.metaKey || event.ctrlKey;
    if (mod && (event.key.toLowerCase() === "s" || event.key === "Enter")) {
      event.preventDefault();
      saveProject(draft);
    } else if (event.key === "Escape") {
      event.preventDefault();
      editingProject = null;
      renderProjects();
    }
  });
  render();
  queueMicrotask(() => box.querySelector('input[type="text"]')?.focus());
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
async function renderJudge() {
  if ($("include-image-paths") && overview) {
    $("include-image-paths").checked = overview.include_image_paths !== false;
  }
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
$("include-image-paths")?.addEventListener("change", async (event) => {
  try {
    const enabled = await invoke("set_include_image_paths", { enabled: event.target.checked });
    if (overview) overview.include_image_paths = enabled;
  } catch (e) {
    alert(e);
  }
});
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

const MAX_CALL_ROWS = 200;
listen("guard-call", ({ payload }) => {
  const list = $("calls");
  if (list.querySelector("p")) list.replaceChildren();
  list.prepend(callRow(payload));
  while (list.children.length > MAX_CALL_ROWS) list.lastElementChild.remove();
});
listen("discovery", async () => {
  await refreshInstalledIds();
  renderLocalCount();
  renderOffers();
  if (modelFilter === "local") {
    renderInstalled();
    invoke("other_apps").then(renderOtherApps);
  }
});

// --- Keyboard shortcuts ---
document.addEventListener("keydown", (event) => {
  const mod = event.metaKey || event.ctrlKey;
  if (mod && !event.shiftKey && !event.altKey) {
    const num = Number(event.key);
    if (num >= 1 && num <= TAB_ORDER.length) {
      event.preventDefault();
      showTab(TAB_ORDER[num - 1]);
      return;
    }
    if (event.key.toLowerCase() === "f") {
      event.preventDefault();
      showTab("models");
      setModelFilter("suggested");
      $("more-models").open = true;
      $("search").focus();
      return;
    }
  }
  if ((mod && event.shiftKey && (event.key === "[" || event.key === "]")) || (event.ctrlKey && event.key === "Tab")) {
    event.preventDefault();
    const delta = event.key === "[" || (event.key === "Tab" && event.shiftKey) ? -1 : 1;
    const idx = TAB_ORDER.indexOf(activeTab);
    showTab(TAB_ORDER[(idx + delta + TAB_ORDER.length) % TAB_ORDER.length]);
    return;
  }
  if (event.key === "Escape" && editingProject) {
    event.preventDefault();
    editingProject = null;
    renderProjects();
  }
});

// --- Start ---
(async () => {
  await refreshOverview();
  await refreshInstalledIds();
  await renderOffers();
  renderLocalCount();
  renderStatus();
  const tab = location.hash.slice(1);
  if (tab) showTab(tab);
})();
