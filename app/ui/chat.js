const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

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

// --- State ---
// The window shows one chat at a time. Both engines are drawn from the Agent Client
// Protocol's updates: OpenCode sends them as they are, and the built-in agent's messages
// are turned into them (see `builtin` below).
let engine = "builtin"; // Entropy (Agent Superviser's lean built-in coding harness)
let projects = [];
let project = null;
let session = null; // null: a new chat, made when the first message is sent
let busy = false;
let status = null; // the model server's
let pending = []; // events for a chat whose id isn't known yet
let awaiting = false;
let usage = null; // {used, size} tokens of the open chat, as OpenCode reports them
let archived = new Set(); // "projectId\nchatId" keys of archived chats across projects
let showArchived = false;
let filterCurrentProject = false;
let allSessions = []; // unified cross-project session list
const workingSinceBySession = new Map(); // sessionId -> epoch ms

const archiveKey = (projectId, chatId) => `${projectId}\n${chatId}`;

function projectMonogram(name) {
  const words = String(name || "").split(/[^A-Za-z0-9]+/).filter(Boolean);
  const chars = words.length >= 2 ? words[0][0] + words[1][0] : String(name || "").replace(/[^A-Za-z0-9]/g, "").slice(0, 2);
  return (chars || "PR").toUpperCase();
}

function projectBadgeStyle(id) {
  const dark = window.matchMedia?.("(prefers-color-scheme: dark)").matches;
  const palette = dark
    ? [
        ["#2f3542", "#cbd5e1"],
        ["#3b2c5e", "#d8b4fe"],
        ["#164e63", "#67e8f9"],
        ["#1e4633", "#86efac"],
        ["#523718", "#fcd34d"],
        ["#522238", "#fda4af"],
        ["#1e3a5f", "#93c5fd"],
        ["#364420", "#bef264"],
      ]
    : [
        ["#e2e8f0", "#334155"],
        ["#f3e8ff", "#6b21a8"],
        ["#cffafe", "#155e75"],
        ["#dcfce7", "#166534"],
        ["#fef3c7", "#92400e"],
        ["#ffe4e6", "#9f1239"],
        ["#dbeafe", "#1e40af"],
        ["#ecfccb", "#3f6212"],
      ];
  let hash = 2166136261;
  for (let i = 0; i < String(id || "").length; i++) {
    hash ^= id.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  const [bg, fg] = palette[(hash >>> 0) % palette.length];
  return `background:${bg};color:${fg}`;
}

function parseSessionTime(raw) {
  const candidate = raw.updatedAt ?? raw.updated_at ?? raw.updated ?? raw.createdAt ?? raw.created_at;
  if (typeof candidate === "number" && Number.isFinite(candidate)) {
    return candidate > 1e10 ? Math.floor(candidate / 1000) : Math.floor(candidate);
  }
  if (typeof candidate === "string") {
    const ms = Date.parse(candidate);
    if (Number.isFinite(ms)) return Math.floor(ms / 1000);
  }
  return 0;
}

function formatRelTime(epochSecs) {
  if (!epochSecs) return "";
  const diff = Math.max(0, Math.floor(Date.now() / 1000) - epochSecs);
  if (diff < 60) return "now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h`;
  if (diff < 86400 * 30) return `${Math.floor(diff / 86400)}d`;
  return `${Math.floor(diff / (86400 * 30))}mo`;
}

function formatWorkingCompact(startMs) {
  const secs = Math.max(0, Math.floor((Date.now() - startMs) / 1000));
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}

const view = {
  tools: new Map(), // toolCallId -> card
  texts: new Map(), // messageId -> {node, text}
  thoughts: new Map(),
  users: new Map(),
};

// --- Drawing ---

// Text with ```fenced``` blocks, `inline code`, **bold** and # headings, built without HTML.
// Inline code naming one of the project's files becomes a link to it.
function richText(text) {
  const box = el("div", { class: "text" });
  text.split("```").forEach((part, i) => {
    if (i % 2) {
      box.append(el("pre", {}, el("code", {}, part.replace(/^[^\n]*\n/, "").replace(/\n$/, ""))));
    } else if (part.trim()) {
      const prose = el("div", { class: "prose" });
      const lines = part.replace(/^\n+|\n+$/g, "").split("\n");
      lines.forEach((line, n) => {
        const heading = /^#{1,6}\s+(.*)$/.exec(line);
        if (heading) prose.append(el("div", { class: "md-h" }, inline(heading[1])));
        else prose.append(...inline(line), n < lines.length - 1 ? "\n" : "");
      });
      box.append(prose);
    }
  });
  return box;
}

function inline(text) {
  return text.split(/`([^`\n]+)`/).flatMap((piece, i) => {
    if (i % 2) {
      const code = el("code", {}, piece);
      if (looksLikePath(piece)) linkPath(code, piece);
      return [code];
    }
    return piece.split(/\*\*([^*\n]+)\*\*/).map((part, j) => (j % 2 ? el("strong", {}, part) : part));
  });
}

const looksLikePath = (text) => !/\s/.test(text) && (text.includes("/") || /\.[A-Za-z0-9]{1,8}$/.test(text)) && !/^[a-z]+:\/\//i.test(text);

// --- Opening the project's files ---

let editors = null; // the installed ones, asked once
const fileInfo = new Map(); // "project\npath" -> what `project_file` said, asked once per chat

// Makes `node` open `path` if it's one of the project's files: a click opens it, a right-click offers more.
async function linkPath(node, path) {
  if (!project || !path || node.dataset.path === path) return;
  node.dataset.path = path;
  const key = `${project.id}\n${path}`;
  if (!fileInfo.has(key)) fileInfo.set(key, invoke("project_file", { project: project.id, path }).catch(() => null));
  const info = await fileInfo.get(key);
  if (!info || node.dataset.path !== path) return;
  node.classList.add("path-link");
  node.tabIndex = 0;
  node.setAttribute("role", "button");
  node.title = info.runs_code ? "Could run as a program: click for options" : `${info.is_dir ? "Show in Finder" : "Open"}; right-click for more`;
  node.onclick = (event) => {
    event.preventDefault();
    event.stopPropagation();
    if (info.runs_code) fileMenu(event, info);
    else openFile(info.path, "default");
  };
  node.onkeydown = (event) => {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    event.stopPropagation();
    if (info.runs_code) {
      const rect = node.getBoundingClientRect();
      fileMenu({ clientX: rect.left, clientY: rect.bottom }, info);
    } else {
      openFile(info.path, "default");
    }
  };
  node.oncontextmenu = (event) => {
    event.preventDefault();
    event.stopPropagation();
    fileMenu(event, info);
  };
}

async function openFile(path, how, editor = null) {
  try {
    await invoke("open_file", { project: project.id, path, how, editor });
  } catch (e) {
    alert(e);
  }
}

let menuTrigger = null;

async function fileMenu(event, info) {
  const trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  closeMenu();
  menuTrigger = trigger;
  editors ??= await invoke("editors");
  const item = (label, action) => el("button", { type: "button", role: "menuitem", class: "menu-item", onclick: () => { closeMenu(); action(); } }, label);
  const menu = el(
    "div",
    { class: "menu", id: "file-menu", role: "menu" },
    info.runs_code
      ? el("div", { class: "menu-note" }, "This could run as a program, outside the sandbox, so it only opens in an editor or Finder.")
      : item(info.is_dir ? "Open in Finder" : "Open", () => openFile(info.path, "default")),
    ...editors.filter((name) => !(info.is_dir && name === "TextEdit")).map((name) => item(`Open in ${name}`, () => openFile(info.path, "editor", name))),
    item("Show in Finder", () => openFile(info.path, "finder")),
  );
  menu.addEventListener("keydown", (e) => {
    const items = [...menu.querySelectorAll(".menu-item")];
    if (!items.length) return;
    const idx = items.indexOf(document.activeElement);
    if (e.key === "ArrowDown" || (e.key === "Tab" && !e.shiftKey)) {
      e.preventDefault();
      items[(idx + 1 + items.length) % items.length].focus();
    } else if (e.key === "ArrowUp" || (e.key === "Tab" && e.shiftKey)) {
      e.preventDefault();
      items[(idx - 1 + items.length) % items.length].focus();
    } else if (e.key === "Home") {
      e.preventDefault();
      items[0].focus();
    } else if (e.key === "End") {
      e.preventDefault();
      items[items.length - 1].focus();
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeMenu(true);
    }
  });
  document.body.append(menu);
  const { innerWidth, innerHeight } = window;
  menu.style.left = `${Math.min(event.clientX, innerWidth - menu.offsetWidth - 8)}px`;
  menu.style.top = `${Math.min(event.clientY, innerHeight - menu.offsetHeight - 8)}px`;
  menu.querySelector(".menu-item")?.focus();
}

function closeMenu(restoreFocus = false) {
  const open = $("file-menu");
  if (!open) return;
  open.remove();
  if (restoreFocus && menuTrigger?.isConnected) menuTrigger.focus();
  menuTrigger = null;
}
document.addEventListener("click", (event) => {
  if (!event.target.closest("#file-menu")) closeMenu();
});
window.addEventListener("blur", () => closeMenu(false));

function textOf(content) {
  return content?.type === "text" ? content.text || "" : "";
}

function list() {
  return $("messages");
}

function clearView() {
  for (const map of Object.values(view)) map.clear();
  // Files may have come or gone since.
  fileInfo.clear();
  list().replaceChildren();
}

const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
const MAX_IMAGES_PER_MESSAGE = 8;
let pendingImages = [];
let attachNotice = "";
let nextImageId = 1;

function formatBytes(bytes) {
  if (!bytes || bytes < 1024) return `${bytes || 0} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function renderPendingImages() {
  const strip = $("pending-images");
  if (!strip) return;
  if (!pendingImages.length && !attachNotice) {
    strip.hidden = true;
    strip.replaceChildren();
    return;
  }
  strip.hidden = false;
  strip.replaceChildren(
    ...(attachNotice ? [el("div", { class: "pending-img-notice" }, attachNotice)] : []),
    ...pendingImages.map((img) =>
      el(
        "div",
        { class: "pending-img-card" },
        el("img", { src: `data:${img.mime_type};base64,${img.data_base64}`, alt: img.name }),
        el(
          "div",
          { class: "pending-img-meta" },
          el("span", { class: "pending-img-name", title: img.name }, img.name),
          el("span", { class: "pending-img-size" }, formatBytes(img.size_bytes)),
        ),
        el("button", {
          type: "button",
          class: "icon",
          title: `Remove ${img.name}`,
          "aria-label": `Remove ${img.name}`,
          onclick: () => {
            pendingImages = pendingImages.filter((item) => item.id !== img.id);
            if (pendingImages.length < MAX_IMAGES_PER_MESSAGE) attachNotice = "";
            renderPendingImages();
          },
        }, "×"),
      ),
    ),
    ...(pendingImages.length > 1
      ? [el("button", {
          type: "button",
          class: "small",
          onclick: () => {
            pendingImages = [];
            attachNotice = "";
            renderPendingImages();
          },
        }, "Clear all")]
      : []),
  );
}

function readFileAsImage(file) {
  return new Promise((resolve, reject) => {
    const type = (file.type || "").toLowerCase();
    if (!type.startsWith("image/")) {
      reject(`${file.name || "File"} is not an image`);
      return;
    }
    if (file.size > MAX_IMAGE_BYTES) {
      reject(`${file.name || "Image"} is too large (max 20 MB)`);
      return;
    }
    const reader = new FileReader();
    reader.onload = () => {
      const result = String(reader.result || "");
      const idx = result.indexOf(",");
      if (idx < 0) {
        reject(`Couldn't read ${file.name || "image"}`);
        return;
      }
      const mime = type === "image/jpg" ? "image/jpeg" : type;
      const ext = mime.split("/")[1] || "png";
      resolve({
        id: nextImageId++,
        name: file.name || `Screenshot ${nextImageId - 1}.${ext}`,
        mime_type: mime,
        data_base64: result.slice(idx + 1),
        size_bytes: file.size,
      });
    };
    reader.onerror = () => reject(`Couldn't read ${file.name || "image"}`);
    reader.readAsDataURL(file);
  });
}

async function attachImageFiles(files) {
  const listFiles = [...(files || [])].filter(Boolean);
  if (!listFiles.length) return false;
  const errors = [];
  let added = 0;
  for (const file of listFiles) {
    if (pendingImages.length >= MAX_IMAGES_PER_MESSAGE) {
      errors.push(`Maximum ${MAX_IMAGES_PER_MESSAGE} images per message.`);
      break;
    }
    try {
      const img = await readFileAsImage(file);
      pendingImages.push(img);
      added++;
    } catch (err) {
      errors.push(String(err));
    }
  }
  attachNotice = errors.join(" · ");
  renderPendingImages();
  if (added > 0) $("input").focus();
  return added > 0 || errors.length > 0;
}

function renderUserBubble(entry) {
  entry.node.replaceChildren(
    ...(entry.images.length
      ? [el(
          "div",
          { class: "msg-images" },
          ...entry.images.map((img) =>
            el(
              "div",
              { class: "msg-image-item" },
              el("img", { src: `data:${img.mime_type};base64,${img.data_base64}`, alt: img.name || "Attached image" }),
              img.name ? el("span", { class: "msg-image-name" }, img.name) : null,
            ),
          ),
        )]
      : []),
    ...(entry.text ? [el("div", {}, entry.text)] : []),
  );
}

function appendUser(id, text, images = []) {
  list().querySelector(".empty")?.remove();
  view.texts.delete("current");
  view.thoughts.delete("current");
  let entry = id && view.users.get(id);
  if (!entry) {
    entry = { node: el("div", { class: "msg user" }), text: "", images: [] };
    if (id) view.users.set(id, entry);
    list().append(entry.node);
  }
  if (text) entry.text += text;
  if (images?.length) entry.images.push(...images);
  renderUserBubble(entry);
  scrollDown();
  return entry.node;
}

// A reply's text grows chunk by chunk; `set` replaces it (the built-in agent sends it whole).
function agentText(id, text, set = false) {
  let entry = view.texts.get(id);
  if (!entry) {
    entry = { node: el("div", { class: "msg assistant" }), text: "" };
    view.texts.set(id, entry);
    list().append(entry.node);
  }
  entry.text = set ? text : entry.text + text;
  // The guard puts its notes where a blocked call was.
  const lines = entry.text.split("\n");
  const notes = lines.filter((l) => l.startsWith("[guard]"));
  const rest = lines.filter((l) => !l.startsWith("[guard]")).join("\n");
  entry.node.replaceChildren(
    ...(rest.trim() ? [richText(rest)] : []),
    ...notes.map((line) => el("div", { class: "guard-note" }, line.replace(/^\[guard\] blocked call to (`[^`]+`)/, "The guard blocked a $1 call").replace(/^\[guard\]\s*/, "The guard: "))),
  );
  entry.node.hidden = !rest.trim() && !notes.length;
  scrollDown();
}

function agentThought(id, text, set = false) {
  let entry = view.thoughts.get(id);
  if (!entry) {
    const body = el("div");
    entry = { node: el("details", { class: "thinking" }, el("summary", {}, "Thinking"), body), body, text: "" };
    view.thoughts.set(id, entry);
    list().append(entry.node);
  }
  entry.text = set ? text : entry.text + text;
  entry.body.textContent = entry.text;
  scrollDown();
}

const VERBS = { read: "Read", edit: "Edit", delete: "Delete", move: "Move", search: "Search", execute: "Run", fetch: "Fetch", think: "Think", switch_mode: "Switch mode" };

// What a call is about, in a few words.
function subject(call) {
  const input = call.rawInput || {};
  const where = (call.locations || [])[0]?.path;
  // The Linux VM sees the main folder as /workspace.
  const main = project?.isolation === "linux-vm" ? "/workspace" : project?.folders[0].path;
  const relative = (path) => (main && path?.startsWith(main + "/") ? path.slice(main.length + 1) : path);
  switch (call.kind) {
    case "execute":
      return input.command || call.title;
    case "search":
      return input.pattern || input.query || call.title;
    case "fetch":
      return input.url || call.title;
    case "read":
    case "edit":
    case "delete":
    case "move":
      return relative(input.path || input.filePath || where) || call.title;
    default:
      return call.title && call.title !== call.name ? call.title : "";
  }
}

// Changed lines between two texts, with two lines around them.
function diffLines(oldText, newText) {
  const a = (oldText || "").split("\n");
  const b = (newText || "").split("\n");
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--;
    endB--;
  }
  const from = Math.max(0, start - 2);
  const line = (kind, sign, text) => el("div", { class: `diff ${kind}` }, `${sign} ${text}`);
  return [
    ...a.slice(from, start).map((t) => line("same", " ", t)),
    ...a.slice(start, endA).map((t) => line("del", "-", t)),
    ...b.slice(start, endB).map((t) => line("add", "+", t)),
    ...b.slice(endB, endB + 2).map((t) => line("same", " ", t)),
  ];
}

function upsertTool(update) {
  let card = view.tools.get(update.toolCallId);
  if (!card) {
    const verb = el("span", { class: "tool-verb" });
    const what = el("code", { class: "tool-what" });
    const state = el("span", { class: "tool-state" });
    const body = el("div", { class: "tool-body" });
    const box = el("details", { class: "tool" }, el("summary", {}, verb, " ", what, state), body);
    card = { box, verb, what, state, body, call: {}, approval: null };
    view.tools.set(update.toolCallId, card);
    list().append(box);
  }
  // Updates only carry what changed.
  for (const [key, value] of Object.entries(update)) {
    if (value !== undefined && value !== null && !(Array.isArray(value) && !value.length && card.call[key]?.length)) card.call[key] = value;
  }
  const call = card.call;
  card.verb.textContent = VERBS[call.kind] || call.name || call.title || "Tool";
  const about = subject(call) || "";
  if (card.what.textContent !== about) card.what.textContent = about;
  if (["read", "edit", "delete", "move"].includes(call.kind) && about) linkPath(card.what, about);
  const failed = call.status === "failed";
  card.box.classList.toggle("failed", failed);
  // A finished call has no question left.
  if (card.approval && (call.status === "completed" || failed)) {
    card.approval.remove();
    card.approval = null;
  }
  if (!card.approval) card.state.textContent = { pending: "", in_progress: "running…", failed: "failed", completed: "" }[call.status] || "";
  const parts = [];
  for (const item of call.content || []) {
    if (item.type === "diff") parts.push(el("div", { class: "diff-box" }, diffLines(item.oldText, item.newText)));
    else if (item.type === "content" && textOf(item.content)) parts.push(el("pre", {}, textOf(item.content)));
  }
  // An edit not yet made shows what it would do.
  if (!parts.length && call.kind === "edit" && (call.rawInput?.oldString !== undefined || call.rawInput?.old_string !== undefined)) {
    parts.push(el("div", { class: "diff-box" }, diffLines(call.rawInput.oldString ?? call.rawInput.old_string, call.rawInput.newString ?? call.rawInput.new_string)));
  }
  card.body.replaceChildren(...parts);
  if (call.kind === "edit" && parts.length) card.box.open = true;
  scrollDown();
}

const OPTION_LABELS = { allow_once: "Allow", allow_always: "Allow for this chat", reject_once: "Deny", reject_always: "Never" };
const OPTION_TITLES = {
  allow_once: "Allow this once",
  allow_always: "Don't ask again for calls like this in this chat. The guard still checks each one.",
  reject_once: "Don't run it; the agent is told and can try something else",
  reject_always: "Don't run calls like this in this chat",
};

function askPermission(request, toolCall, options) {
  upsertTool(toolCall);
  const card = view.tools.get(toolCall.toolCallId);
  card.approval?.remove();
  card.box.open = true;
  card.state.textContent = "waiting for you";
  const answer = (option) => {
    reply(request, option);
    card.approval.remove();
    card.approval = null;
    card.state.textContent = option?.kind.startsWith("allow") ? "running…" : "declined";
    renderWorking();
  };
  const question = { execute: "Run this command?", edit: "Make this change?", delete: "Delete this?", fetch: "Fetch this page?" }[card.call.kind] || "Allow this?";
  const where = project?.name || "the project";
  const why = {
    execute: `It runs in the sandbox: it can change files only in ${where}, and the guard has checked it.`,
    edit: `Only files in ${where} can change.`,
    delete: `Only files in ${where} can be deleted.`,
    fetch: "The guard has checked the address.",
  }[card.call.kind];
  card.approval = el(
    "div",
    { class: "approval" },
    el("div", { class: "approval-text" }, el("strong", {}, question), why ? el("div", { class: "small muted" }, why) : null),
    // "Never" is left out: Deny covers it, and fewer buttons are easier to answer.
    ...options.filter((option) => option.kind !== "reject_always" || !options.some((o) => o.kind === "reject_once")).map((option) => el("button", { class: option.kind === "allow_once" ? "primary" : "", title: OPTION_TITLES[option.kind] || "", onclick: () => answer(option) }, OPTION_LABELS[option.kind] || option.name)),
  );
  card.box.append(card.approval);
  card.box.scrollIntoView({ block: "nearest" });
  renderWorking();
}

function applyUpdate(change) {
  switch (change.sessionUpdate) {
    case "user_message_chunk": {
      const content = change.content;
      if (content?.type === "image" && content.data) {
        appendUser(change.messageId, "", [{
          name: "Image",
          mime_type: content.mimeType || "image/png",
          data_base64: content.data,
        }]);
      } else {
        const t = textOf(content);
        if (!t.trimStart().startsWith("[Attached image file paths]")) {
          appendUser(change.messageId, t);
        }
      }
      break;
    }
    case "agent_message_chunk":
      agentText(change.messageId || "current", textOf(change.content));
      break;
    case "agent_thought_chunk":
      agentThought(change.messageId || "current", textOf(change.content));
      break;
    case "tool_call":
    case "tool_call_update":
      upsertTool(change);
      break;
    case "usage_update":
      if (change.size) {
        usage = { used: change.used, size: change.size };
        $("usage").textContent = `Chat ${Math.round((change.used / change.size) * 100)}% full`;
        $("usage").title = `${Math.round(change.used / 1000)}k of ${Math.round(change.size / 1000)}k tokens of the model's context`;
        update();
      }
      break;
  }
}

function scrollDown() {
  const box = list();
  if (box.scrollHeight - box.scrollTop - box.clientHeight < 240) box.scrollTop = box.scrollHeight;
}

// --- The two engines ---

const opencode = {
  async sessions(projectId = project?.id) {
    const sessions = await invoke("acp_sessions", { project: projectId });
    return sessions.map((s) => ({ id: s.sessionId, title: s.title?.startsWith("New session - ") ? "New chat" : s.title || "Chat" }));
  },
  // Replays the session as events while it opens.
  async open(id, projectId = project?.id) {
    const opened = await invoke("acp_open", { project: projectId, session: id });
    return opened.busy;
  },
  async send(text, { projectId = project?.id, sessionId = session, model = chatModel, images = [], onSession } = {}) {
    let target = sessionId;
    if (!target) {
      awaiting = true;
      try {
        target = (await invoke("acp_open", { project: projectId, session: null })).session;
        onSession?.(target);
      } finally {
        awaiting = false;
      }
    }
    await invoke("acp_prompt", { project: projectId, session: target, text, model, images });
    return target;
  },
  stop: (projectId = project?.id, sessionId = session) => invoke("acp_cancel", { project: projectId, session: sessionId }),
  remove: (id, projectId = project?.id) => invoke("acp_delete", { project: projectId, session: id }),
};

// The built-in agent keeps OpenAI-style messages; these become the same updates.
const builtin = {
  live: 0,
  async sessions(projectId = project?.id) {
    return (await invoke("chat_list", { project: projectId })).map((c) => ({ id: c.id, title: c.title }));
  },
  async open(id, projectId = project?.id) {
    const chat = await invoke("chat_load", { project: projectId, id });
    chat.messages.forEach((message, i) => this.message(message, `m${i}`, true));
    return chat.busy;
  },
  async send(text, { projectId = project?.id, sessionId = session, model = chatModel, images = [], onSession } = {}) {
    awaiting = !sessionId;
    try {
      const id = await invoke("chat_send", { project: projectId, chat: sessionId, text, model, images });
      onSession?.(id);
      return id;
    } finally {
      awaiting = false;
    }
  },
  stop: (_projectId = project?.id, sessionId = session) => invoke("chat_stop", { chat: sessionId }),
  remove: (id, projectId = project?.id) => invoke("chat_delete", { project: projectId, id }),
  message(message, id, replay) {
    if (message.role === "user") {
      if (replay) {
        if (Array.isArray(message.content)) {
          const texts = [];
          const images = [];
          for (const part of message.content) {
            if ((part?.type === "text" || part?.type === "input_text") && part.text && !part.superviser_meta) {
              texts.push(part.text);
            } else if (part?.type === "image_url") {
              const url = typeof part.image_url === "string" ? part.image_url : part.image_url?.url;
              const match = typeof url === "string" && url.match(/^data:(image\/[a-z0-9.+-]+);base64,(.+)$/i);
              if (match) {
                images.push({ name: `Image ${images.length + 1}`, mime_type: match[1].toLowerCase(), data_base64: match[2] });
              }
            }
          }
          appendUser(id, texts.join("\n"), images);
        } else {
          appendUser(id, message.content || "");
        }
      }
    } else if (message.role === "assistant") {
      const key = replay ? id : `live${this.live++}`;
      if (message.reasoning) agentThought(replay ? key : "live-thought", message.reasoning, true);
      agentText(replay ? key : "live-text", message.content || "", true);
      if (!replay) {
        // The next reply starts new blocks.
        view.texts.delete("live-text");
        view.thoughts.delete("live-thought");
      }
      for (const call of message.tool_calls || []) {
        let input = {};
        try {
          input = JSON.parse(call.function.arguments || "{}");
        } catch {}
        const kind = { read: "read", glob: "search", grep: "search", write: "edit", edit: "edit", bash: "execute" }[call.function.name] || "other";
        upsertTool({ toolCallId: call.id, name: call.function.name, title: call.function.name, kind, status: "in_progress", rawInput: input });
      }
    } else if (message.role === "tool") {
      const text = message.content || "";
      const failed = text.startsWith("Error:");
      const declined = text.startsWith("The user declined") || text.startsWith("Not run:");
      upsertTool({ toolCallId: message.tool_call_id, status: failed || declined ? "failed" : "completed", content: [{ type: "content", content: { type: "text", text } }] });
    }
  },
};

const engines = { opencode, builtin };

function reply(request, option) {
  if (engine === "opencode") invoke("acp_permission", { project: project.id, request, option: option?.optionId ?? null });
  else invoke("chat_answer", { chat: session, call: request, allow: option?.kind === "allow_once" });
}

// --- Chats ---

// Ways to begin, for an empty chat. They fill in the message box; nothing is sent until you do.
const STARTERS = [
  ["Explain this project", "Explain what this project does and how its main files fit together. Don't change anything."],
  ["Look for bugs", "Look through this project for bugs. Tell me what you find and how you'd fix each one before changing anything."],
  ["Write a README", "Write a README.md that explains what this project is, how to set it up and how to run it."],
  ["Start something new", "If this folder is empty, make a simple web page in index.html with a title, a short introduction and a contact link, in plain HTML and CSS. If it already has files, tell me what's here first and don't change anything."],
];

function emptyState() {
  const asks = { commands: "You'll be asked before commands run.", everything: "You'll be asked before commands run and files change.", never: "It won't ask before acting; the guard and sandbox still check everything." }[project?.approvals || "commands"];
  if (!project) return el("div", { class: "empty" }, el("h2", {}, "Pick a project"));
  const names = (folders) => folders.map((f) => f.path.split("/").pop()).join(", ");
  const writable = project.folders.filter((f, i) => i === 0 || f.writable);
  const readOnly = project.folders.filter((f, i) => i > 0 && !f.writable);
  const reads = readOnly.length ? ` It can also read ${names(readOnly)}.` : "";
  return el(
    "div",
    { class: "empty" },
    el("h2", {}, `What should the agent do in ${project.name}?`),
    el("p", { class: "muted" }, `It can read and change files in ${names(writable)} and run commands there, in a sandbox.${reads} It can't see your passwords, keys or other folders. ${asks}`),
    el(
      "div",
      { class: "starters" },
      STARTERS.map(([label, text]) =>
        el("button", {
          class: "starter",
          onclick: () => {
            $("input").value = text;
            $("input").focus();
          },
        }, label),
      ),
    ),
    el("p", { class: "small muted" }, "Describe what you want in plain words. One task per chat works best."),
  );
}

let chatGen = 0;
let listGen = 0;
let projectGen = 0;
const SEND_CANCELLED = Symbol("cancelled");
let sending = null; // active send() pre-dispatch cancellation handle

async function showChat(id) {
  const mine = ++chatGen;
  if (sending) {
    sending.cancel();
    sending = null;
  }
  const proj = project;
  session = id;
  busy = false;
  usage = null;
  pendingImages = [];
  attachNotice = "";
  renderPendingImages();
  if (!awaiting) pending.length = 0;
  clearView();
  subagents = [];
  inspectedSubagent = null;
  renderSubagents();
  $("usage").textContent = "";
  $("title").textContent = "New chat";
  if (id && proj) {
    invoke("subagents_list", { project: proj.id, session: id })
      .then((list) => {
        if (mine === chatGen) {
          subagents = list || [];
          renderSubagents();
        }
      })
      .catch(() => {});
    try {
      const isBusy = await engines[engine].open(id, proj.id);
      if (mine !== chatGen) return;
      busy = isBusy;
    } catch (e) {
      if (mine !== chatGen) return;
      list().append(el("div", { class: "banner bad" }, String(e)));
    }
  } else {
    list().append(emptyState());
  }
  // Its own model, unless that one was deleted since.
  const own = id && proj ? await invoke("chat_model", { project: proj.id, chat: id }).catch(() => null) : null;
  if (mine !== chatGen) return;
  chatModel = models.some((m) => m.id === own) ? own : defaultModel();
  await refreshRoom();
  if (mine !== chatGen) return;
  list().scrollTop = list().scrollHeight;
  await renderChatList();
  if (mine !== chatGen) return;
  update();
  if (!id) $("input").focus();
}

let chatTitles = new Map();

function updateHeaderProject() {
  const badge = $("header-badge");
  const projName = $("header-project");
  const settingsBtn = $("project-settings-btn");
  if (!project) {
    badge.hidden = true;
    projName.textContent = "";
    if (settingsBtn) settingsBtn.hidden = true;
    return;
  }
  badge.hidden = false;
  badge.textContent = projectMonogram(project.name);
  badge.setAttribute("style", projectBadgeStyle(project.id));
  projName.textContent = `${project.name} /`;
  if (settingsBtn) {
    settingsBtn.hidden = false;
    settingsBtn.title = `Project settings for ${project.name} (${project.folders[0].path})`;
  }
}

let filterProjectId = null;
let approvalPendingSessions = new Set();
let doneSessions = new Set();
let projectsPopoutOpen = false;
let projectsPopoutSelected = 1;
let newChatModalOpen = false;
let newChatSelected = 0;

function orderedProjects(query = "") {
  const list = [...projects];
  if (project) {
    const idx = list.findIndex((p) => p.id === project.id);
    if (idx > 0) list.unshift(...list.splice(idx, 1));
  }
  const q = query.trim().toLowerCase();
  if (!q) return list;
  return list.filter(
    (p) =>
      p.name.toLowerCase().includes(q) ||
      (p.folders?.[0]?.path || "").toLowerCase().includes(q),
  );
}

function renderFilterChip() {
  const slot = $("project-filter-slot");
  if (!slot) return;
  const filtered = filterProjectId && projects.find((p) => p.id === filterProjectId);
  if (filtered) {
    slot.replaceChildren(
      el(
        "button",
        {
          type: "button",
          class: "filter-dropdown-btn filtered",
          title: "Filter chats by project (⌘⇧P)",
          onclick: (event) => {
            event.stopPropagation();
            if (projectsPopoutOpen) closeProjectsPopout();
            else openProjectsPopout();
          },
        },
        el(
          "span",
          { class: "filter-dropdown-left" },
          el("span", { class: "proj-badge", style: projectBadgeStyle(filtered.id) }, projectMonogram(filtered.name)),
          el("span", { class: "filter-chip-name", title: filtered.name }, filtered.name),
        ),
        el("span", { class: "muted" }, "▾"),
      ),
      el("button", {
        type: "button",
        class: "icon",
        title: "Show chats from all projects",
        "aria-label": "Clear project filter",
        onclick: () => {
          filterProjectId = null;
          drawChatList();
        },
      }, "×"),
    );
  } else {
    slot.replaceChildren(
      el(
        "button",
        {
          type: "button",
          class: "filter-dropdown-btn",
          title: "Filter chats by project (⌘⇧P)",
          onclick: (event) => {
            event.stopPropagation();
            if (projectsPopoutOpen) closeProjectsPopout();
            else openProjectsPopout();
          },
        },
        el("span", { class: "filter-chip-name" }, "All projects"),
        el("span", { class: "muted" }, "▾"),
      ),
    );
  }
}

function renderProjectsPopout() {
  const pop = $("projects-popout");
  const listEl = $("projects-popout-list");
  if (!pop || !listEl) return;
  pop.hidden = !projectsPopoutOpen;
  if (!projectsPopoutOpen) return;
  const matched = orderedProjects($("projects-popout-search")?.value || "");
  listEl.replaceChildren(
    el(
      "button",
      {
        type: "button",
        class: `popout-row${projectsPopoutSelected === 0 ? " selected" : ""}`,
        onclick: () => {
          closeProjectsPopout();
          newProject();
        },
      },
      el("span", { class: "popout-row-left" }, el("span", { class: "proj-badge" }, "+"), el("span", { class: "popout-row-name" }, "Add project")),
    ),
    el(
      "button",
      {
        type: "button",
        class: `popout-row${projectsPopoutSelected === 1 ? " selected" : ""}`,
        onclick: () => {
          filterProjectId = null;
          closeProjectsPopout();
          drawChatList();
        },
      },
      el("span", { class: "popout-row-left" }, el("span", { class: "proj-badge" }, "▤"), el("span", { class: "popout-row-name" }, "All projects")),
      !filterProjectId ? el("span", { class: "popout-check" }, "✓") : null,
    ),
    ...matched.map((p, i) =>
      el(
        "button",
        {
          type: "button",
          class: `popout-row${projectsPopoutSelected === i + 2 ? " selected" : ""}`,
          onclick: () => {
            filterProjectId = p.id;
            closeProjectsPopout();
            drawChatList();
          },
        },
        el(
          "span",
          { class: "popout-row-left" },
          el("span", { class: "proj-badge", style: projectBadgeStyle(p.id) }, projectMonogram(p.name)),
          el("span", { class: "popout-row-name" }, p.name),
        ),
        filterProjectId === p.id ? el("span", { class: "popout-check" }, "✓") : null,
      ),
    ),
  );
}

function openProjectsPopout() {
  closeNewChatModal();
  projectsPopoutOpen = true;
  projectsPopoutSelected = 1;
  if ($("projects-popout-search")) $("projects-popout-search").value = "";
  renderProjectsPopout();
  $("projects-popout-search")?.focus();
}

function closeProjectsPopout() {
  projectsPopoutOpen = false;
  renderProjectsPopout();
}

function renderNewChatModal() {
  const modal = $("new-chat-modal");
  const listEl = $("new-chat-list");
  if (!modal || !listEl) return;
  modal.hidden = !newChatModalOpen;
  if (!newChatModalOpen) return;
  const matched = orderedProjects($("new-chat-search")?.value || "");
  listEl.replaceChildren(
    ...matched.map((p, i) => {
      const isCurrent = project?.id === p.id;
      const env = p.isolation === "linux-vm" ? "Linux VM" : "macOS sandbox";
      const sub = `${isCurrent ? "Current · " : ""}${env} · ${p.folders?.[0]?.path || ""}`;
      return el(
        "button",
        {
          type: "button",
          class: `modal-proj-row${newChatSelected === i ? " selected" : ""}`,
          onclick: () => startNewChatInProject(p.id),
        },
        el(
          "span",
          { class: "modal-proj-left" },
          el("span", { class: "proj-badge", style: projectBadgeStyle(p.id) }, projectMonogram(p.name)),
          el(
            "span",
            { class: "modal-proj-text" },
            el("span", { class: "modal-proj-title" }, p.name),
            el("span", { class: "modal-proj-sub" }, sub),
          ),
        ),
        i < 9 ? el("span", { class: "modal-proj-key" }, `⌘${i + 1}`) : null,
      );
    }),
    el(
      "button",
      {
        type: "button",
        class: `modal-proj-row${newChatSelected === matched.length ? " selected" : ""}`,
        onclick: () => {
          closeNewChatModal();
          newProject();
        },
      },
      el(
        "span",
        { class: "modal-proj-left" },
        el("span", { class: "proj-badge" }, "+"),
        el("span", { class: "modal-proj-title" }, "Add new project…"),
      ),
      el("span", { class: "modal-proj-key" }, "⌘⇧O"),
    ),
  );
}

function openNewChatModal() {
  if (!projects.length) {
    newProject();
    return;
  }
  closeProjectsPopout();
  newChatModalOpen = true;
  newChatSelected = 0;
  if ($("new-chat-search")) $("new-chat-search").value = "";
  renderNewChatModal();
  $("new-chat-search")?.focus();
}

function closeNewChatModal() {
  newChatModalOpen = false;
  renderNewChatModal();
}

async function startNewChatInProject(projectId) {
  closeNewChatModal();
  if (project?.id !== projectId) {
    await openProject(projectId);
  } else {
    await showChat(null);
  }
  $("input")?.focus();
}

async function openSession(projectId, chatId) {
  doneSessions.delete(chatId);
  if (project?.id !== projectId) {
    const targetProj = projects.find((p) => p.id === projectId);
    if (!targetProj) return;
    project = targetProj;
    $("project").value = project.id;
    updateHeaderProject();
    gitPanel = null;
    gitNotice = null;
    gitState = null;
    renderGit();
    refreshGit(true);
  }
  await showChat(chatId);
}

function drawChatList() {
  const box = $("chat-list");
  const query = ($("chat-search")?.value || "").trim().toLowerCase();
  renderFilterChip();

  const filtered = allSessions.filter((chat) => {
    if (filterProjectId && chat.projectId !== filterProjectId) return false;
    if (!query) return true;
    return (
      chat.title.toLowerCase().includes(query) ||
      chat.projectName.toLowerCase().includes(query)
    );
  });

  const item = (chat) => {
    const isActive = chat.id === session && chat.projectId === project?.id;
    const isArch = archived.has(archiveKey(chat.projectId, chat.id));
    const needsApproval = approvalPendingSessions.has(chat.id);
    const workingSince = workingSinceBySession.get(chat.id);
    const isDone = doneSessions.has(chat.id);
    const rel = formatRelTime(chat.updated);
    const envLabel = chat.isolation === "linux-vm" ? "Linux VM" : "macOS sandbox";
    const statusBadge = needsApproval
      ? el("span", { class: "chat-card-approval" }, "⚠ Approval")
      : workingSince
        ? el("span", { class: "chat-card-working" }, el("span", { class: "dot" }), `Working ${formatWorkingCompact(workingSince)}`)
        : isDone
          ? el("span", { class: "chat-card-done" }, "✓ Done")
          : rel
            ? el("span", { class: "chat-card-time" }, rel)
            : null;
    return el(
      "div",
      { class: `chat-card${isActive ? " active" : ""}` },
      el(
        "button",
        {
          type: "button",
          class: "chat-card-open chat-item-title",
          "aria-current": isActive ? "true" : null,
          onclick: () => openSession(chat.projectId, chat.id),
        },
        el(
          "div",
          { class: "chat-card-meta" },
          el(
            "span",
            { class: "chat-card-proj" },
            el("span", { class: "proj-badge", style: projectBadgeStyle(chat.projectId) }, projectMonogram(chat.projectName)),
            el("span", { class: "chat-card-proj-name" }, chat.projectName),
          ),
          statusBadge,
        ),
        el("div", { class: "chat-card-title" }, chat.title),
      ),
      el(
        "div",
        { class: "chat-card-foot" },
        el("span", { class: "chat-card-env" }, envLabel),
        el(
          "span",
          { class: "chat-card-actions" },
          isArch
            ? el("button", { type: "button", class: "chat-item-action", title: "Restore chat to active list", "aria-label": `Restore "${chat.title}"`, onclick: () => archiveChat(chat.id, false, chat.projectId) }, "↩")
            : el("button", { type: "button", class: "chat-item-action", title: "Archive chat", "aria-label": `Archive "${chat.title}"`, onclick: () => archiveChat(chat.id, true, chat.projectId) }, "⤓"),
          el("button", { type: "button", class: "chat-item-action", title: "Delete chat", "aria-label": `Delete "${chat.title}"`, onclick: () => deleteChat(chat) }, "×"),
        ),
      ),
    );
  };

  const activeItems = filtered.filter((c) => !archived.has(archiveKey(c.projectId, c.id)));
  const kept = filtered.filter((c) => archived.has(archiveKey(c.projectId, c.id)));

  const draftCard =
    !session && !query && project
      ? el(
          "div",
          { class: "chat-card draft" },
          el(
            "div",
            { class: "chat-card-meta" },
            el(
              "span",
              { class: "chat-card-proj" },
              el("span", { class: "proj-badge", style: projectBadgeStyle(project.id) }, projectMonogram(project.name)),
              el("span", { class: "chat-card-proj-name" }, project.name),
            ),
            el("span", { class: "chat-card-working" }, "Draft"),
          ),
          el("div", { class: "chat-card-title" }, "New chat"),
          el("div", { class: "chat-card-foot" }, el("span", { class: "chat-card-env" }, project.isolation === "linux-vm" ? "Linux VM" : "macOS sandbox")),
        )
      : null;

  const more = el("details", { class: "archived-chats" }, el("summary", { class: "small muted" }, `Archived (${kept.length})`), ...kept.map(item));
  more.open = showArchived || kept.some((c) => c.id === session);
  more.addEventListener("toggle", () => (showArchived = more.open));
  box.replaceChildren(
    ...(draftCard ? [draftCard] : []),
    ...activeItems.map(item),
    ...(activeItems.length === 0 && query ? [el("p", { class: "small muted" }, "No matching chats.")] : []),
    ...(kept.length ? [more] : []),
  );
}

async function renderChatList() {
  const mine = ++listGen;
  const box = $("chat-list");
  if (!projects.length) {
    allSessions = [];
    box.replaceChildren();
    return;
  }
  if (project && box.dataset.project !== project.id && project.isolation === "linux-vm" && allSessions.length === 0) {
    box.replaceChildren(el("p", { class: "small muted" }, "Starting the Linux VM…"));
  }
  let collected = [];
  let firstError = null;
  try {
    if (engine === "opencode") {
      const perProj = await invoke("acp_all_sessions", { activeProject: project?.id || null });
      if (mine !== listGen) return;
      for (const entry of perProj || []) {
        const projMeta = projects.find((p) => p.id === entry.project);
        if (!projMeta) continue;
        if (entry.error && (entry.project === project?.id || !firstError)) firstError = entry.error;
        for (const s of entry.sessions || []) {
          if (!s.sessionId) continue;
          collected.push({
            id: s.sessionId,
            projectId: projMeta.id,
            projectName: projMeta.name,
            isolation: projMeta.isolation,
            title: s.title?.startsWith("New session - ") ? "New chat" : s.title || "Chat",
            updated: parseSessionTime(s),
          });
        }
      }
    } else {
      const lists = await Promise.all(
        projects.map(async (projMeta) => {
          const rows = await invoke("chat_list", { project: projMeta.id }).catch(() => []);
          return rows.map((c) => ({
            id: c.id,
            projectId: projMeta.id,
            projectName: projMeta.name,
            isolation: projMeta.isolation,
            title: c.title,
            updated: c.updated || 0,
          }));
        }),
      );
      if (mine !== listGen) return;
      collected = lists.flat();
    }
  } catch (e) {
    if (mine !== listGen) return;
    box.replaceChildren(el("p", { class: "small muted" }, String(e)));
    return;
  }

  const archivedEntries = await Promise.all(
    projects.map(async (p) => {
      const ids = await invoke("archived_chats", { project: p.id }).catch(() => []);
      return ids.map((id) => archiveKey(p.id, id));
    }),
  );
  if (mine !== listGen) return;
  archived = new Set(archivedEntries.flat());
  collected.sort((a, b) => {
    const ab = workingSinceBySession.has(a.id) ? 1 : 0;
    const bb = workingSinceBySession.has(b.id) ? 1 : 0;
    if (ab !== bb) return bb - ab;
    return (b.updated || 0) - (a.updated || 0);
  });
  allSessions = collected;
  box.dataset.project = project?.id || "";
  chatTitles = new Map(allSessions.map((c) => [c.id, c.title]));
  if (session && chatTitles.has(session)) $("title").textContent = chatTitles.get(session);
  if (allSessions.length === 0 && firstError) {
    box.replaceChildren(el("p", { class: "small muted" }, firstError));
  } else {
    drawChatList();
  }
  update();
}

// Archived chats are kept, out of the list; sending in one puts it back.
async function archiveChat(id, archive, projectId = project?.id) {
  if (!projectId) return;
  try {
    await invoke("set_chat_archived", { project: projectId, chat: id, archived: archive });
  } catch (e) {
    alert(e);
    return;
  }
  if (archive) archived.add(archiveKey(projectId, id));
  else archived.delete(archiveKey(projectId, id));
  if (archive && id === session && projectId === project?.id) showChat(null);
  else drawChatList();
}

async function deleteChat(chat) {
  const projId = chat.projectId || project?.id;
  if (!projId) return;
  if (!confirm(`Delete “${chat.title}”?`)) return;
  try {
    await engines[engine].remove(chat.id, projId);
  } catch (e) {
    alert(e);
  }
  if (chat.id === session && projId === project?.id) showChat(null);
  else renderChatList();
}

// --- Projects and the model ---

async function loadProjects() {
  projects = (await invoke("projects_info")).projects;
  const select = $("project");
  select.replaceChildren(...projects.map((p) => el("option", { value: p.id }, p.name)));
  if (!projects.length) select.append(el("option", { value: "" }, "No projects yet"));
}

// Shows `id` (or the current, or most recent project) with a new chat. The app calls it too.
async function openProject(id) {
  const mine = ++projectGen;
  await loadProjects();
  if (mine !== projectGen) return;
  project = projects.find((p) => p.id === id) || projects.find((p) => p.id === project?.id) || projects[0] || null;
  $("project").value = project?.id || "";
  updateHeaderProject();
  gitPanel = null;
  gitNotice = null;
  gitState = null;
  renderGit();
  await showChat(null);
  refreshGit(true);
}
window.openProject = openProject;

async function refreshStatus() {
  const overview = await invoke("overview");
  status = overview.status;
  loadedModels = overview.models;
  renderModelPicker();
  await refreshRoom();
  update();
}

// --- The chat's model ---
// Each chat keeps the model it was started with: another model would read the whole chat
// again. Several models can be loaded at once, memory permitting; sending in a chat loads
// its model if needed, unloading the ones used longest ago when there isn't room.

let models = []; // installed models that fit: {id, name}
let loadedModels = []; // loaded, loading or failed, most recently used first
let chatModel = null;
let room = null; // what loading the chat's model takes, when it isn't loaded

const isLoaded = (id) => loadedModels.some((m) => m.id === id && m.state === "running");
const isLoading = (id) => loadedModels.some((m) => m.id === id && m.state === "loading");

async function refreshRoom() {
  room = chatModel && !isLoaded(chatModel) && !isLoading(chatModel) ? await invoke("room_for", { id: chatModel }).catch(() => null) : null;
}
const modelName = (id) => models.find((m) => m.id === id)?.name || "the chat's model";

async function loadModels() {
  const installed = await invoke("installed_models");
  models = installed.filter((m) => m.fit).map((m) => ({ id: m.id, name: m.model.name }));
  // No model picked yet (none were installed when the chat opened), or the picked one is gone.
  if (!models.some((m) => m.id === chatModel)) {
    chatModel = defaultModel();
    await refreshRoom();
    update();
  }
  renderModelPicker();
}

function defaultModel() {
  let last = null;
  try {
    last = localStorage.getItem("chatModel");
  } catch {}
  const latest = loadedModels.find((m) => m.state === "running" || m.state === "loading")?.id;
  return [latest, last, models[0]?.id].find((id) => id && models.some((m) => m.id === id)) || null;
}

function renderModelPicker() {
  const select = $("chat-model");
  select.replaceChildren(...models.map((m) => el("option", { value: m.id }, isLoaded(m.id) ? `${m.name} ●` : m.name)));
  if (!models.length) select.append(el("option", { value: "" }, "No models yet"));
  select.value = chatModel || "";
  select.disabled = busy || !models.length;
  $("unload").hidden = !isLoaded(chatModel);
  $("unload").title = `Unload ${modelName(chatModel)}, freeing its memory`;
  const running = loadedModels.find((m) => m.id === chatModel && m.state === "running");
  const spec = $("spec-badge");
  if (spec) {
    spec.textContent = running?.speculative ? `⚡ ${running.speculative}` : "";
    spec.hidden = !running?.speculative;
  }
}

$("unload").addEventListener("click", async () => {
  if (busy) {
    if (!confirm("The agent is working on this chat. Unloading its model stops it.")) return;
    await engines[engine].stop().catch(() => {});
  }
  await invoke("unload_model", { id: chatModel });
  await refreshStatus();
});

$("chat-model").addEventListener("change", async () => {
  const next = $("chat-model").value;
  const started = list().querySelector(".msg");
  if (session && started && next !== chatModel && !confirm(`Use ${modelName(next)} for this chat? Its next reply reads the whole chat again, which takes a while for a long chat.`)) {
    renderModelPicker();
    return;
  }
  chatModel = next;
  try {
    localStorage.setItem("chatModel", next);
  } catch {}
  if (session) invoke("set_chat_model", { project: project.id, chat: session, model: next }).catch(() => {});
  renderModelPicker();
  await refreshRoom();
  update();
});
window.addEventListener("focus", loadModels);

// A line under the messages while the agent works, so a quiet model doesn't look stuck.
let workingSince = 0;
let workingTimer = null;
function renderWorking() {
  const line = $("working");
  if (!busy) {
    line.hidden = true;
    clearInterval(workingTimer);
    workingTimer = null;
    workingSince = 0;
    return;
  }
  workingSince ||= Date.now();
  workingTimer ||= setInterval(renderWorking, 1000);
  const waiting = [...view.tools.values()].some((card) => card.approval);
  const seconds = Math.round((Date.now() - workingSince) / 1000);
  const elapsed = seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  line.classList.toggle("waiting", waiting);
  $("working-text").textContent = waiting
    ? "Waiting for you: allow or deny the request above."
    : isLoading(chatModel)
      ? `Loading ${modelName(chatModel)}… the first time can take a minute.`
      : `Working… ${elapsed}`;
  line.hidden = false;
}

// What can be done now, and what's in the way.
// Of the context, from where a new chat is suggested: well before OpenCode summarizes it, which
// it does when what's left won't hold a reply (we give replies a quarter of the context).
const LONG_CHAT = 0.6;

function update() {
  const notice = $("notice");
  const reasons = [];
  if (!project) reasons.push(["Make a project first: a folder for the agent to work in.", "New project…", newProject]);
  if (!models.length) reasons.push(["Download a model to chat.", "Models…", () => invoke("open_window", { name: "settings" })]);
  // Not in the way: sending starts the chat's model.
  const notes = [];
  if (isLoading(chatModel) && !busy) notes.push(`Loading ${modelName(chatModel)}…`);
  else if (chatModel && !isLoaded(chatModel) && !busy) {
    const unload = (room?.unload || []).map((id) => loadedModels.find((m) => m.id === id)?.name || id);
    notes.push(unload.length ? `Sending starts ${modelName(chatModel)}, unloading ${unload.join(" and ")} to make room.` : `Sending starts ${modelName(chatModel)}, which can take up to a minute.`);
  }
  notice.replaceChildren(
    ...reasons.map(([text, label, action]) => el("div", { class: "row" }, el("span", { class: "spacer" }, text), el("button", { onclick: action }, label))),
    ...notes.map((text) => el("div", { class: "row info" }, text)),
  );
  // Long chats: a new one for a new task beats OpenCode summarizing this one when it's full.
  const share = usage && usage.size ? usage.used / usage.size : 0;
  const long = session && !busy && share >= LONG_CHAT ? [`This chat is ${Math.round(share * 100)}% full. Long chats get slower and less reliable, and a full one is shortened to a summary. If this task is done, start a new chat.`] : [];
  notice.hidden = reasons.length + notes.length + long.length === 0;
  notice.append(...long.map((text) => el("div", { class: "row info" }, el("span", { class: "spacer" }, text), el("button", { onclick: () => archiveChat(session, true) }, "Archive and start a new chat"))));
  $("archive").hidden = !session || !project || archived.has(archiveKey(project.id, session));
  renderModelPicker();
  $("send").disabled = busy || reasons.length > 0 || !chatModel;
  $("send").hidden = busy;
  $("stop").hidden = !busy;
  renderWorking();
}

async function newProject() {
  const folder = await invoke("choose_folder", { title: "Choose a folder for the agent to work in" });
  if (!folder) return;
  try {
    const made = await invoke("new_project", { folder });
    await openProject(made.id);
  } catch (e) {
    alert(e);
  }
}

// --- Events ---

function finished(error, stopReason) {
  busy = false;
  view.texts.delete("current");
  view.texts.delete("live-text");
  view.thoughts.delete("current");
  view.thoughts.delete("live-thought");
  if (stopReason === "cancelled") list().append(el("div", { class: "small muted stopped" }, "Stopped."));
  if (error) list().append(el("div", { class: "banner bad" }, error));
  renderChatList();
  refreshGit(false);
  scrollDown();
  update();
}

function onOpencode(event) {
  if (event.type === "starting") {
    if (event.project === project?.id) $("chat-list").replaceChildren(el("p", { class: "small muted" }, event.text));
    return;
  }
  if (event.type === "permission") {
    approvalPendingSessions.add(event.session);
    drawChatList();
  }
  if (event.type === "state") {
    if (event.busy) {
      doneSessions.delete(event.session);
      if (!workingSinceBySession.has(event.session)) workingSinceBySession.set(event.session, Date.now());
      drawChatList();
    } else {
      workingSinceBySession.delete(event.session);
      approvalPendingSessions.delete(event.session);
      if (!event.error && event.stop_reason !== "cancelled") doneSessions.add(event.session);
      if (event.project !== project?.id || event.session !== session) renderChatList();
    }
  }
  if (event.project !== project?.id || event.session !== session) {
    return;
  }
  if (event.type === "update") applyUpdate(event.update);
  else if (event.type === "permission") askPermission(event.request, event.tool_call, event.options);
  else if (event.type === "state") {
    if (event.busy) {
      busy = true;
      update();
    } else finished(event.error, event.stop_reason);
  }
}

function onBuiltin(event) {
  if (event.type === "approval") {
    approvalPendingSessions.add(event.chat);
    drawChatList();
  }
  if (event.type === "state") {
    if (event.busy) {
      doneSessions.delete(event.chat);
      if (!workingSinceBySession.has(event.chat)) workingSinceBySession.set(event.chat, Date.now());
      drawChatList();
    } else {
      workingSinceBySession.delete(event.chat);
      approvalPendingSessions.delete(event.chat);
      if (!event.error) doneSessions.add(event.chat);
      if (event.chat !== session) renderChatList();
    }
  }
  if (event.chat !== session) {
    return;
  }
  if (event.type === "message") builtin.message(event.message, null, false);
  else if (event.type === "delta") {
    if (event.reasoning) agentThought("live-thought", event.reasoning, true);
    agentText("live-text", event.content, true);
  } else if (event.type === "approval") {
    askPermission(event.call, { toolCallId: event.call }, [
      { optionId: "allow", kind: "allow_once", name: "Allow" },
      { optionId: "deny", kind: "reject_once", name: "Deny" },
    ]);
  } else if (event.type === "usage") {
    if (event.size > 0) {
      usage = { used: event.used, size: event.size };
      $("usage").textContent = `Chat ${Math.round((event.used / event.size) * 100)}% full`;
      $("usage").title = `${Math.round(event.used / 1000)}k of ${Math.round(event.size / 1000)}k tokens of the model's context`;
    }
  } else if (event.type === "notice") {
    list().append(el("p", { class: "small muted" }, event.text));
    scrollDown();
  } else if (event.type === "state") {
    if (event.busy) {
      busy = true;
      update();
    } else finished(event.error, null);
  }
}

// Events for a chat that's still getting its id wait until it has one.
function deliver(handler) {
  return ({ payload }) => {
    if (awaiting) pending.push(() => handler(payload));
    else handler(payload);
  };
}
listen("acp", deliver(onOpencode));
listen("chat", deliver(onBuiltin));
listen("subagent", deliver(onSubagent));
listen("status", refreshStatus);
listen("discovery", loadModels);
listen("projects", async () => {
  await loadProjects();
  const found = project ? projects.find((p) => p.id === project.id) : null;
  if (found) {
    project = found;
    $("project").value = project.id;
    updateHeaderProject();
    renderChatList();
    update();
  } else if (project || projects.length) {
    await openProject(projects[0]?.id || "");
  } else {
    $("project").value = "";
    updateHeaderProject();
    update();
  }
});

async function send(text, images = []) {
  const proj = project;
  const startSession = session;
  const modelId = chatModel;
  const startGen = chatGen;
  if (!proj || !modelId) return;

  list().querySelector(".empty")?.remove();
  const userNode = appendUser(null, text, images);
  busy = true;
  let resolveAbort;
  const aborted = new Promise((resolve) => (resolveAbort = resolve));
  const token = {
    cancelled: false,
    cancel() {
      if (this.cancelled) return;
      this.cancelled = true;
      resolveAbort(SEND_CANCELLED);
    },
  };
  sending = token;
  const raceAbort = (p) => {
    p.catch(() => {});
    return Promise.race([p, aborted]);
  };
  const checkActive = () => {
    if (startGen !== chatGen) return "switched";
    if (token.cancelled) return "cancelled";
    return null;
  };
  update();
  try {
    // Already loading (setup starts it with the chat): wait for it rather than load it twice,
    // and if it fails, say so instead of trying again.
    let waited = false;
    while (isLoading(modelId)) {
      waited = true;
      const r = await raceAbort(new Promise((resolve) => setTimeout(resolve, 500)));
      if (r === SEND_CANCELLED || checkActive()) break;
      await raceAbort(refreshStatus());
      if (checkActive()) break;
    }
    const afterWait = checkActive();
    if (afterWait === "switched") return;
    if (afterWait === "cancelled") throw SEND_CANCELLED;
    if (waited && !isLoaded(modelId)) {
      const failed = loadedModels.find((m) => m.id === modelId && m.state === "failed");
      throw `Not sent: ${modelName(modelId)} couldn't start${failed?.error ? `: ${failed.error}` : "."}`;
    }
    // The chat's model answers; load it if it isn't, making room if needed.
    if (!isLoaded(modelId)) {
      const needed = await raceAbort(invoke("room_for", { id: modelId }));
      const afterRoom = checkActive();
      if (afterRoom === "switched") return;
      if (afterRoom === "cancelled" || needed === SEND_CANCELLED) throw SEND_CANCELLED;
      if (!needed.fits) {
        const names = needed.unload.map((id) => loadedModels.find((m) => m.id === id)?.name || id);
        if (!confirm(`Loading ${modelName(modelId)} needs the memory ${names.join(" and ")} ${names.length === 1 ? "uses" : "use"}. Unload ${names.length === 1 ? "it" : "them"}?`)) throw "Not sent: the chat's model isn't loaded.";
        const afterConfirm = checkActive();
        if (afterConfirm === "switched") return;
        if (afterConfirm === "cancelled") throw SEND_CANCELLED;
        for (const id of needed.unload) {
          await raceAbort(invoke("unload_model", { id }));
          const afterUnload = checkActive();
          if (afterUnload === "switched") return;
          if (afterUnload === "cancelled") throw SEND_CANCELLED;
        }
      }
      const loadRes = await raceAbort(invoke("load_model", { id: modelId }));
      const afterLoad = checkActive();
      if (afterLoad === "switched") return;
      if (afterLoad === "cancelled" || loadRes === SEND_CANCELLED) throw SEND_CANCELLED;
      await raceAbort(refreshStatus());
      const afterStatus = checkActive();
      if (afterStatus === "switched") return;
      if (afterStatus === "cancelled") throw SEND_CANCELLED;
    }
    if (startSession && archived.has(archiveKey(proj.id, startSession))) {
      await invoke("set_chat_archived", { project: proj.id, chat: startSession, archived: false })
        .then(() => {
          if (startGen === chatGen) archived.delete(archiveKey(proj.id, startSession));
        })
        .catch(() => {});
      const afterUnarchive = checkActive();
      if (afterUnarchive === "switched") return;
      if (afterUnarchive === "cancelled") throw SEND_CANCELLED;
    }
    const wireImages = images.map(({ name, mime_type, data_base64, size_bytes }) => ({
      name,
      mime_type,
      data_base64,
      size_bytes: size_bytes || 0,
    }));
    const targetSession = await engines[engine].send(text, {
      projectId: proj.id,
      sessionId: startSession,
      model: modelId,
      images: wireImages,
      onSession: (id) => {
        if (startGen === chatGen) session = id;
      },
    });
    if (token.cancelled && targetSession) {
      engines[engine].stop(proj.id, targetSession).catch(() => {});
    }
    if (targetSession) invoke("set_chat_model", { project: proj.id, chat: targetSession, model: modelId }).catch(() => {});
  } catch (e) {
    if (startGen !== chatGen) return;
    // Not sent: remove the unsent bubble and give the message back, so it isn't lost or duplicated.
    userNode?.remove();
    if (!$("input").value && text) $("input").value = text;
    if (!pendingImages.length && images.length) {
      pendingImages = images;
      renderPendingImages();
    }
    if (e === SEND_CANCELLED) {
      if (!list().querySelector(".msg") && !session) list().append(emptyState());
      finished(null, "cancelled");
    } else {
      finished(String(e), null);
    }
  } finally {
    if (sending === token) sending = null;
    for (const run of pending.splice(0)) run();
  }
}

$("composer").addEventListener("submit", (event) => {
  event.preventDefault();
  const text = $("input").value.trim();
  if ((!text && !pendingImages.length) || $("send").disabled) return;
  const images = pendingImages.slice();
  $("input").value = "";
  pendingImages = [];
  attachNotice = "";
  renderPendingImages();
  send(text, images);
});
$("attach-image")?.addEventListener("click", () => $("image-input")?.click());
$("image-input")?.addEventListener("change", async (event) => {
  const input = event.target;
  if (input.files?.length) await attachImageFiles(input.files);
  input.value = "";
});
document.addEventListener("paste", (event) => {
  const items = [...(event.clipboardData?.items || [])];
  const imageFiles = items
    .filter((item) => item.kind === "file" && item.type?.startsWith("image/"))
    .map((item) => item.getAsFile())
    .filter(Boolean);
  if (imageFiles.length) {
    event.preventDefault();
    attachImageFiles(imageFiles);
  }
});
{
  const chatPane = document.querySelector(".chat");
  const overlay = $("drop-overlay");
  let dragDepth = 0;
  const hasFiles = (dt) => [...(dt?.types || [])].includes("Files");
  chatPane?.addEventListener("dragenter", (event) => {
    if (!hasFiles(event.dataTransfer)) return;
    event.preventDefault();
    dragDepth++;
    chatPane.classList.add("drag-over");
    if (overlay) overlay.hidden = false;
  });
  chatPane?.addEventListener("dragover", (event) => {
    if (!hasFiles(event.dataTransfer)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
  });
  chatPane?.addEventListener("dragleave", () => {
    if (dragDepth > 0) dragDepth--;
    if (dragDepth === 0) {
      chatPane.classList.remove("drag-over");
      if (overlay) overlay.hidden = true;
    }
  });
  chatPane?.addEventListener("drop", (event) => {
    event.preventDefault();
    dragDepth = 0;
    chatPane.classList.remove("drag-over");
    if (overlay) overlay.hidden = true;
    if (event.dataTransfer?.files?.length) attachImageFiles(event.dataTransfer.files);
  });
}
$("input").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    $("composer").requestSubmit();
  }
});
$("stop").addEventListener("click", () => {
  if (sending) sending.cancel();
  if (session) engines[engine].stop(project?.id, session).catch(() => {});
});
$("new-chat").addEventListener("click", () => openNewChatModal());
$("projects-popout-btn")?.addEventListener("click", (event) => {
  event.stopPropagation();
  if (projectsPopoutOpen) closeProjectsPopout();
  else openProjectsPopout();
});
$("projects-popout-search")?.addEventListener("input", () => {
  projectsPopoutSelected = 0;
  renderProjectsPopout();
});
$("new-chat-search")?.addEventListener("input", () => {
  newChatSelected = 0;
  renderNewChatModal();
});
$("new-chat-modal")?.addEventListener("click", (event) => {
  if (event.target === $("new-chat-modal")) closeNewChatModal();
});
document.addEventListener("click", (event) => {
  if (projectsPopoutOpen && !event.target.closest("#projects-popout") && !event.target.closest("#project-filter-slot")) {
    closeProjectsPopout();
  }
});
$("add-project").addEventListener("click", newProject);
$("chat-search").addEventListener("input", () => drawChatList());
$("chat-search").addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    const first = $("chat-list").querySelector(".chat-card-open");
    if (first) first.click();
  } else if (event.key === "ArrowDown") {
    event.preventDefault();
    $("chat-list").querySelector(".chat-card-open")?.focus();
  } else if (event.key === "Escape") {
    event.preventDefault();
    $("chat-search").value = "";
    drawChatList();
    $("input").focus();
  }
});
$("project-scope").addEventListener("click", () => {
  filterCurrentProject = !filterCurrentProject;
  drawChatList();
});
$("archive").addEventListener("click", () => session && project && archiveChat(session, true, project.id));
$("project").addEventListener("change", () => openProject($("project").value));
$("manage").addEventListener("click", () => invoke("open_window", { name: "settings" }));
$("project-settings-btn")?.addEventListener("click", () => invoke("open_window", { name: "settings" }));
setInterval(() => {
  if (workingSinceBySession.size > 0) drawChatList();
}, 15000);

// --- Host-side Git & GitHub PR bar ---

let gitState = null;
let gitGen = 0;
let gitPanel = null; // null | "commit" | "pr"
let gitBusy = null;
let gitNotice = null; // null | { text, url?, error? }

async function refreshGit(checkPr = false) {
  const proj = project;
  if (!proj) {
    gitState = null;
    renderGit();
    return;
  }
  const mine = ++gitGen;
  try {
    const st = await invoke("git_status", { project: proj.id, checkPr: false });
    if (mine !== gitGen || project?.id !== proj.id) return;
    if (!checkPr && gitState && gitState.branch === st.branch) st.open_pr = gitState.open_pr;
    gitState = st.is_repo ? st : null;
    renderGit();
    if (checkPr && st.is_repo && st.has_remote && st.gh_available && !st.detached) {
      const full = await invoke("git_status", { project: proj.id, checkPr: true });
      if (mine !== gitGen || project?.id !== proj.id) return;
      gitState = full.is_repo ? full : null;
      renderGit();
    }
  } catch {
    if (mine === gitGen) {
      gitState = null;
      renderGit();
    }
  }
}

function renderGit() {
  const bar = $("git-bar");
  const panel = $("git-panel");
  const notice = $("git-notice");
  if (!gitState?.is_repo) {
    bar.hidden = true;
    panel.hidden = true;
    notice.hidden = true;
    return;
  }
  bar.hidden = false;
  const st = gitState;
  const dirty = st.dirty_files.length;
  $("git-branch").textContent = `⎇ ${st.detached ? "detached HEAD" : st.branch || "HEAD"}`;
  const parts = [];
  if (dirty > 0) parts.push(`${dirty} changed`);
  else if (st.ahead === 0 && st.behind === 0) parts.push("Clean");
  if (st.ahead > 0) parts.push(`↑${st.ahead}`);
  if (st.behind > 0) parts.push(`↓${st.behind}`);
  $("git-summary").textContent = parts.join(" · ");
  $("git-busy").textContent = gitBusy || "";
  $("git-busy").hidden = !gitBusy;

  const commitBtn = $("git-commit-btn");
  commitBtn.textContent = dirty > 0 ? `Commit (${dirty})` : "Commit";
  commitBtn.classList.toggle("primary", dirty > 0);
  commitBtn.disabled = dirty === 0 || !!gitBusy;

  const pushBtn = $("git-push-btn");
  pushBtn.textContent = st.ahead > 0 ? `Push ↑${st.ahead}` : !st.upstream && st.has_remote ? "Push branch" : "Push";
  pushBtn.classList.toggle("primary", dirty === 0 && (st.ahead > 0 || !st.upstream) && st.has_remote && !st.detached);
  pushBtn.disabled = !st.has_remote || st.detached || !!gitBusy;

  const prBtn = $("git-pr-btn");
  if (st.open_pr) {
    prBtn.textContent = st.open_pr.number ? `PR #${st.open_pr.number} ↗` : "Open PR ↗";
    prBtn.disabled = false;
  } else {
    prBtn.textContent = "PR";
    prBtn.disabled = !st.has_remote || st.detached || !!gitBusy;
  }

  if (gitNotice) {
    notice.hidden = false;
    notice.classList.toggle("bad", !!gitNotice.error);
    notice.replaceChildren(
      el("span", {}, gitNotice.text),
      el(
        "span",
        { class: "git-bar-actions" },
        gitNotice.url ? el("button", { type: "button", class: "small primary", onclick: () => invoke("open_external_url", { url: gitNotice.url }) }, "Open PR ↗") : null,
        el("button", { type: "button", class: "small", onclick: () => { gitNotice = null; renderGit(); } }, "Dismiss"),
      ),
    );
  } else {
    notice.hidden = true;
  }

  if (!gitPanel) {
    panel.hidden = true;
    panel.replaceChildren();
  }
}

function openCommitPanel() {
  if (gitPanel === "commit") {
    gitPanel = null;
    renderGit();
    return;
  }
  gitPanel = "commit";
  gitNotice = null;
  const st = gitState;
  const panel = $("git-panel");
  const input = el("input", { type: "text", placeholder: "Commit message (e.g. Fix login redirect)" });
  const draftBtn = el("button", {
    type: "button",
    class: "small",
    onclick: async () => {
      draftBtn.disabled = true;
      draftBtn.textContent = "Drafting…";
      try {
        input.value = await invoke("git_generate_commit_message", { project: project.id, model: chatModel });
      } catch (e) {
        gitNotice = { text: String(e), error: true };
        renderGit();
      } finally {
        draftBtn.disabled = false;
        draftBtn.textContent = "Draft message";
      }
    },
  }, "Draft message");
  const runCommit = async (alsoPush) => {
    const message = input.value.trim();
    if (!message) {
      gitNotice = { text: "Enter a commit message first.", error: true };
      renderGit();
      return;
    }
    gitBusy = alsoPush ? "Committing & pushing…" : "Committing…";
    gitNotice = null;
    renderGit();
    try {
      const c = await invoke("git_commit", { project: project.id, message, stageAll: true });
      if (alsoPush) {
        const p = await invoke("git_push", { project: project.id });
        gitNotice = { text: `Committed ${c.hash} and pushed to ${p.remote}/${p.branch}` };
      } else {
        gitNotice = { text: `Committed ${c.hash} — ${c.summary}` };
      }
      gitPanel = null;
    } catch (e) {
      gitNotice = { text: String(e), error: true };
    } finally {
      gitBusy = null;
      await refreshGit(alsoPush);
    }
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      const canPush = Boolean(st?.has_remote && !st.detached);
      runCommit((e.metaKey || e.ctrlKey) && canPush);
    }
  });
  const shown = (st?.dirty_files || []).slice(0, 8);
  const extra = (st?.dirty_files || []).length - shown.length;
  panel.replaceChildren(
    el("div", { class: "git-files" }, ...shown.map((f) => el("code", {}, `${f.status} ${f.path}`)), extra > 0 ? el("span", { class: "small muted" }, `+${extra} more`) : null),
    el("div", { class: "row" }, input, isLoaded(chatModel) ? draftBtn : null),
    el(
      "div",
      { class: "row", style: "justify-content: flex-end;" },
      el("button", { type: "button", class: "small", onclick: () => { gitPanel = null; renderGit(); } }, "Cancel"),
      el("button", { type: "button", class: "small", onclick: () => runCommit(false) }, "Commit"),
      st?.has_remote && !st.detached ? el("button", { type: "button", class: "small primary", onclick: () => runCommit(true) }, "Commit & Push") : null,
    ),
  );
  panel.hidden = false;
  renderGit();
  input.focus();
}

async function openPrPanel() {
  if (gitState?.open_pr?.url) {
    invoke("open_external_url", { url: gitState.open_pr.url }).catch((e) => {
      gitNotice = { text: String(e), error: true };
      renderGit();
    });
    return;
  }
  if (gitPanel === "pr") {
    gitPanel = null;
    renderGit();
    return;
  }
  gitPanel = "pr";
  gitNotice = null;
  const panel = $("git-panel");
  const titleInput = el("input", { type: "text", placeholder: "Pull request title" });
  const bodyInput = el("textarea", { placeholder: "Short summary of what changed (optional)" });
  const draftCheck = el("input", { type: "checkbox" });
  const submitPr = async () => {
    if (submitBtn.disabled) return;
    const title = titleInput.value.trim();
    if (!title) {
      gitNotice = { text: "Enter a pull request title first.", error: true };
      renderGit();
      return;
    }
    gitBusy = "Creating PR…";
    gitNotice = null;
    renderGit();
    try {
      const pr = await invoke("git_create_pr", { project: project.id, title, body: bodyInput.value.trim(), draft: draftCheck.checked });
      gitPanel = null;
      gitNotice = { text: pr.number ? `Created PR #${pr.number}: ${pr.title}` : `Created PR: ${pr.title}`, url: pr.url || null };
    } catch (e) {
      gitNotice = { text: String(e), error: true };
    } finally {
      gitBusy = null;
      await refreshGit(true);
    }
  };
  const submitBtn = el("button", {
    type: "button",
    class: "small primary",
    disabled: !gitState?.gh_available,
    onclick: submitPr,
  }, "Create PR");
  titleInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submitPr();
    } else if (e.key === "Enter") {
      e.preventDefault();
      bodyInput.focus();
    }
  });
  bodyInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submitPr();
    }
  });
  panel.replaceChildren(
    !gitState?.gh_available ? el("div", { class: "small warn-text" }, "GitHub CLI (gh) not found on PATH. Install it with `brew install gh` and run `gh auth login`.") : null,
    el("div", { class: "row" }, titleInput),
    bodyInput,
    el(
      "div",
      { class: "row" },
      el("label", { class: "check small" }, draftCheck, "Draft PR"),
      el("button", { type: "button", class: "small", onclick: () => { gitPanel = null; renderGit(); } }, "Cancel"),
      submitBtn,
    ),
  );
  panel.hidden = false;
  renderGit();
  titleInput.focus();
  invoke("git_generate_pr_draft", { project: project.id, model: chatModel })
    .then((draft) => {
      if (gitPanel === "pr" && !titleInput.value) {
        titleInput.value = draft.title || "";
        bodyInput.value = draft.body || "";
      }
    })
    .catch(() => {});
}

$("git-branch").addEventListener("click", () => refreshGit(true));
$("git-commit-btn").addEventListener("click", openCommitPanel);
$("git-push-btn").addEventListener("click", async () => {
  if (!project || gitBusy) return;
  gitBusy = "Pushing…";
  gitNotice = null;
  renderGit();
  try {
    const p = await invoke("git_push", { project: project.id });
    gitNotice = { text: `Pushed ${p.branch} to ${p.remote}${p.set_upstream ? " (upstream set)" : ""}` };
  } catch (e) {
    gitNotice = { text: String(e), error: true };
  } finally {
    gitBusy = null;
    await refreshGit(true);
  }
});
$("git-pr-btn").addEventListener("click", openPrPanel);

// --- First-class Sub-Agent strip & inspector drawer ---

let subagents = [];
let inspectedSubagent = null;
let subagentTimer = null;

function formatElapsedMs(startMs, endMs) {
  const secs = Math.max(0, Math.floor(((endMs || Date.now()) - startMs) / 1000));
  return secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m ${secs % 60}s`;
}

function subagentGlyph(state) {
  return { running: "●", completed: "✓", failed: "✕", cancelled: "○" }[state] || "●";
}

function onSubagent(rec) {
  if (!rec || rec.project !== project?.id || rec.parent_session !== session) return;
  const idx = subagents.findIndex((s) => s.id === rec.id);
  if (idx >= 0) subagents[idx] = rec;
  else subagents.push(rec);
  renderSubagents();
}

function renderSubagents() {
  const strip = $("subagent-strip");
  const drawer = $("subagent-inspector");
  if (!subagents.length) {
    strip.hidden = true;
    drawer.hidden = true;
    clearInterval(subagentTimer);
    subagentTimer = null;
    return;
  }
  const anyRunning = subagents.some((s) => s.state === "running");
  if (anyRunning && !subagentTimer) subagentTimer = setInterval(renderSubagents, 1000);
  else if (!anyRunning && subagentTimer) {
    clearInterval(subagentTimer);
    subagentTimer = null;
  }

  const runningCount = subagents.filter((s) => s.state === "running").length;
  const heading = runningCount > 0 ? `Sub-agents (${runningCount}/${subagents.length} active)` : `Sub-agents (${subagents.length})`;
  strip.hidden = false;
  strip.replaceChildren(
    el("span", { class: "small muted" }, heading),
    ...subagents.map((rec) => {
      const active = inspectedSubagent === rec.id;
      const elapsed = formatElapsedMs(rec.started_ms, rec.finished_ms);
      const steps = rec.steps?.length ? ` · ${rec.steps.length} steps` : "";
      return el(
        "button",
        {
          type: "button",
          class: `subagent-pill${active ? " active" : ""}`,
          onclick: () => {
            inspectedSubagent = active ? null : rec.id;
            renderSubagents();
          },
        },
        el("span", { class: `dot-${rec.state}` }, subagentGlyph(rec.state)),
        el("strong", {}, rec.role),
        el("span", { class: "muted" }, rec.title),
        el("span", { class: "muted" }, `${elapsed}${steps}`),
      );
    }),
  );

  const selected = subagents.find((s) => s.id === inspectedSubagent);
  if (!selected) {
    drawer.hidden = true;
    drawer.replaceChildren();
    return;
  }
  const elapsed = formatElapsedMs(selected.started_ms, selected.finished_ms);
  const outputText = selected.result || selected.live_text || "";
  drawer.hidden = false;
  drawer.replaceChildren(
    el(
      "div",
      { class: "subagent-inspector-head" },
      el(
        "div",
        { class: "git-bar-left" },
        el("span", { class: `dot-${selected.state}` }, subagentGlyph(selected.state)),
        el("span", { class: "chip" }, selected.role),
        el("strong", {}, selected.title),
      ),
      el(
        "div",
        { class: "git-bar-actions" },
        selected.model ? el("span", { class: "chip" }, selected.model) : null,
        el("span", { class: "small muted" }, `${selected.state} · ${elapsed}`),
        selected.child_session && engine === "opencode"
          ? el("button", { type: "button", class: "small", onclick: () => showChat(selected.child_session) }, "Open session")
          : null,
        selected.state === "running" && selected.cancellable
          ? el("button", { type: "button", class: "small danger", onclick: () => invoke("subagent_cancel", { id: selected.id }) }, "Stop")
          : null,
        el("button", { type: "button", class: "small", onclick: () => { inspectedSubagent = null; renderSubagents(); $("input").focus(); } }, "Close"),
      ),
    ),
    el(
      "div",
      { class: "subagent-inspector-body" },
      selected.prompt ? el("div", {}, el("div", { class: "muted" }, "Task"), el("div", { class: "banner" }, selected.prompt)) : null,
      selected.steps?.length
        ? el(
            "div",
            {},
            el("div", { class: "muted" }, `Steps (${selected.steps.length})`),
            ...selected.steps.map((st) => el("div", { class: "subagent-step" }, el("strong", {}, st.verb), el("code", {}, st.subject || ""), el("span", { class: "muted" }, st.status))),
          )
        : null,
      selected.error ? el("div", { class: "banner bad" }, selected.error) : null,
      outputText ? el("div", {}, el("div", { class: "muted" }, selected.result ? "Result" : "Live output"), el("div", { class: "banner" }, richText(outputText))) : null,
    ),
  );
}

// --- Keyboard navigation & shortcuts ---

$("chat-list").addEventListener("keydown", (event) => {
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  const target = event.target.closest(".chat-item-title");
  if (!target) return;
  const items = [...$("chat-list").querySelectorAll(".chat-item-title")].filter((btn) => btn.offsetParent !== null);
  const idx = items.indexOf(target);
  if (idx < 0) return;
  if (event.key === "ArrowDown") {
    event.preventDefault();
    items[(idx + 1) % items.length].focus();
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    items[(idx - 1 + items.length) % items.length].focus();
  } else if (event.key === "Home") {
    event.preventDefault();
    items[0].focus();
  } else if (event.key === "End") {
    event.preventDefault();
    items[items.length - 1].focus();
  }
});

function switchAdjacentChat(delta) {
  const buttons = [...$("chat-list").querySelectorAll(".chat-item-title")].filter((btn) => btn.offsetParent !== null);
  if (!buttons.length) return;
  const activeIdx = buttons.findIndex((btn) => btn.getAttribute("aria-current") === "true");
  const nextIdx = activeIdx >= 0 ? (activeIdx + delta + buttons.length) % buttons.length : delta >= 0 ? 0 : buttons.length - 1;
  buttons[nextIdx].click();
}

document.addEventListener("keydown", (event) => {
  const mod = event.metaKey || event.ctrlKey;
  const key = event.key.toLowerCase();

  if (event.key === "Escape") {
    if (newChatModalOpen) {
      event.preventDefault();
      closeNewChatModal();
      return;
    }
    if (projectsPopoutOpen) {
      event.preventDefault();
      closeProjectsPopout();
      return;
    }
    if ($("file-menu")) {
      event.preventDefault();
      closeMenu(true);
    } else if (document.activeElement === $("chat-search") || $("chat-search").value) {
      event.preventDefault();
      $("chat-search").value = "";
      drawChatList();
      $("input").focus();
    } else if (gitPanel) {
      event.preventDefault();
      gitPanel = null;
      renderGit();
      $("input").focus();
    } else if (inspectedSubagent) {
      event.preventDefault();
      inspectedSubagent = null;
      renderSubagents();
      $("input").focus();
    } else if (busy && (!$("input").value.trim() || document.activeElement !== $("input"))) {
      event.preventDefault();
      $("stop").click();
    }
    return;
  }

  if (mod && !event.shiftKey && !event.altKey) {
    if (newChatModalOpen && /^[1-9]$/.test(key)) {
      const matched = orderedProjects($("new-chat-search")?.value || "");
      const target = matched[Number(key) - 1];
      if (target) {
        event.preventDefault();
        startNewChatInProject(target.id);
      }
      return;
    }
    if (key === "n") {
      event.preventDefault();
      openNewChatModal();
    } else if (key === "f") {
      event.preventDefault();
      $("chat-search").focus();
    } else if (key === "l" || key === "k") {
      event.preventDefault();
      $("input").focus();
    } else if (key === ",") {
      event.preventDefault();
      invoke("open_window", { name: "settings" });
    } else if (key === ".") {
      if (busy) {
        event.preventDefault();
        $("stop").click();
      }
    } else if (key === "[") {
      event.preventDefault();
      switchAdjacentChat(-1);
    } else if (key === "]") {
      event.preventDefault();
      switchAdjacentChat(1);
    }
    return;
  }

  if (mod && event.shiftKey && !event.altKey) {
    if (key === "o") {
      event.preventDefault();
      newProject();
    } else if (key === "i") {
      event.preventDefault();
      $("image-input")?.click();
    } else if (key === "p") {
      event.preventDefault();
      if (projectsPopoutOpen) closeProjectsPopout();
      else openProjectsPopout();
    } else if (key === "m") {
      event.preventDefault();
      $("chat-model").focus();
    } else if (key === "a") {
      if (session && project && !archived.has(archiveKey(project.id, session))) {
        event.preventDefault();
        archiveChat(session, true, project.id);
      }
    } else if (key === "g") {
      if (gitState?.is_repo && (gitState.dirty_files.length > 0 || gitPanel === "commit")) {
        event.preventDefault();
        openCommitPanel();
      } else {
        refreshGit(true);
      }
    }
    return;
  }

  if (event.altKey && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
    event.preventDefault();
    switchAdjacentChat(event.key === "ArrowDown" ? 1 : -1);
  }
});

(async () => {
  engine = await invoke("chat_engine");
  await loadModels();
  await refreshStatus();
  await openProject(location.hash.slice(1));
})();
