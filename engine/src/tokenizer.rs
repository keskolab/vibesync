//! Machine-specific path tokenization.
//!
//! Two forms occur in tool storage:
//! - Plain paths (`/Users/alice/dev/proj`) — tokenized to `${HOME}/dev/proj`.
//! - Encoded directory names as used by Claude Code's `projects/` layout,
//!   where every non-alphanumeric character of the cwd becomes `-`
//!   (`-Users-alice-dev-proj`) — tokenized to `${EHOME}-dev-proj`.
//!
//! Both replacements are boundary-aware so `/Users/al` never matches inside
//! `/Users/alice`.

pub const HOME_TOKEN: &str = "${HOME}";
pub const EHOME_TOKEN: &str = "${EHOME}";

use crate::gitmap::{git_token, parse_git_token, parse_proj_token, proj_token, GitMap};

#[derive(Debug, Clone)]
pub struct Tokenizer {
    home: String,
    encoded_home: String,
    /// Windows paths (and Claude's encodings of them) vary in drive-letter
    /// case (`c--Github` vs `C--Users-...` observed on one machine), so
    /// prefix matching is case-insensitive there.
    case_insensitive: bool,
    /// Known git repos: (identity, plain root, encoded root), longest root
    /// first so nested clones match innermost. Git roots outrank `${HOME}` —
    /// a repo identity is portable across machines whose homes differ AND
    /// whose repo locations differ, which `${HOME}` alone can't express.
    /// Fourth field: usable for expansion (current root) vs tokenize-only
    /// alias (a former root after a folder move/rename).
    git_roots: Vec<(String, String, String, bool)>,
    /// User-configured project mappings: (name, plain root, encoded root),
    /// longest root first. Outrank git roots — an explicit mapping is intent.
    proj_roots: Vec<(String, String, String)>,
}

/// Claude Code's cwd encoding: every non-alphanumeric byte becomes '-'.
pub fn encode_cwd(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

impl Tokenizer {
    pub fn new(home: &str) -> Self {
        Self::with_case_sensitivity(home, cfg!(windows))
    }

    pub fn with_case_sensitivity(home: &str, case_insensitive: bool) -> Self {
        let home = home.trim_end_matches(['/', '\\']).to_string();
        let encoded_home = encode_cwd(&home);
        Self { home, encoded_home, case_insensitive, git_roots: Vec::new(), proj_roots: Vec::new() }
    }

    /// Attach user-configured project mappings (fleet name -> local folder);
    /// they take priority over git identities and `${HOME}` in both
    /// directions. Invalid names are skipped.
    pub fn with_manual_projects(
        mut self,
        map: &std::collections::BTreeMap<String, String>,
    ) -> Self {
        self.proj_roots = map
            .iter()
            .filter(|(name, _)| crate::gitmap::valid_project_name(name))
            .map(|(name, root)| {
                let root = root.trim_end_matches(['/', '\\']).to_string();
                let encoded = encode_cwd(&root);
                (name.clone(), root, encoded)
            })
            .collect();
        self.proj_roots.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        self
    }

    /// Attach known git repo roots; their identities then take priority over
    /// `${HOME}` in both tokenize directions.
    pub fn with_gitmap(mut self, map: &GitMap) -> Self {
        self.git_roots = map
            .roots
            .iter()
            .map(|(id, root)| (id, root, true))
            .chain(
                map.aliases
                    .iter()
                    .flat_map(|(id, olds)| olds.iter().map(move |r| (id, r, false))),
            )
            .map(|(id, root, current)| {
                let root = root.trim_end_matches(['/', '\\']).to_string();
                let encoded = encode_cwd(&root);
                (id.clone(), root, encoded, current)
            })
            .collect();
        // Longest plain root first (encode_cwd is length-preserving, so this
        // orders the encoded forms identically).
        self.git_roots.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        self
    }

    fn strip<'a>(&self, s: &'a str, prefix: &str) -> Option<&'a str> {
        if let Some(rest) = s.strip_prefix(prefix) {
            return Some(rest);
        }
        if self.case_insensitive {
            if let Some(head) = s.get(..prefix.len()) {
                if head.eq_ignore_ascii_case(prefix) {
                    return Some(&s[prefix.len()..]);
                }
            }
        }
        None
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        Ok(Self::new(&home.to_string_lossy()))
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    /// `C:\Temp\vibesync\app` → `${GIT:github.com:owner:repo}/app` when inside
    /// a known repo (tail separators canonicalized to `/`), else
    /// `/Users/alice/x` → `${HOME}/x` (boundary-aware; non-matching unchanged).
    pub fn tokenize_plain(&self, path: &str) -> String {
        for (name, root, _) in &self.proj_roots {
            if let Some(rest) = self.strip(path, root) {
                if rest.is_empty() {
                    return proj_token(name);
                }
                if rest.starts_with('/') || rest.starts_with('\\') {
                    return format!("{}{}", proj_token(name), rest.replace('\\', "/"));
                }
            }
        }
        for (id, root, _, _) in &self.git_roots {
            if let Some(rest) = self.strip(path, root) {
                if rest.is_empty() {
                    return git_token(id);
                }
                if rest.starts_with('/') || rest.starts_with('\\') {
                    return format!("{}{}", git_token(id), rest.replace('\\', "/"));
                }
            }
        }
        if let Some(rest) = self.strip(path, &self.home) {
            if rest.is_empty() {
                return HOME_TOKEN.to_string();
            }
            if rest.starts_with('/') || rest.starts_with('\\') {
                return format!("{HOME_TOKEN}{rest}");
            }
        }
        path.to_string()
    }

    /// `${GIT:...}/x` → this machine's clone + `/x` (token left untouched when
    /// the repo is unknown here — callers park/skip such paths);
    /// `${HOME}/x` → `/Users/alice/x`.
    pub fn expand_plain(&self, path: &str) -> String {
        if let Some((name, rest)) = parse_proj_token(path) {
            if let Some((_, root, _)) = self.proj_roots.iter().find(|(n, _, _)| n == name) {
                return format!("{root}{rest}");
            }
            return path.to_string();
        }
        if let Some((id, rest)) = parse_git_token(path) {
            if let Some((_, root, _, _)) = self.git_roots.iter().find(|(i, _, _, cur)| *i == id && *cur) {
                return format!("{root}{rest}");
            }
            return path.to_string();
        }
        match path.strip_prefix(HOME_TOKEN) {
            Some(rest) => format!("{}{}", self.home, rest),
            None => path.to_string(),
        }
    }

    /// `C--Temp-vibesync-app` → `${GIT:...}-app` when the component starts
    /// with a known repo root's encoding, else
    /// `-Users-alice-dev-proj` → `${EHOME}-dev-proj` (boundary-aware).
    pub fn tokenize_encoded(&self, s: &str) -> String {
        for (name, _, eroot) in &self.proj_roots {
            if let Some(rest) = self.strip(s, eroot) {
                if rest.is_empty() {
                    return proj_token(name);
                }
                if rest.starts_with('-') {
                    return format!("{}{}", proj_token(name), rest);
                }
            }
        }
        for (id, _, eroot, _) in &self.git_roots {
            if let Some(rest) = self.strip(s, eroot) {
                if rest.is_empty() {
                    return git_token(id);
                }
                if rest.starts_with('-') {
                    return format!("{}{}", git_token(id), rest);
                }
            }
        }
        if let Some(rest) = self.strip(s, &self.encoded_home) {
            if rest.is_empty() {
                return EHOME_TOKEN.to_string();
            }
            if rest.starts_with('-') {
                return format!("{EHOME_TOKEN}{rest}");
            }
        }
        s.to_string()
    }

    /// `${GIT:...}-app` → this machine's encoded clone root + `-app` (token
    /// left untouched when the repo is unknown here);
    /// `${EHOME}-dev-proj` → `-Users-alice-dev-proj`.
    pub fn expand_encoded(&self, s: &str) -> String {
        if let Some((name, rest)) = parse_proj_token(s) {
            if let Some((_, _, eroot)) = self.proj_roots.iter().find(|(n, _, _)| n == name) {
                return format!("{eroot}{rest}");
            }
            return s.to_string();
        }
        if let Some((id, rest)) = parse_git_token(s) {
            if let Some((_, _, eroot, _)) = self.git_roots.iter().find(|(i, _, _, cur)| *i == id && *cur) {
                return format!("{eroot}{rest}");
            }
            return s.to_string();
        }
        match s.strip_prefix(EHOME_TOKEN) {
            Some(rest) => format!("{}{}", self.encoded_home, rest),
            None => s.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_roundtrip() {
        let t = Tokenizer::new("/Users/alice");
        let tok = t.tokenize_plain("/Users/alice/dev/proj");
        assert_eq!(tok, "${HOME}/dev/proj");
        assert_eq!(t.expand_plain(&tok), "/Users/alice/dev/proj");
    }

    #[test]
    fn plain_boundary_not_matched() {
        let t = Tokenizer::new("/Users/al");
        assert_eq!(t.tokenize_plain("/Users/alice/x"), "/Users/alice/x");
    }

    #[test]
    fn plain_exact_home() {
        let t = Tokenizer::new("/Users/alice/");
        assert_eq!(t.tokenize_plain("/Users/alice"), "${HOME}");
        assert_eq!(t.expand_plain("${HOME}"), "/Users/alice");
    }

    #[test]
    fn encoded_roundtrip() {
        let t = Tokenizer::new("/Users/alice");
        let enc = "-Users-alice-Development-8-swift-Minimal-Todo";
        let tok = t.tokenize_encoded(enc);
        assert_eq!(tok, "${EHOME}-Development-8-swift-Minimal-Todo");
        assert_eq!(t.expand_encoded(&tok), enc);
    }

    #[test]
    fn encoded_boundary_not_matched() {
        let t = Tokenizer::new("/Users/al");
        // encoded home "-Users-al" must not match inside "-Users-alice-x"...
        // it does share the prefix, but the boundary char check requires the
        // next char to be '-'; "-Users-alice-x" continues with 'i'.
        assert_eq!(t.tokenize_encoded("-Users-alice-x"), "-Users-alice-x");
    }

    #[test]
    fn encode_cwd_non_alnum() {
        assert_eq!(
            encode_cwd("/Users/björn/My Proj"),
            "-Users-bj-rn-My-Proj"
        );
    }

    #[test]
    fn windows_drive_letter_case_is_ignored() {
        // Observed on a real Windows machine: `c--Github` and `C--Users-...`
        // in the same projects dir. Prefix matching must tolerate case.
        let t = Tokenizer::with_case_sensitivity("C:\\Users\\you", true);
        assert_eq!(
            t.tokenize_encoded("c--Users-you-dev-proj"),
            "${EHOME}-dev-proj"
        );
        assert_eq!(
            t.tokenize_plain("c:\\Users\\you\\dev\\proj"),
            "${HOME}\\dev\\proj"
        );
        // Expansion always emits this machine's canonical casing.
        assert_eq!(
            t.expand_encoded("${EHOME}-dev-proj"),
            "C--Users-you-dev-proj"
        );
    }

    #[test]
    fn case_sensitive_by_default_on_unix() {
        let t = Tokenizer::with_case_sensitivity("/Users/alice", false);
        assert_eq!(t.tokenize_plain("/users/alice/x"), "/users/alice/x");
    }

    #[test]
    fn cross_machine_remap() {
        let a = Tokenizer::new("/Users/alice");
        let b = Tokenizer::new("/home/bob");
        let logical = a.tokenize_encoded("-Users-alice-dev-proj");
        assert_eq!(b.expand_encoded(&logical), "-home-bob-dev-proj");
    }

    /// The real fleet shape: Windows clone outside home, Mac clone inside it.
    fn fleet() -> (Tokenizer, Tokenizer) {
        const ID: &str = "github.com/johnkesko/vibesync";
        let mut w_map = crate::gitmap::GitMap::default();
        w_map.roots.insert(ID.into(), "C:\\Temp\\vibesync".into());
        let mut m_map = crate::gitmap::GitMap::default();
        m_map.roots.insert(ID.into(), "/Users/you/Development/7_rust/vibesync".into());
        let w = Tokenizer::with_case_sensitivity("C:\\Users\\you", true).with_gitmap(&w_map);
        let m = Tokenizer::with_case_sensitivity("/Users/you", false).with_gitmap(&m_map);
        (w, m)
    }

    #[test]
    fn git_plain_cross_machine_and_canonical() {
        let (w, m) = fleet();
        // Windows session cwd -> canonical token -> Mac local path.
        let tok = w.tokenize_plain("C:\\Temp\\vibesync");
        assert_eq!(tok, "${GIT:github.com:johnkesko:vibesync}");
        assert_eq!(m.expand_plain(&tok), "/Users/you/Development/7_rust/vibesync");
        // Subdirectory tails survive with canonical separators.
        let tok = w.tokenize_plain("C:\\Temp\\vibesync\\app\\src-tauri");
        assert_eq!(tok, "${GIT:github.com:johnkesko:vibesync}/app/src-tauri");
        // tokenize(expand(x)) == x on both machines — the no-duplication law.
        assert_eq!(w.tokenize_plain(&w.expand_plain(&tok)), tok);
        assert_eq!(m.tokenize_plain(&m.expand_plain(&tok)), tok);
        // Mac side tokenizes to the SAME key even though its clone is under home.
        assert_eq!(
            m.tokenize_plain("/Users/you/Development/7_rust/vibesync/app/src-tauri"),
            tok
        );
    }

    #[test]
    fn git_encoded_cross_machine() {
        let (w, m) = fleet();
        let tok = w.tokenize_encoded("C--Temp-vibesync");
        assert_eq!(tok, "${GIT:github.com:johnkesko:vibesync}");
        assert_eq!(
            m.expand_encoded(&tok),
            "-Users-you-Development-7-rust-vibesync"
        );
        // Boundary: a sibling dir sharing the prefix must not match.
        assert_eq!(w.tokenize_encoded("C--Temp-vibesyncX"), "C--Temp-vibesyncX");
        // Encoded subdir tail.
        let tok = m.tokenize_encoded("-Users-you-Development-7-rust-vibesync-app");
        assert_eq!(tok, "${GIT:github.com:johnkesko:vibesync}-app");
        assert_eq!(w.expand_encoded(&tok), "C--Temp-vibesync-app");
    }

    #[test]
    fn moved_repo_old_dirs_keep_canonical_keys() {
        let mut map = crate::gitmap::GitMap::default();
        map.roots.insert("github.com/o/r".into(), "/Users/u/new-spot".into());
        map.aliases.insert("github.com/o/r".into(), vec!["/Users/u/old-spot".into()]);
        let t = Tokenizer::with_case_sensitivity("/Users/u", false).with_gitmap(&map);
        // Old transcript dirs (named after the pre-move path) still produce
        // the canonical token...
        assert_eq!(t.tokenize_plain("/Users/u/old-spot/x"), "${GIT:github.com:o:r}/x");
        assert_eq!(t.tokenize_encoded("-Users-u-old-spot-x"), "${GIT:github.com:o:r}-x");
        // ...while expansion always targets the current location.
        assert_eq!(t.expand_plain("${GIT:github.com:o:r}/x"), "/Users/u/new-spot/x");
        assert_eq!(t.expand_encoded("${GIT:github.com:o:r}-x"), "-Users-u-new-spot-x");
    }

    #[test]
    fn git_token_unresolvable_passes_through() {
        let (w, _) = fleet();
        let foreign = "${GIT:github.com:someone:other-repo}/x";
        assert_eq!(w.expand_plain(foreign), foreign);
        let foreign_enc = "${GIT:github.com:someone:other-repo}-x";
        assert_eq!(w.expand_encoded(foreign_enc), foreign_enc);
    }

    #[test]
    fn manual_project_outranks_git_and_home() {
        let (w, m) = fleet();
        let mut manual_w = std::collections::BTreeMap::new();
        manual_w.insert("vibesync".to_string(), "C:\\Temp\\vibesync".to_string());
        let w = w.with_manual_projects(&manual_w);
        let mut manual_m = std::collections::BTreeMap::new();
        manual_m.insert(
            "vibesync".to_string(),
            "/Users/you/Development/7_rust/vibesync".to_string(),
        );
        let m = m.with_manual_projects(&manual_m);

        // Manual wins over the git identity for the same folder.
        let tok = w.tokenize_plain("C:\\Temp\\vibesync\\engine");
        assert_eq!(tok, "${PROJ:vibesync}/engine");
        assert_eq!(
            m.expand_plain(&tok),
            "/Users/you/Development/7_rust/vibesync/engine"
        );
        // Encoded form, canonical both ways.
        let etok = w.tokenize_encoded("C--Temp-vibesync-engine");
        assert_eq!(etok, "${PROJ:vibesync}-engine");
        assert_eq!(
            m.expand_encoded(&etok),
            "-Users-you-Development-7-rust-vibesync-engine"
        );
        assert_eq!(m.tokenize_encoded(&m.expand_encoded(&etok)), etok);
        // Unmapped machines leave the token alone (parking semantics).
        let plain_w_only = "${PROJ:only-on-w}/x";
        assert_eq!(m.expand_plain(plain_w_only), plain_w_only);
        // Invalid names are ignored entirely.
        let mut bad = std::collections::BTreeMap::new();
        bad.insert("has/slash".to_string(), "C:\\X".to_string());
        bad.insert(String::new(), "C:\\Y".to_string());
        let t = Tokenizer::with_case_sensitivity("C:\\Users\\u", true).with_manual_projects(&bad);
        assert_eq!(t.tokenize_plain("C:\\X\\a"), "C:\\X\\a");
    }

    #[test]
    fn git_outranks_home_for_in_home_clones() {
        let (_, m) = fleet();
        // A repo under home must produce the GIT token, not ${HOME}.
        let tok = m.tokenize_plain("/Users/you/Development/7_rust/vibesync");
        assert!(tok.starts_with("${GIT:"), "{tok}");
        // Non-repo home paths still tokenize to ${HOME}.
        assert_eq!(m.tokenize_plain("/Users/you/other"), "${HOME}/other");
    }
}
