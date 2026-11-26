//! Bounded home- and data-folder path rewriting.
//!
//! Used both by offline setup bundles (`zlo import`, direct source → target
//! rewrite) and continuous config sync (export normalizes to `$ZLO_HOME` /
//! `$HOME` tokens; apply expands tokens back to the target's folders).
//!
//! A prefix is only rewritten where it starts a value or a quoted string and
//! ends at a separator, a quote, or the end, so `/Users/ana` never matches
//! inside `/Users/anabel` or in the middle of a sentence, and `$HOME` never
//! matches `$HOME2`. `~/…` paths are already portable and stay as they are.

/// Canonical token representing the ZLO data folder (`~/.zlo` / `ZLO_HOME`).
pub const ZLO_HOME_TOKEN: &str = "$ZLO_HOME";

/// Canonical token representing the user's home directory (`HOME`).
pub const USER_HOME_TOKEN: &str = "$HOME";

/// Ordered prefix replacements (most specific first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomePathRewrite {
    pairs: Vec<(String, String)>,
}

impl HomePathRewrite {
    /// Direct source → target rewrite for absolute paths: the source data
    /// folder becomes the target data folder, then anything else under the
    /// source home becomes the same path under the target home.
    pub fn new(
        source_data_folder: &str,
        target_data_folder: &str,
        source_home: &str,
        target_home: &str,
    ) -> Self {
        let mut pairs = Vec::new();
        for (from, to) in [
            (source_data_folder, target_data_folder),
            (source_home, target_home),
        ] {
            let from = from.trim_end_matches('/');
            let to = to.trim_end_matches('/');
            // A root or relative "home" would match everything.
            if from.len() > 1 && from.starts_with('/') && from != to {
                pairs.push((from.to_string(), to.to_string()));
            }
        }
        Self { pairs }
    }

    /// Export normalizer: rewrites the host's `data_folder` to `$ZLO_HOME` and
    /// `home` to `$HOME` (data folder first so `$ZLO_HOME` wins when it sits
    /// inside `$HOME`).
    pub fn to_tokens(data_folder: &str, home: &str) -> Self {
        let mut pairs = Vec::new();
        for (from, to) in [(data_folder, ZLO_HOME_TOKEN), (home, USER_HOME_TOKEN)] {
            let from = from.trim_end_matches('/');
            if from.len() > 1 && from.starts_with('/') {
                pairs.push((from.to_string(), to.to_string()));
            }
        }
        Self { pairs }
    }

    /// Apply expander: rewrites `$ZLO_HOME` to `target_data_folder` and `$HOME`
    /// to `target_home`.
    pub fn from_tokens(target_data_folder: &str, target_home: &str) -> Self {
        let mut pairs = Vec::new();
        for (from, to) in [
            (ZLO_HOME_TOKEN, target_data_folder),
            (USER_HOME_TOKEN, target_home),
        ] {
            let to = to.trim_end_matches('/');
            if to.len() > 1 && to.starts_with('/') {
                pairs.push((from.to_string(), to.to_string()));
            }
        }
        Self { pairs }
    }

    pub fn is_identity(&self) -> bool {
        self.pairs.is_empty()
    }

    /// `text` with every bounded occurrence rewritten, or `None` when
    /// nothing changed.
    pub fn rewrite(&self, text: &str) -> Option<String> {
        let mut out = String::with_capacity(text.len());
        let mut changed = false;
        let mut rest = text;
        let mut at_start = true;
        'outer: while !rest.is_empty() {
            let starts_value = at_start || out.ends_with(['"', '\'', '=', ' ']);
            if starts_value {
                for (from, to) in &self.pairs {
                    if let Some(after) = rest.strip_prefix(from.as_str())
                        && (after.is_empty() || after.starts_with(['/', '"', '\'', '\n']))
                    {
                        out.push_str(to);
                        rest = after;
                        changed = true;
                        at_start = false;
                        continue 'outer;
                    }
                }
            }
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                out.push(c);
            }
            rest = chars.as_str();
            at_start = false;
        }
        changed.then_some(out)
    }

    /// Convenience helper returning the rewritten string or the original
    /// string when nothing matched.
    pub fn rewrite_or_keep(&self, text: &str) -> String {
        self.rewrite(text).unwrap_or_else(|| text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_to_linux() -> HomePathRewrite {
        HomePathRewrite::new("/Users/ana/.zlo", "/srv/zlo", "/Users/ana", "/home/ana")
    }

    #[test]
    fn whole_values_json_and_toml_strings_are_rewritten() {
        let r = mac_to_linux();
        assert_eq!(
            r.rewrite("/Users/ana/src/app").as_deref(),
            Some("/home/ana/src/app")
        );
        assert_eq!(r.rewrite("/Users/ana").as_deref(), Some("/home/ana"));
        assert_eq!(
            r.rewrite(r#"{"working_folder":"/Users/ana/work","x":1}"#)
                .as_deref(),
            Some(r#"{"working_folder":"/home/ana/work","x":1}"#)
        );
        assert_eq!(
            r.rewrite(
                "[update]\nrepo = \"/Users/ana/ironclaw\"\nother = '/Users/ana/.zlo/skills'\n"
            )
            .as_deref(),
            Some("[update]\nrepo = \"/home/ana/ironclaw\"\nother = '/srv/zlo/skills'\n")
        );
    }

    #[test]
    fn data_folder_wins_over_home_and_lookalikes_are_left_alone() {
        let r = mac_to_linux();
        assert_eq!(
            r.rewrite("/Users/ana/.zlo/workspace").as_deref(),
            Some("/srv/zlo/workspace")
        );
        assert_eq!(r.rewrite("/Users/anabel/src"), None);
        assert_eq!(
            r.rewrite("cwd = /Users/ana/x").as_deref(),
            Some("cwd = /home/ana/x")
        );
        assert_eq!(r.rewrite("prefix/Users/ana/src"), None);
        assert_eq!(r.rewrite("~/src/app"), None);
        assert_eq!(
            r.rewrite("/Users/ana/naïve/✓").as_deref(),
            Some("/home/ana/naïve/✓")
        );
    }

    #[test]
    fn same_home_or_root_home_rewrites_nothing() {
        assert!(
            HomePathRewrite::new("/home/a/.zlo", "/home/a/.zlo", "/home/a", "/home/a")
                .is_identity()
        );
        assert!(HomePathRewrite::new("/", "/x", "/", "/y").is_identity());
    }

    #[test]
    fn token_round_trip_normalizes_and_expands() {
        let to_tok = HomePathRewrite::to_tokens("/Users/ana/.zlo", "/Users/ana");
        let from_tok = HomePathRewrite::from_tokens("/srv/zlo", "/home/bob");

        let norm_src = to_tok.rewrite_or_keep("/Users/ana/src/app");
        assert_eq!(norm_src, "$HOME/src/app");
        assert_eq!(from_tok.rewrite_or_keep(&norm_src), "/home/bob/src/app");

        let norm_zlo = to_tok.rewrite_or_keep("/Users/ana/.zlo/skills");
        assert_eq!(norm_zlo, "$ZLO_HOME/skills");
        assert_eq!(from_tok.rewrite_or_keep(&norm_zlo), "/srv/zlo/skills");

        // Token lookalikes are untouched.
        assert_eq!(from_tok.rewrite("$HOME2/src"), None);
        assert_eq!(from_tok.rewrite("$ZLO_HOME_OLD/x"), None);
        assert_eq!(from_tok.rewrite("~/src/app"), None);
    }
}
