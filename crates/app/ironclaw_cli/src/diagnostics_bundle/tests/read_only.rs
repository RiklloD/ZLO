use super::*;

/// Path, kind, permissions, length, modification time and contents of every
/// entry under `root` (symlinks not followed).
fn snapshot(root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).expect("stat");
            let contents = if meta.is_file() {
                std::fs::read(&path).expect("read")
            } else {
                Vec::new()
            };
            if meta.is_dir() {
                stack.push(path.clone());
            }
            out.push((
                path,
                format!(
                    "{:?} {:?} {} {:?} {:?}",
                    meta.file_type(),
                    meta.permissions(),
                    meta.len(),
                    meta.modified().ok(),
                    contents
                ),
            ));
        }
    }
    out.sort();
    out
}

#[test]
fn bundle_leaves_a_populated_home_byte_for_byte_unchanged() {
    let (tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    let storage = home.join("local-dev");
    write(
        &home.join("webui-token"),
        "zlo-diag-webui-token-canary-0123456789abcdef",
    );
    write(
        &home.join("config.toml"),
        "[llm.default]\nprovider_id = \"openai\"\n",
    );
    write(&home.join("providers.json"), "[]");
    write(
        &storage.join(".reborn-local-dev-secrets-master-key"),
        &"ab".repeat(32),
    );
    write(
        &storage.join("reborn-local-dev.db"),
        "SQLite format 3\0rows",
    );
    write(&storage.join("workspace").join("notes.md"), "memory");
    write(
        &storage
            .join("system")
            .join("extensions")
            .join("demo")
            .join("manifest.toml"),
        "id = \"demo\"\nversion = \"1.0.0\"\n",
    );
    write(
        &home.join("logs").join("serve.stderr.log"),
        "line one\nline two\n",
    );
    write(
        &home.join("logs").join("serve.stdout.log"),
        "partial line without newline",
    );
    let before = snapshot(tmp.path());
    assert!(before.len() > 10, "the home is populated: {before:?}");

    let (_out_dir, out, result) = run_bundle(&context, &[], true);
    result.expect("first write");
    assert_eq!(
        snapshot(tmp.path()),
        before,
        "the bundle must not touch the home"
    );

    let options = BundleOptions {
        out: out.clone(),
        include_logs: true,
        force: false,
    };
    let inputs = BundleInputs {
        service: ServiceStateDto::Unknown,
        env: &[],
    };
    let error = write_bundle_with(&context, &options, &inputs)
        .err()
        .expect("second write without --force must fail");
    assert!(error.to_string().contains("--force"), "{error}");
    let manifest: Value =
        serde_json::from_slice(&read_bundle(&out)["manifest.json"]).expect("manifest json");
    assert_eq!(manifest["format_version"], BUNDLE_FORMAT_VERSION);
    assert_eq!(snapshot(tmp.path()), before);
}

#[test]
fn bundle_does_not_create_a_missing_home() {
    let (_tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    let (_out_dir, _out, result) = run_bundle(&context, &[], true);
    result.expect("bundle writes");
    assert!(!home.exists(), "the bundle must not create the home");
}
