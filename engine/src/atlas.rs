//! Fleet-wide project atlas.
//!
//! `git_roots.json` knows where THIS machine keeps each repo; the atlas is
//! its fleet union, stored as one object (`meta/git_atlas.json`): identity ->
//! every root any machine has used, home-relative (`${HOME}/...`) where
//! possible so it translates across users and OSes. Machines feed these into
//! the Tokenizer as tokenize-only aliases: paths under ANOTHER machine's
//! clone then produce the same canonical `${GIT}` key instead of a literal
//! path key — which is what keeps one repo = one project across the fleet
//! and retires the pre-identity `${EHOME}-...` store keys for good.
//!
//! Union-only, no deletions: a stale alias can at worst tokenize a dead path
//! nobody uses; removing a live one would split projects again.

use std::collections::BTreeMap;

use crate::gitmap::GitMap;
use crate::scanner::hash_bytes;
use crate::store::{RemoteMeta, SyncStore};
use crate::tokenizer::HOME_TOKEN;

pub const KEY: &str = "meta/git_atlas.json";

pub type Atlas = BTreeMap<String, Vec<String>>;

/// Per-machine attribution: `machine -> identity -> that machine's root`.
///
/// The atlas above is a flat union of paths, which is all the tokenizer
/// needs — but it cannot answer "where does this project live on the
/// MacBook?", because nothing records WHICH machine contributed a path.
/// This is the same information keyed for people instead of for matching.
/// Kept as a separate object rather than folded into the atlas so older
/// versions keep reading the atlas unchanged.
pub const MACHINES_KEY: &str = "meta/project_machines.json";

pub type Machines = BTreeMap<String, BTreeMap<String, String>>;

/// Publish this machine's `identity -> root` table and return the fleet's.
///
/// Last-writer-wins PER MACHINE: each machine owns exactly its own key, so
/// concurrent syncs can't clobber each other's rows. A machine that has
/// forgotten a repo drops it from its own row, which is correct — that is
/// what "it no longer lives here" means.
pub fn sync_machines(store: &dyn SyncStore, map: &GitMap, home: &str, machine: &str) -> Machines {
    let (mut all, may_publish): (Machines, bool) = match store.get(MACHINES_KEY) {
        Ok(Some((b, _))) => match serde_json::from_slice(&b) {
            Ok(m) => (m, true),
            Err(_) => (Machines::default(), true),
        },
        Ok(None) => (Machines::default(), true),
        // A failed fetch must never republish over the fleet's copy.
        Err(e) => {
            crate::dlog::warn(|| format!("project machines: fetch failed ({e})"));
            return Machines::default();
        }
    };
    let home = home.trim_end_matches(['/', '\\']);
    let ci = home.as_bytes().get(1) == Some(&b':');
    let mine: BTreeMap<String, String> = map
        .roots
        .iter()
        .map(|(id, root)| {
            let rel = root
                .strip_prefix(home)
                .or_else(|| {
                    if ci && root.get(..home.len()).map(|h| h.eq_ignore_ascii_case(home)).unwrap_or(false) {
                        root.get(home.len()..)
                    } else {
                        None
                    }
                })
                .filter(|r| r.starts_with('/') || r.starts_with('\\'))
                .map(|r| format!("{HOME_TOKEN}{r}"))
                .unwrap_or_else(|| root.clone());
            (id.clone(), rel)
        })
        .collect();
    let changed = all.get(machine) != Some(&mine);
    all.insert(machine.to_string(), mine);
    if changed && may_publish {
        if let Ok(bytes) = serde_json::to_vec_pretty(&all) {
            let meta = RemoteMeta {
                hash: hash_bytes(&bytes),
                mtime_ms: 0,
                size: bytes.len() as u64,
                source: machine.to_string(),
            };
            if let Err(e) = store.put(MACHINES_KEY, &bytes, &meta) {
                crate::dlog::warn(|| format!("project machines: publish failed ({e})"));
            }
        }
    }
    all
}

/// Download-merge-publish: fold this machine's roots (and rename aliases)
/// into the store's atlas, home-tokenized, uploading only when something new
/// was added. Best-effort — a sync must never fail over atlas trouble.
pub fn sync_atlas(store: &dyn SyncStore, map: &GitMap, home: &str, machine: &str) -> Atlas {
    // An absent atlas is a fresh start; a FAILED fetch is not — republishing
    // over an unreadable store copy would wipe the fleet union. On failure,
    // use local roots for this sync and publish nothing.
    let (mut atlas, may_publish): (Atlas, bool) = match store.get(KEY) {
        Ok(Some((b, _))) => match serde_json::from_slice(&b) {
            Ok(a) => (a, true),
            Err(_) => {
                crate::dlog::warn(|| "project atlas: store copy unreadable — rebuilding from local roots".to_string());
                (Atlas::default(), true)
            }
        },
        Ok(None) => (Atlas::default(), true),
        Err(e) => {
            crate::dlog::warn(|| format!("project atlas: fetch failed ({e}) — using local roots only this sync"));
            (Atlas::default(), false)
        }
    };
    let home = home.trim_end_matches(['/', '\\']);
    // Windows drive letters vary in case in real fleet data.
    let ci = home.as_bytes().get(1) == Some(&b':');
    let mut changed = false;
    {
        let mut add = |id: &str, root: &str| {
            let stripped = root.strip_prefix(home).or_else(|| {
                if ci && root.get(..home.len()).map(|h| h.eq_ignore_ascii_case(home)).unwrap_or(false)
                {
                    root.get(home.len()..)
                } else {
                    None
                }
            });
            let rel = if home.is_empty() {
                root.to_string()
            } else {
                stripped
                    .filter(|r| r.starts_with('/') || r.starts_with('\\'))
                    .map(|r| format!("{HOME_TOKEN}{r}"))
                    .unwrap_or_else(|| root.to_string())
            };
            let v = atlas.entry(id.to_string()).or_default();
            if !v.iter().any(|x| x == &rel) {
                v.push(rel);
                v.sort();
                changed = true;
            }
        };
        for (id, root) in &map.roots {
            add(id, root);
        }
        for (id, olds) in &map.aliases {
            for r in olds {
                add(id, r);
            }
        }
    }
    if changed && may_publish {
        if let Ok(bytes) = serde_json::to_vec_pretty(&atlas) {
            let meta = RemoteMeta {
                hash: hash_bytes(&bytes),
                mtime_ms: 0,
                size: bytes.len() as u64,
                source: machine.to_string(),
            };
            match store.put(KEY, &bytes, &meta) {
                Ok(()) => {
                    crate::dlog::info(|| format!("project atlas: published {} identities", atlas.len()))
                }
                Err(e) => crate::dlog::warn(|| format!("project atlas: publish failed ({e})")),
            }
        }
    }
    atlas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FolderStore;

    #[test]
    fn atlas_unions_across_machines() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        const ID: &str = "github.com/o/r";

        let mut map_a = GitMap::default();
        map_a.roots.insert(ID.into(), "/Users/alice/Desktop/r".into());
        let atlas = sync_atlas(&store, &map_a, "/Users/alice", "a");
        assert_eq!(atlas[ID], vec!["${HOME}/Desktop/r".to_string()]);

        // Machine B adds its own location; A's stays.
        let mut map_b = GitMap::default();
        map_b.roots.insert(ID.into(), "/home/bob/dev/r".into());
        let atlas = sync_atlas(&store, &map_b, "/home/bob", "b");
        assert_eq!(atlas[ID], vec!["${HOME}/Desktop/r".to_string(), "${HOME}/dev/r".to_string()]);

        // Re-sync with nothing new: no growth, same content.
        let atlas2 = sync_atlas(&store, &map_b, "/home/bob", "b");
        assert_eq!(atlas, atlas2);

        // A root outside home stays raw (machine-specific, harmless elsewhere).
        let mut map_w = GitMap::default();
        map_w.roots.insert(ID.into(), "C:\\Temp\\r".into());
        let atlas = sync_atlas(&store, &map_w, "C:\\Users\\w", "w");
        assert!(atlas[ID].contains(&"C:\\Temp\\r".to_string()));
    }
}
