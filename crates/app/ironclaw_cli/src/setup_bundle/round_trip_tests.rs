//! Export → import between two temporary "computers" (temp homes only: no
//! test here resolves the real `~/.zlo`, keychain or backend).

use std::path::{Path, PathBuf};

use ironclaw_composition::setup_transfer::{execute_sql_for_test, query_strings_for_test};
use secrecy::{ExposeSecret, SecretString};

use super::export::{ExportRequest, export_setup};
use super::format::{BundleWriter, MIN_ITERATIONS, TAG_END, TAG_FILE, TAG_MANIFEST, TAG_SYMLINK};
use super::import::{ImportRequest, import_setup};
use super::{BundleError, ItemKind, Manifest, ManifestItem, SourceInfo, ZloBuild};

const PASS: &str = "moving day passphrase";

struct Computer {
    _dir: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
}

fn computer(home_name: &str) -> Computer {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join(home_name);
    std::fs::create_dir_all(&home).expect("home");
    let data = home.join(".zlo");
    Computer {
        _dir: dir,
        home,
        data,
    }
}

fn pass(text: &str) -> SecretString {
    SecretString::from(text.to_string())
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, contents).expect("write");
}

async fn key_store(root: &Path) -> ironclaw_operator::LlmKeyStore {
    let store = ironclaw_composition::open_standalone_secret_store(root)
        .await
        .expect("secret store");
    ironclaw_operator::LlmKeyStore::new(
        ironclaw_composition::RuntimeOperatorSecretValueStore::shared(store),
    )
}

/// A source computer with records, a provider key, settings and the
/// machine-local state an export must leave out.
async fn seed_source(source: &Computer, environment_id: &str) {
    let root = source.data.join("local-dev");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join(ironclaw_composition::setup_transfer::SECRETS_MASTER_KEY_FILE),
        format!(
            "{}\n",
            ironclaw_secrets::keychain::generate_master_key_hex()
        ),
    )
    .expect("master key");
    let store = ironclaw_composition::open_standalone_secret_store(&root)
        .await
        .expect("store");
    ironclaw_operator::LlmKeyStore::new(
        ironclaw_composition::RuntimeOperatorSecretValueStore::shared(store),
    )
    .put_plaintext("nearai", "sk-moved-with-me".to_string())
    .await
    .expect("put key");
    let home = source.home.display().to_string();
    let profile = format!(
        "INSERT INTO orch_profiles VALUES ('t','u','work','{{\"working_folder\":\"{home}/src/app\"}}',1,1,NULL)"
    );
    let environment = format!(
        "INSERT INTO root_filesystem_entries (path, contents) VALUES \
         ('/tenants/default/shared/reborn-environment/environment.json', '{environment_id}')"
    );
    execute_sql_for_test(
        &ironclaw_composition::setup_transfer::standalone_database_file(&root),
        &[
            "CREATE TABLE IF NOT EXISTS orch_threads (thread_id TEXT PRIMARY KEY, title TEXT)",
            "INSERT INTO orch_threads VALUES ('th1', 'Moving day')",
            "CREATE TABLE IF NOT EXISTS orch_profiles (tenant_id TEXT, user_id TEXT, profile_id TEXT, settings TEXT, created_at INTEGER, updated_at INTEGER, archived_at INTEGER)",
            &profile,
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/memory/tenants/t/users/u/MEMORY.md', 'likes tea')",
            &environment,
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/tenants/default/shared/reborn-environment/devices/d1.json', 'device-token-hash')",
        ],
    )
    .await;
    write(
        &source.data.join("config.toml"),
        &format!("[update]\nmethod = \"source\"\nrepo = \"{home}/ironclaw\"\n"),
    );
    write(&source.data.join("providers.json"), "{\"providers\":[]}");
    write(&root.join("workspace/notes.md"), "notes");
    write(&root.join("system/skills/demo/SKILL.md"), "# demo");
    for left_out in [
        "webui-token",
        "logs/serve.stdout.log",
        "bin/zlo",
        "update/run.json",
        "retired-com.ironclaw.reborn.plist",
    ] {
        write(&source.data.join(left_out), "machine-local");
    }
}

async fn export(source: &Computer, out: &Path) {
    export_setup(ExportRequest {
        data_folder: &source.data,
        user_home: &source.home,
        hostname: "ana-mac",
        out,
        passphrase: &pass(PASS),
        iterations: MIN_ITERATIONS,
    })
    .await
    .expect("export");
}

async fn import(
    target: &Computer,
    archive: &[u8],
    passphrase: &str,
    replace: bool,
) -> anyhow::Result<super::import::ImportReport> {
    import_setup(
        archive,
        ImportRequest {
            data_folder: &target.data,
            user_home: &target.home,
            passphrase: &pass(passphrase),
            replace,
            now: chrono::Utc::now(),
        },
    )
    .await
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_setup_moves_to_a_fresh_computer_and_its_provider_key_still_decrypts() {
    let mac = computer("Users-ana");
    seed_source(&mac, "mac-environment").await;
    let out = mac.home.join("setup.zlobundle");
    export(&mac, &out).await;
    let archive = std::fs::read(&out).expect("archive");
    assert!(
        !archive
            .windows(b"sk-moved-with-me".len())
            .any(|w| w == b"sk-moved-with-me"),
        "the archive is encrypted"
    );

    let server = computer("home-ana");
    let report = import(&server, &archive, PASS, false)
        .await
        .expect("import");
    assert_eq!(report.master_keys, 1);
    let (name, summary) = &report.databases[0];
    assert_eq!(name, "local-dev/reborn-local-dev.db");
    assert_eq!(summary.threads, 1);
    assert_eq!(summary.profiles, 1);
    assert_eq!(summary.memory_documents, 1);
    assert_eq!(summary.secrets, 1, "the provider key record is counted");
    assert!(report.moved_aside.is_none());

    // The provider key decrypts on the new computer.
    let root = server.data.join("local-dev");
    let key = key_store(&root)
        .await
        .read("nearai")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(key.expose_secret(), "sk-moved-with-me");

    // Settings and files came along, with paths moved to the new home.
    let config = std::fs::read_to_string(server.data.join("config.toml")).expect("config");
    assert!(
        config.contains(&format!("repo = \"{}/ironclaw\"", server.home.display())),
        "{config}"
    );
    assert!(server.data.join("providers.json").is_file());
    assert!(root.join("workspace/notes.md").is_file());
    assert!(root.join("system/skills/demo/SKILL.md").is_file());
    let new_folder = format!("{}/src/app", server.home.display());
    assert_eq!(report.missing_folders, std::slice::from_ref(&new_folder));
    let db = ironclaw_composition::setup_transfer::standalone_database_file(&root);
    let settings = query_strings_for_test(&db, "SELECT settings FROM orch_profiles").await;
    assert!(settings[0].contains(&new_folder), "{settings:?}");

    // Not carried: environment identity, devices, machine-local files.
    let environment = query_strings_for_test(
        &db,
        "SELECT path FROM root_filesystem_entries WHERE path LIKE '%reborn-environment%'",
    )
    .await;
    assert!(environment.is_empty(), "{environment:?}");
    for left_out in [
        "webui-token",
        "logs",
        "bin",
        "update",
        "retired-com.ironclaw.reborn.plist",
    ] {
        assert!(
            !server.data.join(left_out).exists(),
            "{left_out} must not move"
        );
    }
    // The source is untouched.
    assert!(mac.data.join("webui-token").is_file());
    assert_eq!(
        query_strings_for_test(
            &ironclaw_composition::setup_transfer::standalone_database_file(
                &mac.data.join("local-dev")
            ),
            "SELECT path FROM root_filesystem_entries WHERE path LIKE '%reborn-environment%'",
        )
        .await
        .len(),
        2
    );
}

#[tokio::test]
async fn wrong_passphrase_tamper_and_newer_format_write_nothing() {
    let mac = computer("Users-ana");
    seed_source(&mac, "mac-environment").await;
    let out = mac.home.join("setup.zlobundle");
    export(&mac, &out).await;
    let archive = std::fs::read(&out).expect("archive");
    let server = computer("home-ana");
    let before = listing(&server.home);

    let wrong = import(&server, &archive, "not the passphrase", false)
        .await
        .expect_err("wrong passphrase");
    assert!(matches!(
        wrong.downcast_ref::<BundleError>(),
        Some(BundleError::WrongPassphraseOrDamaged)
    ));
    assert_eq!(listing(&server.home), before, "nothing written");

    let mut tampered = archive.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0x40;
    let error = import(&server, &tampered, PASS, false)
        .await
        .expect_err("tampered");
    assert!(
        matches!(
            error.downcast_ref::<BundleError>(),
            Some(BundleError::Damaged | BundleError::WrongPassphraseOrDamaged)
        ),
        "{error}"
    );
    assert_eq!(listing(&server.home), before, "staging removed");

    let mut newer = archive;
    newer[8..10].copy_from_slice(&(super::format::FORMAT_VERSION + 1).to_be_bytes());
    let error = import(&server, &newer, PASS, false)
        .await
        .expect_err("newer");
    assert!(error.to_string().contains("newer ZLO"), "{error}");
    assert_eq!(listing(&server.home), before);
}

#[tokio::test]
async fn existing_setup_needs_replace_and_is_moved_aside_keeping_this_computers_identity() {
    let mac = computer("Users-ana");
    seed_source(&mac, "mac-environment").await;
    let out = mac.home.join("setup.zlobundle");
    export(&mac, &out).await;
    let archive = std::fs::read(&out).expect("archive");

    let server = computer("home-ana");
    seed_source(&server, "server-environment").await;
    write(
        &server.data.join("config.toml"),
        "# the server's own config\n",
    );

    let error = import(&server, &archive, PASS, false)
        .await
        .expect_err("needs --replace");
    assert!(error.to_string().contains("--replace"), "{error}");
    assert_eq!(
        std::fs::read_to_string(server.data.join("config.toml")).expect("config"),
        "# the server's own config\n"
    );

    let report = import(&server, &archive, PASS, true)
        .await
        .expect("replace");
    let aside = report.moved_aside.clone().expect("moved aside");
    assert!(
        aside
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(".zlo.before-import-")),
        "{aside:?}"
    );
    assert_eq!(
        std::fs::read_to_string(aside.join("config.toml")).expect("old config kept"),
        "# the server's own config\n"
    );
    assert!(aside.join("local-dev/reborn-local-dev.db").is_file());
    // The installed binary stays where it runs from.
    assert!(server.data.join("bin/zlo").is_file());
    assert!(!aside.join("bin").exists());
    // This computer keeps its own identity; the Mac's did not come along.
    assert_eq!(report.environment_records_kept, 2);
    let identity = query_strings_for_test(
        &ironclaw_composition::setup_transfer::standalone_database_file(
            &server.data.join("local-dev"),
        ),
        "SELECT CAST(contents AS TEXT) FROM root_filesystem_entries \
         WHERE path LIKE '%reborn-environment/environment.json'",
    )
    .await;
    assert_eq!(identity, ["server-environment"]);
}

/// Hand-build an archive: `records` as (tag, name, bytes), then a manifest.
fn crafted(records: &[(u8, &str, &[u8])], manifest_items: Vec<ManifestItem>) -> Vec<u8> {
    let mut writer = BundleWriter::new(Vec::new(), &pass(PASS), MIN_ITERATIONS).expect("writer");
    let mut record = |tag: u8, name: &str, bytes: &[u8]| {
        writer.write_all(&[tag]).expect("tag");
        writer
            .write_all(&(name.len() as u16).to_be_bytes())
            .expect("len");
        writer.write_all(name.as_bytes()).expect("name");
        writer
            .write_all(&(bytes.len() as u64).to_be_bytes())
            .expect("size");
        writer.write_all(bytes).expect("bytes");
    };
    for (tag, name, bytes) in records {
        record(*tag, name, bytes);
    }
    let manifest = Manifest {
        format_version: super::format::FORMAT_VERSION,
        created_at: chrono::Utc::now(),
        source: SourceInfo {
            os: "macos".into(),
            arch: "aarch64".into(),
            hostname: "x".into(),
            user_home: "/Users/x".into(),
            data_folder: "/Users/x/.zlo".into(),
        },
        zlo: ZloBuild::current(),
        items: manifest_items,
        left_out: Vec::new(),
    };
    let bytes = serde_json::to_vec(&manifest).expect("manifest");
    record(TAG_MANIFEST, "manifest.json", &bytes);
    writer.write_all(&[TAG_END]).expect("end");
    writer.finish().expect("finish")
}

#[tokio::test]
async fn traversal_symlink_and_mismatching_entries_are_refused() {
    for (name, tag) in [
        ("../outside.txt", TAG_FILE),
        ("/etc/zlo-owned", TAG_FILE),
        ("a/../../outside", TAG_FILE),
        ("link", TAG_SYMLINK),
    ] {
        let server = computer("home-ana");
        let archive = crafted(&[(tag, name, b"x")], Vec::new());
        let error = import(&server, &archive, PASS, false)
            .await
            .expect_err("refused");
        assert!(
            matches!(
                error.downcast_ref::<BundleError>(),
                Some(BundleError::UnsafeEntry { .. })
            ),
            "{name}: {error}"
        );
        assert!(!server.data.exists(), "{name}: nothing imported");
        assert!(
            !server
                .home
                .parent()
                .expect("p")
                .join("outside.txt")
                .exists()
        );
        assert_eq!(listing(&server.home), Vec::<String>::new());
    }

    // A file the list does not describe (wrong hash).
    let server = computer("home-ana");
    let archive = crafted(
        &[(TAG_FILE, "config.toml", b"x = 1\n")],
        vec![ManifestItem {
            path: "config.toml".into(),
            kind: ItemKind::File,
            size: 6,
            sha256: "00".repeat(32),
        }],
    );
    let error = import(&server, &archive, PASS, false)
        .await
        .expect_err("mismatch");
    assert!(
        matches!(
            error.downcast_ref::<BundleError>(),
            Some(BundleError::ManifestMismatch(_))
        ),
        "{error}"
    );
    assert!(!server.data.exists());
}
