const transcript = document.getElementById("transcript");
const composer = document.getElementById("composer");
const messageInput = document.getElementById("message");
const tierCard = document.getElementById("tier-card");
const tierBadge = document.getElementById("tier-badge");
const tierModel = document.getElementById("tier-model");
const weakCounter = document.getElementById("weak-counter");
const weakCounterLabel = document.getElementById("weak-counter-label");
const pegBanner = document.getElementById("peg-banner");
const routeLog = document.getElementById("route-log");
const sessionTag = document.getElementById("session-tag");
const newSessionBtn = document.getElementById("new-session");
const logLines = document.getElementById("log-lines");
const logFollow = document.getElementById("log-follow");

const MAX_LOG_LINES = 500;

function classifyLogLine(line) {
  if (/\bERROR\b/.test(line)) return "level-error";
  if (/\bWARN\b/.test(line)) return "level-warn";
  if (/escalation ratchet held/.test(line)) return "ratchet";
  if (/switchyard_route:/.test(line)) return "route";
  return "";
}

function appendLogLine(line) {
  const row = document.createElement("div");
  row.className = `log-line ${classifyLogLine(line)}`.trim();
  row.textContent = line;
  logLines.appendChild(row);
  while (logLines.childElementCount > MAX_LOG_LINES) {
    logLines.removeChild(logLines.firstChild);
  }
  if (logFollow.checked) {
    logLines.scrollTop = logLines.scrollHeight;
  }
}

function connectLogStream() {
  const source = new EventSource("/api/logs");
  source.onmessage = (event) => {
    try {
      const { line } = JSON.parse(event.data);
      appendLogLine(line);
    } catch {
      // ignore malformed frames
    }
  };
  source.onerror = () => {
    // EventSource auto-reconnects; nothing to do here.
  };
}

const state = {
  sessionId: crypto.randomUUID(),
  weakTurns: 0,
  turnCount: 0,
  pegged: false,
  busy: false,
};

function shortId(id) {
  return id.slice(0, 8);
}

function resetUi() {
  transcript.innerHTML = "";
  routeLog.innerHTML = "";
  tierCard.dataset.tier = "idle";
  tierBadge.textContent = "—";
  tierModel.textContent = "waiting for first turn";
  weakCounter.textContent = "0";
  weakCounterLabel.textContent = "turns answered by the small model";
  pegBanner.classList.add("hidden");
  sessionTag.textContent = `session ${shortId(state.sessionId)}`;
  addBubble("system", `New session ${shortId(state.sessionId)} — independent routing floor.`);
}

function addBubble(kind, text, tag) {
  const bubble = document.createElement("div");
  bubble.className = `bubble ${kind}`;
  bubble.textContent = text;
  if (tag) {
    const pill = document.createElement("div");
    pill.className = `tag ${tag.tier}`;
    pill.textContent = `${tag.tier} · ${tag.model} · ${tag.latency_ms}ms`;
    bubble.appendChild(document.createElement("br"));
    bubble.appendChild(pill);
  }
  transcript.appendChild(bubble);
  transcript.scrollTop = transcript.scrollHeight;
  return bubble;
}

function addLogEntry(turnNo, tier, model) {
  const li = document.createElement("li");
  const dot = document.createElement("span");
  dot.className = `dot ${tier}`;
  const label = document.createElement("span");
  label.className = "turn-no";
  label.textContent = turnNo;
  const text = document.createElement("span");
  text.textContent = `${tier} — ${model}`;
  li.appendChild(label);
  li.appendChild(dot);
  li.appendChild(text);
  routeLog.appendChild(li);
  routeLog.scrollTop = routeLog.scrollHeight;
}

function applyTier(tier, model) {
  tierCard.dataset.tier = tier;
  tierBadge.textContent = tier.toUpperCase();
  tierModel.textContent = model;

  if (tier === "weak" && !state.pegged) {
    state.weakTurns += 1;
    weakCounter.textContent = String(state.weakTurns);
  }

  if (tier === "strong" && !state.pegged) {
    state.pegged = true;
    weakCounterLabel.textContent = `turns on the small model before escalation`;
    pegBanner.classList.remove("hidden");
  }
}

async function sendMessage(text) {
  if (state.busy || !text.trim()) return;
  state.busy = true;
  messageInput.value = "";
  addBubble("user", text);

  const thinking = addBubble("assistant", "…routing and generating…");

  try {
    const response = await fetch("/api/chat", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ session_id: state.sessionId, message: text }),
    });
    const data = await response.json();
    thinking.remove();

    if (!response.ok) {
      addBubble("error", `routing unavailable: ${data.error || response.status}`);
      return;
    }

    state.turnCount += 1;
    addBubble("assistant", data.reply, { tier: data.tier, model: data.model, latency_ms: data.latency_ms });
    addLogEntry(state.turnCount, data.tier, data.model);
    applyTier(data.tier, data.model);
  } catch (err) {
    thinking.remove();
    addBubble("error", `request failed: ${err}`);
  } finally {
    state.busy = false;
  }
}

composer.addEventListener("submit", (event) => {
  event.preventDefault();
  sendMessage(messageInput.value);
});

document.querySelectorAll(".chip").forEach((chip) => {
  chip.addEventListener("click", () => sendMessage(chip.dataset.prompt));
});

newSessionBtn.addEventListener("click", () => {
  state.sessionId = crypto.randomUUID();
  state.weakTurns = 0;
  state.turnCount = 0;
  state.pegged = false;
  resetUi();
});

fetch("/api/config")
  .then((r) => r.json())
  .then((cfg) => {
    document.getElementById("legend-weak").textContent = cfg.weak_model;
    document.getElementById("legend-strong").textContent = cfg.strong_model;
    document.getElementById("legend-judge").textContent = cfg.judge_model;
  })
  .catch(() => {});

resetUi();
connectLogStream();
