// Run: node --test app/ui/tests/ui-logic.test.js   (no dependencies)
import { test } from "node:test";
import assert from "node:assert/strict";
import "../ui-logic.js";

const UI = globalThis.VibeSyncUI;

// ------------------------------------------------------------- escaping

test("esc neutralizes markup from folder names and other foreign text", () => {
	const name = `<img src=x onerror="invoke('set_store')">`;
	const out = UI.esc(name);
	assert.ok(!out.includes("<") && !out.includes(">") && !out.includes('"'));
	assert.equal(out, "&lt;img src=x onerror=&quot;invoke(&#39;set_store&#39;)&quot;&gt;");
	assert.equal(UI.esc("R&D / notes"), "R&amp;D / notes");
	assert.equal(UI.esc(42), "42");
	assert.equal(UI.esc("plain"), "plain");
});

// --------------------------------------------------------- Continue gate

const passphraseStep = (over = {}) => ({
	step: 4,
	storeOk: true,
	needsPassphrase: true,
	passphraseValid: true,
	checking: false,
	checked: "matches",
	override: false,
	finishing: false,
	...over,
});

test("a verdict for this exact passphrase is required before Continue", () => {
	assert.equal(UI.continueBlocked(passphraseStep()), false);
	// Pending (debounce) or running: blocked — the old gap let a quick
	// click commit an unchecked passphrase.
	assert.equal(UI.continueBlocked(passphraseStep({ checking: true, checked: null })), true);
	assert.equal(UI.continueBlocked(passphraseStep({ passphraseValid: false })), true);
});

test("a mismatch blocks until deliberately overridden", () => {
	assert.equal(UI.continueBlocked(passphraseStep({ checked: "mismatch" })), true);
	assert.equal(UI.continueBlocked(passphraseStep({ checked: "mismatch", override: true })), false);
});

test("settings the storage rejected always block, override or not", () => {
	assert.equal(UI.continueBlocked(passphraseStep({ checked: "invalid" })), true);
	assert.equal(UI.continueBlocked(passphraseStep({ checked: "invalid", override: true })), true);
});

test("inconclusive, new storage and mixed verdicts may continue", () => {
	for (const checked of ["inconclusive", "newStorage", "mixed", "matches"]) {
		assert.equal(UI.continueBlocked(passphraseStep({ checked })), false, checked);
	}
});

test("folder storage needs no passphrase; step 3 needs a complete config", () => {
	assert.equal(
		UI.continueBlocked(passphraseStep({ needsPassphrase: false, passphraseValid: false, checked: null })),
		false,
	);
	assert.equal(UI.continueBlocked({ step: 3, storeOk: false }), true);
	assert.equal(UI.continueBlocked({ step: 3, storeOk: true }), false);
});

test("nothing can be clicked twice while the setup is saving", () => {
	assert.equal(UI.continueBlocked({ step: 5, finishing: true }), true);
	assert.equal(UI.continueBlocked({ step: 5, finishing: false }), false);
});

// ------------------------------------------------- latest check wins

test("only the newest check may set the verdict", () => {
	const t = UI.makeLatest();
	const first = t.next();
	const second = t.next(); // the user typed again
	assert.equal(t.isCurrent(first), false, "stale result ignored");
	assert.equal(t.isCurrent(second), true);
	t.invalidate(); // Back + other storage
	assert.equal(t.isCurrent(second), false);
});

test("a slow check resolving after a newer one cannot overwrite its verdict", async () => {
	const t = UI.makeLatest();
	let verdict = null;
	const check = async (result, ms) => {
		const ticket = t.next();
		await new Promise((r) => setTimeout(r, ms));
		if (t.isCurrent(ticket)) verdict = result;
	};
	await Promise.all([check("old storage: mismatch", 30), check("new storage: matches", 5)]);
	assert.equal(verdict, "new storage: matches");
});

// --------------------------------------------------- two-click override

test("the override needs two deliberate clicks", () => {
	let confirmed = 0;
	const click = UI.makeTwoClick(() => confirmed++);
	assert.equal(click(), "armed");
	assert.equal(confirmed, 0);
	assert.equal(click(), "confirmed");
	assert.equal(confirmed, 1);
});

// ---------------------------------------------------- settings switches

test("a switch whose write fails flips back and reports", async () => {
	const input = { checked: true }; // the user just turned it on
	let error = null;
	let ok = null;
	const r = await UI.applySwitch(input, () => Promise.reject(new Error("not configured yet")), {
		onOk: (s) => (ok = s),
		onError: (e) => (error = String(e)),
	});
	assert.equal(r, false);
	assert.equal(input.checked, false, "back to what the backend has");
	assert.match(error, /not configured yet/);
	assert.equal(ok, null);
});

test("a switch whose write succeeds keeps its state and hands over the status", async () => {
	const input = { checked: false };
	let status = null;
	const r = await UI.applySwitch(input, () => Promise.resolve({ configured: true }), {
		onOk: (s) => (status = s),
	});
	assert.equal(r, true);
	assert.equal(input.checked, false);
	assert.deepEqual(status, { configured: true });
});

// ---------------------------------------------------------- finish flow

function fakeHost(setStore) {
	const calls = [];
	return {
		calls,
		invoke: async (cmd, args) => {
			calls.push(["invoke", cmd, args]);
			if (cmd === "set_store") return setStore(args);
		},
		emit: (ev, payload) => calls.push(["emit", ev, payload]),
		close: () => calls.push(["close"]),
	};
}

test("finishing saves first, then reports and closes", async () => {
	const host = fakeHost(() => ({ configured: true }));
	const r = await UI.finishSetup({
		...host,
		store: { type: "folder", path: "/x", encrypted: false },
		passphrase: null,
		disabledTools: ["zed"],
	});
	assert.deepEqual(r, { ok: true });
	assert.deepEqual(
		host.calls.map((c) => c.slice(0, 2)),
		[["invoke", "set_store"], ["emit", "setup-complete"], ["close"]],
	);
	assert.deepEqual(host.calls[1][2], { storeSaved: true, disabledTools: ["zed"] });
	assert.equal("passphrase" in host.calls[1][2], false, "the passphrase never travels in an event");
});

test("a failed save keeps the assistant open with the reason", async () => {
	const host = fakeHost(() => {
		throw "A sync is running right now — try again in a moment, once it finishes.";
	});
	const r = await UI.finishSetup({ ...host, store: {}, passphrase: "p", disabledTools: [] });
	assert.equal(r.ok, false);
	assert.match(r.error, /sync is running/);
	assert.deepEqual(host.calls.map((c) => c[0]), ["invoke"], "no event, no close");
});
