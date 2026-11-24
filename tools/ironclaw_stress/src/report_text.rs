//! Infallible text building for the human-readable reports.
//!
//! The renderers append to an in-memory `String`, whose `fmt::Write` impl
//! cannot fail, so the `fmt::Result` a `write!` returns carries no
//! information. [`ReportText`] is the one place it is discarded, instead of a
//! `let _ = writeln!(...)` (and an exemption) at every line of every report.

use std::fmt::{self, Write as _};

pub(crate) trait ReportText {
    /// Appends `args`.
    fn push_fmt(&mut self, args: fmt::Arguments<'_>);

    /// Appends `args` and a newline (the `writeln!` shape).
    fn push_line(&mut self, args: fmt::Arguments<'_>) {
        self.push_fmt(args);
        self.push_fmt(format_args!("\n"));
    }
}

impl ReportText for String {
    fn push_fmt(&mut self, args: fmt::Arguments<'_>) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "fmt::Write for String cannot fail"
        )]
        let _ = self.write_fmt(args);
    }
}

#[cfg(test)]
mod tests {
    use super::ReportText;

    #[test]
    fn push_line_matches_writeln() {
        let mut output = String::new();
        let value = 7;
        output.push_fmt(format_args!("a={value}"));
        output.push_line(format_args!(" {:<3}|", "b"));
        output.push_line(format_args!("c"));
        assert_eq!(output, "a=7 b  |\nc\n");
    }
}
