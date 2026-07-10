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

#[derive(Debug, Clone)]
pub struct Tokenizer {
    home: String,
    encoded_home: String,
}

/// Claude Code's cwd encoding: every non-alphanumeric byte becomes '-'.
pub fn encode_cwd(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

impl Tokenizer {
    pub fn new(home: &str) -> Self {
        let home = home.trim_end_matches(['/', '\\']).to_string();
        let encoded_home = encode_cwd(&home);
        Self { home, encoded_home }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        Ok(Self::new(&home.to_string_lossy()))
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    /// `/Users/alice/x` → `${HOME}/x` (boundary-aware; non-matching input unchanged).
    pub fn tokenize_plain(&self, path: &str) -> String {
        if path == self.home {
            return HOME_TOKEN.to_string();
        }
        if let Some(rest) = path.strip_prefix(&self.home) {
            if rest.starts_with('/') || rest.starts_with('\\') {
                return format!("{HOME_TOKEN}{rest}");
            }
        }
        path.to_string()
    }

    /// `${HOME}/x` → `/Users/alice/x`.
    pub fn expand_plain(&self, path: &str) -> String {
        match path.strip_prefix(HOME_TOKEN) {
            Some(rest) => format!("{}{}", self.home, rest),
            None => path.to_string(),
        }
    }

    /// `-Users-alice-dev-proj` → `${EHOME}-dev-proj` (boundary-aware).
    pub fn tokenize_encoded(&self, s: &str) -> String {
        if s == self.encoded_home {
            return EHOME_TOKEN.to_string();
        }
        if let Some(rest) = s.strip_prefix(&self.encoded_home) {
            if rest.starts_with('-') {
                return format!("{EHOME_TOKEN}{rest}");
            }
        }
        s.to_string()
    }

    /// `${EHOME}-dev-proj` → `-Users-alice-dev-proj`.
    pub fn expand_encoded(&self, s: &str) -> String {
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
    fn cross_machine_remap() {
        let a = Tokenizer::new("/Users/alice");
        let b = Tokenizer::new("/home/bob");
        let logical = a.tokenize_encoded("-Users-alice-dev-proj");
        assert_eq!(b.expand_encoded(&logical), "-home-bob-dev-proj");
    }
}
