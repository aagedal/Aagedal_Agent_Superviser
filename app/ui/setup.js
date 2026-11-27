// The setup guide, a window of its own so the advanced settings stay out of the way.
// Two choices (a model and a folder), then the chat. Each step says what it has, and only
// shows as done when it is: a model on this Mac or on its way, a project folder.
const GUIDE_STEPS = ["Model", "Folder", "Start"];
let guideStep = 0;
let guideProject = null; // id of the project made or picked in step 2
let guideState = { models: [], project: null }; // what's set up, for the step markers

async function fetchGuideState() {
  const models = (await refreshInstalledIds()).filter((m) => m.fit);
  const { projects } = await invoke("projects_info");
  const project = projects.find((p) => p.id === guideProject) || projects[0] || null;
  return { models, project };
}

// The model download under way, as [id, progress], if any.
const modelDownload = () => [...downloads].find(([id]) => id !== judgeDownload);

// A loaded model, else one on this Mac that fits: {id, name}.
function readyModel() {
  const running = overview?.models.find((m) => m.state === "running");
  if (running) return { id: running.id, name: running.name };
  const local = guideState.models[0];
  return local ? { id: local.id, name: local.model.name } : null;
}
const readyModelName = () => readyModel()?.name || null;

// What a step has: {state: "done" | "busy" | "todo", note}.
function stepStatus(step) {
  if (step === 0) {
    const ready = readyModelName();
    if (ready) return { state: "done", note: ready };
    const pending = modelDownload();
    if (pending) return { state: "busy", note: `Downloading, ${percent(pending[1])}%` };
    return { state: "todo", note: "Not chosen yet" };
  }
  if (step === 1) return guideState.project ? { state: "done", note: guideState.project.name } : { state: "todo", note: "Not chosen yet" };
  return { state: "todo", note: "Open the chat" };
}

function renderGuideSteps() {
  const statuses = GUIDE_STEPS.map((_, i) => stepStatus(i));
  const chosen = statuses.slice(0, 2).filter((s) => s.state !== "todo").length;
  // Once set up, leaving isn't skipping anything.
  $("guide-skip").textContent = statuses[0].state === "done" && statuses[1].state === "done" ? "Open Advanced Settings" : "Skip setup and go to Advanced Settings";
  $("guide-progress").textContent =
    chosen === 0 ? "Two choices and you're coding. It takes about a minute, plus the model's download, and you can change everything later."
    : chosen === 1 ? "1 of 2 chosen. One more to go."
    : statuses[0].state === "busy" ? "Both chosen. You can start as soon as the model has downloaded."
    : "All set. Start chatting whenever you like.";
  // Updated in place: downloads report progress several times a second, and a click
  // shouldn't land on a marker that's just been replaced.
  const list = $("guide-steps");
  if (list.children.length !== GUIDE_STEPS.length) {
    list.replaceChildren(
      ...GUIDE_STEPS.map((name, i) =>
        el(
          "li",
          {},
            el(
              "button",
              {
                type: "button",
                class: "step-btn",
                onclick: () => goToStep(i),
                onkeydown: (event) => {
                  if (event.metaKey || event.ctrlKey || event.altKey) return;
                  let next = -1;
                  if (event.key === "ArrowRight") next = (i + 1) % GUIDE_STEPS.length;
                  else if (event.key === "ArrowLeft") next = (i - 1 + GUIDE_STEPS.length) % GUIDE_STEPS.length;
                  else if (event.key === "Home") next = 0;
                  else if (event.key === "End") next = GUIDE_STEPS.length - 1;
                  if (next >= 0) {
                    event.preventDefault();
                    goToStep(next);
                    list.children[next]?.querySelector("button.step-btn")?.focus();
                  }
                },
              },
              el("span", { class: "num" }),
              el("span", { class: "step-text" }, el("span", {}, name), el("span", { class: "step-note" })),
            ),
        ),
      ),
    );
  }
  GUIDE_STEPS.forEach((_, i) => {
    const { state, note } = statuses[i];
    const item = list.children[i];
    item.className = `${i === guideStep ? "current" : ""} ${state}`;
    const btn = item.querySelector("button.step-btn");
    if (i === guideStep) btn.setAttribute("aria-current", "step");
    else btn.removeAttribute("aria-current");
    item.querySelector(".num").textContent = state === "done" ? "✓" : i + 1;
    item.querySelector(".step-note").textContent = note;
  });
}

// Renders can overlap (a folder chosen, the projects event, a download ending): only the
// latest one draws, so an earlier one finishing late can't show stale choices.
let guideRender = 0;
async function renderGuide() {
  const mine = ++guideRender;
  await refreshOverview();
  const state = await fetchGuideState();
  if (mine !== guideRender) return;
  guideState = state;
  if (state.project) guideProject = state.project.id;
  const content = await [guideModel, guideFolder, guideStart][guideStep]();
  if (mine !== guideRender) return;
  $("guide").dataset.step = guideStep;
  renderGuideSteps();
  $("guide-body").replaceChildren(...content.filter(Boolean));
  $("guide-back").hidden = guideStep === 0;
  $("guide-next").hidden = guideStep === GUIDE_STEPS.length - 1;
  $("guide-next").textContent = ["Next: choose a folder", "Next: start"][guideStep] || "Next";
  $("guide-next").classList.toggle("primary", stepStatus(guideStep).state !== "todo");
}

function goToStep(step) {
  guideStep = Math.max(0, Math.min(GUIDE_STEPS.length - 1, step));
  renderGuide();
}

async function guideModel() {
  const { picks } = await invoke("catalog_offers");
  // The suggested one first, so the eye lands on it.
  const first = startingPick(picks);
  const ordered = first ? [first, ...picks.filter((p) => p !== first)] : picks;
  const ready = readyModelName();
  const loading = overview.status.state === "loading";
  return [
    el("h2", {}, "Pick a model"),
    el("p", { class: "muted" }, "The model is the AI that writes the code. It runs on this Mac, so what you ask stays here. Every choice below fits this Mac; if you're not sure, take the one marked “Good place to start”. You can add others later."),
    ready ? el("div", { class: "banner good" }, `✓ ${ready} is ready. Go on to the next step.`)
    : loading ? el("div", { class: "banner" }, statusText(overview.status))
    : null,
    ...pickCards(ordered, { load: false }),
    picks.length ? null : el("div", { class: "banner" }, "None of the suggested models fit in this machine's memory."),
    el("p", { class: "small muted" }, "More models, and models other apps already downloaded, are in Advanced Settings, under Models."),
  ];
}

async function guideFolder() {
  const { project } = guideState;
  const choose = el("button", {
    class: project ? "" : "primary",
    onclick: async () => {
      const made = await chooseProjectFolder();
      if (made) {
        guideProject = made.id;
        renderGuide();
      }
    },
  }, project ? "Choose another folder…" : "Choose folder…");
  const parts = [
    el("h2", {}, "Where should the agent work?"),
    el("p", { class: "muted" }, "Pick the folder with your code, or an empty folder to start something new (the folder picker has a New Folder button). The agent can change files only in this folder, and can't read your passwords, keys or other private files."),
  ];
  if (project) {
    const internet = el("input", { type: "checkbox" });
    internet.checked = project.network === "internet";
    internet.addEventListener("change", async () => {
      const { choices, ...rest } = project;
      project.network = internet.checked ? "internet" : "local-only";
      await invoke("save_project", { project: { ...rest, network: project.network } });
    });
    parts.push(
      el("div", { class: "card" }, el("div", { class: "body" }, el("div", { class: "title" }, project.name), el("p", { class: "muted small selectable" }, project.folders[0].path))),
      el("label", { class: "check" }, internet, "Let the agent use the internet, to install packages and look things up. The guard still checks every command."),
    );
  }
  parts.push(el("div", { class: "row" }, choose), el("p", { class: "small muted" }, "Related folders, allowed websites and limits on tools are in Advanced Settings, under Projects, whenever you want them."));
  return parts;
}

const ASKS = {
  commands: " It asks you before running commands.",
  everything: " It asks you before running commands or changing files.",
  never: "",
};

// "Gemma 4 12B: 42%, about 6 min left", for a download under way.
const downloadLine = ([id, progress]) => [`${downloadNames.get(id) || "Downloading"}: ${percent(progress)}%`, timeLeft(progress)].filter(Boolean).join(", ");

async function guideStart() {
  const judge = await invoke("judge_settings");
  judgeDownload = judge.download;
  const { project } = guideState;
  const ready = readyModelName();
  const pending = modelDownload();
  const item = (state, title, text, action = null) =>
    el("li", { class: state }, el("span", { class: "mark" }, state === "done" ? "✓" : state === "busy" ? "…" : "!"), el("div", { class: "spacer" }, el("strong", {}, title), " ", text), action);
  const where = project ? project.name : "your folder";
  const checklist = el(
    "ul",
    { class: "checklist" },
    ready ? item("done", "Model", ready)
    : pending ? item("busy", "Model", el("span", { id: "guide-model-line" }, downloadLine(pending)))
    : item("todo", "Model", "not chosen yet", el("button", { class: "small", onclick: () => goToStep(0) }, "Pick one")),
    project ? item("done", "Folder", project.folders[0].path) : item("todo", "Folder", "not chosen yet", el("button", { class: "small", onclick: () => goToStep(1) }, "Choose one")),
    item("done", "Safety", `on. The agent can only change files in ${where} and can't see your passwords or keys.${ASKS[project?.approvals || "commands"]}`),
  );
  const canStart = Boolean(ready && project);
  const why = canStart ? "" : pending && project ? "Ready as soon as the download finishes." : "Pick a model and a folder first.";
  const parts = [
    el("h2", {}, canStart ? "You're ready to code" : "Almost there"),
    checklist,
    el(
      "div",
      { class: "row start-row" },
      el("button", { class: "primary big", disabled: !canStart, onclick: () => startChatting(project, readyModel()) }, project ? `Start chatting about ${project.name}` : "Start chatting"),
      why ? el("span", { class: "small muted" }, why) : null,
    ),
    el("p", { class: "small muted" }, "Tell the agent what you want in plain words, like “add a contact form” or “explain this project”. Later, open the chat from the menu bar icon."),
  ];
  if (judge.model) parts.push(judgeOption(judge));
  // OpenCode in Terminal needs the model running already; the chat starts it itself.
  if (project && overview.status.state === "running") {
    parts.push(el("p", { class: "small muted" }, "Prefer OpenCode in Terminal? ", el("button", { class: "link", onclick: () => invoke("open_project", { id: project.id }) }, "Open it in OpenCode"), "."));
  }
  return parts;
}

// The optional second check, with what it costs in memory.
function judgeOption(judge) {
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
  // The model already chosen (or on its way) may not have room beside it.
  const chosen = [
    ...guideState.models.map((m) => ({ name: m.model.name, required: m.fit.required })),
    ...[...downloadNeeds].filter(([id]) => downloads.has(id)).map(([id, required]) => ({ name: downloadNames.get(id), required })),
  ];
  const biggest = chosen.reduce((max, m) => (!max || m.required > max.required ? m : max), null);
  const tight = biggest && !judge.enabled && biggest.required + judge.memory > overview.hardware.model_budget;
  const note = !judge.fits && !judge.enabled
    ? "This Mac doesn't have the memory to run it beside a model."
    : `Safety is already on. For extra caution, a small second model (${judge.model.name}) can double-check borderline commands: a ${gib(judge.model.quant.size)} download that takes about ${gib(judge.memory)} of memory while a model runs.`;
  return el(
    "div",
    { class: "optional" },
    el("div", { class: "small muted" }, "Optional"),
    el("label", { class: "check" }, box, "Double-check borderline commands"),
    el("p", { class: "small muted indent" }, note, downloads.has(judge.download) ? " Downloading now." : ""),
    tight ? el("p", { class: "small indent warn-text" }, `${biggest.name} uses most of this Mac's memory: with this on, it would remember less of each chat, or not fit at all.`) : null,
  );
}

// Setup is done either way: the chat opens, or Settings for those who skip; this window closes.
// Starting the chat also starts its model, so the first message doesn't wait for it.
const startChatting = (project, model) => invoke("leave_setup", { to: "chat", project: project.id, model: model?.id ?? null });
const skipSetup = () => invoke("leave_setup", { to: "settings", project: null, model: null });

$("guide-back").addEventListener("click", () => goToStep(guideStep - 1));
$("guide-next").addEventListener("click", () => goToStep(guideStep + 1));
$("guide-skip").addEventListener("click", skipSetup);

document.addEventListener("keydown", (event) => {
  const mod = event.metaKey || event.ctrlKey;
  if (mod && !event.shiftKey && !event.altKey) {
    const num = Number(event.key);
    if (num >= 1 && num <= GUIDE_STEPS.length) {
      event.preventDefault();
      goToStep(num - 1);
      return;
    }
    if (event.key === "[") {
      event.preventDefault();
      goToStep(guideStep - 1);
      return;
    }
    if (event.key === "]") {
      event.preventDefault();
      goToStep(guideStep + 1);
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      if (guideStep < GUIDE_STEPS.length - 1) {
        goToStep(guideStep + 1);
      } else {
        const startBtn = $("guide-body").querySelector("button.big");
        if (startBtn && !startBtn.disabled) startBtn.click();
      }
      return;
    }
  }
  if (event.altKey && !mod && (event.key === "ArrowLeft" || event.key === "ArrowRight")) {
    event.preventDefault();
    goToStep(guideStep + (event.key === "ArrowRight" ? 1 : -1));
  }
});

// Keep the steps current: downloads, models loading, folders chosen elsewhere.
hooks.started.push(() => renderGuide());
hooks.finished.push(() => renderGuide());
hooks.judge.push(() => renderGuide());
// A full render from progress, at most one at a time: progress arrives every 250 ms, and each
// new render would cancel the last before it could draw.
let progressRender = null;
hooks.progress.push((event) => {
  if (!overview) return; // the first render hasn't finished yet; it will show this
  renderGuideSteps();
  if (event.id !== judgeDownload && guideStep === 2 && !$("guide-model-line") && !readyModel()) {
    progressRender ||= renderGuide().finally(() => (progressRender = null));
    return;
  }
  const line = $("guide-model-line");
  if (line && event.id !== judgeDownload) line.textContent = downloadLine([event.id, downloads.get(event.id)]);
});
listen("status", () => renderGuide());
listen("discovery", () => renderGuide());
listen("projects", () => renderGuide());
listen("judge", () => renderGuide());

(async () => {
  await refreshOverview();
  await refreshInstalledIds();
  await renderGuide();
})();
