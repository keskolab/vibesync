// Code Sync onboarding — pure UX mock. The flow adapts to the chosen storage:
// iCloud skips credentials and passphrase; everything else routes through them.

const tauri = window.__TAURI__;
const $ = (id) => document.getElementById(id);

// ---------- data ----------

const S_ICONS = {
  cloud: '<svg class="s-icon" viewBox="0 0 20 20"><path d="M15.5 8.1A5.5 5.5 0 0 0 5 6.9 4.5 4.5 0 0 0 5.5 16h9a4 4 0 0 0 1-7.9z"/></svg>',
  folder: '<svg class="s-icon" viewBox="0 0 20 20"><path d="M2 5c0-1.1.9-2 2-2h4l2 2h6c1.1 0 2 .9 2 2v8c0 1.1-.9 2-2 2H4c-1.1 0-2-.9-2-2V5z"/></svg>',
  bucket: '<svg class="s-icon" viewBox="0 0 20 20"><path d="M3 4c0-1.1 3.1-2 7-2s7 .9 7 2-3.1 2-7 2-7-.9-7-2zm0 2.5C4.6 7.4 7.1 8 10 8s5.4-.6 7-1.5V16c0 1.1-3.1 2-7 2s-7-.9-7-2V6.5z"/></svg>',
  disk: '<svg class="s-icon" viewBox="0 0 20 20"><path d="M4 3h12c1.1 0 2 .9 2 2v10c0 1.1-.9 2-2 2H4c-1.1 0-2-.9-2-2V5c0-1.1.9-2 2-2zm11 10.5a1.5 1.5 0 1 0 0 3 1.5 1.5 0 0 0 0-3zM4 5v4h12V5H4z"/></svg>',
};

// Real app: detected at runtime. The recommended default flips per platform —
// iCloud on macOS, OneDrive on Windows (built in, already signed in).
const PLATFORM = "macos";

const ALL_BACKENDS = {
  icloud: { icon: "cloud", name: "iCloud", desc: { macos: "Private database in your iCloud. Works instantly on all your Macs.", windows: "Via your iCloud for Windows folder." } },
  onedrive: { icon: "folder", name: "OneDrive", desc: { macos: "Encrypted archive inside your existing OneDrive folder.", windows: "Built into Windows — encrypted archive in your OneDrive folder." } },
  dropbox: { icon: "folder", name: "Dropbox", desc: { macos: "Encrypted archive inside your existing Dropbox folder.", windows: "Encrypted archive inside your existing Dropbox folder." } },
  gdrive: { icon: "folder", name: "Google Drive", desc: { macos: "Encrypted archive inside your existing Drive folder.", windows: "Encrypted archive inside your existing Drive folder." } },
  r2: { icon: "bucket", name: "Cloudflare R2", desc: { macos: "Your own bucket. Bucket name + API token required.", windows: "Your own bucket. Bucket name + API token required." } },
  s3: { icon: "bucket", name: "Amazon S3", desc: { macos: "Your own bucket. Access keys required.", windows: "Your own bucket. Access keys required." } },
  usb: { icon: "disk", name: "External disk / USB", desc: { macos: "Syncs whenever the disk is connected.", windows: "Syncs whenever the disk is connected." } },
};

const LAYOUT = {
  macos: { recommended: "icloud", folders: ["onedrive", "dropbox", "gdrive"], advanced: ["r2", "s3", "usb"] },
  windows: { recommended: "onedrive", folders: ["gdrive", "dropbox", "icloud"], advanced: ["r2", "s3", "usb"] },
};

function buildBackends(platform) {
  const l = LAYOUT[platform];
  const mk = (id, group, badge) => ({ id, group, badge, icon: ALL_BACKENDS[id].icon, name: ALL_BACKENDS[id].name, desc: ALL_BACKENDS[id].desc[platform] });
  return [
    mk(l.recommended, "Recommended", "No setup"),
    ...l.folders.map((id) => mk(id, "Your cloud folder")),
    ...l.advanced.map((id) => mk(id, "Advanced")),
  ];
}

const BACKENDS = buildBackends(PLATFORM);

const OB_TOOLS = [
  { name: "Claude Code", sub: "86 sessions", found: true, on: true },
  { name: "Codex", sub: "14 sessions", found: true, on: true },
  { name: "OpenCode", sub: "7 sessions", found: true, on: false },
  { name: "Zed", sub: "Not installed", found: false },
  { name: "VS Code", sub: "Not installed", found: false },
];

const STEPS = 7;
let step = 0;
let storage = LAYOUT[PLATFORM].recommended; // pre-selected per platform

// ---------- rendering ----------

function backend() {
  return BACKENDS.find((b) => b.id === storage);
}

function renderTools() {
  const ul = $("ob-tools");
  ul.innerHTML = "";
  for (const t of OB_TOOLS) {
    const li = document.createElement("li");
    if (!t.found) li.className = "muted";
    li.innerHTML = `
      <div class="tlabel">${t.name}<span class="tsub">${t.sub}</span></div>
      ${t.found ? `<label class="switch"><input type="checkbox" ${t.on ? "checked" : ""} /><span class="knob"></span></label>` : `<span class="na">—</span>`}`;
    ul.appendChild(li);
  }
}

function renderStorage() {
  const host = $("storage-options");
  host.innerHTML = "";
  let lastGroup = null;
  for (const b of BACKENDS) {
    if (b.group !== lastGroup) {
      const gl = document.createElement("div");
      gl.className = "group-label";
      gl.textContent = b.group;
      host.appendChild(gl);
      lastGroup = b.group;
    }
    const card = document.createElement("button");
    card.className = "storage-card" + (b.id === storage ? " selected" : "");
    card.innerHTML = `
      ${S_ICONS[b.icon]}
      <div class="s-text">
        <span class="s-name">${b.name}${b.badge ? `<span class="badge">${b.badge}</span>` : ""}</span>
        <span class="s-desc">${b.desc}</span>
      </div>
      <span class="radio"></span>`;
    card.addEventListener("click", () => {
      storage = b.id;
      renderStorage();
    });
    host.appendChild(card);
  }
}

function renderConfigure() {
  const b = backend();
  const body = $("configure-body");
  $("configure-title").textContent = `Set up ${b.name}`;

  if (b.id === "icloud") {
    $("configure-sub").textContent = "Code Sync uses the iCloud account this Mac is signed into.";
    body.innerHTML = `
      <div class="big-check">
        <div class="ok-circle"><svg viewBox="0 0 20 20"><path d="M8 13.6L4.4 10 3 11.4l5 5 9-9L15.6 6z"/></svg></div>
        <b>Signed in as JohnKesko@users.noreply.github.com</b>
        <span>Nothing else to set up.</span>
      </div>`;
  } else if (["onedrive", "dropbox", "gdrive"].includes(b.id)) {
    const path = { onedrive: "~/OneDrive/Apps/Code Sync", dropbox: "~/Dropbox/Apps/Code Sync", gdrive: "~/Google Drive/Code Sync" }[b.id];
    $("configure-sub").textContent = `Code Sync keeps an encrypted archive inside your ${b.name} folder. ${b.name}'s own app syncs it between machines.`;
    body.innerHTML = `
      <div class="path-row"><span class="path" id="cfg-path">${path}</span><button class="mini-btn" id="cfg-choose">Choose&hellip;</button></div>
      <p class="inline-note" style="margin-top:10px">Detected your ${b.name} folder automatically. On your other machines, pick the same folder.</p>`;
    body.querySelector("#cfg-choose").addEventListener("click", () => {
      body.querySelector("#cfg-path").textContent = path;
    });
  } else if (b.id === "usb") {
    $("configure-sub").textContent = "Choose a connected disk. Sessions sync whenever it's plugged in.";
    body.innerHTML = `
      <div class="grant-list">
        <div class="grant-row"><div class="tlabel">Samsung T7<span class="tsub">1.4 TB free of 2 TB</span></div><span class="radio-mini">●</span></div>
        <div class="grant-row" style="opacity:.6"><div class="tlabel">SanDisk Extreme<span class="tsub">210 GB free of 1 TB</span></div><span class="radio-mini">○</span></div>
      </div>
      <p class="inline-note" style="margin-top:10px">Great for air-gapped setups &mdash; carry your sessions with you.</p>`;
  } else {
    // r2 / s3
    $("configure-sub").textContent = `Point Code Sync at your own ${b.name} bucket.`;
    body.innerHTML = `
      <div class="form">
        ${b.id === "r2" ? `<div class="field"><label>Account ID</label><input placeholder="d8efee78803a1e14&hellip;" /></div>` : `<div class="field"><label>Region</label><input placeholder="eu-north-1" /></div>`}
        <div class="field"><label>Bucket</label><input placeholder="code-sync" /></div>
        <div class="field"><label>Access key ID</label><input placeholder="&bull;&bull;&bull;&bull;&bull;&bull;&bull;&bull;" /></div>
        <div class="field"><label>Secret access key</label><input type="password" placeholder="&bull;&bull;&bull;&bull;&bull;&bull;&bull;&bull;&bull;&bull;&bull;&bull;" /></div>
        <div class="test-row"><button class="mini-btn" id="cfg-test">Test connection</button><span class="test-result" id="cfg-test-result">&#10003; Connected &mdash; bucket reachable</span></div>
      </div>`;
    body.querySelector("#cfg-test").addEventListener("click", () => {
      const r = body.querySelector("#cfg-test-result");
      r.classList.remove("show");
      setTimeout(() => r.classList.add("show"), 700);
    });
  }
}

function renderEncryption() {
  const b = backend();
  const body = $("enc-body");
  if (b.id === "icloud") {
    $("enc-title").textContent = "Protected by iCloud";
    $("enc-sub").textContent = "Your data is encrypted by Apple, tied to your Apple ID. With Advanced Data Protection it's end-to-end encrypted.";
    body.innerHTML = `
      <div class="big-check">
        <div class="ok-circle"><svg viewBox="0 0 20 20"><path d="M10 1l7 3v5c0 4.4-3 8.4-7 9.5C6 17.4 3 13.4 3 9V4l7-3zm-1.6 12.1l5.3-5.3-1.4-1.4-3.9 3.9-1.7-1.7-1.4 1.4 3.1 3.1z"/></svg></div>
        <b>No passphrase needed</b>
        <span>Nothing to remember, nothing to lose.</span>
      </div>`;
  } else {
    $("enc-title").textContent = "Choose a passphrase";
    $("enc-sub").textContent = `Everything is encrypted on this Mac before it reaches ${b.name}. ${b.name === "External disk / USB" ? "Anyone with the disk" : b.name} only ever sees ciphertext.`;
    body.innerHTML = `
      <div class="form">
        <div class="field"><label>Passphrase</label><input type="password" id="pp1" placeholder="At least 12 characters" /></div>
        <div class="strength"><div class="fill" id="pp-strength"></div></div>
        <div class="field"><label>Confirm passphrase</label><input type="password" id="pp2" /></div>
        <p class="inline-note">You'll enter the same passphrase on each machine. It never leaves your Macs &mdash; if you lose it, the data can't be recovered.</p>
      </div>`;
    body.querySelector("#pp1").addEventListener("input", (e) => {
      const n = e.target.value.length;
      const f = body.querySelector("#pp-strength");
      f.style.width = Math.min(100, n * 7) + "%";
      f.style.background = n < 8 ? "var(--destructive)" : n < 14 ? "#ff9f0a" : "var(--ok)";
    });
  }
}

function renderDone() {
  const b = backend();
  $("done-summary").innerHTML = `
    <div><span>Storage</span><b>${b.name}</b></div>
    <div><span>Encryption</span><b>${b.id === "icloud" ? "iCloud keys" : "Passphrase (age)"}</b></div>
    <div><span>Tools</span><b>Claude Code, Codex</b></div>
    <div><span>This machine</span><b>MacBook&nbsp;Pro</b></div>`;
}

// ---------- navigation ----------

function renderDots() {
  const d = $("dots");
  d.innerHTML = "";
  for (let i = 0; i < STEPS; i++) {
    const s = document.createElement("span");
    if (i === step) s.className = "on";
    d.appendChild(s);
  }
}

function accessComplete() {
  return [...document.querySelectorAll(".grant-row[data-granted]")].every((r) => r.dataset.granted === "true");
}

function update() {
  $("steps-track").style.transform = `translateX(${-step * (100 / STEPS)}%)`;
  renderDots();
  $("ob-back").classList.toggle("hidden-btn", step === 0 || step === STEPS - 1);
  const next = $("ob-next");
  next.textContent = step === 0 ? "Get Started" : step === STEPS - 1 ? "Start First Sync" : "Continue";
  next.disabled = step === 5 && !accessComplete();

  if (step === 3) renderConfigure();
  if (step === 4) renderEncryption();
  if (step === 6) renderDone();
}

window.addEventListener("DOMContentLoaded", () => {
  renderTools();
  renderStorage();
  update();

  // Replay-first-launch reset from the main window.
  tauri?.event.listen("first-run-reset", () => location.reload());

  $("ob-next").addEventListener("click", () => {
    if (step === STEPS - 1) {
      tauri?.event.emit("setup-complete");
      tauri?.core.invoke("close_onboarding");
      // reset so reopening from Settings starts fresh
      setTimeout(() => {
        step = 0;
        document.querySelectorAll(".grant-row").forEach((r) => {
          r.dataset.granted = "false";
          r.querySelector(".grant-btn").textContent = "Grant…";
        });
        update();
      }, 400);
      return;
    }
    step++;
    update();
  });
  $("ob-back").addEventListener("click", () => {
    if (step > 0) step--;
    update();
  });

  document.querySelectorAll(".grant-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      btn.closest(".grant-row").dataset.granted = "true";
      btn.textContent = "✓ Granted";
      update();
    });
  });
});
