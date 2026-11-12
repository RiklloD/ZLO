use super::*;

const LINES: [&str; 3] = [
    "MULTIline-secret-first-AAAAAAAA",
    "MULTIline-secret-second-BBBBBBBB",
    "MULTIline-secret-third-CCCCCCCC",
];

fn secret() -> String {
    LINES.join("\n")
}

fn forbidden() -> Vec<String> {
    LINES.iter().map(|line| (*line).to_string()).collect()
}

/// Write `body` as the service log, bundle it with the multiline secret in
/// the environment, and return the archived tail (after asserting no entry
/// carries any line of the secret).
fn archived_tail(body: &str) -> String {
    let (_tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    write(&home.join("logs").join("serve.stderr.log"), body);
    let env = env(&[("ZLO_DIAG_MULTILINE_TOKEN", &secret())]);
    let (_out_dir, out, result) = run_bundle(&context, &env, true);
    result.expect("bundle writes");
    let entries = read_bundle(&out);
    assert_no_leaks(&entries, &forbidden());
    String::from_utf8_lossy(&entries["logs/serve.stderr.log.tail.txt"]).into_owned()
}

#[test]
fn a_known_multiline_secret_across_the_byte_cutoff_is_not_archived_in_part() {
    let filler = format!("{}\n", "f".repeat(99));
    let after = format!("{}\n", "a".repeat(99));
    // Place the secret so the LOG_TAIL_BYTES cutoff falls in the middle of
    // its second line: everything from there to the end is exactly
    // LOG_TAIL_BYTES long.
    let half = LINES[1].len() / 2;
    let fixed = (LINES[1].len() - half) + 1 + LINES[2].len() + 1 + "last line\n".len();
    let rest = LOG_TAIL_BYTES as usize - fixed;
    let after_lines = rest / after.len() - 1;
    let pad = rest - after_lines * after.len() - 1;
    let body = format!(
        "{}token={}\n{}{}\nlast line\n",
        filler.repeat(1200),
        secret(),
        after.repeat(after_lines),
        "p".repeat(pad)
    );
    let cut = body.len() - LOG_TAIL_BYTES as usize;
    assert_eq!(&body[cut - half..cut + 1], &LINES[1][..=half]);
    let tail = archived_tail(&body);
    assert!(tail.ends_with("last line"), "{}", &tail[tail.len() - 40..]);
    assert!(tail.len() as u64 <= LOG_TAIL_BYTES);
}

#[test]
fn a_known_multiline_secret_across_the_line_cutoff_is_not_archived_in_part() {
    // Short lines, so the LOG_TAIL_LINES cutoff binds before the byte cutoff
    // and falls between the secret's first and second line.
    let mut lines: Vec<String> = (0..3000).map(|i| format!("l{i}")).collect();
    let at = 3000 - LOG_TAIL_LINES - 1;
    lines[at] = format!("key={}", LINES[0]);
    lines[at + 1] = LINES[1].to_string();
    lines[at + 2] = LINES[2].to_string();
    let tail = archived_tail(&(lines.join("\n") + "\n"));
    assert!(tail.lines().count() <= LOG_TAIL_LINES);
    assert!(tail.ends_with("l2999"), "{tail}");
}

#[test]
fn a_secret_still_being_written_at_the_end_is_not_archived_in_part() {
    let body = format!(
        "start ok\nkey={}\n{}\n{}",
        LINES[0],
        LINES[1],
        &LINES[2][..10]
    );
    let tail = archived_tail(&body);
    assert!(tail.starts_with("start ok"), "{tail}");
    assert!(tail.ends_with(redact::REDACTED), "{tail}");
}

#[test]
fn log_tail_is_bounded_and_drops_the_cut_line() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("big.log");
    let line = "x".repeat(99);
    let body: String = (0..5000).map(|i| format!("{i:05} {line}\n")).collect();
    std::fs::write(&path, body).expect("write");
    let tail = tail_log(&path, &[]).expect("tail").expect("present");
    assert!(tail.len() as u64 <= LOG_TAIL_BYTES);
    assert!(tail.lines().count() <= LOG_TAIL_LINES);
    assert!(tail.ends_with(&format!("04999 {line}")));
    assert!(
        tail.lines().all(|l| l.len() == 105),
        "no partial first line"
    );
    assert!(
        tail_log(&dir.path().join("absent.log"), &[])
            .expect("ok")
            .is_none()
    );
}
