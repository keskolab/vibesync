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
    fn path_tokenize_roundtrip() {
        let tok = Tokenizer::new("/Users/alice");
        let mut e = entry("aaaa", 1000);
        tokenize_paths(&mut e, &tok);
        assert_eq!(e["cwd"], "${HOME}/dev/proj");
        let bob = Tokenizer::new("/home/bob");
        expand_paths(&mut e, &bob);
        assert_eq!(e["cwd"], "/home/bob/dev/proj");
    }
}
