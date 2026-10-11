//! Git-origin project identity mapping.
//!
//! The same project lives at different absolute paths on different machines
//! (`C:\Temp\vibesync` here, `~/Development/7_rust/vibesync` there). `${HOME}`
//! tokenization only unifies home-relative layouts; the stable cross-machine
//! identity is the **normalized git origin URL**. Paths inside a repo with an
//! origin tokenize to `${GIT:<identity>}` (identity's `/` written as `:` so
//! the token stays a single path component), and each machine expands the
//! token to *its own* clone location.
//!
//! Like `${HOME}`, the token is canonical in the store: `tokenize(expand(x))
//! == x`, so every machine produces identical logical keys for the same
//! project and content merges instead of duplicating. A machine that has no
//! clone of the repo cannot expand the token; callers leave such items in the
//! store untouched (transcripts park, sidebar entries ghost-guard away) until
//! the repo appears and a later sync materializes them.
//!
//! Each machine learns `identity -> local repo root` from the cwds of its own
//! Claude sidebar entries and persists them (`git_roots.json` in the app data
//! dir). One root per identity: first mapping wins, replaced only if its path
//! disappears from disk. Only a full clone can be that root. A linked
//! worktree, a second clone of an already-mapped repo, and the home folder
//! itself (a dotfiles repo) are keyed like plain folders instead: the token
//! always expands to the ONE root, so anything else sharing it has its
//! sessions pulled into that root's project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `${GIT:github.com:owner:repo}` — identity with `/` swapped to `:`.
pub const GIT_TOKEN_PREFIX: &str = "${GIT:";
/// `${PROJ:name}` — user-chosen fleet-wide project name, configured per
/// machine ("this local folder is project <name>"). Manual mappings outrank
/// git identities: an explicit mapping is intent, and it covers repos with
/// no remote as well as folders that aren't repos at all.
pub const PROJ_TOKEN_PREFIX: &str = "${PROJ:";

/// How a literal `:` inside an identity is written in its token. Without an
/// escape it would read back as `/` and the token could never expand.
/// Identities are lowercase (see [`normalize_origin`]), so this uppercase
/// form can't collide with a percent-escape the remote URL itself carried —
/// and every identity without a `:` (all ordinary remotes) keeps its token
/// byte-for-byte.
const COLON_ESCAPE: &str = "%3A";

pub fn git_token(identity: &str) -> String {
    format!("{GIT_TOKEN_PREFIX}{}}}", identity.replace(':', COLON_ESCAPE).replace('/', ":"))
}

/// Split `${GIT:...}rest` into (identity, rest). None if not a git token.
pub fn parse_git_token(s: &str) -> Option<(String, &str)> {
    let inner = s.strip_prefix(GIT_TOKEN_PREFIX)?;
    let end = inner.find('}')?;
    Some((inner[..end].replace(':', "/").replace(COLON_ESCAPE, ":"), &inner[end + 1..]))
}

pub fn proj_token(name: &str) -> String {
    format!("{PROJ_TOKEN_PREFIX}{name}}}")
}

/// Split `${PROJ:name}rest` into (name, rest). None if not a project token.
pub fn parse_proj_token(s: &str) -> Option<(&str, &str)> {
    let inner = s.strip_prefix(PROJ_TOKEN_PREFIX)?;
    let end = inner.find('}')?;
    Some((&inner[..end], &inner[end + 1..]))
}

/// Project names live inside tokens and travel across machines: keep them to
/// a safe, unambiguous charset.
pub fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// True if the string still carries a token this machine could not expand
/// (repo not cloned / project not mapped here) — such paths must park, never
/// materialize literally on disk.
pub fn has_unresolved_token(s: &str) -> bool {
    s.contains(GIT_TOKEN_PREFIX) || s.contains(PROJ_TOKEN_PREFIX)
}

/// Normalize a git remote URL to a stable identity: `host/path`, lowercased,
/// credentials/ports/`.git` stripped. Returns None for URLs with no stable
/// host+path shape (e.g. local filesystem remotes).
pub fn normalize_origin(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        // scheme://[user@]host[:port]/path
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?, path)
    } else if let Some((left, path)) = url.split_once(':') {
        // scp-like [user@]host:path — a single letter before ':' is a
        // Windows drive, not a host.
        if left.len() <= 1 || path.starts_with('\\') || path.starts_with('/') {
            return None;
        }
        (left.split_once('@').map(|(_, h)| h).unwrap_or(left), path)
    } else {
        return None;
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    let id = format!("{host}/{path}").to_lowercase();
    Some(azure_devops_identity(&id).unwrap_or(id))
}

/// Azure DevOps serves one repo under URL shapes that differ in more than
/// the host, so the generic `host/path` gave ssh and https clones of the
/// same repo different identities:
///
/// - `https://[org@]dev.azure.com/org/proj/_git/repo`
/// - `git@ssh.dev.azure.com:v3/org/proj/repo`
/// - `https://org.visualstudio.com/[DefaultCollection/]proj/_git/repo`
/// - `org@vs-ssh.visualstudio.com:v3/org/proj/repo`
///
/// All map onto the modern https form's identity — the one https clones
/// have always produced, so their keys stay put. `_git/repo` with no
/// project is Azure's short form for a repo named like its project.
fn azure_devops_identity(id: &str) -> Option<String> {
    let (host, path) = id.split_once('/')?;
    let seg: Vec<&str> = path.split('/').collect();
    let (org, proj, repo) = match host {
        "ssh.dev.azure.com" | "vs-ssh.visualstudio.com" => match seg[..] {
            ["v3", org, proj, repo] => (org, proj, repo),
            _ => return None,
        },
        "dev.azure.com" => match seg[..] {
            [org, proj, "_git", repo] => (org, proj, repo),
            [org, "_git", repo] => (org, repo, repo),
            _ => return None,
        },
        _ => {
            let org = host.strip_suffix(".visualstudio.com")?;
            let seg = match seg[..] {
                ["defaultcollection", ref rest @ ..] => rest,
                ref all => all,
            };
            match *seg {
                [proj, "_git", repo] => (org, proj, repo),
                ["_git", repo] => (org, repo, repo),
                _ => return None,
            }
        }
    };
    Some(format!("dev.azure.com/{org}/{proj}/_git/{repo}"))
}

/// True when `dir` is the home folder or one of its ancestors. Neither is
/// ever a project root: a dotfiles repo at `~` would otherwise claim every
/// plain folder under home as `${GIT:me/dotfiles}/...` — and expand them
/// into whatever another machine's dotfiles root happens to be.
/// String-level so it also works on recorded, foreign-shaped paths.
pub(crate) fn covers_home(dir: &str, home: &str, case_insensitive: bool) -> bool {
    let norm = |p: &str| {
        let p = p.strip_prefix("\\\\?\\").unwrap_or(p).replace('\\', "/");
        let p = p.trim_end_matches('/');
        if case_insensitive {
            p.to_ascii_lowercase()
        } else {
            p.to_string()
        }
    };
    let (dir, home) = (norm(dir), norm(home));
    if home.is_empty() {
        return false;
    }
    // An empty `dir` is the filesystem root, an ancestor of every home.
    dir.is_empty() || home == dir || home.starts_with(&format!("{dir}/"))
}

fn covers_home_path(dir: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|h| covers_home(&dir.to_string_lossy(), &h.to_string_lossy(), cfg!(windows)))
}

/// Resolve `dir/.git` — a FILE (`gitdir: <path>`) — to the gitdir it names.
fn gitfile_dir(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join(".git")).ok()?;
    let gd = text.strip_prefix("gitdir:")?.trim();
    Some(if Path::new(gd).is_absolute() { PathBuf::from(gd) } else { dir.join(gd) })
}

/// `dir` is a linked worktree: `git worktree add` leaves a `.git` FILE whose
/// gitdir (`<main>/.git/worktrees/<name>`) has a `commondir` leading back to
/// the main clone. Submodules and `--separate-git-dir` clones use a `.git`
/// file too, but their gitdir has no `commondir` — they are repos of their
/// own.
pub fn is_linked_worktree(dir: &Path) -> bool {
    dir.join(".git").is_file() && gitfile_dir(dir).is_some_and(|gd| gd.join("commondir").is_file())
}

/// The main clone a linked worktree was added from, when that clone is
/// non-bare (its `.git` directory is the worktree's common dir).
fn worktree_main_clone(wt: &Path) -> Option<PathBuf> {
    let gd = gitfile_dir(wt)?;
    let c = std::fs::read_to_string(gd.join("commondir")).ok()?;
    let c = c.trim();
    let common = if Path::new(c).is_absolute() { PathBuf::from(c) } else { gd.join(c) };
    // `commondir` is usually relative (`../..`). Folded lexically, not
    // canonicalized: resolving symlinks (macOS `/var` -> `/private/var`)
    // would yield a spelling no tool ever records as a cwd.
    let mut folded = PathBuf::new();
    for comp in common.components() {
        match comp {
            std::path::Component::ParentDir => {
                folded.pop();
            }
            std::path::Component::CurDir => {}
            other => folded.push(other),
        }
    }
    if folded.file_name()? != ".git" {
        return None; // bare repo: no main working tree
    }
    folded.parent().map(Path::to_path_buf)
}

/// Walk up from `start` to the enclosing repo root and return
/// `(root, identity)` from its `remote.origin.url`, against the real home
/// folder — see [`discover_with_home`].
pub fn discover(start: &Path) -> Option<(PathBuf, String)> {
    discover_with_home(start, dirs::home_dir().as_deref())
}

/// [`discover`] against an explicit home folder.
///
/// Only a FULL clone is a root. A linked worktree is passed over and the
/// walk continues upward: recorded as its repo's root, it either became an
/// alias of the main clone (so its sessions expanded INTO the main clone)
/// or, learned first, the root the whole repo expanded into. A worktree
/// nested in its main clone (`<repo>/.claude/worktrees/x`) therefore
/// resolves to that clone and keys as a subfolder of it; one elsewhere keys
/// like a plain folder. A `.git` file without `commondir` (submodule,
/// separate gitdir) is that repo's own root. The walk stops at `home`:
/// home itself and its ancestors are never a root (see [`covers_home`]).
pub fn discover_with_home(start: &Path, home: Option<&Path>) -> Option<(PathBuf, String)> {
    let mut dir: Option<PathBuf> = Some(start.to_path_buf());
    while let Some(d) = dir {
        if covers_home_path(&d, home) {
            return None;
        }
        let dotgit = d.join(".git");
        if dotgit.is_dir() {
            return origin_identity(&dotgit.join("config")).map(|id| (d, id));
        }
        if dotgit.is_file() {
            let gd = gitfile_dir(&d)?;
            if !gd.join("commondir").is_file() {
                return origin_identity(&gd.join("config")).map(|id| (d, id));
            }
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// Minimal INI walk of a git config for `[remote "origin"] url = ...`.
fn origin_identity(config: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.eq_ignore_ascii_case("[remote \"origin\"]");
        } else if in_origin {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case("url") {
                    return normalize_origin(&config_value(v));
                }
            }
        }
    }
    None
}

/// A config value the way git reads it: `"..."` quoting with backslash
/// escapes, and `#`/`;` opening a comment outside quotes. Trailing
/// whitespace outside quotes is dropped. A plain unquoted URL — every
/// ordinary remote — passes through unchanged.
fn config_value(raw: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    // Whitespace outside quotes is kept only once something follows it.
    let mut gap = String::new();
    let mut chars = raw.trim_start().chars();
    while let Some(c) = chars.next() {
        match c {
            '#' | ';' if !quoted => break,
            c if c.is_whitespace() && !quoted => gap.push(c),
            '"' => {
                out.push_str(&std::mem::take(&mut gap));
                quoted = !quoted;
            }
            '\\' => {
                out.push_str(&std::mem::take(&mut gap));
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{8}'),
                    Some(other) => out.push(other),
                    None => {}
                }
            }
            c => {
                out.push_str(&std::mem::take(&mut gap));
                out.push(c);
            }
        }
    }
    out
}

/// Directory names never descended into when hunting for clones: dependency
/// and build trees are enormous, deep, and never hold a clone worked in.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    "vendor",
    "Pods",
    "DerivedData",
    "venv",
    "__pycache__",
    "Library",
];

/// Collect git clone ROOTS under `root`, at most `max_depth` levels down.
///
/// This is the seed a cold machine otherwise lacks. Clone discovery can only
/// probe locations the fleet already published or siblings of roots already
/// learned, so a clone somewhere no machine has seen — an external drive, an
/// unrelated tree — stays invisible until the user opens it in a tool. That
/// inverts the point of syncing: you want the history there BEFORE you start
/// working. Walking a folder the user declares as "where I keep code" makes
/// such a clone findable on the first sync instead.
///
/// A directory that is itself a clone root is collected and NOT descended
/// into — its contents are the big tree we most want to skip. A linked
/// worktree is skipped the same way but never collected: it is not a root
/// (see [`discover_with_home`]). A home folder that is itself a repo
/// (dotfiles) is not a root either, so a declared `~` is still walked.
/// Hidden directories, [`SKIP_DIRS`], and symlinks are passed over
/// (`DirEntry` file types don't follow links, so symlink loops can't occur).
pub fn find_clone_roots(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_for_clones(root, max_depth, dirs::home_dir().as_deref(), &mut out);
    out
}

fn walk_for_clones(dir: &Path, depth_left: usize, home: Option<&Path>, out: &mut Vec<PathBuf>) {
    if dir.join(".git").exists() && !covers_home_path(dir, home) {
        if !is_linked_worktree(dir) {
            out.push(dir.to_path_buf());
        }
        return;
    }
    if depth_left == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)) {
            continue;
        }
        walk_for_clones(&e.path(), depth_left - 1, home, out);
    }
}

/// identity -> this machine's repo root (absolute plain path).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GitMap {
    pub roots: BTreeMap<String, String>,
    /// Former roots (the folder was moved/renamed): old transcript dirs keep
    /// the old path baked into their names forever, so these stay recognized
    /// when TOKENIZING — history keeps its canonical `${GIT}` keys instead of
    /// re-keying and splitting into a second project. Never used to expand.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, Vec<String>>,
}

impl GitMap {
    /// Load the map, reporting whether the result can be TRUSTED as this
    /// machine's full picture.
    ///
    /// Returns `(map, trusted)`. `trusted` is false when the file exists but
    /// could not be read or parsed — a torn write, a bad disk, a partial
    /// restore. This distinction matters more than it looks: a transcript's
    /// logical key DEPENDS on this map (`${GIT:id}/...` with a root known,
    /// `${EHOME}-...` without it). Silently falling back to an empty map
    /// re-keys every file on the next scan, which `SyncState::mark_deletions`
    /// then reads as "the user deleted all of them" — and `deleted_locally`
    /// is one-way, so those sessions stop syncing to this machine forever.
    /// Callers must suppress deletion marking when this is false.
    ///
    /// A map written by an older build is brought in line with today's
    /// rules on the way in (see [`GitMap::repair_with_home`]); the repaired
    /// form is what callers see and what the next save writes.
    pub fn load(path: &Path) -> (Self, bool) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            // Absent is the normal first-run case, and it IS trustworthy:
            // nothing has been learned yet, so nothing can be lost.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), true),
            Err(e) => {
                crate::dlog::warn(|| {
                    format!("project map: {} unreadable ({e}) — deletion marking suppressed this sync", path.display())
                });
                return (Self::default(), false);
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut m) => {
                // Debug, not info: the file keeps its old form until the
                // next save, so this repeats on every load until then.
                if m.repair_with_home(dirs::home_dir().as_deref()) {
                    crate::dlog::debug(|| {
                        "project map: ignoring worktree / second-clone / home-folder roots \
                         recorded by an older version"
                            .to_string()
                    });
                }
                (m, true)
            }
            Err(e) => {
                crate::dlog::warn(|| {
                    format!("project map: {} is corrupt ({e}) — deletion marking suppressed this sync", path.display())
                });
                (Self::default(), false)
            }
        }
    }

    /// Atomic write: a torn `git_roots.json` costs far more than the rename
    /// (see [`GitMap::load`]), so never write the live path in place.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Learn the repo containing `cwd`. Returns true if the map changed.
    /// First mapping wins; a stored root replaced only once its path is gone
    /// — and the replaced root is kept as a tokenize-only alias so existing
    /// transcript dirs (named after the old path) keep their canonical keys.
    ///
    /// A SECOND living clone of an already-mapped repo also becomes a
    /// tokenize-only alias: its sessions join the repo's one identity, while
    /// expansion keeps targeting the primary clone. Linked worktrees and the
    /// home folder are never roots or aliases — see [`discover_with_home`].
    pub fn learn(&mut self, cwd: &Path) -> bool {
        self.learn_with_home(cwd, dirs::home_dir().as_deref())
    }

    /// [`GitMap::learn`] against an explicit home folder.
    pub fn learn_with_home(&mut self, cwd: &Path, home: Option<&Path>) -> bool {
        let Some((root, id)) = discover_with_home(cwd, home) else {
            crate::dlog::debug(|| {
                format!("project map: {} is not in a git repo with an origin", cwd.display())
            });
            return false;
        };
        // Extended-length Windows prefixes come in via tool-recorded cwds;
        // stored verbatim they never prefix-match a stripped input path.
        let root = root.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
        let root = root.strip_prefix("\\\\?\\").unwrap_or(&root).to_string();
        match self.roots.get(&id).cloned() {
            Some(existing) if existing == root => false,
            Some(existing) if usable_root(&existing, home) => {
                // A second full clone joins the repo's one identity as a
                // tokenize-only alias (expansion keeps targeting the
                // primary) — the deliberate design for a repo re-cloned to a
                // new place while the old checkout lingers, e.g. on a backup
                // drive. Worktrees never get here (see discover_with_home).
                let aliases = self.aliases.entry(id.clone()).or_default();
                if aliases.contains(&root) {
                    return false;
                }
                crate::dlog::info(|| {
                    format!("project map: second clone of {id} at {root} (tokenize-only alias)")
                });
                aliases.push(root);
                true
            }
            old => {
                if let Some(old_root) = old {
                    // Only a root that is GONE (moved, renamed) is worth
                    // remembering: its transcript dirs keep the old path in
                    // their names. A living one — a worktree or the home
                    // folder an older build accepted — would go on
                    // collapsing its own sessions into this root.
                    if !Path::new(&old_root).exists() {
                        self.add_alias(&id, old_root);
                    }
                }
                self.drop_alias(&id, &root);
                crate::dlog::info(|| format!("project map: learned {id} -> {root}"));
                self.roots.insert(id, root);
                true
            }
        }
    }

    /// Bring a map written by an older build in line with today's rules;
    /// returns whether anything changed. Those builds recorded every living
    /// second clone and worktree as a tokenize-only alias (and a worktree
    /// learned first as the root itself), and accepted a dotfiles repo at
    /// `~` as a root — each of which kept pulling sessions into the wrong
    /// project, and none of which `learn` would ever revisit. Called by
    /// [`GitMap::load`], so every caller gets the repaired map.
    ///
    /// Moved-away roots (aliases whose path is gone) are kept: that is what
    /// aliases are for.
    pub fn repair_with_home(&mut self, home: Option<&Path>) -> bool {
        let mut changed = false;
        let ids: std::collections::BTreeSet<String> =
            self.roots.keys().chain(self.aliases.keys()).cloned().collect();
        for id in ids {
            // The main clone a dropped worktree root stood in for.
            let mut main_clone: Option<PathBuf> = None;
            if let Some(root) = self.roots.get(&id).cloned() {
                let p = Path::new(&root);
                if covers_home_path(p, home) || is_linked_worktree(p) {
                    main_clone = worktree_main_clone(p);
                    self.roots.remove(&id);
                    changed = true;
                }
            }
            // Root gone (or just dropped): a living clone of the repo is
            // the natural successor — `learn` would have promoted it the
            // moment the old root disappeared, and promoting keeps its
            // sessions on the `${GIT}` key they already have. Recorded
            // aliases first (their spelling is the one tools record), then
            // the dropped worktree's own main clone.
            if !self.roots.get(&id).is_some_and(|r| Path::new(r).exists()) {
                let next = self
                    .aliases
                    .get(&id)
                    .and_then(|a| a.iter().find(|a| clone_root_of(Path::new(a), &id, home)).cloned())
                    .or_else(|| {
                        main_clone
                            .filter(|m| clone_root_of(m, &id, home))
                            .map(|m| m.to_string_lossy().into_owned())
                    });
                if let Some(next) = next {
                    if let Some(old) = self.roots.insert(id.clone(), next.clone()) {
                        if !Path::new(&old).exists() {
                            self.add_alias(&id, old);
                        }
                    }
                    changed = true;
                }
            }
            let root = self.roots.get(&id).cloned();
            if let Some(list) = self.aliases.get_mut(&id) {
                let before = list.len();
                // Anything still living as a checkout of its own (second
                // clone, worktree, another repo now) or covering home is
                // not a former root of this one.
                // A living second clone of THIS repo stays: that alias is by
                // design (see learn_with_home). A worktree, the home folder,
                // or a path that now holds a different repo goes.
                list.retain(|a| {
                    let p = Path::new(a);
                    Some(a) != root.as_ref()
                        && !covers_home_path(p, home)
                        && !is_linked_worktree(p)
                        && (!p.join(".git").exists() || clone_root_of(p, &id, home))
                });
                changed |= list.len() != before;
                if list.is_empty() {
                    self.aliases.remove(&id);
                }
            }
        }
        changed
    }

    fn add_alias(&mut self, id: &str, root: String) {
        let aliases = self.aliases.entry(id.to_string()).or_default();
        if !aliases.contains(&root) {
            aliases.push(root);
        }
    }

    /// Forget `root` as an alias of `id`; true if it was one.
    fn drop_alias(&mut self, id: &str, root: &str) -> bool {
        let Some(aliases) = self.aliases.get_mut(id) else { return false };
        let before = aliases.len();
        aliases.retain(|a| a != root);
        let changed = aliases.len() != before;
        if aliases.is_empty() {
            self.aliases.remove(id);
        }
        changed
    }
}

/// A stored root that still stands as this repo's one clone: present, a
/// full clone (not a worktree), and not the home folder.
fn usable_root(root: &str, home: Option<&Path>) -> bool {
    let p = Path::new(root);
    p.exists() && !is_linked_worktree(p) && !covers_home_path(p, home)
}

/// `dir` is itself the root of a full clone of `id`.
fn clone_root_of(dir: &Path, id: &str, home: Option<&Path>) -> bool {
    matches!(discover_with_home(dir, home), Some((root, found)) if found == id && root == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_variants_agree() {
        for url in [
            "git@github.com:JohnKesko/vibesync.git",
            "https://github.com/JohnKesko/vibesync.git",
            "https://user@github.com/JohnKesko/vibesync",
            "ssh://git@github.com:22/JohnKesko/vibesync.git",
            "https://github.com/JohnKesko/vibesync/",
        ] {
            assert_eq!(
                normalize_origin(url).as_deref(),
                Some("github.com/johnkesko/vibesync"),
                "{url}"
            );
        }
    }

    #[test]
    fn normalize_rejects_unstable() {
        assert_eq!(normalize_origin("C:\\repos\\bare.git"), None);
        assert_eq!(normalize_origin("/srv/git/repo.git"), None);
        assert_eq!(normalize_origin(""), None);
    }

    #[test]
    fn rename_keeps_old_root_as_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old-spot");
        std::fs::create_dir_all(old.join(".git")).unwrap();
        std::fs::write(
            old.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:owner/repo.git\n",
        )
        .unwrap();
        let mut map = GitMap::default();
        assert!(map.learn(&old));
        // "Move" the repo: old path disappears, new one appears.
        let new = tmp.path().join("new-spot");
        std::fs::rename(&old, &new).unwrap();
        assert!(map.learn(&new));
        let id = "github.com/owner/repo";
        assert_eq!(map.roots.get(id).unwrap(), &new.to_string_lossy().to_string());
        assert_eq!(map.aliases.get(id).unwrap(), &vec![old.to_string_lossy().to_string()]);
    }

    #[test]
    fn token_roundtrip() {
        let t = git_token("github.com/johnkesko/vibesync");
        assert_eq!(t, "${GIT:github.com:johnkesko:vibesync}");
        let with_tail = format!("{t}/app/src");
        let (id, rest) = parse_git_token(&with_tail).unwrap();
        assert_eq!(id, "github.com/johnkesko/vibesync");
        assert_eq!(rest, "/app/src");
    }

    #[test]
    fn discover_and_learn() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            repo.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:Owner/Proj.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .unwrap();
        let deep = repo.join("app").join("src");
        std::fs::create_dir_all(&deep).unwrap();

        let (root, id) = discover(&deep).unwrap();
        assert_eq!(root, repo);
        assert_eq!(id, "github.com/owner/proj");

        let mut map = GitMap::default();
        assert!(map.learn(&deep));
        assert!(!map.learn(&deep)); // already known, root still exists
        assert_eq!(map.roots["github.com/owner/proj"], repo.to_string_lossy().trim_end_matches(['/', '\\']));
    }

    #[test]
    fn second_clone_is_a_tokenize_only_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let r = tmp.path().join(name);
            std::fs::create_dir_all(r.join(".git")).unwrap();
            std::fs::write(
                r.join(".git/config"),
                "[remote \"origin\"]\n\turl = git@github.com:o/r.git\n",
            )
            .unwrap();
            r
        };
        let first = mk("first-clone");
        let second = mk("second-clone");
        let mut map = GitMap::default();
        assert!(map.learn(&first));
        // The living primary stays; the second clone joins the identity as
        // a tokenize-only alias (deliberate: a re-cloned repo whose old
        // checkout lingers keeps its sessions in the one project).
        assert!(map.learn(&second));
        assert!(!map.learn(&second), "already an alias");
        let id = "github.com/o/r";
        assert_eq!(map.roots[id], first.to_string_lossy().to_string());
        assert_eq!(map.aliases[id], vec![second.to_string_lossy().to_string()]);

        let t = crate::tokenizer::Tokenizer::with_case_sensitivity(
            &tmp.path().to_string_lossy(),
            false,
        )
        .with_gitmap(&map);
        assert_eq!(t.tokenize_plain(&first.join("x").to_string_lossy()), "${GIT:github.com:o:r}/x");
        let second_key = t.tokenize_plain(&second.join("x").to_string_lossy());
        assert_eq!(second_key, "${GIT:github.com:o:r}/x");
        assert_eq!(std::path::Path::new(&t.expand_plain(&second_key)), first.join("x").as_path());
    }

    #[test]
    fn relearning_an_aliased_second_clone_keeps_the_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        mk_repo(&first);
        mk_repo(&second);
        let mut map = GitMap::default();
        map.roots.insert("github.com/o/r".into(), first.to_string_lossy().into_owned());
        map.aliases.insert("github.com/o/r".into(), vec![second.to_string_lossy().into_owned()]);
        assert!(!map.learn(&second));
        assert_eq!(map.aliases["github.com/o/r"], vec![second.to_string_lossy().into_owned()]);
        // Loading keeps it too: a living second clone of the same repo is
        // not something the repair removes.
        let path = tmp.path().join("git_roots.json");
        map.save(&path).unwrap();
        let (loaded, _) = GitMap::load(&path);
        assert_eq!(loaded.aliases, map.aliases);
    }

    #[test]
    fn a_root_replaced_by_a_living_worktree_is_not_kept_as_alias() {
        // A worktree learned by an older build as the primary is replaced by
        // the real clone; the living worktree must not become an alias.
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        mk_repo(&main);
        let gd = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::write(gd.join("commondir"), "../..\n").unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
        assert!(is_linked_worktree(&wt));
        assert!(!is_linked_worktree(&main));
        assert_eq!(worktree_main_clone(&wt).as_deref(), Some(main.as_path()));

        let mut map = GitMap::default();
        map.roots.insert("github.com/o/r".into(), wt.to_string_lossy().into_owned());
        assert!(map.learn(&main));
        assert_eq!(map.roots["github.com/o/r"], main.to_string_lossy());
        assert!(map.aliases.is_empty());
    }

    #[test]
    fn covers_home_is_boundary_aware() {
        assert!(covers_home("/Users/me", "/Users/me", false));
        assert!(covers_home("/Users/me/", "/Users/me", false));
        assert!(covers_home("/Users", "/Users/me", false));
        assert!(covers_home("/", "/Users/me", false));
        assert!(!covers_home("/Users/me/dev", "/Users/me", false));
        assert!(!covers_home("/Users/m", "/Users/me", false));
        assert!(!covers_home("/Users/Me", "/Users/me", false));
        assert!(covers_home("c:\\users\\me", "C:\\Users\\me", true));
        assert!(covers_home("C:\\", "C:\\Users\\me", true));
        assert!(!covers_home("D:\\code", "C:\\Users\\me", true));
    }

    #[test]
    fn a_dotfiles_home_is_walked_not_collected() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        mk_repo(&home);
        let proj = home.join("dev").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        mk_repo(&proj);
        let mut out = Vec::new();
        walk_for_clones(&home, 4, Some(&home), &mut out);
        assert_eq!(out, vec![proj]);
    }

    #[test]
    fn config_values_follow_git_quoting() {
        assert_eq!(config_value(" git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(config_value(" \"https://h/o/r\" "), "https://h/o/r");
        assert_eq!(config_value(" https://h/o/r # comment"), "https://h/o/r");
        assert_eq!(config_value(" \"https://h/a#b\""), "https://h/a#b");
        assert_eq!(config_value(" \"C:\\\\repos\\\\x\""), "C:\\repos\\x");
    }

    #[test]
    fn absent_map_is_trusted_corrupt_map_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("git_roots.json");

        // First run: nothing learned yet, so an empty map IS the truth.
        let (map, trusted) = GitMap::load(&path);
        assert!(trusted);
        assert!(map.roots.is_empty());

        let mut m = GitMap::default();
        m.roots.insert("github.com/o/r".into(), "/repos/r".into());
        m.save(&path).unwrap();
        let (loaded, trusted) = GitMap::load(&path);
        assert!(trusted);
        assert_eq!(loaded.roots["github.com/o/r"], "/repos/r");

        // A torn write must NOT read as "this machine knows no repos" — that
        // silently re-keys every ${GIT} path and reads as a mass deletion.
        std::fs::write(&path, b"{\"roots\":{\"github.com/o/r\"").unwrap();
        let (map, trusted) = GitMap::load(&path);
        assert!(!trusted);
        assert!(map.roots.is_empty());
    }

    #[test]
    fn save_leaves_no_partial_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("git_roots.json");
        let mut m = GitMap::default();
        m.roots.insert("github.com/o/r".into(), "/repos/r".into());
        m.save(&path).unwrap();
        m.roots.insert("github.com/o/r2".into(), "/repos/r2".into());
        m.save(&path).unwrap();
        // The temp sibling must not survive a successful save.
        assert!(!path.with_extension("tmp").exists());
        let (loaded, trusted) = GitMap::load(&path);
        assert!(trusted);
        assert_eq!(loaded.roots.len(), 2);
    }

    fn mk_repo(at: &Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
        std::fs::write(at.join(".git/config"), "[remote \"origin\"]\n\turl = git@github.com:o/r.git\n")
            .unwrap();
    }

    #[test]
    fn finds_nested_clones_and_stops_at_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code");
        // The real shape: <code>/<language>/<project>, two levels down.
        let proj = code.join("7_rust").join("vibesync");
        std::fs::create_dir_all(&proj).unwrap();
        mk_repo(&proj);
        // A submodule-ish nested repo must NOT be reported separately: the
        // walk stops at the outer clone root.
        let nested = proj.join("vendor_lib");
        std::fs::create_dir_all(&nested).unwrap();
        mk_repo(&nested);

        let found = find_clone_roots(&code, 4);
        assert_eq!(found, vec![proj]);
    }

    #[test]
    fn skips_dependency_trees_and_hidden_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code");
        for junk in ["node_modules", "target", ".cache"] {
            let p = code.join(junk).join("pkg");
            std::fs::create_dir_all(&p).unwrap();
            mk_repo(&p);
        }
        let real = code.join("real");
        std::fs::create_dir_all(&real).unwrap();
        mk_repo(&real);

        let found = find_clone_roots(&code, 4);
        assert_eq!(found, vec![real]);
    }

    #[test]
    fn respects_the_depth_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code");
        let deep = code.join("a").join("b").join("c").join("proj");
        std::fs::create_dir_all(&deep).unwrap();
        mk_repo(&deep);

        assert!(find_clone_roots(&code, 2).is_empty(), "too deep for the bound");
        assert_eq!(find_clone_roots(&code, 4), vec![deep]);
    }

    #[test]
    fn a_declared_folder_that_is_itself_a_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("just-one-repo");
        std::fs::create_dir_all(&repo).unwrap();
        mk_repo(&repo);
        assert_eq!(find_clone_roots(&repo, 4), vec![repo]);
    }

    #[test]
    fn declared_folder_unlocks_a_clone_no_machine_has_seen() {
        // End to end: the Bug 4 case. A repo cloned somewhere the fleet has
        // never published, never opened in a tool here — learn() has no way
        // to reach it, but a declared code folder does.
        let tmp = tempfile::tempdir().unwrap();
        let drive = tmp.path().join("Volumes").join("Backup").join("_DEV");
        let proj = drive.join("7_rust").join("vibesync");
        std::fs::create_dir_all(&proj).unwrap();
        mk_repo(&proj);

        let mut map = GitMap::default();
        assert!(map.roots.is_empty());
        for c in find_clone_roots(&drive, 4) {
            if let Some((root, _)) = discover(&c) {
                if root == c {
                    map.learn(&c);
                }
            }
        }
        assert_eq!(map.roots["github.com/o/r"], proj.to_string_lossy());
    }

    #[test]
    fn no_repo_learns_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut map = GitMap::default();
        assert!(!map.learn(tmp.path()));
        assert!(map.roots.is_empty());
    }
}
