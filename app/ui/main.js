// VibeSync popover — M2: real engine data for Claude Code; other tools are
// shown as "adapter coming" placeholders until their adapters land.

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);
const IS_MAC = /Mac/i.test(navigator.platform || navigator.userAgent);
const DEVICE = IS_MAC ? "Mac" : "PC";
if (!IS_MAC) document.documentElement.classList.add("win");

const ICONS = {
  "claude-code": '<svg class="row-icon" viewBox="0 0 16 16"><path d="M8 1l1.6 4.2L14 6.8l-3.4 2.9 1 4.3L8 11.7 4.4 14l1-4.3L2 6.8l4.4-1.6z"/></svg>',
  codex: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M8 1.2l5.9 3.4v6.8L8 14.8l-5.9-3.4V4.6L8 1.2zm0 1.7L3.6 5.4v5.2L8 13.1l4.4-2.5V5.4L8 2.9zM8 5a3 3 0 1 1 0 6 3 3 0 0 1 0-6z"/></svg>',
  vscode: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M6.2 8L2.8 5.2l1-1.2L8 7l3.4-6 2.6 1v12l-2.6 1L8 9 3.8 12l-1-1.2L6.2 8z"/></svg>',
  zed: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M3 2h10v2.2L7.5 11H13v3H3v-2.2L8.5 5H3V2z"/></svg>',
};

const COMING_SOON = [
  { id: "codex", name: "Codex" },
  { id: "zed", name: "Zed" },
];

let status = null; // last get_status result
const isSetup = () => !!localStorage.getItem("setupDone");

// ---------- helpers ----------

// Real timestamp: "14:02" today, "Yesterday 14:02", else "11 Jul 14:02".
function syncStamp(ts) {
  const d = new Date(ts);
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const today = new Date();
  const sameDay = (a, b) =>
    a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
  if (sameDay(d, today)) return time;
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (sameDay(d, yesterday)) return `Yesterday ${time}`;
  return `${d.toLocaleDateString([], { day: "numeric", month: "short" })} ${time}`;
}

function fmtMB(bytes) {
  return (bytes / (1024 * 1024)).toFixed(bytes > 50e6 ? 0 : 1);
}

// Reusable stat-card row (used on the main page and every tool page).
function statCards(el, items) {
  el.innerHTML = items
    .map(({ value, label }) => `<div class="count"><b>${value}</b><span>${label}</span></div>`)
    .join('<div class="vsep"></div>');
}

let currentPage = 0;

function goTo(page) {
  currentPage = page;
  $("pages").style.transform = `translateX(${-page * (100 / 3)}%)`;
  fitWindow();
}

function fitWindow() {
  requestAnimationFrame(() => {
    const page = $("pages").children[currentPage];
    const footer = document.querySelector("footer");
    const h = Math.min(640, page.offsetHeight + footer.offsetHeight + 2);
    invoke("fit_popover", { width: 320, height: h }).catch(() => {});
  });
}

// ---------- rendering ----------

function setPending(pending) {
  $("setup-pending").style.display = pending ? "flex" : "none";
  for (const id of ["counts", "hint", "tools-label", "tool-list", "shared-label", "shared-list", "sync-now", "progress"]) {
    $(id).style.display = pending ? "none" : "";
  }
  if (pending) {
    $("status-dot").className = "dot";
    $("status-text").textContent = "Not set up";
    $("substatus").textContent = "Waiting for setup";
  }
  const setStore = $("set-store");
  if (setStore && pending) setStore.textContent = "Not set up";
  const changeBtn = $("change-store");
  if (changeBtn) changeBtn.textContent = pending ? "Set up storage\u2026" : "Change storage\u2026";
  fitWindow();
}

let autosyncOn = null; // mirrored from settings

function renderStatusLine() {
  if (!status) return;
  const when = status.lastSyncMs ? syncStamp(status.lastSyncMs) : "never";
  const loc = status.storeDesc || "not set";
  const auto = autosyncOn === null ? "" : `<span><span class="kv-label">Auto-sync:</span> ${autosyncOn ? "on" : "off"}</span>`;
  const html = `<span><span class="kv-label">Last sync:</span> ${when}</span>${auto}<span class="span-2"><span class="kv-label">Location:</span> ${loc}</span>`;
  if ($("substatus").dataset.line !== html) {
    $("substatus").dataset.line = html;
    $("substatus").innerHTML = html;
    $("substatus").title = status.storeDetail || "";
    setTimeout(fitWindow, 50); // height changes with content
  }
}

function renderAll() {
  if (!status) return;
  // Totals across the apps that are actually syncing (installed + enabled).
  const active = status.tools.filter((t) => t.installed && t.enabled);
  statCards($("counts"), [
    { value: active.reduce((n, t) => n + t.sessions, 0), label: "sessions" },
    { value: active.length, label: "apps syncing" },
    { value: fmtMB(active.reduce((n, t) => n + t.bytes, 0)), label: "MB local" },
  ]);
  $("status-dot").className = "dot ok";
  $("status-text").textContent = "Synced";
  const setStore = $("set-store");
  if (setStore) {
    setStore.textContent = status.storeDesc || "not set";
    setStore.title = status.storeDetail || "";
  }
  renderStatusLine();

  const ul = $("tool-list");
  ul.innerHTML = "";
  for (const t of status.tools) {
    const li = document.createElement("li");
    li.className = t.installed ? "nav" : "muted";
    li.innerHTML = `
      ${ICONS[t.id] || ""}
      <div class="tlabel">${t.name}<span class="tsub">${
        t.installed ? `${t.sessions} sessions · ${t.plans} plans · ${fmtMB(t.bytes)} MB` : "Not installed"
      }</span></div>
      ${t.installed ? `<label class="switch"><input type="checkbox" ${t.enabled ? "checked" : ""} /><span class="knob"></span></label>
       <svg class="chevron" viewBox="0 0 16 16"><path d="M5.5 3l5 5-5 5-1-1 4-4-4-4z"/></svg>` : `<span class="na">—</span>`}`;
    if (t.installed) {
      // Clicks on the switch (label + knob, not just the hidden input) must
      // toggle without navigating into the tool page.
      li.querySelector("label.switch").addEventListener("click", (e) => e.stopPropagation());
      li.querySelector("input").addEventListener("change", async (e) => {
        status = await invoke("set_tool_enabled", { id: t.id, enabled: e.target.checked });
        renderAll();
      });
      li.addEventListener("click", () => openTool(t));
    }
    ul.appendChild(li);
  }
  // Shared (cross-tool) content: global skills per the Agent Skills spec.
  const sl = $("shared-list");
  const sLabel = $("shared-label");
  if (status.sharedInstalled) {
    sLabel.style.display = "";
    sl.style.display = "";
    sl.innerHTML = `<li>
      <svg class="row-icon" viewBox="0 0 16 16"><path d="M8 1l1.8 3.6L14 5.2l-3 2.9.7 4.1L8 10.3l-3.7 1.9.7-4.1-3-2.9 4.2-.6L8 1z"/></svg>
      <div class="tlabel">Global skills<span class="tsub">${status.sharedSkills} skill${status.sharedSkills === 1 ? "" : "s"} · ${fmtMB(status.sharedBytes)} MB<br>~/.agents/skills — shared by all AI tools</span></div>
      <label class="switch"><input type="checkbox" ${status.sharedEnabled ? "checked" : ""} /><span class="knob"></span></label>`;
    sl.querySelector("label.switch").addEventListener("click", (e) => e.stopPropagation());
    sl.querySelector("input").addEventListener("change", async (e) => {
      status = await invoke("set_scope_enabled", { scope: "shared", enabled: e.target.checked });
      renderAll();
    });
  } else {
    sLabel.style.display = "none";
    sl.style.display = "none";
  }
  for (const t of COMING_SOON) {
    const li = document.createElement("li");
    li.className = "muted";
    li.innerHTML = `${ICONS[t.id] || ""}
      <div class="tlabel">${t.name}<span class="tsub">Adapter coming soon</span></div>
      <span class="na">—</span>`;
    ul.appendChild(li);
  }
  fitWindow();
}

async function refreshStatus() {
  status = await invoke("get_status");
  renderAll();
}

function openTool(t) {
  $("tool-title").textContent = t.name;
  statCards($("tool-counts"), [
    { value: t.sessions, label: t.sessions === 1 ? "session" : "sessions" },
    { value: t.projects, label: t.projects === 1 ? "project" : "projects" },
    { value: fmtMB(t.bytes), label: "MB local" },
  ]);
  const extra = $("tool-counts-extra");
  if (t.id === "claude-code") {
    extra.style.display = "";
    statCards(extra, [
      { value: t.plans, label: t.plans === 1 ? "plan" : "plans" },
      { value: t.agents, label: t.agents === 1 ? "agent" : "agents" },
      { value: t.skills, label: t.skills === 1 ? "skill" : "skills" },
    ]);
  } else {
    extra.style.display = "none";
  }
  const ul = $("tool-scopes");
  ul.innerHTML = "";
  const offScopes = status.disabledScopes || [];
  const on = (id) => !offScopes.includes(id);
  const SCOPES = {
    "claude-code": [
      { scope: "sessions", name: "Sessions & memory", sub: "Transcripts, subagents, auto-memory", on: on("sessions") },
      { scope: "plans", name: "Plans, tasks & history", sub: "Plans, tasks, command history", on: on("plans") },
      { scope: "config", name: "Agents, skills & settings", sub: "Custom agents, skills, rules, CLAUDE.md", on: on("config") },
      { scope: "plugins", name: "Plugins", sub: "Can be large — off by default", on: status.syncPlugins },
      { scope: "registry", name: "App sidebar", sub: "Claude desktop session list", on: status.syncRegistry },
    ],
    vscode: [
      { tool: "vscode", name: "Copilot chats", sub: "Chat history per project folder", on: t.enabled },
      { scope: "vscode-index", name: "Chat history panel", sub: "Synced chats appear in matching folders", on: on("vscode-index") },
    ],
  };
  for (const s of SCOPES[t.id] || []) {
    const li = document.createElement("li");
    li.innerHTML = `
      <div class="tlabel">${s.name}<span class="tsub">${s.sub}</span></div>
      <label class="switch"><input type="checkbox" ${s.on ? "checked" : ""} /><span class="knob"></span></label>`;
    li.querySelector("input").addEventListener("change", async (e) => {
      status = s.tool
        ? await invoke("set_tool_enabled", { id: s.tool, enabled: e.target.checked })
        : await invoke("set_scope_enabled", { scope: s.scope, enabled: e.target.checked });
      renderAll();
    });
    ul.appendChild(li);
  }
  $("tool-storage").innerHTML = `
    <div><span>Last activity</span><b>${t.lastActivityMs ? syncStamp(t.lastActivityMs) : "—"}</b></div>
    <div><span>Store</span><b>${status.storeDesc || "—"}</b></div>
    <div><span>Adapter</span><b>${t.id} v1</b></div>`;
  goTo(1);
}

// ---------- sync ----------

function setBusy(busy, label) {
  const btn = $("sync-now");
  btn.classList.toggle("busy", busy);
  btn.disabled = busy;
  $("sync-label").textContent = busy ? label : "Sync Now";
  $("status-dot").className = busy ? "dot busy" : "dot ok";
  $("status-text").textContent = busy ? "Syncing" : "Synced";
  $("progress").classList.toggle("active", busy);
  if (!busy) $("progress-bar").style.width = "0";
  setTimeout(fitWindow, 50); // busy label/progress change the page height
}

async function runSync(firstRun) {
  setBusy(true, firstRun ? "First sync…" : "Syncing…");
  $("progress-bar").style.width = "0";
  if (firstRun) $("substatus").textContent = "First sync in progress…";
  try {
    const outcome = await invoke("sync_now");
    await refreshStatus();
    const notes = [];
    if (outcome.registryApplied > 0) {
      notes.push(`${outcome.registryApplied} session${outcome.registryApplied === 1 ? "" : "s"} added to your Claude sidebar — restart the Claude app to see them.`);
    }
    if (outcome.registryHealed > 0) {
      notes.push(`${outcome.registryHealed} expired session${outcome.registryHealed === 1 ? "" : "s"} removed — Claude auto-deletes conversations after ~30 days; your storage still keeps them.`);
    } else if (outcome.registryGhosts > 0 && outcome.registryApplied > 0) {
      notes.push(`${outcome.registryGhosts} expired session${outcome.registryGhosts === 1 ? " was" : "s were"} skipped (auto-deleted by Claude after ~30 days).`);
    }
    if (notes.length > 0) {
      $("hint").querySelector("span").textContent = notes.join(" ");
      $("hint").classList.remove("hidden");
    } else if (outcome.pulled > 0) {
      $("hint").querySelector("span").textContent =
        `${outcome.pulled} session${outcome.pulled === 1 ? "" : "s"} pulled — available via claude --resume.`;
      $("hint").classList.remove("hidden");
    }
  } catch (e) {
    $("status-dot").className = "dot";
    $("status-text").textContent = "Error";
    $("substatus").textContent = String(e);
  } finally {
    setBusy(false);
    setTimeout(fitWindow, 260);
  }
}

// ---------- boot ----------

window.addEventListener("DOMContentLoaded", async () => {
  setInterval(renderStatusLine, 15000);

  setPending(!isSetup());
  $("open-setup").addEventListener("click", () => invoke("show_onboarding"));

  // Real progress from the engine's chunked push.
  tauri?.event.listen("sync-progress", (e) => {
    const { done, total } = e.payload;
    $("progress-bar").style.width = `${Math.round((done / total) * 100)}%`;
  });

  // Background autosync finished while the popover may be open.
  tauri?.event.listen("autosync-done", () => refreshStatus().catch(() => {}));

  // Settings toggles: launch at login + autosync.
  invoke("get_settings").then((s) => {
    $("opt-autostart").checked = s.autostart;
    $("opt-autosync").checked = s.autosync;
    autosyncOn = s.autosync;
    renderStatusLine();
  });
  $("opt-autostart").addEventListener("change", (e) =>
    invoke("set_autostart", { enabled: e.target.checked }).catch(() => (e.target.checked = !e.target.checked))
  );
  $("opt-autosync").addEventListener("change", (e) =>
    invoke("set_autosync", { enabled: e.target.checked })
      .then(() => {
        autosyncOn = e.target.checked;
        renderStatusLine();
      })
      .catch(() => (e.target.checked = !e.target.checked))
  );

  if (isSetup()) {
    try {
      await refreshStatus();
    } catch (e) {
      $("substatus").textContent = String(e);
    }
  }

  // Onboarding finished → default store + real first sync.
  tauri?.event.listen("setup-complete", async (e) => {
    localStorage.setItem("setupDone", "1");
    setPending(false);
    invoke("show_popover");
    const choice = e.payload || {};
    try {
      status = choice.store
        ? await invoke("set_store", { store: choice.store, passphrase: choice.passphrase ?? null })
        : await invoke("configure_default_store");
    } catch (err) {
      $("substatus").textContent = String(err);
      return;
    }
    if (choice.claudeEnabled === false) {
      try { status = await invoke("set_tool_enabled", { id: "claude-code", enabled: false }); } catch {}
    }
    renderAll();
    await runSync(true);
  });

  $("hint-close").addEventListener("click", () => {
    $("hint").classList.add("hidden");
    setTimeout(fitWindow, 260);
  });

  $("sync-now").addEventListener("click", () => runSync(false));

  // Navigation
  $("cog").addEventListener("click", () => goTo(2));
  $("back-tool").addEventListener("click", () => goTo(0));
  $("back-settings").addEventListener("click", () => goTo(0));

  // Change storage: open the assistant directly at the storage step.
  $("change-store").addEventListener("click", async () => {
    await invoke("show_onboarding");
    tauri?.event.emit("open-at-storage");
  });

  $("quit").addEventListener("click", () => invoke("quit_app"));
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") tauri?.window.getCurrentWindow().hide();
  });
});
