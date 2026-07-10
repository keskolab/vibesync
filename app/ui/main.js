// Code Sync UX prototype — multi-tool mock. All data is fake; the only real
// calls are quit and hide (Tauri IPC).

const tauri = window.__TAURI__;
const $ = (id) => document.getElementById(id);

// ---------- mock data ----------

const ICONS = {
  claude: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M8 1l1.6 4.2L14 6.8l-3.4 2.9 1 4.3L8 11.7 4.4 14l1-4.3L2 6.8l4.4-1.6z"/></svg>',
  codex: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M8 1.2l5.9 3.4v6.8L8 14.8l-5.9-3.4V4.6L8 1.2zm0 1.7L3.6 5.4v5.2L8 13.1l4.4-2.5V5.4L8 2.9zM8 5a3 3 0 1 1 0 6 3 3 0 0 1 0-6z"/></svg>',
  opencode: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M2 3.5C2 2.7 2.7 2 3.5 2h9c.8 0 1.5.7 1.5 1.5v9c0 .8-.7 1.5-1.5 1.5h-9C2.7 14 2 13.3 2 12.5v-9zm2.2 2.1l2.4 2.4-2.4 2.4.9.9 3.3-3.3-3.3-3.3-.9.9zM8.5 11h3.3v1H8.5v-1z"/></svg>',
  zed: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M3 2h10v2.2L7.5 11H13v3H3v-2.2L8.5 5H3V2z"/></svg>',
  vscode: '<svg class="row-icon" viewBox="0 0 16 16"><path d="M6.2 8L2.8 5.2l1-1.2L8 7l3.4-6 2.6 1v12l-2.6 1L8 9 3.8 12l-1-1.2L6.2 8z"/></svg>',
};

const TOOLS = [
  {
    id: "claude",
    name: "Claude Code",
    installed: true,
    enabled: true,
    sessions: 86,
    size: "26 MB",
    scopes: [
      { name: "Sessions", sub: "CLI transcripts", on: true },
      { name: "App sidebar", sub: "Desktop app sessions", on: true },
      { name: "Plans", sub: "Plan documents", on: true },
      { name: "Plugins", sub: "Installed plugin list", on: false },
    ],
  },
  {
    id: "codex",
    name: "Codex",
    installed: true,
    enabled: true,
    sessions: 14,
    size: "4.2 MB",
    scopes: [
      { name: "Sessions", sub: "~/.codex/sessions", on: true },
      { name: "Config", sub: "Prompts & settings", on: false },
    ],
  },
  {
    id: "opencode",
    name: "OpenCode",
    installed: true,
    enabled: false,
    sessions: 7,
    size: "1.1 MB",
    scopes: [{ name: "Sessions", sub: "Chat history", on: true }],
  },
  {
    id: "vscode",
    name: "VS Code",
    installed: true,
    enabled: true,
    sessions: 23,
    size: "2.1 MB",
    scopes: [
      { name: "Copilot Chat", sub: "Per-workspace sessions", on: true },
      { name: "Window chats", sub: "Chats outside a workspace", on: false },
    ],
  },
  { id: "zed", name: "Zed", installed: false },
];

// ---------- state / helpers ----------

let lastSynced = Date.now() - 2 * 60 * 1000;

const isSetup = () => !!localStorage.getItem("setupDone");

// Toggle between the "not set up yet" and the normal synced UI.
function setPending(pending) {
  $("setup-pending").style.display = pending ? "flex" : "none";
  for (const id of ["counts", "hint", "tools-label", "tool-list", "sync-now", "progress"]) {
    $(id).style.display = pending ? "none" : "";
  }
  if (pending) {
    $("status-dot").className = "dot";
    $("status-text").textContent = "Not set up";
    $("substatus").textContent = "Waiting for setup";
  } else {
    $("status-dot").className = "dot ok";
    $("status-text").textContent = "Synced";
    renderStatus();
  }
  fitWindow();
}

// The first sync after onboarding completes: local push only, one machine.
function runFirstSync() {
  ["c-sessions", "c-tools", "c-macs"].forEach((id) => ($(id).textContent = "–"));
  $("status-dot").className = "dot busy";
  $("status-text").textContent = "Syncing";
  $("substatus").textContent = "First sync in progress…";
  const btn = $("sync-now");
  btn.classList.add("busy");
  btn.disabled = true;
  $("sync-label").textContent = "Syncing…";
  $("progress").classList.add("active");

  const start = Date.now();
  const tick = setInterval(() => {
    const pct = Math.min(100, ((Date.now() - start) / 3200) * 100);
    $("progress-bar").style.width = pct + "%";
    if (pct >= 100) {
      clearInterval(tick);
      $("c-sessions").textContent = "123";
      $("c-tools").textContent = "3";
      $("c-macs").textContent = "1"; // first machine enrolled
      lastSynced = Date.now();
      $("status-dot").className = "dot ok";
      $("status-text").textContent = "Synced";
      renderStatus();
      btn.classList.remove("busy");
      btn.disabled = false;
      $("sync-label").textContent = "Sync Now";
      $("progress").classList.remove("active");
      setTimeout(() => ($("progress-bar").style.width = "0"), 300);
      $("hint").classList.remove("hidden");
      setTimeout(fitWindow, 260);
    }
  }, 60);
}

function relTime(ts) {
  const s = Math.round((Date.now() - ts) / 1000);
  if (s < 10) return "just now";
  if (s < 60) return `${s} s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  return `${Math.round(m / 60)} h ago`;
}

function renderStatus() {
  $("substatus").textContent = `Last synced ${relTime(lastSynced)} · 31 MB in iCloud`;
}

let currentPage = 0;

function goTo(page) {
  currentPage = page;
  $("pages").style.transform = `translateX(${-page * (100 / 3)}%)`;
  fitWindow();
}

// Resize the native window to fit the visible page, like a real popover.
function fitWindow() {
  requestAnimationFrame(() => {
    const page = $("pages").children[currentPage];
    const footer = document.querySelector("footer");
    const h = Math.min(640, page.offsetHeight + footer.offsetHeight + 2);
    const w = tauri?.window;
    if (w && tauri?.dpi) {
      w.getCurrentWindow().setSize(new tauri.dpi.LogicalSize(320, h));
    }
  });
}

// ---------- tools list ----------

function toolSub(t) {
  if (!t.installed) return "Not installed";
  if (!t.enabled) return `${t.sessions} sessions · sync off`;
  return `${t.sessions} sessions · ${t.size}`;
}

function renderTools() {
  const ul = $("tool-list");
  ul.innerHTML = "";
  for (const t of TOOLS) {
    const li = document.createElement("li");
    li.className = t.installed ? "nav" : "muted";
    li.innerHTML = `
      ${ICONS[t.id]}
      <div class="tlabel">${t.name}<span class="tsub">${toolSub(t)}</span></div>
      ${
        t.installed
          ? `<label class="switch"><input type="checkbox" ${t.enabled ? "checked" : ""} /><span class="knob"></span></label>
             <svg class="chevron" viewBox="0 0 16 16"><path d="M5.5 3l5 5-5 5-1-1 4-4-4-4z"/></svg>`
          : `<span class="na">—</span>`
      }`;
    if (t.installed) {
      li.querySelector("input").addEventListener("click", (e) => {
        e.stopPropagation();
        t.enabled = e.target.checked;
        li.querySelector(".tsub").textContent = toolSub(t);
      });
      li.addEventListener("click", () => openTool(t));
    }
    ul.appendChild(li);
  }
}

// ---------- tool detail ----------

function openTool(t) {
  $("tool-title").textContent = t.name;
  $("tool-substatus").textContent = `${t.sessions} sessions on this Mac · last change 4 min ago`;
  const ul = $("tool-scopes");
  ul.innerHTML = "";
  for (const s of t.scopes) {
    const li = document.createElement("li");
    li.innerHTML = `
      <div class="tlabel">${s.name}<span class="tsub">${s.sub}</span></div>
      <label class="switch"><input type="checkbox" ${s.on ? "checked" : ""} /><span class="knob"></span></label>`;
    li.querySelector("input").addEventListener("change", (e) => (s.on = e.target.checked));
    ul.appendChild(li);
  }
  $("tool-storage").innerHTML = `
    <div><span>In iCloud</span><b>${t.size}</b></div>
    <div><span>Last pushed from</span><b>MacBook&nbsp;Pro</b></div>
    <div><span>Adapter</span><b>${t.id} v1</b></div>`;
  goTo(1);
}

// ---------- boot ----------

window.addEventListener("DOMContentLoaded", () => {
  renderStatus();
  setInterval(renderStatus, 15000);
  renderTools();

  // First-run state machine: nothing opens on its own — the user finds the
  // tray icon, sees the pending state, and opens the assistant themselves.
  setPending(!isSetup());
  $("open-setup").addEventListener("click", () => tauri?.core.invoke("show_onboarding"));

  // Onboarding finished → flip to synced UI and run the first sync.
  tauri?.event.listen("setup-complete", () => {
    localStorage.setItem("setupDone", "1");
    setPending(false);
    tauri?.core.invoke("show_popover");
    runFirstSync();
  });

  // Replay first launch: clear state, reload both windows.
  $("reset-firstrun").addEventListener("click", async () => {
    localStorage.removeItem("setupDone");
    await tauri?.event.emit("first-run-reset").catch(() => {});
    location.reload();
  });

  $("hint-close").addEventListener("click", () => {
    $("hint").classList.add("hidden");
    setTimeout(fitWindow, 260); // after the collapse animation
  });
  fitWindow();

  // Fake sync
  const btn = $("sync-now");
  btn.addEventListener("click", () => {
    if (btn.classList.contains("busy")) return;
    btn.classList.add("busy");
    btn.disabled = true;
    $("sync-label").textContent = "Syncing…";
    $("status-dot").className = "dot busy";
    $("status-text").textContent = "Syncing";
    $("progress").classList.add("active");

    const start = Date.now();
    const tick = setInterval(() => {
      const pct = Math.min(100, ((Date.now() - start) / 2000) * 100);
      $("progress-bar").style.width = pct + "%";
      if (pct >= 100) {
        clearInterval(tick);
        lastSynced = Date.now();
        renderStatus();
        btn.classList.remove("busy");
        btn.disabled = false;
        $("sync-label").textContent = "Sync Now";
        $("status-dot").className = "dot ok";
        $("status-text").textContent = "Synced";
        $("progress").classList.remove("active");
        setTimeout(() => ($("progress-bar").style.width = "0"), 300);
        $("hint").classList.remove("hidden");
        setTimeout(fitWindow, 260);
      }
    }, 60);
  });

  // Navigation
  $("cog").addEventListener("click", () => goTo(2));
  $("back-tool").addEventListener("click", () => goTo(0));
  $("back-settings").addEventListener("click", () => goTo(0));

  // Reopen onboarding (real IPC)
  $("onboarding").addEventListener("click", () => tauri?.core.invoke("show_onboarding"));

  // Grant access — cosmetic
  $("grant").addEventListener("click", (e) => {
    e.target.classList.remove("shake");
    void e.target.offsetWidth;
    e.target.classList.add("shake");
  });

  // Quit + Escape-to-hide (real IPC)
  $("quit").addEventListener("click", () => tauri?.core.invoke("quit_app"));
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") tauri?.window.getCurrentWindow().hide();
  });
});
