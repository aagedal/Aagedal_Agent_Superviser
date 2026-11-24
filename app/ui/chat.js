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
let engine = "opencode"; // or "builtin"
let projects = [];
let project = null;
let session = null; // null: a new chat, made when the first message is sent
let busy = false;
let status = null; // the model server's
let pending = []; // events for a chat whose id isn't known yet
let awaiting = false;
let usage = null; // {used, size} tokens of the open chat, as OpenCode reports them
let archived = new Set(); // the project's archived chats
let showArchived = false;

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

async function fileMenu(event, info) {
  closeMenu();
  editors ??= await invoke("editors");
  const item = (label, action) => el("button", { class: "menu-item", onclick: () => { closeMenu(); action(); } }, label);
  const menu = el(
    "div",
    { class: "menu", id: "file-menu" },
    info.runs_code
      ? el("div", { class: "menu-note" }, "This could run as a program, outside the sandbox, so it only opens in an editor or Finder.")
      : item(info.is_dir ? "Open in Finder" : "Open", () => openFile(info.path, "default")),
    ...editors.filter((name) => !(info.is_dir && name === "TextEdit")).map((name) => item(`Open in ${name}`, () => openFile(info.path, "editor", name))),
    item("Show in Finder", () => openFile(info.path, "finder")),
  );
  document.body.append(menu);
  const { innerWidth, innerHeight } = window;
  menu.style.left = `${Math.min(event.clientX, innerWidth - menu.offsetWidth - 8)}px`;
  menu.style.top = `${Math.min(event.clientY, innerHeight - menu.offsetHeight - 8)}px`;
}

function closeMenu() {
  $("file-menu")?.remove();
}
document.addEventListener("click", (event) => {
  if (!event.target.closest("#file-menu")) closeMenu();
});
document.addEventListener("keydown", (event) => event.key === "Escape" && closeMenu());
window.addEventListener("blur", closeMenu);

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

function appendUser(id, text) {
  list().querySelector(".empty")?.remove();
  view.texts.delete("current");
  view.thoughts.delete("current");
  let entry = id && view.users.get(id);
  if (!entry) {
    entry = { node: el("div", { class: "msg user" }), text: "" };
    if (id) view.users.set(id, entry);
    list().append(entry.node);
  }
  entry.text += text;
  entry.node.textContent = entry.text;
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
    case "user_message_chunk":
      appendUser(change.messageId, textOf(change.content));
      break;
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
  async send(text, { projectId = project?.id, sessionId = session, model = chatModel, onSession } = {}) {
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
    await invoke("acp_prompt", { project: projectId, session: target, text, model });
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
  async send(text, { projectId = project?.id, sessionId = session, model = chatModel, onSession } = {}) {
    awaiting = !sessionId;
    try {
      const id = await invoke("chat_send", { project: projectId, chat: sessionId, text, model });
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
      if (replay) appendUser(id, message.content || "");
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
  if (!awaiting) pending.length = 0;
  clearView();
  $("usage").textContent = "";
  $("title").textContent = "New chat";
  if (id && proj) {
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
}

let chatTitles = new Map();
async function renderChatList() {
  const mine = ++listGen;
  const box = $("chat-list");
  const proj = project;
  if (!proj) return box.replaceChildren();
  let chats = [];
  // A Linux VM takes a few seconds to start, more the first time.
  if (box.dataset.project !== proj.id && proj.isolation === "linux-vm") box.replaceChildren(el("p", { class: "small muted" }, "Starting the Linux VM…"));
  try {
    chats = await engines[engine].sessions(proj.id);
  } catch (e) {
    if (mine !== listGen || proj.id !== project?.id) return;
    box.replaceChildren(el("p", { class: "small muted" }, String(e)));
    return;
  }
  if (mine !== listGen || proj.id !== project?.id) return;
  box.dataset.project = proj.id;
  const archivedList = await invoke("archived_chats", { project: proj.id }).catch(() => []);
  if (mine !== listGen || proj.id !== project?.id) return;
  archived = new Set(archivedList);
  chatTitles = new Map(chats.map((c) => [c.id, c.title]));
  if (session && chatTitles.has(session)) $("title").textContent = chatTitles.get(session);
  const item = (chat) =>
    el(
      "div",
      { class: `chat-item${chat.id === session ? " active" : ""}` },
      el(
        "button",
        {
          type: "button",
          class: "chat-item-title",
          "aria-current": chat.id === session ? "true" : null,
          onclick: () => showChat(chat.id),
        },
        chat.title,
      ),
      archived.has(chat.id)
        ? el("button", { type: "button", class: "chat-item-action", title: "Put back in the list", "aria-label": `Put "${chat.title}" back in the list`, onclick: () => archiveChat(chat.id, false) }, "↩")
        : el("button", { type: "button", class: "chat-item-action", title: "Archive chat", "aria-label": `Archive "${chat.title}"`, onclick: () => archiveChat(chat.id, true) }, "⤓"),
      el("button", { type: "button", class: "chat-item-action", title: "Delete chat", "aria-label": `Delete "${chat.title}"`, onclick: () => deleteChat(chat) }, "×"),
    );
  const kept = chats.filter((c) => archived.has(c.id));
  const more = el("details", { class: "archived-chats" }, el("summary", { class: "small muted" }, `Archived (${kept.length})`), ...kept.map(item));
  more.open = showArchived || kept.some((c) => c.id === session);
  more.addEventListener("toggle", () => (showArchived = more.open));
  box.replaceChildren(...chats.filter((c) => !archived.has(c.id)).map(item), ...(kept.length ? [more] : []));
  update();
}

// Archived chats are kept, out of the list; sending in one puts it back.
async function archiveChat(id, archive) {
  const proj = project;
  if (!proj) return;
  try {
    await invoke("set_chat_archived", { project: proj.id, chat: id, archived: archive });
  } catch (e) {
    alert(e);
    return;
  }
  if (proj.id !== project?.id) return;
  if (archive && id === session) showChat(null);
  else renderChatList();
}

async function deleteChat(chat) {
  const proj = project;
  if (!proj) return;
  if (!confirm(`Delete “${chat.title}”?`)) return;
  try {
    await engines[engine].remove(chat.id, proj.id);
  } catch (e) {
    alert(e);
  }
  if (proj.id !== project?.id) return;
  if (chat.id === session) showChat(null);
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
  $("where").textContent = project ? `· ${project.folders[0].path}` : "";
  await showChat(null);
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
  else if (project.isolation !== "macos-sandbox" && engine !== "opencode") reasons.push(["The chat runs Linux VM projects with OpenCode, which this build doesn't include.", "Open in OpenCode", () => invoke("open_project", { id: project.id })]);
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
  $("archive").hidden = !session || archived.has(session);
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
  scrollDown();
  update();
}

function onOpencode(event) {
  if (event.type === "starting") {
    if (event.project === project?.id) $("chat-list").replaceChildren(el("p", { class: "small muted" }, event.text));
    return;
  }
  if (event.project !== project?.id || event.session !== session) {
    if (event.type === "state" && !event.busy && event.project === project?.id) renderChatList();
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
  if (event.chat !== session) {
    if (event.type === "state") renderChatList();
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
listen("status", refreshStatus);
listen("discovery", loadModels);
listen("projects", async () => {
  await loadProjects();
  const found = project ? projects.find((p) => p.id === project.id) : null;
  if (found) {
    project = found;
    $("project").value = project.id;
    $("where").textContent = `· ${project.folders[0].path}`;
    update();
  } else if (project || projects.length) {
    await openProject(projects[0]?.id || "");
  } else {
    $("project").value = "";
    $("where").textContent = "";
    update();
  }
});

async function send(text) {
  const proj = project;
  const startSession = session;
  const modelId = chatModel;
  const startGen = chatGen;
  if (!proj || !modelId) return;

  list().querySelector(".empty")?.remove();
  const userNode = appendUser(null, text);
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
    if (startSession && archived.has(startSession)) {
      await invoke("set_chat_archived", { project: proj.id, chat: startSession, archived: false })
        .then(() => {
          if (startGen === chatGen) archived.delete(startSession);
        })
        .catch(() => {});
      const afterUnarchive = checkActive();
      if (afterUnarchive === "switched") return;
      if (afterUnarchive === "cancelled") throw SEND_CANCELLED;
    }
    const targetSession = await engines[engine].send(text, {
      projectId: proj.id,
      sessionId: startSession,
      model: modelId,
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
    if (!$("input").value) $("input").value = text;
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
  if (!text || $("send").disabled) return;
  $("input").value = "";
  send(text);
});
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
$("new-chat").addEventListener("click", () => showChat(null));
$("archive").addEventListener("click", () => session && archiveChat(session, true));
$("project").addEventListener("change", () => openProject($("project").value));
$("manage").addEventListener("click", () => invoke("open_window", { name: "settings" }));

(async () => {
  engine = await invoke("chat_engine");
  await loadModels();
  await refreshStatus();
  await openProject(location.hash.slice(1));
})();
