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

	// Counts that stay readable and steady at any size: exact up to 99,999
	// ("12,345"), compact above ("123K", "1.2M") — a six- or seven-digit
	// raw number widened its card and read as noise. Pair with
	// `fmtCountTitle` for the exact value on hover.
	function fmtCount(n, locale) {
		const v = Number(n) || 0;
		if (Math.abs(v) < 100000) return v.toLocaleString(locale);
		return new Intl.NumberFormat(locale, {
			notation: "compact",
			maximumFractionDigits: Math.abs(v) < 1e6 ? 0 : 1,
		}).format(v);
	}
	const fmtCountTitle = (n, locale) => (Number(n) || 0).toLocaleString(locale);

	// Sync progress as a stable-width percentage (never two running counts,
	// whose width changed on almost every tick and jostled the button), with
	// the exact counts kept for a tooltip. Floors, so 100% means done.
	function progressView(done, total, locale) {
		const d = Math.max(0, Number(done) || 0);
		const t = Math.max(0, Number(total) || 0);
		if (t === 0) return { pct: null, title: "" };
		const pct = Math.min(100, Math.floor((d / t) * 100));
		return { pct, title: `${d.toLocaleString(locale)} of ${t.toLocaleString(locale)} files` };
	}

	// Call `fn` with the latest arguments at most once per `ms` — the first
	// call at once, later ones coalesced, and the last one never dropped (a
	// trailing call fires when the window closes). `now`/`schedule` are
	// injectable for tests.
	function makeThrottle(fn, ms, now = () => Date.now(), schedule = setTimeout) {
		let last = -Infinity;
		let pending = null;
		let timer = false;
		const fire = () => {
			timer = false;
			if (!pending) return;
			const args = pending;
			pending = null;
			last = now();
			fn(...args);
		};
		return (...args) => {
			pending = args;
			const wait = last + ms - now();
			if (wait <= 0 && !timer) {
				fire();
			} else if (!timer) {
				timer = true;
				schedule(fire, Math.max(0, wait));
			}
		};
	}

	g.VibeSyncUI = {
		esc,
		continueBlocked,
		makeLatest,
		makeTwoClick,
		applySwitch,
		finishSetup,
		fmtCount,
		fmtCountTitle,
		progressView,
		makeThrottle,
	};
})(globalThis);
