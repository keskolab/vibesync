//! Claude desktop registry entries (`local_*.json`) — the sidebar's source
//! of truth.
//!
//! HARD RULES, each learned from a real failure during Gate A testing:
//! - All timestamps are EPOCH-MILLISECONDS INTEGERS. One ISO-string
//!   timestamp made the app drop ALL sessions from the sidebar (it parses
//!   the registry as a batch and fails wholesale).
//! - `cliSessionId` must be unique across entries; a duplicate silently
//!   drops every colliding entry.
//! - Entries are compact single-line JSON, mode 0600.
//! - Unknown fields pass through untouched — the schema is undocumented
//!   and evolving (~30 fields observed).
//!
//! This module is the pure core: validation and merging. Writing into the
//! live registry directory happens in the app layer, which must validate
//! every entry with [`validate`] first.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::tokenizer::Tokenizer;

/// Timestamp-ish fields that must be integer millis when present.
const INT_TIMESTAMPS: &[&str] = &["lastActivityAt", "lastFocusedAt", "createdAt"];
/// Plain-path fields that get `${HOME}` tokenization for the store.
const PATH_FIELDS: &[&str] = &["cwd", "originCwd", "planPath"];

/// Validate an entry against everything the desktop app is known to require.
pub fn validate(entry: &Value) -> Result<()> {
    let obj = match entry.as_object() {
        Some(o) => o,
        None => bail!("registry entry is not a JSON object"),
    };
    let sid = obj
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing sessionId"))?;
    if !sid.starts_with("local_") || sid.len() < 7 {
        bail!("sessionId must look like local_<uuid>, got {sid:?}");
    }
    for key in INT_TIMESTAMPS {
        if let Some(v) = obj.get(*key) {
            if !v.is_i64() && !v.is_u64() {
                bail!("{key} must be an integer epoch-ms timestamp, got {v}");
            }
        }
    }
    if let Some(cli) = obj.get("cliSessionId") {
        if !cli.is_string() {
            bail!("cliSessionId must be a string");
        }
    }
    Ok(())
}

/// Tokenize machine-specific plain paths for storage; [`expand_paths`]
/// reverses it on the target machine.
pub fn tokenize_paths(entry: &mut Value, tok: &Tokenizer) {
    map_paths(entry, &|s| tok.tokenize_plain(s));
}

pub fn expand_paths(entry: &mut Value, tok: &Tokenizer) {
    map_paths(entry, &|s| tok.expand_plain(s));
}

/// True when any path field still holds a project token this machine
/// cannot expand — such an entry belongs to a project that isn't here yet
/// and must park (never apply, never count as a ghost, never be written).
pub fn has_unresolved_paths(entry: &Value) -> bool {
    PATH_FIELDS.iter().any(|k| {
        entry
            .get(*k)
            .and_then(Value::as_str)
            .map(crate::gitmap::has_unresolved_token)
            .unwrap_or(false)
    })
}

/// Snap an entry's machine-local paths to their canonical local form:
/// tokenize then expand through an alias-aware tokenizer, so a cwd recorded
/// under another machine's (or a pre-rename) clone path lands on THIS
/// machine's clone of the same project. Returns false (entry untouched) when
/// nothing changes or when a path would be left holding a token this machine
/// cannot expand — a live registry entry must never contain one.
pub fn canonicalize_entry(entry: &mut Value, tok: &Tokenizer) -> bool {
    let before = entry.clone();
    tokenize_paths(entry, tok);
    expand_paths(entry, tok);
    normalize_separators(entry);
    if has_unresolved_paths(entry) || *entry == before {
        *entry = before;
        return false;
    }
    true
}

/// Make each path field's separators self-consistent after cross-platform
/// expansion: a `${HOME}`-tokenized entry keeps the origin machine's
/// separators in its tail, so expanding on the other OS yields mixed
/// separators (`C:\Users\x/dev/proj`). Native Windows entries use `\` and
/// POSIX entries use `/`; the desktop app parses the registry unforgivingly,
/// so match the native shape exactly. Drive-letter paths get `\`, absolute
/// POSIX paths get `/`; anything else is left untouched.
pub fn normalize_separators(entry: &mut Value) {
    map_paths(entry, &|s| {
        let bytes = s.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            s.replace('/', "\\")
        } else if s.starts_with('/') {
            s.replace('\\', "/")
        } else {
            s.to_string()
        }
    });
}

fn map_paths(entry: &mut Value, f: &dyn Fn(&str) -> String) {
    if let Some(obj) = entry.as_object_mut() {
        for key in PATH_FIELDS {
            if let Some(Value::String(s)) = obj.get_mut(*key) {
                *s = f(s);
            }
        }
    }
}

fn ts(entry: &Value, key: &str) -> i64 {
    entry.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Field-level merge of two versions of the same entry.
/// The side with the newer `lastActivityAt` supplies scalar values;
/// `enabledMcpTools` unions key-wise; monotonic counters take the max;
/// unknown keys pass through from whichever side has them.
pub fn merge(local: &Value, remote: &Value) -> Value {
    let local_newer = ts(local, "lastActivityAt") >= ts(remote, "lastActivityAt");
    let (newer, older) = if local_newer { (local, remote) } else { (remote, local) };

    let mut out = newer.clone();
    let (Some(out_obj), Some(older_obj)) = (out.as_object_mut(), older.as_object()) else {
        return out;
    };

    // Union: keys only the older side has.
    for (k, v) in older_obj {
        out_obj.entry(k.clone()).or_insert_with(|| v.clone());
    }
    // Archive state is a PER-MACHINE choice, never merged from remote:
    // current Claude archives the ORIGINAL entry when it forks a synced
    // session on resume (live-hit 2026-07-25) — syncing that flag would
    // hide the untouched original on every other machine, and re-archive
    // an entry the user just unarchived. The local side always wins;
    // a machine's archive state changes only through its own app.
    for key in ["isArchived", "archivedAt"] {
        match local.get(key) {
            Some(v) => {
                out_obj.insert(key.to_string(), v.clone());
            }
            None => {
                out_obj.remove(key);
            }
        }
    }
    // Monotonic fields: take the max of both sides.
    for key in ["lastActivityAt", "lastFocusedAt", "completedTurns"] {
        let m = ts(local, key).max(ts(remote, key));
        if m > 0 {
            out_obj.insert(key.to_string(), Value::from(m));
        }
    }
    // enabledMcpTools: key-wise union, newer side wins conflicts (it's the
    // base object, so only fill in keys it lacks).
    if let (Some(Value::Object(out_tools)), Some(Value::Object(old_tools))) = (
        {
            let v = out_obj.get_mut("enabledMcpTools");
            v.map(|v| &mut *v)
        },
        older_obj.get("enabledMcpTools"),
    ) {
        for (k, v) in old_tools {
            out_tools.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(sid: &str, activity: i64) -> Value {
        json!({
            "sessionId": format!("local_{sid}"),
            "cliSessionId": sid,
            "cwd": "/Users/alice/dev/proj",
            "title": "T",
            "lastActivityAt": activity,
            "createdAt": 1,
            "isArchived": false,
        })
    }

    #[test]
    fn validate_accepts_healthy_entry() {
        assert!(validate(&entry("aaaa", 1000)).is_ok());
    }

    #[test]
    fn validate_rejects_iso_string_timestamps() {
        // The exact bug that blanked the real sidebar.
        let mut e = entry("aaaa", 1000);
        e["lastActivityAt"] = json!("2026-07-10T19:31:00.000Z");
        assert!(validate(&e).is_err());
    }

    #[test]
    fn validate_rejects_bad_session_id() {
        let mut e = entry("aaaa", 1000);
        e["sessionId"] = json!("aaaa");
        assert!(validate(&e).is_err());
    }

    #[test]
    fn merge_newer_wins_scalars_unions_rest() {
        let mut local = entry("aaaa", 2000);
        local["title"] = json!("newer title");
        local["enabledMcpTools"] = json!({"a": true});
        let mut remote = entry("aaaa", 1000);
        remote["title"] = json!("older title");
        remote["onlyRemote"] = json!(42);
        remote["enabledMcpTools"] = json!({"b": false});
        remote["completedTurns"] = json!(9);

        let m = merge(&local, &remote);
        assert_eq!(m["title"], "newer title");
        assert_eq!(m["onlyRemote"], 42); // union of unknown keys
        assert_eq!(m["enabledMcpTools"]["a"], true);
        assert_eq!(m["enabledMcpTools"]["b"], false);
        assert_eq!(m["completedTurns"], 9); // max wins
        assert_eq!(m["lastActivityAt"], 2000);
        assert!(validate(&m).is_ok());
    }

    #[test]
    fn normalize_separators_matches_native_shape() {
        let mut e = entry("aaaa", 1000);
        // Mac-tokenized tail expanded on Windows: mixed separators.
        e["cwd"] = json!("C:\\Users\\you/Development/7-rust/vibesync");
        // Windows-tokenized tail expanded on macOS.
        e["originCwd"] = json!("/Users/you\\Temp\\vibesync");
        normalize_separators(&mut e);
        assert_eq!(e["cwd"], "C:\\Users\\you\\Development\\7-rust\\vibesync");
        assert_eq!(e["originCwd"], "/Users/you/Temp/vibesync");
        // Relative/other paths untouched.
        e["planPath"] = json!("plans/foo.md");
        normalize_separators(&mut e);
        assert_eq!(e["planPath"], "plans/foo.md");
    }

    #[test]
    fn path_tokenize_roundtrip() {
        let tok = Tokenizer::new("/Users/alice");
        let mut e = entry("aaaa", 1000);
        tokenize_paths(&mut e, &tok);
        assert_eq!(e["cwd"], "${HOME}/dev/proj");
        let bob = Tokenizer::new("/home/bob");
        expand_paths(&mut e, &bob);
        assert_eq!(e["cwd"], "/home/bob/dev/proj");
    }

    /// Live catch (2026-07-25): Claude archives the ORIGINAL entry when it
    /// forks a synced session on resume. Archive state must never merge
    /// across machines — the untouched original would vanish everywhere.
    #[test]
    fn archive_state_is_machine_local() {
        // Remote is NEWER and archived (the machine where the fork
        // happened); local is untouched — local's visibility must survive.
        let mut remote = entry("aaaa", 2000);
        remote["isArchived"] = json!(true);
        remote["archivedAt"] = json!(1999);
        let mut local = entry("aaaa", 1000);
        local["isArchived"] = json!(false);
        let out = merge(&local, &remote);
        assert_eq!(out["isArchived"], false, "remote fork-archive must not hide local");
        assert!(out.get("archivedAt").is_none(), "archive timestamp is local-only too");
        // Newer scalars still merge normally.
        assert_eq!(ts(&out, "lastActivityAt"), 2000);

        // Reverse: the user archived locally; an active remote copy must
        // not resurrect the entry into the main list here.
        let mut local2 = entry("bbbb", 1000);
        local2["isArchived"] = json!(true);
        let remote2 = entry("bbbb", 2000);
        let out2 = merge(&local2, &remote2);
        assert_eq!(out2["isArchived"], true, "local archive choice sticks");

        // Local never had the key (older app): a remote archive flag must
        // not sneak in through the newer-side base.
        let mut local3 = entry("cccc", 1000);
        local3.as_object_mut().unwrap().remove("isArchived");
        let mut remote3 = entry("cccc", 2000);
        remote3["isArchived"] = json!(true);
        let out3 = merge(&local3, &remote3);
        assert!(out3.get("isArchived").is_none(), "no local opinion = no flag");
    }
}
