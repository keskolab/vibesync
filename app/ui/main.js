// VibeSync popover — all data comes from the engine via get_status; the only
// static text here is labels. No mock values: anything dynamic-looking must
// be filled from a backend call or left empty until one succeeds.

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);
const IS_MAC = /Mac/i.test(navigator.platform || navigator.userAgent);
const DEVICE = IS_MAC ? "Mac" : "PC";
if (!IS_MAC) document.documentElement.classList.add("win");

const ICONS = {
	// Claude: starburst (8-spoke asterisk).
	"claude-code":
		'<svg class="row-icon" viewBox="0 0 16 16">' +
		'<rect x="7.25" y="1" width="1.5" height="14" rx="0.75"/>' +
		'<rect x="7.25" y="1" width="1.5" height="14" rx="0.75" transform="rotate(45 8 8)"/>' +
		'<rect x="7.25" y="1" width="1.5" height="14" rx="0.75" transform="rotate(90 8 8)"/>' +
		'<rect x="7.25" y="1" width="1.5" height="14" rx="0.75" transform="rotate(135 8 8)"/>' +
		"</svg>",
	// Codex: six-petal knot.
	codex:
		'<svg class="row-icon" viewBox="0 0 16 16">' +
		[0, 60, 120, 180, 240, 300]
			.map(
				(a) =>
					`<ellipse cx="8" cy="4.6" rx="1.5" ry="3.4" transform="rotate(${a} 8 8)"/>`,
			)
			.join("") +
		"</svg>",
	// VS Code: folded-ribbon mark silhouette.
	vscode:
		'<svg class="row-icon" viewBox="0 0 16 16"><path fill-rule="evenodd" d="M10.8 1.2L15 3v10l-4.2 1.8L4.6 9.2l-2.5 2-1.1-.7L3.5 8 1 5.5l1.1-.7 2.5 2 6.2-5.6zM10.8 4.7L7.1 8l3.7 3.3V4.7z"/></svg>',
	// Zed: framed bold Z.
	zed: '<svg class="row-icon" viewBox="0 0 16 16"><path fill-rule="evenodd" d="M3 1.5h10A1.5 1.5 0 0 1 14.5 3v10a1.5 1.5 0 0 1-1.5 1.5H3A1.5 1.5 0 0 1 1.5 13V3A1.5 1.5 0 0 1 3 1.5zm0 1.5v10h10V3H3zm2 1.7h6v1.5l-3.9 4.1H11v1.5H5v-1.5l3.9-4.1H5V4.7z"/></svg>',
	// Copilot CLI: goggle visor with two eyes.
	copilot:
		'<svg class="row-icon" viewBox="0 0 16 16"><path d="M8 2.5c-2.9 0-5.2 1-5.6 2.9L2 8.1c-.6.2-1 .8-1 1.4v1.6c0 .3.1.5.3.7 1.6 1.3 4 2 6.7 2s5.1-.7 6.7-2c.2-.2.3-.4.3-.7V9.5c0-.6-.4-1.2-1-1.4l-.4-2.7C13.2 3.5 10.9 2.5 8 2.5zm-2.4 6c.5 0 .9.4.9.9v1.4c0 .5-.4.9-.9.9s-.9-.4-.9-.9V9.4c0-.5.4-.9.9-.9zm4.8 0c.5 0 .9.4.9.9v1.4c0 .5-.4.9-.9.9s-.9-.4-.9-.9V9.4c0-.5.4-.9.9-.9zM8 4c2.3 0 3.9.7 4.1 1.8.2 1 .1 1.6-.2 1.9-.6.5-2 .7-3.9.7s-3.3-.2-3.9-.7c-.3-.3-.4-.9-.2-1.9C4.1 4.7 5.7 4 8 4z"/></svg>',
	// OpenCode: open bracket-block.
	opencode:
		'<svg class="row-icon" viewBox="0 0 16 16"><path fill-rule="evenodd" d="M2 3.5A1.5 1.5 0 0 1 3.5 2h9A1.5 1.5 0 0 1 14 3.5v9A1.5 1.5 0 0 1 12.5 14h-9A1.5 1.5 0 0 1 2 12.5v-9zm3.6 2.1L3.2 8l2.4 2.4.9-.9L4.9 8l1.6-1.5-.9-.9zM10 5.6l-.9.9L10.7 8 9.1 9.5l.9.9L12.4 8 10 5.6z"/></svg>',
	// Shared skills: four-point spark.
	shared:
		'<svg class="row-icon" viewBox="0 0 16 16"><path d="M8 0.8l1.7 5.5L15.2 8l-5.5 1.7L8 15.2 6.3 9.7 0.8 8l5.5-1.7L8 0.8z"/></svg>',
};

let status = null; // last get_status result
const isSetup = () => !!localStorage.getItem("setupDone");

// ---------- helpers ----------

// Real timestamp: "14:02" today, "Yesterday 14:02", else "11 Jul 14:02".
function syncStamp(ts) {
	const d = new Date(ts);
	const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
	const today = new Date();
	const sameDay = (a, b) =>
		a.getFullYear() === b.getFullYear() &&
		a.getMonth() === b.getMonth() &&
		a.getDate() === b.getDate();
	if (sameDay(d, today)) return time;
	const yesterday = new Date(today);
	yesterday.setDate(today.getDate() - 1);
	if (sameDay(d, yesterday)) return `Yesterday ${time}`;
	return `${d.toLocaleDateString([], { day: "numeric", month: "short" })} ${time}`;
}

function fmtMB(bytes) {
	return (bytes / (1024 * 1024)).toFixed(bytes > 50e6 ? 0 : 1);
}

// 220 MB stays MB; past 1024 MB switch to GB ("1.23 GB").
function fmtSize(bytes) {
	const mb = bytes / (1024 * 1024);
	if (mb >= 1024) {
		const gb = mb / 1024;
		return {
			v: gb.toLocaleString(undefined, {
				maximumFractionDigits: gb >= 100 ? 0 : 2,
			}),
			unit: "GB",
		};
	}
	return { v: fmtMB(bytes), unit: "MB" };
}
const fmtSizeStr = (b) => {
	const s = fmtSize(b);
	return `${s.v} ${s.unit}`;
};

// Reusable stat-card row (used on the main page and every tool page).
function statCards(el, items) {
	el.innerHTML = items
		.map(
			({ value, label }) =>
				`<div class="count"><b>${value}</b><span>${label}</span></div>`,
		)
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
		// scrollHeight, not offsetHeight: pages scroll internally now, so
		// offsetHeight is clamped to the current window and could never grow it.
		const h = Math.min(640, page.scrollHeight + footer.offsetHeight + 2);
		invoke("fit_popover", { width: 320, height: h }).catch(() => {});
	});
}

// ---------- rendering ----------

function setSubText(msg) {
	const el = $("substatus");
	el.dataset.line = "";
	el.innerHTML = `<span class="span-2"></span>`;
	el.firstChild.textContent = msg;
}

function setPending(pending) {
	$("setup-pending").style.display = pending ? "flex" : "none";
	for (const id of [
		"counts",
		"hint",
		"tools-label",
		"tool-list",
		"shared-label",
		"shared-list",
		"sync-now",
		"progress",
	]) {
		$(id).style.display = pending ? "none" : "";
	}
	if (pending) {
		$("status-dot").className = "dot";
		$("status-text").textContent = "Not set up";
		setSubText("Choose where your sessions live to start syncing");
	}
	const setStore = $("set-store");
	if (setStore && pending) setStore.textContent = "Not set up";
	const changeBtn = $("change-store");
	if (changeBtn)
		changeBtn.textContent = pending
			? "Set up storage\u2026"
			: "Change storage\u2026";
	fitWindow();
}

let autosyncOn = null; // mirrored from settings
let autosyncMins = 15; // interval, mirrored from settings

function fmtClock(ms) {
	return new Date(ms).toLocaleTimeString([], {
		hour: "2-digit",
		minute: "2-digit",
	});
}

// "+N new · 16:04" chip for a tool/shared card with unseen synced items.
function newBadge(count, ms) {
	if (!count) return "";
	return ` <span class="badge-new">+${count} new${ms ? ` · ${fmtClock(ms)}` : ""}</span>`;
}

function renderStatusLine() {
	if (!status || !status.configured) return;
	const when = status.lastSyncMs ? syncStamp(status.lastSyncMs) : "never";
	const loc = status.storeDesc || "not set";
	let auto = "";
	if (autosyncOn === false) {
		auto = `<span><span class="kv-label">Auto-sync:</span> off</span>`;
	} else if (autosyncOn === true) {
		// Worker due-check: interval after the last sync, polled once a minute —
		// an overdue machine (just woke or launched) syncs within one: "soon".
		const next = (status.lastSyncMs || 0) + autosyncMins * 60000;
		auto = `<span><span class="kv-label">Next sync:</span> ${next <= Date.now() ? "soon" : fmtClock(next)}</span>`;
	}
	const html = `<span><span class="kv-label">Last sync:</span> ${when}</span>${auto}<span class="span-2"><span class="kv-label">Location:</span> ${loc}</span>`;
	if ($("substatus").dataset.line !== html) {
		$("substatus").dataset.line = html;
		$("substatus").innerHTML = html;
		$("substatus").title = status.storeDetail || "";
		setTimeout(fitWindow, 50); // height changes with content
	}
}

function renderAll() {
	if (!status || !status.configured) return;
	// Totals across the apps that are actually syncing (installed + enabled).
	const active = status.tools.filter((t) => t.installed && t.enabled);
	const totalSize = fmtSize(active.reduce((n, t) => n + t.bytes, 0));
	statCards($("counts"), [
		{ value: active.reduce((n, t) => n + t.sessions, 0), label: "sessions" },
		{ value: active.length, label: "apps syncing" },
		{ value: totalSize.v, label: `${totalSize.unit} local` },
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
				t.installed
					? `${t.sessions} sessions · ${t.plans} plans · ${fmtSizeStr(t.bytes)}${newBadge(t.newItems, t.newMs)}`
					: "Not installed"
			}</span></div>
      ${
				t.installed
					? `<label class="switch"><input type="checkbox" ${t.enabled ? "checked" : ""} /><span class="knob"></span></label>
       <svg class="chevron" viewBox="0 0 16 16"><path d="M5.5 3l5 5-5 5-1-1 4-4-4-4z"/></svg>`
					: `<span class="na">—</span>`
			}`;
		if (t.installed) {
			// Clicks on the switch (label + knob, not just the hidden input) must
			// toggle without navigating into the tool page.
			li.querySelector("label.switch").addEventListener("click", (e) =>
				e.stopPropagation(),
			);
			li.querySelector("input").addEventListener("change", async (e) => {
				status = await invoke("set_tool_enabled", {
					id: t.id,
					enabled: e.target.checked,
				});
				renderAll();
			});
			li.addEventListener("click", () => openTool(t));
		}
		ul.appendChild(li);
	}
	// Shared (cross-tool) content: global skills per the Agent Skills spec.
	// Always shown — when the folder is missing, instruct + offer to create it.
	const sl = $("shared-list");
	const sLabel = $("shared-label");
	const SKILLS_PATH = IS_MAC
		? "~/.agents/skills"
		: "%USERPROFILE%\\.agents\\skills";
	sLabel.style.display = "";
	sl.style.display = "";
	if (status.sharedInstalled) {
		sl.innerHTML = `<li>
      ${ICONS.shared}
      <div class="tlabel"><span class="tname">Global Skills <span class="name-note">(used by all AI tools)</span></span><span class="tsub">${status.sharedSkills} skill${status.sharedSkills === 1 ? "" : "s"}, ${fmtSizeStr(status.sharedBytes)}${newBadge(status.sharedNew, status.sharedNewMs)}<br>Path: ${SKILLS_PATH}</span></div>
      <label class="switch"><input type="checkbox" ${status.sharedEnabled ? "checked" : ""} /><span class="knob"></span></label>
      <span class="chevron-spacer"></span>`;
		sl.querySelector("label.switch").addEventListener("click", (e) =>
			e.stopPropagation(),
		);
		sl.querySelector("input").addEventListener("change", async (e) => {
			status = await invoke("set_scope_enabled", {
				scope: "shared",
				enabled: e.target.checked,
			});
			renderAll();
		});
		if (status.sharedNew > 0) {
			sl.querySelector("li").addEventListener("click", () => {
				invoke("ack_new", { id: "shared" })
					.then((s) => {
						status = s;
						renderAll();
					})
					.catch(() => {});
			});
		}
	} else {
		sl.innerHTML = `<li class="muted">
      ${ICONS.shared}
      <div class="tlabel"><span class="tname">Global Skills <span class="name-note">(used by all AI tools)</span></span><span class="tsub">No skills folder on this ${DEVICE} yet.<br>Path: ${SKILLS_PATH} — press Create to make it.</span></div>
      <button class="row-btn" id="create-skills">Create</button>`;
		sl.querySelector("#create-skills").addEventListener("click", async () => {
			status = await invoke("create_skills_dir");
			renderAll();
		});
	}
	fitWindow();
}

async function refreshStatus() {
	status = await invoke("get_status");
	if (!status.configured) localStorage.removeItem("setupDone");
	setPending(!status.configured);
	renderAll();
}

function openTool(t) {
	if (t.newItems > 0) {
		invoke("ack_new", { id: t.id })
			.then((s) => {
				status = s;
				renderAll();
			})
			.catch(() => {});
	}
	$("tool-title").textContent = t.name;
	const sz = fmtSize(t.bytes);
	statCards($("tool-counts"), [
		{ value: t.sessions, label: t.sessions === 1 ? "session" : "sessions" },
		{ value: t.projects, label: t.projects === 1 ? "project" : "projects" },
		{ value: sz.v, label: `${sz.unit} local` },
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
			{
				scope: "sessions",
				name: "Sessions & memory",
				sub: "Transcripts, subagents, auto-memory",
				on: on("sessions"),
			},
			{
				scope: "plans",
				name: "Plans, tasks & history",
				sub: "Plans, tasks, command history",
				on: on("plans"),
			},
			{
				scope: "config",
				name: "Agents, skills & settings",
				sub: "Custom agents, skills, rules, CLAUDE.md",
				on: on("config"),
			},
			{
				scope: "plugins",
				name: "Plugins",
				sub: "Can be large — off by default",
				on: status.syncPlugins,
			},
			{
				scope: "registry",
				name: "App sidebar",
				sub: "New sessions appear after restarting Claude",
				on: status.syncRegistry,
			},
		],
		vscode: [
			{
				tool: "vscode",
				name: "Copilot chats",
				sub: "Chat history per project folder",
				on: t.enabled,
			},
			{
				scope: "vscode-index",
				name: "Chat history panel",
				sub: "Synced chats appear in matching folders",
				on: on("vscode-index"),
			},
		],
		codex: [
			{
				tool: "codex",
				name: "Sessions",
				sub: "Rollout transcripts + session index",
				on: t.enabled,
			},
		],
		opencode: [
			{
				tool: "opencode",
				name: "Sessions",
				sub: "Chat records (archive — see notes)",
				on: t.enabled,
			},
		],
		zed: [
			{
				tool: "zed",
				name: "Agent threads",
				sub: "Zed AI threads (sync while Zed is closed)",
				on: t.enabled,
			},
		],
		copilot: [
			{
				tool: "copilot",
				name: "CLI sessions",
				sub: "Standalone copilot sessions (VS Code chats sync via VS Code)",
				on: t.enabled,
			},
		],
	};
	for (const s of SCOPES[t.id] || []) {
		const li = document.createElement("li");
		li.innerHTML = `
      <div class="tlabel">${s.name}<span class="tsub">${s.sub}</span></div>
      <label class="switch"><input type="checkbox" ${s.on ? "checked" : ""} /><span class="knob"></span></label>`;
		li.querySelector("input").addEventListener("change", async (e) => {
			status = s.tool
				? await invoke("set_tool_enabled", {
						id: s.tool,
						enabled: e.target.checked,
					})
				: await invoke("set_scope_enabled", {
						scope: s.scope,
						enabled: e.target.checked,
					});
			renderAll();
		});
		ul.appendChild(li);
	}
	$("tool-storage").innerHTML = `
    <div><span>Last activity</span><b>${t.lastActivityMs ? syncStamp(t.lastActivityMs) : "—"}</b></div>
    <div><span>Store</span><b>${status.storeDesc || "—"}</b></div>
    <div><span>Adapter</span><b>${t.id}</b></div>`;
	goTo(1);
}

// ---------- sync ----------

function setBusy(busy, label) {
	const btn = $("sync-now");
	btn.classList.toggle("busy", busy);
	btn.disabled = busy;
	$("sync-label").textContent = busy ? label : "Sync Now";
	if (busy) {
		$("status-dot").className = "dot busy";
		$("status-text").textContent = "Syncing";
	}
	$("progress").classList.toggle("active", busy);
	if (!busy) $("progress-bar").style.width = "0";
	setTimeout(fitWindow, 50); // busy label/progress change the page height
}

async function runSync(firstRun) {
	setBusy(true, firstRun ? "First sync…" : "Syncing…");
	$("progress-bar").style.width = "0";
	if (firstRun) setSubText("First sync in progress…");
	try {
		const outcome = await invoke("sync_now");
		await refreshStatus();
		const notes = [];
		if (outcome.registryHealed > 0) {
			notes.push(
				`${outcome.registryHealed} expired session${outcome.registryHealed === 1 ? "" : "s"} removed — Claude auto-deletes conversations after ~30 days; your storage still keeps them.`,
			);
		} else if (outcome.registryGhosts > 0 && outcome.registryApplied > 0) {
			notes.push(
				`${outcome.registryGhosts} expired session${outcome.registryGhosts === 1 ? " was" : "s were"} skipped (auto-deleted by Claude after ~30 days).`,
			);
		}
		if (notes.length > 0) {
			$("hint").querySelector("span").textContent = notes.join(" ");
			$("hint").classList.remove("hidden");
		}
	} catch (e) {
		$("status-dot").className = "dot";
		$("status-text").textContent = "Error";
		setSubText(String(e));
	} finally {
		setBusy(false);
		setTimeout(fitWindow, 260);
	}
}

// ---------- boot ----------

window.addEventListener("DOMContentLoaded", async () => {
	setInterval(renderStatusLine, 15000);

	setPending(!isSetup());
	invoke("engine_version")
		.then((v) => {
			$("version").textContent = `v${v}`;
		})
		.catch(() => {});
	$("open-setup").addEventListener("click", () => invoke("show_onboarding"));

	// Real progress from the engine's chunked push.
	tauri?.event.listen("sync-progress", (e) => {
		const { done, total } = e.payload;
		$("progress-bar").style.width = `${Math.round((done / total) * 100)}%`;
		$("sync-label").textContent =
			`${done.toLocaleString()} / ${total.toLocaleString()} files`;
	});

	// Every open re-fetches status: events fired while the window was hidden
	// (autosync finishing, badges arriving) may never have been delivered.
	tauri?.event.listen("popover-shown", () => refreshStatus().catch(() => {}));

	// Background autosync: mirror the tray's busy state inside the popover.
	tauri?.event.listen("autosync-start", () => setBusy(true, "Syncing\u2026"));
	tauri?.event.listen("autosync-done", () => {
		setBusy(false);
		refreshStatus().catch(() => {});
	});
	tauri?.event.listen("autosync-error", (e) => {
		setBusy(false);
		$("status-dot").className = "dot";
		$("status-text").textContent = "Error";
		setSubText(String(e.payload || "Sync failed"));
	});

	// Settings toggles: launch at login + autosync.
	invoke("get_settings").then((s) => {
		$("opt-autostart").checked = s.autostart;
		$("opt-autosync").checked = s.autosync;
		$("autosync-sub").textContent =
			`Sync every ${s.autosyncIntervalMins} minutes`;
		autosyncMins = s.autosyncIntervalMins;
		autosyncOn = s.autosync;
		renderStatusLine();
	});
	$("opt-autostart").addEventListener("change", (e) =>
		invoke("set_autostart", { enabled: e.target.checked }).catch(
			() => (e.target.checked = !e.target.checked),
		),
	);
	$("opt-autosync").addEventListener("change", (e) =>
		invoke("set_autosync", { enabled: e.target.checked })
			.then(() => {
				autosyncOn = e.target.checked;
				renderStatusLine();
			})
			.catch(() => (e.target.checked = !e.target.checked)),
	);

	try {
		await refreshStatus();
	} catch (e) {
		setSubText(String(e));
	}

	// Onboarding finished → default store + real first sync.
	tauri?.event.listen("setup-complete", async (e) => {
		localStorage.setItem("setupDone", "1");
		setPending(false);
		invoke("show_popover");
		const choice = e.payload || {};
		try {
			status = choice.store
				? await invoke("set_store", {
						store: choice.store,
						passphrase: choice.passphrase ?? null,
					})
				: await invoke("configure_default_store");
		} catch (err) {
			$("substatus").textContent = String(err);
			return;
		}
		if (choice.claudeEnabled === false) {
			try {
				status = await invoke("set_tool_enabled", {
					id: "claude-code",
					enabled: false,
				});
			} catch {}
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
