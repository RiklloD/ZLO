//! The one place absolute paths from the source machine are moved to the
//! target machine: the source data folder becomes the target data folder,
//! then anything else under the source home becomes the same path under the
//! target home (`/Users/ana/src/app` → `/home/ana/src/app`).
//!
//! A path is only rewritten where it starts a value or a quoted string and
//! ends at a separator, a quote or the end, so `/Users/ana` never matches
//! inside `/Users/anabel` or in the middle of a sentence. `~/…` paths are
//! already portable and stay as they are.

/// Ordered prefix replacements (most specific first).
#[derive(Debug, Clone)]
pub(crate) struct HomePathRewrite {
    pairs: Vec<(String, String)>,
}

impl HomePathRewrite {
    pub(crate) fn new(
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

    pub(crate) fn is_identity(&self) -> bool {
        self.pairs.is_empty()
    }

    /// `text` with every bounded occurrence rewritten, or `None` when
    /// nothing changed.
    pub(crate) fn rewrite(&self, text: &str) -> Option<String> {
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
}
