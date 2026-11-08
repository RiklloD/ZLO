//! A Codex review's findings, parsed from the text Codex renders for one
//! (`render_review_output_text` in codex-rs `protocol/src/review_format.rs`):
//!
//! ```text
//! <overall explanation>
//!
//! Full review comments:            (or "Review comment:")
//!
//! - [P1] Title — /abs/path.rs:10-12
//!   body line
//!   body line
//! ```
//!
//! UI-free so the Changes tab's annotations can be unit-tested.

/// One finding with its code location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFinding {
    /// `0` is the most urgent (`[P0]`); `None` when the title has no tag.
    pub priority: Option<u8>,
    /// The title without its priority tag.
    pub title: String,
    /// As Codex wrote it (usually absolute).
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub body: String,
}

impl ReviewFinding {
    /// `path` relative to `root` when it lies inside it.
    pub fn relative_to(&self, root: &std::path::Path) -> String {
        let path = std::path::Path::new(&self.path);
        path.strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| self.path.clone())
    }

    /// `P1`, or empty without a tag.
    pub fn priority_label(&self) -> String {
        self.priority.map(|p| format!("P{p}")).unwrap_or_default()
    }
}

/// A parsed review: the overall explanation and the findings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Review {
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
}

/// Parse a review's text. Lines that do not match the finding shape stay in
/// the summary (before the findings) or the body (after one).
pub fn parse(text: &str) -> Review {
    let mut review = Review::default();
    let mut summary: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(finding) = finding_line(line) {
            review.findings.push(finding);
            continue;
        }
        match review.findings.last_mut() {
            Some(finding) if line.starts_with("  ") || line.trim().is_empty() => {
                let body_line = line.strip_prefix("  ").unwrap_or(line.trim());
                if !finding.body.is_empty() || !body_line.is_empty() {
                    if !finding.body.is_empty() {
                        finding.body.push('\n');
                    }
                    finding.body.push_str(body_line);
                }
            }
            Some(_) => {}
            None => {
                let header = matches!(line.trim(), "Full review comments:" | "Review comment:");
                if !header {
                    summary.push(line);
                }
            }
        }
    }
    for finding in &mut review.findings {
        finding.body = finding.body.trim_end().to_string();
    }
    review.summary = summary.join("\n").trim().to_string();
    review
}

/// `- [x] Title — path:start-end` (checkbox optional).
fn finding_line(line: &str) -> Option<ReviewFinding> {
    let rest = line.strip_prefix("- ")?;
    let rest = rest
        .strip_prefix("[x] ")
        .or_else(|| rest.strip_prefix("[ ] "))
        .unwrap_or(rest);
    let (title, location) = rest.rsplit_once(" — ")?;
    let (path, range) = location.trim().rsplit_once(':')?;
    let (start, end) = range.split_once('-')?;
    let (start, end) = (start.parse::<u32>().ok()?, end.parse::<u32>().ok()?);
    let (priority, title) = match title
        .strip_prefix("[P")
        .and_then(|t| t.split_once("] "))
        .and_then(|(p, t)| Some((p.parse::<u8>().ok()?, t)))
    {
        Some((p, t)) => (Some(p), t),
        None => (None, title),
    };
    Some(ReviewFinding {
        priority,
        title: title.trim().to_string(),
        path: path.to_string(),
        start,
        end: end.max(start),
        body: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_review_text_into_findings() {
        let text = "The change mostly works.\n\nFull review comments:\n\n\
            - [P1] Off by one in loop — /repo/src/a.rs:10-12\n  The loop skips the last element.\n  Use `<=`.\n\n\
            - Missing test — /repo/tests/b.rs:3-3\n  Add a regression test.";
        let review = parse(text);
        assert_eq!(review.summary, "The change mostly works.");
        assert_eq!(review.findings.len(), 2);
        let first = &review.findings[0];
        assert_eq!(first.priority, Some(1));
        assert_eq!(first.title, "Off by one in loop");
        assert_eq!((first.start, first.end), (10, 12));
        assert_eq!(first.body, "The loop skips the last element.\nUse `<=`.");
        assert_eq!(first.relative_to(std::path::Path::new("/repo")), "src/a.rs");
        assert_eq!(review.findings[1].priority, None);
        assert_eq!(review.findings[1].body, "Add a regression test.");
    }

    #[test]
    fn a_review_without_findings_is_all_summary() {
        let review = parse("No issues found.");
        assert_eq!(review.summary, "No issues found.");
        assert!(review.findings.is_empty());
    }

    #[test]
    fn checkbox_markers_and_dashes_in_titles_parse() {
        let f = finding_line("- [x] [P0] Panic — on empty — /r/x.rs:4-2").unwrap();
        assert_eq!(f.title, "Panic — on empty");
        assert_eq!((f.start, f.end), (4, 4));
    }
}
