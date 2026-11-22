//! Fail-closed classifier for pure inspection `builtin.shell` commands.
//!
//! [`classify_read_only_shell_command`] answers one question: is this command
//! line, *as the shell will parse it*, nothing but read-only inspection
//! (`git status`, `rg foo src`, `ls -la | head`)? Anything it cannot prove
//! read-only — an unknown command, an unknown flag, a redirect, an expansion,
//! a chain, an unbalanced quote — is [`ShellCommandClass::RequiresApproval`].
//! It never says "dangerous"; it only ever says "provably harmless" or "ask".
//!
//! The proof is allowlist-only at every level, so a construct this module has
//! never heard of falls through to approval:
//!
//! * **Characters.** Unquoted text may contain only `[A-Za-z0-9_./:=,+@%^-]`,
//!   whitespace, quotes, and `|`. That one rule removes redirection
//!   (`>`, `<`, `&>`), substitution (`$()`, backticks, `<()`), subshells and
//!   groups (`()`, `{}`), chaining (`;`, `&&`, `||`, `&`), globbing
//!   (`*?[`, which could expand to a file named like a flag), history (`!`),
//!   comments (`#`), escapes (`\`), and home expansion (`~`, admitted only
//!   mid-word after an alphanumeric, as in `HEAD~1`). Single quotes
//!   are literal; double quotes additionally refuse `$`, backticks, `\`, `!`.
//!   Control characters (newlines included) are refused everywhere.
//! * **Commands.** The first word of every pipeline segment must be an exact
//!   allowlisted name; a leading `NAME=value` assignment is refused. Only the
//!   first segment may be a general inspection command; later segments must be
//!   pure filters (`head tail wc sort uniq rg grep cut`).
//! * **Flags.** Each command carries its own flag allowlist (long names, short
//!   clusters, and flags that take a value). Mutating or executing flags —
//!   `sort -o`, `rg --pre`/`-z`, `tree -o`, `file -C`, `tail -f`,
//!   `git diff --output`, `git branch -D` — are simply absent from them.
//! * **Operands.** A path-like operand may not be absolute, home-relative, or
//!   contain a `..` component, so an auto-approved read stays inside the
//!   working directory. (Dispatch still runs `validate_command`'s sensitive
//!   path checks on top.)
//!
//! `env`, `printenv`, interpreters, network clients, `find`, `xargs`, `sed`,
//! and `awk` are deliberately not allowlisted.
//!
//! **Git caveat — read before wiring this into approval.** A git subcommand
//! that is read-only on its flags can still execute repository-configured
//! programs (`core.fsmonitor` on `status`, `diff.external` and textconv
//! drivers on `diff`/`log -p`/`show`/`blame`). The classifier refuses every
//! git global option (`-c`, `--git-dir`, `-C`, `--exec-path`, …) so the model
//! cannot set those itself, but a repository config the model previously
//! wrote could. Hence [`ShellCommandClass::ReadOnly::invokes_git`]: an
//! approval relaxation must either keep git commands gated or run them with
//! repository-config execution hooks neutralized.

/// The classifier's verdict for one `builtin.shell` command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommandClass {
    /// Provably read-only inspection under the rules in the module docs.
    ReadOnly {
        /// Whether any pipeline segment runs `git` (see the git caveat).
        invokes_git: bool,
    },
    /// Not provably read-only; the normal approval posture applies.
    RequiresApproval(ApprovalReason),
}

/// Why a command was not classified read-only (stable, for diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReason {
    Empty,
    TooLong,
    ControlCharacter,
    UnbalancedQuote,
    ShellSyntax,
    EmptyPipelineSegment,
    UnknownCommand,
    NotAPipelineFilter,
    UnsafeFlag,
    MissingFlagValue,
    UnsafeOperand,
    TooManyOperands,
    UnsafeGitInvocation,
}

impl ShellCommandClass {
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }
}

const MAX_CLASSIFIED_COMMAND_BYTES: usize = 4096;

/// Classify `command` (the raw `builtin.shell` `command` string).
pub fn classify_read_only_shell_command(command: &str) -> ShellCommandClass {
    match classify(command) {
        Ok(invokes_git) => ShellCommandClass::ReadOnly { invokes_git },
        Err(reason) => ShellCommandClass::RequiresApproval(reason),
    }
}

fn classify(command: &str) -> Result<bool, ApprovalReason> {
    if command.len() > MAX_CLASSIFIED_COMMAND_BYTES {
        return Err(ApprovalReason::TooLong);
    }
    let segments = tokenize(command)?;
    if segments.iter().all(Vec::is_empty) {
        return Err(ApprovalReason::Empty);
    }
    let mut invokes_git = false;
    for (index, segment) in segments.iter().enumerate() {
        let Some((name, args)) = segment.split_first() else {
            return Err(ApprovalReason::EmptyPipelineSegment);
        };
        if name.contains('=') {
            return Err(ApprovalReason::ShellSyntax);
        }
        if name == "git" {
            if index > 0 {
                return Err(ApprovalReason::NotAPipelineFilter);
            }
            check_git(args)?;
            invokes_git = true;
            continue;
        }
        let spec = command_spec(name).ok_or(ApprovalReason::UnknownCommand)?;
        if index > 0 && !spec.filter {
            return Err(ApprovalReason::NotAPipelineFilter);
        }
        check_args(&spec.flags, args)?;
    }
    Ok(invokes_git)
}

/// Split into pipeline segments of de-quoted words, refusing every construct
/// outside the module's character allowlist.
fn tokenize(command: &str) -> Result<Vec<Vec<String>>, ApprovalReason> {
    #[derive(Clone, Copy, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut segments = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = Quote::None;
    let mut previous_was_pipe = false;
    for ch in command.chars() {
        if ch.is_control() && ch != '\t' {
            return Err(ApprovalReason::ControlCharacter);
        }
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(ch);
                }
                continue;
            }
            Quote::Double => {
                match ch {
                    '"' => quote = Quote::None,
                    '$' | '`' | '\\' | '!' => return Err(ApprovalReason::ShellSyntax),
                    _ => word.push(ch),
                }
                continue;
            }
            Quote::None => {}
        }
        match ch {
            '\'' => {
                quote = Quote::Single;
                in_word = true;
            }
            '"' => {
                quote = Quote::Double;
                in_word = true;
            }
            ' ' | '\t' => {
                if in_word {
                    if let Some(segment) = segments.last_mut() {
                        segment.push(std::mem::take(&mut word));
                    }
                    in_word = false;
                }
            }
            '|' => {
                if previous_was_pipe {
                    return Err(ApprovalReason::ShellSyntax);
                }
                if in_word {
                    if let Some(segment) = segments.last_mut() {
                        segment.push(std::mem::take(&mut word));
                    }
                    in_word = false;
                }
                segments.push(Vec::new());
                previous_was_pipe = true;
                continue;
            }
            ch if ch.is_ascii_alphanumeric() || "_./:=,+@%^-".contains(ch) => {
                word.push(ch);
                in_word = true;
            }
            // A mid-word `~` after an alphanumeric (`HEAD~1`) is literal; the
            // shell expands `~` only at a word start or after `=`/`:`.
            '~' if word
                .chars()
                .last()
                .is_some_and(|last| last.is_ascii_alphanumeric()) =>
            {
                word.push(ch);
            }
            _ => return Err(ApprovalReason::ShellSyntax),
        }
        previous_was_pipe = false;
    }
    if quote != Quote::None {
        return Err(ApprovalReason::UnbalancedQuote);
    }
    if in_word && let Some(segment) = segments.last_mut() {
        segment.push(word);
    }
    if segments.len() > 1 && segments.iter().any(Vec::is_empty) {
        return Err(ApprovalReason::EmptyPipelineSegment);
    }
    Ok(segments)
}

/// One command's argument grammar.
struct FlagSpec {
    /// Long or short flags that take no value (`--hidden`, `-q`).
    switches: &'static [&'static str],
    /// Flags that take a value: next word, `--flag=value`, or `-Xvalue`.
    valued: &'static [&'static str],
    /// Letters that may be combined into one short cluster (`-la`).
    cluster: &'static str,
    /// Whether `-N` (digits only) is a count shorthand (`head -20`).
    numeric_shorthand: bool,
    max_operands: Option<usize>,
}

struct CommandSpec {
    flags: FlagSpec,
    /// May appear after a `|`.
    filter: bool,
}

const fn spec(
    switches: &'static [&'static str],
    valued: &'static [&'static str],
    cluster: &'static str,
    numeric_shorthand: bool,
    max_operands: Option<usize>,
    filter: bool,
) -> CommandSpec {
    CommandSpec {
        flags: FlagSpec {
            switches,
            valued,
            cluster,
            numeric_shorthand,
            max_operands,
        },
        filter,
    }
}

fn command_spec(name: &str) -> Option<CommandSpec> {
    Some(match name {
        "pwd" => spec(&["-L", "-P"], &[], "", false, Some(0), false),
        "ls" => spec(
            &[
                "--all",
                "--almost-all",
                "--human-readable",
                "--recursive",
                "--color=never",
            ],
            &[],
            "laAhRtSr1dFGpis",
            false,
            None,
            false,
        ),
        "wc" => spec(
            &["--lines", "--words", "--bytes", "--chars"],
            &[],
            "lwcmL",
            false,
            None,
            true,
        ),
        "head" => spec(
            &["--quiet", "--verbose"],
            &["-n", "-c", "--lines", "--bytes"],
            "qv",
            true,
            None,
            true,
        ),
        // `-f`/`-F`/`--follow` are absent: following never terminates.
        "tail" => spec(
            &["--quiet", "--verbose"],
            &["-n", "-c", "--lines", "--bytes"],
            "qv",
            true,
            None,
            true,
        ),
        "cat" => spec(
            &["--number", "--show-all"],
            &[],
            "nbsAvET",
            false,
            None,
            false,
        ),
        // No `--pre`/`--pre-glob` (runs a preprocessor), no `-z`/`--search-zip`
        // (spawns decompressors), no `--ignore-file`/`-f` (reads arbitrary files).
        "rg" => spec(
            &[
                "--hidden",
                "--no-ignore",
                "--files",
                "--count",
                "--line-number",
                "--no-heading",
                "--heading",
                "--ignore-case",
                "--smart-case",
                "--word-regexp",
                "--fixed-strings",
                "--files-with-matches",
                "--only-matching",
                "--invert-match",
                "--no-messages",
                "--color=never",
                "--json",
            ],
            &[
                "-e",
                "-g",
                "-t",
                "-T",
                "-A",
                "-B",
                "-C",
                "-m",
                "--regexp",
                "--glob",
                "--type",
                "--type-not",
                "--context",
                "--after-context",
                "--before-context",
                "--max-count",
                "--max-depth",
            ],
            "isSwxvnNlcFoHhu",
            false,
            None,
            true,
        ),
        "grep" => spec(
            &[
                "--recursive",
                "--ignore-case",
                "--line-number",
                "--count",
                "--files-with-matches",
                "--word-regexp",
                "--fixed-strings",
                "--extended-regexp",
                "--invert-match",
                "--only-matching",
                "--color=never",
            ],
            &[
                "-e",
                "-A",
                "-B",
                "-C",
                "-m",
                "--regexp",
                "--include",
                "--exclude",
                "--exclude-dir",
                "--max-count",
            ],
            "ivnlLcwxFErRhHoqs",
            false,
            None,
            true,
        ),
        // No `-o` (writes its listing to a file).
        "tree" => spec(
            &["--noreport", "--dirsfirst"],
            &["-L", "-I", "-P"],
            "adfi",
            false,
            None,
            false,
        ),
        // No `-C` (compiles a magic file) and no `-m` (loads one).
        "file" => spec(&["--brief", "--mime"], &[], "biL", false, None, false),
        "stat" => spec(&["--dereference", "--terse"], &[], "Lt", false, None, false),
        "which" => spec(&[], &[], "a", false, None, false),
        "uname" => spec(&[], &[], "asrmnvpo", false, Some(0), false),
        // No `-o`/`--output` (writes a file), no `--compress-program` (executes),
        // no `-T`/`--temporary-directory`.
        "sort" => spec(
            &[
                "--numeric-sort",
                "--reverse",
                "--unique",
                "--human-numeric-sort",
                "--ignore-case",
                "--version-sort",
            ],
            &["-k", "-t", "--key", "--field-separator"],
            "nruhfbV",
            false,
            None,
            true,
        ),
        // At most one operand: `uniq IN OUT` writes OUT.
        "uniq" => spec(
            &["--count", "--repeated", "--unique", "--ignore-case"],
            &["-f", "-s"],
            "cdui",
            false,
            Some(1),
            true,
        ),
        "cut" => spec(
            &[],
            &["-d", "-f", "-c", "-b", "--delimiter", "--fields"],
            "s",
            false,
            None,
            true,
        ),
        _ => return None,
    })
}

fn check_args(spec: &FlagSpec, args: &[String]) -> Result<(), ApprovalReason> {
    let mut operands = 0usize;
    let mut options_ended = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if options_ended || arg == "-" || !arg.starts_with('-') {
            check_operand(arg)?;
            operands += 1;
            continue;
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, has_value) = match long.split_once('=') {
                Some((name, _)) => (name, true),
                None => (long, false),
            };
            let flag = format!("--{name}");
            if spec.switches.contains(&arg.as_str()) {
                continue;
            }
            if spec.valued.contains(&flag.as_str()) {
                if !has_value && iter.next().is_none() {
                    return Err(ApprovalReason::MissingFlagValue);
                }
                continue;
            }
            return Err(ApprovalReason::UnsafeFlag);
        }
        // Short flag forms.
        let rest = arg.strip_prefix('-').unwrap_or_default();
        if spec.numeric_shorthand && rest.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if spec.switches.contains(&arg.as_str()) {
            continue;
        }
        if let Some(first) = rest.chars().next() {
            let short = format!("-{first}");
            if spec.valued.contains(&short.as_str()) {
                let attached = rest.len() > first.len_utf8();
                if !attached && iter.next().is_none() {
                    return Err(ApprovalReason::MissingFlagValue);
                }
                continue;
            }
        }
        if !rest.is_empty() && rest.chars().all(|letter| spec.cluster.contains(letter)) {
            continue;
        }
        return Err(ApprovalReason::UnsafeFlag);
    }
    if spec.max_operands.is_some_and(|max| operands > max) {
        return Err(ApprovalReason::TooManyOperands);
    }
    Ok(())
}

/// A positional argument may name a file: keep auto-approved reads inside the
/// working directory.
fn check_operand(operand: &str) -> Result<(), ApprovalReason> {
    if operand.starts_with('/') || operand.starts_with('~') {
        return Err(ApprovalReason::UnsafeOperand);
    }
    if operand.split('/').any(|component| component == "..") {
        return Err(ApprovalReason::UnsafeOperand);
    }
    Ok(())
}

fn check_git(args: &[String]) -> Result<(), ApprovalReason> {
    let mut args = args;
    // `--no-pager` is the only global option admitted; every other one
    // (`-c`, `-C`, `--git-dir`, `--work-tree`, `--exec-path`, `--config-env`,
    // `--namespace`, …) can redirect the repository or inject configuration.
    while let Some((first, rest)) = args.split_first() {
        if first == "--no-pager" {
            args = rest;
        } else {
            break;
        }
    }
    let Some((subcommand, args)) = args.split_first() else {
        return Err(ApprovalReason::UnsafeGitInvocation);
    };
    // Every spec below omits `--output` (writes a file), `--ext-diff`, and
    // `--textconv` (run configured programs).
    let spec = match subcommand.as_str() {
        "status" => FlagSpec {
            switches: &[
                "--short",
                "--branch",
                "--porcelain",
                "--porcelain=v1",
                "--porcelain=v2",
                "--untracked-files=no",
                "--untracked-files=normal",
                "--untracked-files=all",
                "--ignored",
                "--no-renames",
            ],
            valued: &[],
            cluster: "sbu",
            numeric_shorthand: false,
            max_operands: None,
        },
        "diff" => FlagSpec {
            switches: &[
                "--stat",
                "--shortstat",
                "--numstat",
                "--name-only",
                "--name-status",
                "--cached",
                "--staged",
                "--no-color",
                "--color=never",
                "--ignore-all-space",
                "--no-ext-diff",
                "--no-textconv",
                "--check",
                "--word-diff",
                "--patch",
            ],
            valued: &["-U", "--unified", "--diff-filter", "--stat-width"],
            cluster: "wbpR",
            numeric_shorthand: false,
            max_operands: None,
        },
        "log" => FlagSpec {
            switches: &[
                "--oneline",
                "--stat",
                "--shortstat",
                "--graph",
                "--decorate",
                "--all",
                "--name-only",
                "--name-status",
                "--reverse",
                "--no-merges",
                "--merges",
                "--first-parent",
                "--no-color",
                "--color=never",
                "--no-ext-diff",
                "--no-textconv",
                "--follow",
                "--patch",
                "--abbrev-commit",
            ],
            valued: &[
                "-n",
                "--max-count",
                "--format",
                "--pretty",
                "--since",
                "--until",
                "--after",
                "--before",
                "--author",
                "--grep",
                "--skip",
                "-L",
            ],
            cluster: "p",
            numeric_shorthand: true,
            max_operands: None,
        },
        "show" => FlagSpec {
            switches: &[
                "--stat",
                "--shortstat",
                "--name-only",
                "--name-status",
                "--oneline",
                "--no-patch",
                "--no-color",
                "--color=never",
                "--no-ext-diff",
                "--no-textconv",
                "--abbrev-commit",
            ],
            valued: &["--format", "--pretty"],
            cluster: "sp",
            numeric_shorthand: false,
            max_operands: None,
        },
        "branch" => return check_git_branch(args),
        "rev-parse" => FlagSpec {
            switches: &[
                "--abbrev-ref",
                "--short",
                "--show-toplevel",
                "--show-prefix",
                "--git-dir",
                "--is-inside-work-tree",
                "--verify",
                "--symbolic-full-name",
                "--quiet",
            ],
            valued: &[],
            cluster: "q",
            numeric_shorthand: false,
            max_operands: None,
        },
        "describe" => FlagSpec {
            switches: &[
                "--tags",
                "--always",
                "--long",
                "--dirty",
                "--all",
                "--exact-match",
            ],
            valued: &["--abbrev", "--match"],
            cluster: "",
            numeric_shorthand: false,
            max_operands: None,
        },
        "ls-files" => FlagSpec {
            switches: &[
                "--others",
                "--modified",
                "--deleted",
                "--cached",
                "--exclude-standard",
                "--stage",
                "--ignored",
                "--directory",
            ],
            valued: &[],
            cluster: "modcsiz",
            numeric_shorthand: false,
            max_operands: None,
        },
        "blame" => FlagSpec {
            switches: &["--porcelain", "--line-porcelain", "--show-email"],
            valued: &["-L"],
            cluster: "wesnlCM",
            numeric_shorthand: false,
            max_operands: None,
        },
        _ => return Err(ApprovalReason::UnsafeGitInvocation),
    };
    check_args(&spec, args)
}

/// `git branch` creates, deletes, renames, or copies a branch from operands
/// and several flags; only pure listing forms are read-only.
fn check_git_branch(args: &[String]) -> Result<(), ApprovalReason> {
    const LISTING_SWITCHES: &[&str] = &[
        "--list",
        "--all",
        "--remotes",
        "--show-current",
        "--verbose",
        "--no-color",
        "--color=never",
        "-a",
        "-r",
        "-v",
        "-vv",
        "-l",
    ];
    const FILTERS_WITH_VALUE: &[&str] = &[
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--sort",
        "--format",
    ];
    let listing = args.iter().any(|arg| arg == "--list" || arg == "-l");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if LISTING_SWITCHES.contains(&arg.as_str()) {
            continue;
        }
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        if FILTERS_WITH_VALUE.contains(&name) {
            if !arg.contains('=') && iter.next().is_none() {
                return Err(ApprovalReason::MissingFlagValue);
            }
            continue;
        }
        if arg.starts_with('-') {
            // -d -D -m -M -c -C -f -u --delete --move --copy --force
            // --set-upstream-to --unset-upstream --edit-description …
            return Err(ApprovalReason::UnsafeGitInvocation);
        }
        // A bare operand creates a branch unless `--list` makes it a pattern.
        if !listing {
            return Err(ApprovalReason::UnsafeGitInvocation);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_only(command: &str) -> bool {
        classify_read_only_shell_command(command).is_read_only()
    }

    #[test]
    fn plain_inspection_commands_are_read_only() {
        for command in [
            "pwd",
            "ls",
            "ls -la",
            "ls -la src",
            "wc -l Cargo.toml",
            "head -n 20 README.md",
            "head -20 README.md",
            "tail -n5 log.txt",
            "cat src/main.rs",
            "rg foo",
            "rg -n 'fn main' src",
            "rg -i -g '*.rs' 'a|b' crates",
            "rg --type rust -C 2 TODO",
            "grep -rn TODO src",
            "grep -e 'x;y' -- file.txt",
            "tree -L 2",
            "file Cargo.toml",
            "stat Cargo.toml",
            "which cargo",
            "uname -a",
            "  ls   -la  ",
        ] {
            assert!(read_only(command), "{command:?} should be read-only");
        }
    }

    #[test]
    fn read_only_pipelines_through_filters_are_read_only() {
        for command in [
            "rg -l foo | wc -l",
            "ls -la | head -n 5",
            "cat Cargo.toml | grep version | head -1",
            "rg -o 'v[0-9]+' | sort | uniq -c | sort -rn | head",
            "git log --oneline | head -n 10",
            "cut -d , -f 1 data.csv | sort -u",
        ] {
            assert!(read_only(command), "{command:?} should be read-only");
        }
    }

    #[test]
    fn git_read_only_subcommands_are_read_only_and_flag_git() {
        for command in [
            "git status",
            "git status --short",
            "git status -sb",
            "git diff",
            "git diff --stat HEAD~1",
            "git diff --cached -- src/lib.rs",
            "git log --oneline -n 5",
            "git log -20 --format=%H",
            "git show --stat HEAD",
            "git branch",
            "git branch -a",
            "git branch --show-current",
            "git branch --list 'feat-*'",
            "git branch --contains HEAD",
            "git rev-parse --abbrev-ref HEAD",
            "git rev-parse --show-toplevel",
            "git describe --tags --always",
            "git ls-files -m",
            "git blame -L 1,20 src/lib.rs",
            "git --no-pager log -n 3",
        ] {
            assert_eq!(
                classify_read_only_shell_command(command),
                ShellCommandClass::ReadOnly { invokes_git: true },
                "{command:?}"
            );
        }
        assert_eq!(
            classify_read_only_shell_command("ls"),
            ShellCommandClass::ReadOnly { invokes_git: false }
        );
    }

    #[test]
    fn redirection_substitution_subshells_and_chains_fail_closed() {
        for command in [
            "ls > out.txt",
            "ls >> out.txt",
            "ls 2>&1",
            "ls &> out",
            "cat <> file",
            "cat < input",
            "echo $(id)",
            "ls $(pwd)",
            "ls `pwd`",
            "cat <(ls)",
            "diff >(ls) x",
            "(ls)",
            "{ ls; }",
            "ls; rm -rf x",
            "ls && rm x",
            "ls || rm x",
            "ls & rm x",
            "ls &",
            "ls\nrm x",
            "ls\rrm x",
            "ls \"$HOME\"",
            "ls \"`id`\"",
            "ls $HOME",
            "ls ${HOME}",
            "ls *",
            "rg foo *",
            "ls ?",
            "ls [ab]",
            "ls ~",
            "ls ~/x",
            "ls a=~/x",
            "ls a:~/x",
            "git diff ~HEAD",
            "ls # comment",
            "ls \\; rm",
            "ls !!",
            "ls 'unbalanced",
            "ls \"unbalanced",
            "ls |",
            "| ls",
            "ls | | wc",
            "ls ||wc",
            "",
            "   ",
            "ls\u{0}",
        ] {
            assert!(!read_only(command), "{command:?} must require approval");
        }
    }

    #[test]
    fn assignments_eval_source_and_unknown_commands_fail_closed() {
        for command in [
            "FOO=bar ls",
            "PATH=. ls",
            "eval ls",
            "source x.sh",
            ". x.sh",
            "env",
            "printenv",
            "env ls",
            "sh -c ls",
            "bash -c 'ls'",
            "python -c 'print(1)'",
            "python3 x.py",
            "node -e 1",
            "awk '{print}' f",
            "perl -e 1",
            "curl https:x",
            "wget x",
            "xargs ls",
            "find . -name x",
            "find . -exec rm {} ;",
            "find . -delete",
            "sed -i s,a,b, f",
            "sed -n 1p f",
            "rm x",
            "mv a b",
            "cp a b",
            "touch x",
            "mkdir x",
            "chmod 777 x",
            "/bin/ls",
            "./ls",
            "'ls' ;",
            "sudo ls",
            "nohup ls",
            "time ls",
            "command ls",
            "exec ls",
        ] {
            assert!(!read_only(command), "{command:?} must require approval");
        }
    }

    #[test]
    fn mutating_or_executing_flags_fail_closed() {
        for command in [
            "sort -o out.txt in.txt",
            "sort --output=out in",
            "sort --compress-program=sh in",
            "uniq in out",
            "rg --pre cat foo",
            "rg --pre=cat foo",
            "rg --pre-glob '*' foo",
            "rg -z foo",
            "rg --search-zip foo",
            "rg -f patterns.txt",
            "rg --ignore-file x foo",
            "tree -o out.txt",
            "file -C -m magic",
            "file -m magic x",
            "tail -f log",
            "tail -F log",
            "tail --follow log",
            "ls --unknown-flag",
            "ls -Z",
            "grep -f patterns x",
            "cat -x f",
            "head -n",
            "rg -e",
            "pwd extra",
            "uname extra",
            "which -x ls",
        ] {
            assert!(!read_only(command), "{command:?} must require approval");
        }
    }

    #[test]
    fn non_filter_commands_after_a_pipe_fail_closed() {
        for command in [
            "ls | sh",
            "ls | bash",
            "ls | xargs rm",
            "ls | tee out.txt",
            "ls | cat",
            "ls | ls",
            "ls | git status",
            "rg foo | python3 -",
            "rg foo | sort -o out",
        ] {
            assert!(!read_only(command), "{command:?} must require approval");
        }
    }

    #[test]
    fn operands_outside_the_working_directory_fail_closed() {
        for command in [
            "cat /etc/passwd",
            "ls /",
            "cat ~/.ssh/id_rsa",
            "cat ../secrets",
            "cat src/../../etc",
            "rg foo /",
            "ls -- /etc",
            "git diff -- ../x",
            "head '/etc/shadow'",
        ] {
            assert!(!read_only(command), "{command:?} must require approval");
        }
    }

    #[test]
    fn unsafe_git_invocations_fail_closed() {
        for command in [
            "git",
            "git -c core.pager=sh log",
            "git -c core.fsmonitor=x status",
            "git --config-env=a=b status",
            "git -C /tmp status",
            "git --git-dir=/x status",
            "git --work-tree=/x status",
            "git --exec-path=/x status",
            "git --no-pager",
            "git branch -d main",
            "git branch -D main",
            "git branch -m a b",
            "git branch -M a b",
            "git branch -c a b",
            "git branch -C a b",
            "git branch --delete main",
            "git branch --move a b",
            "git branch --copy a b",
            "git branch -f main HEAD",
            "git branch -u origin",
            "git branch --set-upstream-to=origin",
            "git branch newbranch",
            "git diff --output=x",
            "git diff --ext-diff",
            "git log --textconv",
            "git log --output=x",
            "git show --textconv HEAD",
            "git commit -m x",
            "git push",
            "git checkout x",
            "git reset --hard",
            "git config core.pager sh",
            "git stash",
            "git clean -fd",
            "git fetch",
            "git pull",
            "git apply x",
            "git status | sh",
            "git status; rm x",
            "ls | git log",
        ] {
            assert!(!read_only(command), "{command:?} must require approval");
        }
    }

    #[test]
    fn overlong_commands_fail_closed() {
        let command = format!("ls {}", "a".repeat(MAX_CLASSIFIED_COMMAND_BYTES));
        assert_eq!(
            classify_read_only_shell_command(&command),
            ShellCommandClass::RequiresApproval(ApprovalReason::TooLong)
        );
    }

    /// Every allowlisted command must also pass the dispatch-time
    /// `validate_command` gate; otherwise an auto-approved command would be
    /// refused after the approval decision, which is incoherent.
    #[test]
    fn read_only_commands_also_pass_dispatch_validation() {
        for command in [
            "git status --short",
            "rg -n TODO src | head -20",
            "ls -la | wc -l",
            "cat Cargo.toml",
        ] {
            assert!(read_only(command));
            assert!(
                super::super::shell_core::validate_command(command, false).is_ok(),
                "{command:?} must pass validate_command"
            );
        }
    }
}
