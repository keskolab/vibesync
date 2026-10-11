// VibeSync UI decisions that don't need a DOM: loaded by both pages before
// their own script, and imported by the Node tests in tests/. Each piece
// here exists because getting it wrong once shipped a real bug.

(function (g) {
	// Anything not written in this repo — folder paths, project and machine
	// names, error text — goes through this before it reaches innerHTML.
	// macOS allows `<`, `>`, `"` and `=` in folder names, and with the Tauri
	// API on the page a crafted name could reconfigure the app.
	const esc = (s) =>
		String(s).replace(
			/[&<>"']/g,
			(c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
		);

	// Onboarding: is Continue (or Finish) blocked on this step?
	//
	// Step 3 needs a complete storage config. Step 4 (cloud storage only)
	// needs a valid passphrase AND a verdict for exactly that passphrase:
	// while a check is pending or running, Continue stays off — clicking
	// past the check is how a wrong passphrase gets committed. A mismatch
	// blocks until the user deliberately overrides it; settings the storage
	// itself rejected always block.
	function continueBlocked(s) {
		if (s.finishing) return true;
		if (s.step === 3 && !s.storeOk) return true;
		if (s.step === 4 && s.needsPassphrase) {
			if (!s.passphraseValid || s.checking) return true;
			if (s.checked === "invalid") return true;
			if (s.checked === "mismatch" && !s.override) return true;
		}
		return false;
	}

	// Latest-wins tickets for async checks: only the newest ticket may act
	// on its result. A check still running from an earlier keystroke — or
	// from before the user went Back and picked other storage — must not
	// land its verdict on the current one.
	function makeLatest() {
		let seq = 0;
		return {
			next: () => ++seq,
			isCurrent: (ticket) => ticket === seq,
			invalidate: () => {
				seq++;
			},
		};
	}

	// A deliberate two-step confirmation without window.confirm (not
	// reliable in every webview): the first activation arms, the second
	// confirms.
	function makeTwoClick(onConfirm) {
		let armed = false;
		return () => {
			if (!armed) {
				armed = true;
				return "armed";
			}
			onConfirm();
			return "confirmed";
		};
	}

	// A settings switch whose write fails flips back, so it never shows a
	// state the backend didn't take.
	async function applySwitch(input, call, { onOk, onError } = {}) {
		try {
			const result = await call();
			onOk?.(result);
			return true;
		} catch (e) {
			input.checked = !input.checked;
			onError?.(e);
			return false;
		}
	}

	// Finish the setup assistant: save the storage FIRST and report back
	// only once that worked. Closing first left a failed save to be found
	// by the popover, where the next status refresh painted over it.
	async function finishSetup({ invoke, emit, close, store, passphrase, disabledTools }) {
		try {
			await invoke("set_store", { store, passphrase });
		} catch (e) {
			return { ok: false, error: String(e) };
		}
		emit("setup-complete", { storeSaved: true, disabledTools });
		close();
		return { ok: true };
	}

	g.VibeSyncUI = { esc, continueBlocked, makeLatest, makeTwoClick, applySwitch, finishSetup };
})(globalThis);
