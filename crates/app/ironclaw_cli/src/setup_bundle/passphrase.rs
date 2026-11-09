//! The archive passphrase: typed twice on a terminal, read from a file
//! (`--passphrase-file`), or from the first line of stdin
//! (`--passphrase-stdin`, which `zlo transfer` uses over SSH). Never an
//! argument: argv is visible to every user on the machine.

use std::io::{BufRead, IsTerminal as _, Read, Write as _};
use std::path::Path;

use anyhow::Context as _;
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroize as _;

const MAX_PASSPHRASE_BYTES: usize = 1024;
/// For a new passphrase only; an existing archive opens with whatever it used.
pub(crate) const MIN_NEW_PASSPHRASE_CHARS: usize = 8;

fn finish(mut raw: Vec<u8>) -> anyhow::Result<SecretString> {
    while raw.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        raw.pop();
    }
    let text = String::from_utf8(raw).map_err(|error| {
        let mut bytes = error.into_bytes();
        bytes.zeroize();
        anyhow::anyhow!("the passphrase is not valid UTF-8")
    })?;
    if text.is_empty() {
        anyhow::bail!("the passphrase is empty");
    }
    Ok(SecretString::from(text))
}

/// The first line of the file at `path`.
pub(crate) fn from_file(path: &Path) -> anyhow::Result<SecretString> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("could not open the passphrase file {}", path.display()))?;
    let mut raw = Vec::new();
    std::io::BufReader::new(file)
        .take(MAX_PASSPHRASE_BYTES as u64 + 1)
        .read_until(b'\n', &mut raw)
        .with_context(|| format!("could not read {}", path.display()))?;
    if raw.len() > MAX_PASSPHRASE_BYTES {
        raw.zeroize();
        anyhow::bail!("the passphrase file's first line is too long");
    }
    finish(raw)
}

/// The first line of `input`, leaving the rest of it unread.
pub(crate) fn from_first_line(input: &mut impl BufRead) -> anyhow::Result<SecretString> {
    let mut raw = Vec::new();
    input
        .take(MAX_PASSPHRASE_BYTES as u64 + 1)
        .read_until(b'\n', &mut raw)
        .context("could not read the passphrase from stdin")?;
    if raw.len() > MAX_PASSPHRASE_BYTES || raw.last() != Some(&b'\n') {
        raw.zeroize();
        anyhow::bail!("expected the passphrase on the first line of stdin");
    }
    finish(raw)
}

/// Ask on the terminal; `confirm` asks twice and requires a minimum length.
pub(crate) fn prompt(confirm: bool) -> anyhow::Result<SecretString> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        anyhow::bail!(
            "no terminal to ask for the passphrase; pass --passphrase-file <file> instead"
        );
    }
    let ask = |label: &str| -> anyhow::Result<SecretString> {
        eprint!("{label}");
        std::io::stderr().flush()?;
        let line = crate::commands::onboard::prompts::read_masked_line()
            .context("could not read the passphrase")?;
        eprintln!();
        finish(line.into_bytes())
    };
    if !confirm {
        return ask("Passphrase for the setup file: ");
    }
    eprintln!(
        "Choose a passphrase for the setup file. It protects your provider keys; you will \
         type it again on the other computer. ZLO cannot recover it."
    );
    let first = ask("Passphrase: ")?;
    if first.expose_secret().chars().count() < MIN_NEW_PASSPHRASE_CHARS {
        anyhow::bail!("use at least {MIN_NEW_PASSPHRASE_CHARS} characters");
    }
    let second = ask("Same passphrase again: ")?;
    if first.expose_secret() != second.expose_secret() {
        anyhow::bail!("the two passphrases differ");
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_is_the_passphrase_and_the_rest_stays() {
        let mut input = std::io::Cursor::new(b"open sesame\r\nARCHIVE".to_vec());
        let pass = from_first_line(&mut input).expect("pass");
        assert_eq!(pass.expose_secret(), "open sesame");
        let mut rest = Vec::new();
        input.read_to_end(&mut rest).expect("rest");
        assert_eq!(rest, b"ARCHIVE");
        assert!(from_first_line(&mut std::io::Cursor::new(b"no newline".to_vec())).is_err());
        assert!(from_first_line(&mut std::io::Cursor::new(b"\n".to_vec())).is_err());
    }

    #[test]
    fn file_passphrase_is_its_first_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pass");
        std::fs::write(&path, "from a file\nignored\n").expect("write");
        assert_eq!(
            from_file(&path).expect("pass").expose_secret(),
            "from a file"
        );
    }
}
