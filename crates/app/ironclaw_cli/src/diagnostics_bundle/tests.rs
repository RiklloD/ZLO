use std::collections::BTreeMap;

use super::*;

/// Built at runtime so upstream secret scanners do not match the literal.
fn slack_like() -> String {
    format!("xoxb-{}-{}", "4242424242", "diagnosticsBundleLeakCanary")
}

const WEBUI_TOKEN: &str = "zlo-diag-webui-token-canary-0123456789abcdef";
const PROVIDER_KEY: &str = "provider-key-canary-value-77777";
const DB_PASSWORD: &str = "dbpass-canary-31337";
const MASTER_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CONVERSATION: &str = "Please remember my blue umbrella from the Tuesday chat";
const MEMORY: &str = "Ana prefers oolong tea over coffee in the afternoon";

fn read_bundle(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let file = std::fs::File::open(path).expect("open bundle");
    let mut archive = zip::ZipArchive::new(file).expect("bundle is a zip");
    let mut out = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).expect("entry");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("read entry");
        out.insert(entry.name().to_string(), bytes);
    }
    out
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

/// Seed a home with every kind of material the bundle must never carry,
/// write the bundle through the same entry point `doctor --bundle` uses,
/// and assert none of those bytes appear in any entry or entry name.
#[test]
fn bundle_carries_no_seeded_secret_or_conversation_bytes() {
    let (_tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    let storage = home.join("local-dev");

    write(&home.join("webui-token"), WEBUI_TOKEN);
    write(
        &home.join("config.toml"),
        "[llm.default]\nprovider_id = \"openai\"\napi_key_env = \"ZLO_DIAG_TEST_PROVIDER_KEY\"\n\
         base_url = \"https://llm.example.com/v1\"\n",
    );
    write(
        &home.join(".env"),
        &format!("OPENAI_API_KEY={PROVIDER_KEY}\n"),
    );
    write(
        &storage.join(".reborn-local-dev-secrets-master-key"),
        MASTER_KEY,
    );
    write(
        &storage.join("reborn-local-dev.db"),
        &format!("SQLite format 3\0{CONVERSATION}\0{}", slack_like()),
    );
    write(&storage.join("workspace").join("notes.md"), MEMORY);
    write(
        &storage
            .join("tenants")
            .join("t")
            .join("threads")
            .join("1.json"),
        &format!("{{\"content\":\"{CONVERSATION}\"}}"),
    );
    write(
        &storage
            .join("system")
            .join("extensions")
            .join("demo")
            .join("manifest.toml"),
        "schema_version = \"reborn.extension_manifest.v3\"\nid = \"demo\"\nversion = \"9.8.7\"\n\
         description = \"demo package\"\n",
    );
    write(
        &home.join("logs").join("serve.stderr.log"),
        &format!(
            "\u{1b}[32m INFO\u{1b}[0m webui ready on 127.0.0.1:3000\n\
             login: http://127.0.0.1:3000/login?token={WEBUI_TOKEN}\n\
             connecting to postgres://zlo:{DB_PASSWORD}@db.internal:5432/zlo\n\
             request header Authorization: Bearer {PROVIDER_KEY}\n\
             provider api_key={PROVIDER_KEY} slack={}\n\
             heartbeat ok\n",
            slack_like()
        ),
    );

    let env = |name: &str| match name {
        "ZLO_DIAG_TEST_PROVIDER_KEY" | "OPENAI_API_KEY" => Some(PROVIDER_KEY.to_string()),
        _ => None,
    };
    let out_dir = tempfile::tempdir().expect("out dir");
    let out = out_dir.path().join("bundle.zip");
    let report = write_bundle_with(
        &context,
        &BundleOptions {
            out: out.clone(),
            include_logs: true,
            force: false,
        },
        &BundleInputs {
            service: ServiceStateDto::Unknown,
            env: &env,
        },
    )
    .expect("bundle writes");

    let entries = read_bundle(&out);
    assert_eq!(entries.keys().cloned().collect::<Vec<_>>(), {
        let mut names = report.entries.clone();
        names.sort();
        names
    });
    for expected in [
        "manifest.json",
        "doctor.json",
        "status.json",
        "system.json",
        "config.json",
        "llm.json",
        "extensions.json",
        "logs/serve.stderr.log.tail.txt",
    ] {
        assert!(
            entries.contains_key(expected),
            "missing {expected}: {:?}",
            entries.keys()
        );
    }

    let forbidden = [
        WEBUI_TOKEN.to_string(),
        PROVIDER_KEY.to_string(),
        DB_PASSWORD.to_string(),
        MASTER_KEY.to_string(),
        CONVERSATION.to_string(),
        "blue umbrella".to_string(),
        MEMORY.to_string(),
        "oolong".to_string(),
        slack_like(),
    ];
    for (name, bytes) in &entries {
        for needle in &forbidden {
            assert!(
                !contains(bytes, needle.as_bytes()) && !name.contains(needle.as_str()),
                "{name} leaks {needle:?}:\n{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    // The useful parts survive.
    let text = |name: &str| String::from_utf8_lossy(&entries[name]).into_owned();
    let log = text("logs/serve.stderr.log.tail.txt");
    assert!(log.contains("webui ready on 127.0.0.1:3000"), "{log}");
    assert!(log.contains("heartbeat ok"), "{log}");
    assert!(
        log.contains("postgres://[REDACTED]@db.internal:5432/zlo"),
        "{log}"
    );
    assert!(!log.contains('\u{1b}'), "ANSI escapes are stripped: {log}");
    let extensions = text("extensions.json");
    assert!(
        extensions.contains("\"demo\"") && extensions.contains("9.8.7"),
        "{extensions}"
    );
    let config = text("config.json");
    assert!(
        config.contains("ZLO_DIAG_TEST_PROVIDER_KEY"),
        "env var names are kept: {config}"
    );
    assert!(config.contains("llm.default.provider_id"), "{config}");
    let system = text("system.json");
    assert!(system.contains(env!("CARGO_PKG_VERSION")), "{system}");
    assert!(system.contains(std::env::consts::OS), "{system}");
    let doctor = text("doctor.json");
    assert!(doctor.contains("text_only_driver"), "{doctor}");
}

#[test]
fn bundle_is_read_only_and_refuses_to_clobber() {
    let (tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    let before: Vec<_> = walk(tmp.path());

    let out_dir = tempfile::tempdir().expect("out dir");
    let out = out_dir.path().join("bundle.zip");
    let env = |_: &str| None;
    let inputs = BundleInputs {
        service: ServiceStateDto::Unknown,
        env: &env,
    };
    let options = BundleOptions {
        out: out.clone(),
        include_logs: true,
        force: false,
    };
    write_bundle_with(&context, &options, &inputs).expect("first write");
    assert!(!home.exists(), "the bundle must not create the home");
    assert_eq!(
        walk(tmp.path()),
        before,
        "the bundle must not touch the home"
    );

    let error = write_bundle_with(&context, &options, &inputs)
        .err()
        .expect("second write without --force must fail");
    assert!(error.to_string().contains("--force"), "{error}");

    let manifest: Value =
        serde_json::from_slice(&read_bundle(&out)["manifest.json"]).expect("manifest json");
    assert_eq!(manifest["format_version"], BUNDLE_FORMAT_VERSION);
}

#[test]
fn log_tail_is_bounded_and_drops_the_cut_line() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("big.log");
    let line = "x".repeat(99);
    let body: String = (0..5000).map(|i| format!("{i:05} {line}\n")).collect();
    std::fs::write(&path, body).expect("write");
    let tail = tail_log(&path).expect("tail").expect("present");
    assert!(tail.len() as u64 <= LOG_TAIL_BYTES);
    assert!(tail.lines().count() <= LOG_TAIL_LINES);
    assert!(tail.ends_with(&format!("04999 {line}")));
    assert!(
        tail.lines().all(|l| l.len() == 105),
        "no partial first line"
    );
    assert!(
        tail_log(&dir.path().join("absent.log"))
            .expect("ok")
            .is_none()
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
