// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]
use super::*;

const WEBUI_TOKEN: &str = "zlo-diag-webui-token-canary-0123456789abcdef";
const PROVIDER_KEY: &str = "provider-key-canary/value+77777";
const SERVICE_TOKEN: &str = "svc-token-canary-5Hq2Lm9W";
const MASTER_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DB_PASSWORD: &str = "dbpass-canary-31337";
const CONVERSATION: &str = "Please remember my blue umbrella from the Tuesday chat";
const MEMORY: &str = "Ana prefers oolong tea over coffee in the afternoon";

fn b64(value: &str, url_safe: bool) -> String {
    use std::fmt::Write as _;
    let alphabet: &[u8] = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut out = String::new();
    for chunk in value.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let _ = write!(
                    out,
                    "{}",
                    char::from(alphabet[((n >> (18 - 6 * i)) & 63) as usize])
                );
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn json_escaped(value: &str) -> String {
    value
        .chars()
        .map(|c| format!("\\u{:04x}", c as u32))
        .collect()
}

fn percent(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Seed a home with secrets in every place the bundle reads (token file,
/// master key file, config, env, and logs in every spelling from the review
/// findings), write the bundle through the `doctor --bundle` entry point, and
/// assert no entry carries any of them in any form.
#[test]
fn bundle_carries_no_seeded_secret_in_any_form() {
    let (_tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    let storage = home.join("local-dev");

    // Secrets the bundle does not know: only shapes and keys can catch them.
    let unknown: Vec<String> = (0..16).map(|i| format!("Unk{i:02}x7Qp2mWx9LrT")).collect();
    let telegram = format!("{}:{}", "1234567890", "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw");
    let notion = format!("{}{}", "secret_", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4");
    let ntn = format!("{}{}", "ntn_", "4242424242aBcDeFgHiJkLmNoPqRs");
    let refresh = format!("{}{}", "1//", "0gAbCdEfGhIjKlMnOpQrStUvWxYz-_12");
    let pem_body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSj";

    write(&home.join("webui-token"), WEBUI_TOKEN);
    write(
        &storage.join(".reborn-local-dev-secrets-master-key"),
        MASTER_KEY,
    );
    write(
        &home.join("config.toml"),
        "[llm.default]\nprovider_id = \"openai\"\napi_key_env = \"ZLO_DIAG_TEST_PROVIDER_KEY\"\n\
         base_url = \"https://llm.example.com/v1\"\n",
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
        "schema_version = \"reborn.extension_manifest.v3\"\nid = \"demo\"\nversion = \"9.8.7\"\n",
    );

    let u = &unknown;
    let mut log = vec![
        "\u{1b}[32m INFO\u{1b}[0m webui ready on 127.0.0.1:3000".to_string(),
        format!("login: http://127.0.0.1:3000/login?token={WEBUI_TOKEN}"),
        format!("connecting to postgres://zlo:{DB_PASSWORD}@db.internal:5432/zlo"),
        // Finding 1: spacing, quoting, flags.
        format!("authorization=Bearer {}", u[0]),
        format!("authorization=\"Bearer {}\"", u[1]),
        format!("Authorization:Bearer {}", u[2]),
        format!("Authorization:\tBearer\t{}", u[3]),
        format!("sent Bearer\t{}", u[4]),
        format!("password=\"{} two words\"", u[5]),
        format!("api_key=\"{} words\"", u[6]),
        format!("exec zlo --token {}", u[7]),
        format!("exec zlo --api-key {}", u[8]),
        // Finding 2: known values JSON-escaped, percent-encoded, base64.
        format!("{{\"note\":\"{}\"}}", json_escaped(PROVIDER_KEY)),
        format!("detail={}", json_escaped(WEBUI_TOKEN)),
        format!("cb=https://h/cb#{}", percent(PROVIDER_KEY)),
        format!("blob {}", b64(SERVICE_TOKEN, false)),
        format!("blob {}", b64(MASTER_KEY, true).trim_end_matches('=')),
        format!(
            "header Proxy-Thing: Basic {}",
            b64(&format!("ana:{PROVIDER_KEY}"), false)
        ),
        format!("raw {MASTER_KEY} and {SERVICE_TOKEN}"),
        // Finding 5: store-only shapes.
        format!("GET https://api.telegram.org/bot{telegram}/getMe"),
        format!("bot {telegram} ok"),
        format!("notion {notion} {ntn}"),
        format!("google {refresh}"),
        format!("pass={} pwd={} auth=user:{}", u[9], u[10], u[11]),
        // Finding 6: non-string values under sensitive keys, secret keys.
        format!(
            "body={{\"token\":{{\"value\":\"{}\"}},\"api_key\":[\"{}\"],\"pin\":918273}}",
            u[12], u[13]
        ),
        format!(
            "{{\"{SERVICE_TOKEN}\":\"x\",\"{}\":2,\"ok\":1}}",
            slack_like()
        ),
        format!("provider api_key={PROVIDER_KEY} slack={}", slack_like()),
        "-----BEGIN PRIVATE KEY-----".to_string(),
        pem_body.to_string(),
        "-----END PRIVATE KEY-----".to_string(),
        "heartbeat ok".to_string(),
    ];
    log.push(String::new());
    write(&home.join("logs").join("serve.stderr.log"), &log.join("\n"));

    let env = env(&[
        ("ZLO_DIAG_TEST_PROVIDER_KEY", PROVIDER_KEY),
        ("ACME_SERVICE_TOKEN", SERVICE_TOKEN),
        ("SECRETS_MASTER_KEY", MASTER_KEY),
        ("PATH", "/usr/bin:/bin"),
    ]);
    let (_out_dir, out, result) = run_bundle(&context, &env, true);
    let report = result.expect("bundle writes");
    assert!(report.degraded.is_empty(), "{:?}", report.degraded);

    let entries = read_bundle(&out);
    let mut names = report.entries.clone();
    names.sort();
    assert_eq!(entries.keys().cloned().collect::<Vec<_>>(), names);
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

    let mut forbidden: Vec<String> = unknown[..14].to_vec();
    forbidden.extend(
        [
            WEBUI_TOKEN,
            PROVIDER_KEY,
            SERVICE_TOKEN,
            MASTER_KEY,
            DB_PASSWORD,
            CONVERSATION,
            "blue umbrella",
            MEMORY,
            "oolong",
            "918273",
            "two words",
            pem_body,
            "AAHdqTcvCH1v",
            "a1B2c3D4e5F6",
            "4242424242aBcDeF",
            "0gAbCdEfGhIj",
        ]
        .map(String::from),
    );
    forbidden.push(slack_like());
    forbidden.push(b64(SERVICE_TOKEN, false));
    assert_no_leaks(&entries, &forbidden);

    // The useful parts survive.
    let text = |name: &str| String::from_utf8_lossy(&entries[name]).into_owned();
    let log = text("logs/serve.stderr.log.tail.txt");
    assert!(log.contains("webui ready on 127.0.0.1:3000"), "{log}");
    assert!(log.contains("heartbeat ok"), "{log}");
    assert!(
        log.contains("postgres://[REDACTED]@db.internal:5432/zlo"),
        "{log}"
    );
    assert!(log.contains("Authorization:\tBearer\t[REDACTED]"), "{log}");
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
    assert!(
        system.contains("\"effective_kind\": \"local_libsql\""),
        "{system}"
    );
    let doctor = text("doctor.json");
    assert!(doctor.contains("text_only_driver"), "{doctor}");
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).expect("manifest");
    assert_eq!(manifest["redaction_inputs"]["degraded"], false);
}

/// A known value in an encoding replacement does not enumerate (one byte
/// percent-encoded mid-value) survives scrubbing; the final check must
/// refuse the archive and write nothing.
#[test]
fn bundle_is_refused_when_a_known_secret_survives_scrubbing() {
    let (_tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    let partial = format!("{}%2D{}", &SERVICE_TOKEN[..3], &SERVICE_TOKEN[4..]);
    write(
        &home.join("logs").join("serve.stdout.log"),
        &format!("note {partial}\nok\n"),
    );
    let env = env(&[("ACME_SERVICE_TOKEN", SERVICE_TOKEN)]);
    let (_out_dir, out, result) = run_bundle(&context, &env, true);
    let error = result.err().expect("bundle must be refused");
    let message = error.to_string();
    assert!(
        message.contains("logs/serve.stdout.log.tail.txt"),
        "{message}"
    );
    assert!(message.contains("--no-logs"), "{message}");
    assert!(!out.exists(), "a refused bundle writes nothing");

    let (_out_dir, out, result) = run_bundle(&context, &env, false);
    result.expect("without logs the bundle writes");
    assert_no_leaks(&read_bundle(&out), &[SERVICE_TOKEN.to_string()]);
}

/// With config.toml and providers.json broken, env names still come from
/// the built-in catalog and a raw scan of config.toml; the manifest and the
/// report say the known-secret list is degraded.
#[test]
fn broken_config_degrades_visibly_and_still_knows_env_secrets() {
    let (_tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    write(
        &home.join("config.toml"),
        "[llm.default\napi_key_env = \"ZLO_DIAG_BROKEN_KEY\"\n",
    );
    write(&home.join("providers.json"), "{ not json");
    let builtin_name = ironclaw_operator::RebornProviderAdmin::builtin_credential_env_names()
        .into_iter()
        .find(|name| name.ends_with("_API_KEY"))
        .expect("catalog names an API key env var");
    let configured = "configured-key-canary-4815162342";
    let builtin = "builtin-key-canary-2718281828";
    let unset_default = "pg-url-canary-1414213562";
    write(
        &home.join("logs").join("serve.stderr.log"),
        &format!(
            "a {configured}\nb {}\nc {}\nd {unset_default}\n",
            b64(builtin, false),
            json_escaped(configured)
        ),
    );
    let env = env(&[
        ("ZLO_DIAG_BROKEN_KEY", configured),
        (builtin_name.as_str(), builtin),
        ("IRONCLAW_REBORN_POSTGRES_URL", unset_default),
    ]);
    let (_out_dir, out, result) = run_bundle(&context, &env, true);
    let report = result.expect("bundle writes");
    assert!(!report.degraded.is_empty());
    let entries = read_bundle(&out);
    assert_no_leaks(
        &entries,
        &[
            configured.to_string(),
            builtin.to_string(),
            unset_default.to_string(),
        ],
    );
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).expect("manifest");
    assert_eq!(manifest["redaction_inputs"]["degraded"], true, "{manifest}");
    let reasons = manifest["redaction_inputs"]["reasons"].to_string();
    assert!(reasons.contains("config.toml"), "{reasons}");
    let system: Value = serde_json::from_slice(&entries["system.json"]).expect("system");
    assert_eq!(
        system["storage"]["effective_kind"],
        "unknown (config.toml did not load)"
    );
}

#[test]
fn system_json_reports_the_real_storage_kind() {
    let (_tmp, context) = RebornCliContext::test_context();
    let home = context.boot_config().home().path().to_path_buf();
    write(
        &home.join("config.toml"),
        "[storage]\nbackend = \"postgres\"\n",
    );
    let (_out_dir, out, result) = run_bundle(&context, &[], false);
    result.expect("bundle writes");
    let system: Value = serde_json::from_slice(&read_bundle(&out)["system.json"]).expect("system");
    assert_eq!(system["storage"]["effective_kind"], "postgres", "{system}");
}
