const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let history = [];
let api = null;
let busy = false;

async function refreshApi() {
  api = await invoke("api_info");
  $("model").textContent = api.model || "No model running";
  $("send").disabled = !api.model || busy;
}

function bubble(role, text = "") {
  const node = document.createElement("div");
  node.className = `msg ${role}`;
  node.textContent = text;
  $("messages").append(node);
  node.scrollIntoView({ block: "end" });
  return node;
}

async function send(text) {
  history.push({ role: "user", content: text });
  bubble("user", text);
  const reply = bubble("assistant", "…");
  let thinking = null;
  let content = "";
  let reasoning = "";
  busy = true;
  $("send").disabled = true;
  try {
    const response = await fetch(`${api.base_url}/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${api.api_key}` },
      body: JSON.stringify({ model: api.model, messages: history, stream: true }),
    });
    if (!response.ok) throw new Error((await response.text()) || response.statusText);
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let boundary;
      while ((boundary = buffer.indexOf("\n\n")) >= 0) {
        const event = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        for (const line of event.split("\n")) {
          if (!line.startsWith("data:")) continue;
          const data = line.slice(5).trim();
          if (data === "[DONE]") continue;
          const delta = JSON.parse(data).choices?.[0]?.delta || {};
          if (delta.reasoning_content) {
            reasoning += delta.reasoning_content;
            if (!thinking) {
              thinking = document.createElement("details");
              thinking.append(Object.assign(document.createElement("summary"), { textContent: "Thinking" }), document.createElement("div"));
            }
            thinking.lastChild.textContent = reasoning;
          }
          if (delta.content) content += delta.content;
        }
      }
      reply.replaceChildren(...(thinking ? [thinking] : []), document.createTextNode(content || (reasoning ? "" : "…")));
      reply.scrollIntoView({ block: "end" });
    }
    history.push({ role: "assistant", content });
  } catch (e) {
    reply.textContent = `Error: ${e.message || e}`;
    history.pop();
  } finally {
    busy = false;
    refreshApi();
  }
}

$("composer").addEventListener("submit", (event) => {
  event.preventDefault();
  const text = $("input").value.trim();
  if (!text || busy || !api?.model) return;
  $("input").value = "";
  send(text);
});
$("input").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    $("composer").requestSubmit();
  }
});
$("clear").addEventListener("click", () => {
  history = [];
  $("messages").replaceChildren();
});
listen("status", refreshApi);
refreshApi();
