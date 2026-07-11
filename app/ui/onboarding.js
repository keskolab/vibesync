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

const BACKENDS = [
  { id: "folder", group: "Recommended", icon: "folder", name: "Choose folder\u2026", badge: "Simple", desc: "iCloud Drive, OneDrive, Dropbox, Google Drive, an external disk \u2014 any folder you control." },
  { id: "r2", group: "Cloud storage", icon: "bucket", name: "Cloudflare R2", desc: "Your own bucket. Account ID, bucket + API token." },
  { id: "s3", group: "Cloud storage", icon: "bucket", name: "Amazon S3", desc: "Your own bucket. Region, bucket + access keys." },
  { id: "azure", group: "Cloud storage", icon: "cloud", name: "Azure Blob", desc: "Paste a container SAS URL \u2014 no account keys." },
];

// Collected inputs for the chosen backend.
const chosen = { path: null, fields: {}, passphrase: "" };
let existing = null; // current app status, if already configured

function buildStore() {
  const f = chosen.fields;
  switch (storage) {
    case "folder":
      return chosen.path ? { type: "folder", path: chosen.path, encrypted: false } : null;
    case "r2":
      return f.account && f.bucket && f.key && f.secret
        ? { type: "s3", endpoint: `https://${f.account}.r2.cloudflarestorage.com`, region: "auto",
            bucket: f.bucket, access_key_id: f.key, secret_access_key: f.secret } : null;
    case "s3":
      return f.region && f.bucket && f.key && f.secret
        ? { type: "s3", endpoint: `https://s3.${f.region}.amazonaws.com`, region: f.region,
            bucket: f.bucket, access_key_id: f.key, secret_access_key: f.secret } : null;
    case "azure":
      return f.sas ? { type: "azure_sas", container_sas_url: f.sas } : null;
  }
  return null;
}

const needsPassphrase = () => storage !== "folder";

const OB_TOOLS = [
  { name: "Claude Code", sub: "86 sessions", found: true, on: true },
  { name: "Codex", sub: "14 sessions", found: true, on: true },
  { name: "OpenCode", sub: "7 sessions", found: true, on: false },
  { name: "Zed", sub: "Not installed", found: false },
  { name: "VS Code", sub: "Not installed", found: false },
];

const STEPS = 7;
let step = 0;
let storage = "folder";

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
      renderedStep = -1; // force configure re-render for the new backend
      renderStorage();
      refreshNav();
    });
    host.appendChild(card);
  }
}

function renderConfigure() {
  const b = backend();
  const body = $("configure-body");
  $("configure-title").textContent = `Set up ${b.name.replace("\u2026", "")}`;
  const field = (id, label, ph, type = "text") =>
    `<div class="field"><label>${label}</label><input id="${id}" type="${type}" placeholder="${ph}" /></div>`;

  if (b.id === "folder") {
    $("configure-sub").textContent =
      "Pick any folder \u2014 an iCloud Drive / OneDrive / Dropbox / Google Drive folder syncs it between machines automatically; a local or USB folder stays where it is.";
    body.innerHTML = `
      <div class="path-row"><span class="path" id="cfg-path">${chosen.path || "No folder selected"}</span>
      <button class="mini-btn" id="cfg-choose">Choose\u2026</button></div>
      <p class="inline-note" style="margin-top:10px">On your other machines, pick the same folder.</p>`;
    body.querySelector("#cfg-choose").addEventListener("click", async () => {
      const p = await tauri?.core.invoke("pick_folder");
      if (p) { chosen.path = p; body.querySelector("#cfg-path").textContent = p; }
      refreshNav();
    });
    return;
  }

  const forms = {
    r2: field("cfg-bucket", "Bucket", "codesync") + field("cfg-account", "Account ID", "d8efee78803a1e14\u2026")
      + field("cfg-key", "Access key ID", "") + field("cfg-secret", "Secret access key", "", "password"),
    s3: field("cfg-bucket", "Bucket", "codesync") + field("cfg-region", "Region", "eu-north-1")
      + field("cfg-key", "Access key ID", "") + field("cfg-secret", "Secret access key", "", "password"),
    azure: field("cfg-sas", "Container SAS URL", "https://acct.blob.core.windows.net/container?sv=\u2026"),
  };
  $("configure-sub").textContent = b.id === "azure"
    ? "In the Azure portal: container \u2192 Shared access tokens \u2192 create with read/write/list \u2192 copy the URL."
    : `Point Code Sync at your own ${b.name} bucket.`;
  body.innerHTML = `<div class="form">${forms[b.id]}
    <div class="test-row"><button class="mini-btn" id="cfg-test">Test connection</button>
    <span class="test-result" id="cfg-test-result"></span></div></div>`;

  const map = { "cfg-account": "account", "cfg-region": "region", "cfg-bucket": "bucket",
                "cfg-key": "key", "cfg-secret": "secret", "cfg-sas": "sas" };
  for (const [id, k] of Object.entries(map)) {
    const el = body.querySelector(`#${id}`);
    if (!el) continue;
    el.value = chosen.fields[k] || "";
    el.addEventListener("input", (e) => { chosen.fields[k] = e.target.value.trim(); refreshNav(); });
  }
  body.querySelector("#cfg-test").addEventListener("click", async () => {
    const r = body.querySelector("#cfg-test-result");
    const store = buildStore();
    if (!store) { r.textContent = "Fill in all fields first"; r.style.color = "var(--destructive)"; r.classList.add("show"); return; }
    r.textContent = "Testing\u2026"; r.style.color = "var(--text-2)"; r.classList.add("show");
    try {
      const n = await tauri?.core.invoke("test_store", { store, passphrase: chosen.passphrase || "test" });
      r.textContent = `\u2713 Connected \u2014 ${n} objects in store`; r.style.color = "var(--ok)";
    } catch (e) {
      r.textContent = `\u2717 ${String(e).slice(0, 120)}`; r.style.color = "var(--destructive)";
    }
  });
}

function renderEncryption() {
  const b = backend();
  const body = $("enc-body");
  if (b.id === "folder") {
    $("enc-title").textContent = "No passphrase needed";
    $("enc-sub").textContent = "This folder is under your control; content is compressed but not encrypted. Pick a cloud backend if you want client-side encryption.";
    body.innerHTML = `
      <div class="big-check">
        <div class="ok-circle"><svg viewBox="0 0 20 20"><path d="M10 1l7 3v5c0 4.4-3 8.4-7 9.5C6 17.4 3 13.4 3 9V4l7-3zm-1.6 12.1l5.3-5.3-1.4-1.4-3.9 3.9-1.7-1.7-1.4 1.4 3.1 3.1z"/></svg></div>
        <b>Your folder, your rules</b>
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
      chosen.passphrase = e.target.value;
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
    <div><span>Encryption</span><b>${needsPassphrase() ? "Passphrase (age)" : "None (your folder)"}</b></div>
    <div><span>Tools</span><b>Claude Code, Codex</b></div>
    <div><span>This machine</span><b>MacBook&nbsp;Pro</b></div>`;
  const old = document.getElementById("switch-warning");
  if (old) old.remove();
  if (existing?.configured) {
    const warn = document.createElement("p");
    warn.id = "switch-warning";
    warn.className = "inline-note";
    warn.style.cssText = "margin-top:12px;max-width:330px;text-align:left;color:var(--text-2)";
    warn.innerHTML = "\u26a0\ufe0e <b>You are changing where sessions are stored.</b> " +
      "Everything will be uploaded again to the new location on the next sync " +
      "(this can take a few minutes). Your old location is not touched \u2014 " +
      "it keeps its archive until you delete it yourself.";
    document.getElementById("done-summary").after(warn);
  }
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

// Nav-only refresh: safe to call on every keystroke — never rebuilds DOM
// that the user is typing into.
function refreshNav() {
  renderDots();
  $("ob-back").classList.toggle("hidden-btn", step === 0 || step === STEPS - 1);
  const next = $("ob-next");
  next.textContent = step === 0 ? "Get Started" : step === STEPS - 1 ? (existing?.configured ? "Switch Storage & Sync" : "Start First Sync") : "Continue";
  next.disabled = (step === 3 && !buildStore()) || (step === 5 && !accessComplete());
}

let renderedStep = -1;

function update() {
  $("steps-track").style.transform = `translateX(${-step * (100 / STEPS)}%)`;
  // Rebuild step content only when the visible step changes — re-rendering
  // on input events destroys focused fields (the one-character-per-keypress
  // bug).
  if (step !== renderedStep) {
    renderedStep = step;
    if (step === 3) renderConfigure();
    if (step === 4) renderEncryption();
    if (step === 6) renderDone();
  }
  refreshNav();
}

window.addEventListener("DOMContentLoaded", () => {
  tauri?.core.invoke("get_status").then((s) => { existing = s; }).catch(() => {});
  renderTools();
  renderStorage();
  update();

  // Replay-first-launch reset from the main window.
  tauri?.event.listen("first-run-reset", () => location.reload());

  $("ob-next").addEventListener("click", () => {
    if (step === STEPS - 1) {
      tauri?.event.emit("setup-complete", { store: buildStore(), passphrase: needsPassphrase() ? chosen.passphrase || null : null });
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
