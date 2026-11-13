//! Built-in `skill_install`: inline content, bundles, and the URL-fetch path.

#![deny(clippy::let_underscore_must_use)]
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_api::{
    action::NetworkMethod, http::RuntimeHttpEgressError, resource::ResourceEstimate,
    runtime::RuntimeKind,
};
use ironclaw_host_runtime::{
    HostRuntime, RuntimeCapabilityOutcome, SKILL_INSTALL_CAPABILITY_ID, SKILL_LIST_CAPABILITY_ID,
};
use serde_json::json;

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{
    context::*, doubles::*, fixtures::*, invoke::*, runtimes::*,
};

// arch-exempt: large-test-file, URL install tests share this first-party runtime harness; split plan #4062
#[tokio::test]
async fn builtin_skill_install_accepts_content_without_url_fetch() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let runtime = runtime_with_filesystem(filesystem);

    // `skill_install` advertises `EffectKind::Network` because the same
    // capability handles URL/GitHub installs, so `NetworkMode::Deny` rejects it
    // before content-only dispatch. This case keeps the non-fetch path covered.
    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"content": "---\nname: offline-helper\n---\nOffline prompt.\n"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts.clone(),
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["installed"], json!(true));
    assert_eq!(installed["name"], json!("offline-helper"));
    assert_eq!(installed["source"], json!("user"));

    let listed = invoke_with_context(
        &runtime,
        SKILL_LIST_CAPABILITY_ID,
        json!({}),
        execution_context_with_mounts([SKILL_LIST_CAPABILITY_ID], mounts),
    )
    .await
    .unwrap();
    assert_eq!(listed["count"], json!(1));
    assert_eq!(listed["skills"][0]["name"], json!("offline-helper"));
    assert_eq!(listed["skills"][0]["source"], json!("user"));
}

#[tokio::test]
async fn builtin_skill_install_accepts_and_replays_named_plain_markdown_content() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let runtime = runtime_with_filesystem(filesystem);
    let input = json!({
        "name": "daily digest email docs",
        "content": "# Daily Digest\n\nSummarize updates for an email.\n"
    });

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        input.clone(),
        // skill_install currently declares Network for URL installs too, so the
        // first-party harness needs a non-empty policy even for content input.
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts.clone(),
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["installed"], json!(true));
    assert_eq!(installed["name"], json!("daily-digest-email-docs"));
    assert_eq!(installed["source"], json!("user"));

    let replayed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        input,
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts.clone(),
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(replayed["installed"], json!(true));
    assert_eq!(replayed["name"], json!("daily-digest-email-docs"));

    let listed = invoke_with_context(
        &runtime,
        SKILL_LIST_CAPABILITY_ID,
        json!({}),
        execution_context_with_mounts([SKILL_LIST_CAPABILITY_ID], mounts),
    )
    .await
    .unwrap();
    assert_eq!(listed["count"], json!(1));
    assert_eq!(
        listed["skills"][0]["name"],
        json!("daily-digest-email-docs")
    );
}

/// An agent must not be able to forge install provenance: `source`/`source_url` are set by the
/// URL-fetch path, so accepting them inline would let an agent label its own output as fetched.
///
/// A `content` + `files` case was removed from here deliberately, not by accident -- rejecting it
/// was the bug this PR fixes.
#[tokio::test]
async fn builtin_skill_install_rejects_forged_provenance_fields() {
    let cases = [
        json!({
            "content": "---\nname: hidden-source\n---\nPrompt.\n",
            "source": "installed_url"
        }),
        json!({
            "content": "---\nname: hidden-source-url\n---\nPrompt.\n",
            "source_url": "https://api.example.test/skills/hidden-source-url/SKILL.md"
        }),
    ];

    for input in cases {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let runtime = runtime_with_filesystem(filesystem);

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            input,
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(temp.path().read_dir().unwrap().next().is_none());
    }
}

// The direct-install input contract, asserted at the capability boundary rather than against
// whichever helper currently normalizes the input -- the normalizer has already moved crates once,
// and these were written to pass on both sides of that move. The dividing line is provenance, not
// shape: `files` is caller content, `source`/`source_url` are not.

/// A direct install may carry the rest of the bundle, not just prose.
///
/// Refused before nearai/ironclaw#6745, and it failed the ENTIRE install rather than dropping the
/// file: on the 31-task SkillsBench subset (nearai/benchmarks#287) 18 correctly-shaped entries
/// across 9 calls were all refused. A skill that cannot carry a script is a prose description.
#[tokio::test]
async fn builtin_skill_install_accepts_an_agent_authored_bundle_with_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let runtime = runtime_with_filesystem(filesystem);

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({
            "content": "---\nname: egfr-calc\ndescription: CKD-EPI eGFR\n---\nRun scripts/egfr.py.\n",
            "files": [
                { "path": "scripts/egfr.py", "text": "print('eGFR')\n" },
                { "path": "references/units.md", "text": "mg/dL -> mmol/L: x 0.0555\n" }
            ]
        }),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts.clone(),
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["installed"], json!(true));
    assert_eq!(installed["name"], json!("egfr-calc"));
    // `source` stays `user`: authoring a bundle locally is not a URL install, and the
    // normalizer must not infer provenance from the presence of `files`.
    assert_eq!(installed["source"], json!("user"));
    assert_eq!(installed["files_installed"], json!(2));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("egfr-calc/scripts/egfr.py")).unwrap(),
        "print('eGFR')\n",
        "the script the agent authored must be on disk verbatim, or the skill it belongs to \
         cannot run it"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("egfr-calc/references/units.md")).unwrap(),
        "mg/dL -> mmol/L: x 0.0555\n"
    );

    let listed = invoke_with_context(
        &runtime,
        SKILL_LIST_CAPABILITY_ID,
        json!({}),
        execution_context_with_mounts([SKILL_LIST_CAPABILITY_ID], mounts),
    )
    .await
    .unwrap();
    assert_eq!(listed["count"], json!(1));
    assert_eq!(listed["skills"][0]["name"], json!("egfr-calc"));
    assert_eq!(listed["skills"][0]["source"], json!("user"));
}

/// `bytes_base64` is the shape the URL-fetch path constructs, so the direct arm must accept it too;
/// both arms converge on one install request.
#[tokio::test]
async fn builtin_skill_install_accepts_base64_bundle_files_on_a_direct_install() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let runtime = runtime_with_filesystem(filesystem);

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({
            "content": "---\nname: binary-helper\ndescription: has a binary asset\n---\nPrompt.\n",
            "files": [{ "path": "assets/blob.bin", "bytes_base64": "aGVsbG8=" }]
        }),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["files_installed"], json!(1));
    assert_eq!(
        std::fs::read(temp.path().join("binary-helper/assets/blob.bin")).unwrap(),
        b"hello"
    );
}

/// Carrying `files` does not buy the right to forge provenance. Sends both, and the install must be
/// refused whole rather than accepted with the provenance ignored.
#[tokio::test]
async fn builtin_skill_install_rejects_a_bundle_that_also_forges_provenance() {
    let cases = [
        json!({
            "content": "---\nname: forged-with-files\n---\nPrompt.\n",
            "files": [{ "path": "scripts/x.py", "text": "print(1)\n" }],
            "source": "installed_url"
        }),
        json!({
            "content": "---\nname: forged-url-with-files\n---\nPrompt.\n",
            "files": [{ "path": "scripts/x.py", "text": "print(1)\n" }],
            "source_url": "https://api.example.test/skills/forged/SKILL.md"
        }),
    ];

    for input in cases {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let runtime = runtime_with_filesystem(filesystem);

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            input,
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(
            temp.path().read_dir().unwrap().next().is_none(),
            "a refused install must leave the skill root untouched"
        );
    }
}

/// A bundle-relative path may not climb out of its own skill directory. Accepting caller `files`
/// means each path is caller-controlled, so this is what makes that safe.
#[tokio::test]
async fn builtin_skill_install_rejects_a_bundle_file_escaping_its_skill_directory() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let runtime = runtime_with_filesystem(filesystem);

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({
            "content": "---\nname: escaper\ndescription: tries to climb out\n---\nPrompt.\n",
            "files": [{ "path": "../../escaped.py", "text": "print('escaped')\n" }]
        }),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        !temp.path().join("escaped.py").exists(),
        "a traversing bundle path must not write outside the skill directory"
    );
}

// URL-install coverage stays in this integration file because these cases assert
// the first-party runtime dispatch path, not only the URL parser helpers.
#[tokio::test]
async fn builtin_skill_install_url_path_fetches_through_host_runtime_egress() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        b"---\nname: fetched-helper\ndescription: fetched skill\n---\nFetched prompt.\n".to_vec(),
    ));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));
    let context = execution_context_with_mounts_and_network(
        [SKILL_INSTALL_CAPABILITY_ID, SKILL_LIST_CAPABILITY_ID],
        mounts,
        http_test_policy(),
    );

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/fetched-helper/SKILL.md"}),
        context.clone(),
    )
    .await
    .unwrap();

    assert_eq!(installed["installed"], json!(true));
    assert_eq!(installed["name"], json!("fetched-helper"));
    assert_eq!(installed["path"], json!("/skills/fetched-helper/SKILL.md"));
    assert_eq!(installed["source"], json!("installed"));

    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].runtime, RuntimeKind::FirstParty);
    assert_eq!(
        requests[0].capability_id,
        capability_id(SKILL_INSTALL_CAPABILITY_ID)
    );
    assert_eq!(requests[0].method, NetworkMethod::Get);
    assert_eq!(
        requests[0].url,
        "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/fetched-helper/SKILL.md"
    );
    assert_eq!(requests[0].response_body_limit, Some(10 * 1024 * 1024));
    assert_eq!(requests[0].timeout_ms, Some(10_000));

    let listed = invoke_with_context(&runtime, SKILL_LIST_CAPABILITY_ID, json!({}), context)
        .await
        .unwrap();
    assert_eq!(listed["count"], json!(1));
    assert_eq!(listed["skills"][0]["name"], json!("fetched-helper"));
    assert_eq!(listed["skills"][0]["source"], json!("installed"));
}

#[tokio::test]
async fn builtin_skill_install_url_path_ignores_caller_supplied_hidden_bundle_files() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        b"---\nname: fetched-helper\ndescription: fetched skill\n---\nFetched prompt.\n".to_vec(),
    ));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, egress);

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({
            "url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/fetched.md",
            "files": [{
                "path": "references/injected.md",
                "bytes": [35, 32, 73, 110, 106, 101, 99, 116, 101, 100, 10],
            }],
        }),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["files_installed"], json!(0));
    assert!(
        !temp
            .path()
            .join("fetched-helper/references/injected.md")
            .exists()
    );
}

#[tokio::test]
async fn builtin_skill_install_url_path_serializes_concurrent_fetches_from_same_url() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        b"---\nname: concurrent-helper\ndescription: fetched skill\n---\nFetched prompt.\n"
            .to_vec(),
    ));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));
    let context = execution_context_with_mounts_and_network(
        [SKILL_INSTALL_CAPABILITY_ID],
        mounts,
        http_test_policy(),
    );
    let url =
        "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/concurrent-helper/SKILL.md";

    let (first, second) = tokio::join!(
        invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": url}),
            context.clone(),
        ),
        invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": url}),
            context,
        )
    );
    // Both concurrent installs of the same URL fetch independently (egress == 2),
    // then serialize on the per-skill mutation lock in
    // ironclaw_skills::management::install_skill. Install is idempotent for
    // identical content (replay-safety, nearai/ironclaw#4385): the second install
    // observes the first's matching install and returns success instead of a
    // conflict, so the skill is written exactly once.
    assert!(
        first.is_ok(),
        "first concurrent install must succeed: {first:?}"
    );
    assert!(
        second.is_ok(),
        "second concurrent install of identical content must succeed idempotently: {second:?}"
    );
    assert_eq!(egress.requests().len(), 2);
    let installed = temp.path().join("concurrent-helper/SKILL.md");
    assert!(installed.exists());
    assert!(
        std::fs::read_to_string(&installed)
            .unwrap()
            .contains("Fetched prompt."),
        "installed SKILL.md must contain the fetched skill content"
    );
}

#[tokio::test]
async fn builtin_skill_install_url_path_installs_zip_bundle_supporting_files() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(skill_bundle_zip(&[
        (
            "bundle-main/zip-helper/SKILL.md",
            b"---\nname: zip-helper\ndescription: bundled skill\n---\nZip prompt.\n",
        ),
        (
            "bundle-main/zip-helper/references/guide.md",
            b"# Guide\nUse carefully.\n",
        ),
        ("bundle-main/zip-helper/scripts/run.py", b"print('ok')\n"),
    ])));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://codeload.github.com/Pika-Labs/Pika-Skills/legacy.zip/main"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["installed"], json!(true));
    assert_eq!(installed["name"], json!("zip-helper"));
    assert_eq!(installed["files_installed"], json!(2));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("zip-helper/references/guide.md")).unwrap(),
        "# Guide\nUse carefully.\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("zip-helper/scripts/run.py")).unwrap(),
        "print('ok')\n"
    );
}

#[tokio::test]
async fn builtin_skill_install_url_path_installs_github_repo_bundle_supporting_files() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let archive = skill_bundle_zip(&[
        (
            "Pika-Skills-main/pikastream-video-meeting/SKILL.md",
            b"---\nname: pikastream-video-meeting\ndescription: repo skill\n---\nRepo prompt.\n",
        ),
        (
            "Pika-Skills-main/pikastream-video-meeting/scripts/run.py",
            b"print('repo')\n",
        ),
    ]);
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(BTreeMap::from([
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills".to_string(),
            (
                200,
                br#"{"default_branch":"main"}"#.to_vec(),
            ),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/commits?sha=main&per_page=1"
                .to_string(),
            (
                200,
                br#"[{"sha":"abcdef0123456789abcdef0123456789abcdef01"}]"#.to_vec(),
            ),
        ),
        (
            "https://codeload.github.com/Pika-Labs/Pika-Skills/legacy.zip/abcdef0123456789abcdef0123456789abcdef01".to_string(),
            (200, archive),
        ),
    ])));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://github.com/Pika-Labs/Pika-Skills"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["name"], json!("pikastream-video-meeting"));
    assert_eq!(installed["files_installed"], json!(1));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("pikastream-video-meeting/scripts/run.py"))
            .unwrap(),
        "print('repo')\n"
    );
    assert_eq!(egress.requests().len(), 3);
}

#[tokio::test]
async fn builtin_skill_install_url_path_installs_github_blob_skill_md() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(BTreeMap::from([
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/git/matching-refs/heads/main"
                .to_string(),
            (
                200,
                br#"[{"ref":"refs/heads/main"}]"#.to_vec(),
            ),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/contents/SKILL.md?ref=main"
                .to_string(),
            (
                200,
                br#"{"type":"file","download_url":"https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/SKILL.md"}"#.to_vec(),
            ),
        ),
        (
            "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/SKILL.md".to_string(),
            (
                200,
                b"---\nname: blob-helper\ndescription: blob skill\n---\nBlob prompt.\n".to_vec(),
            ),
        ),
    ])));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://github.com/Pika-Labs/Pika-Skills/blob/main/SKILL.md"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["name"], json!("blob-helper"));
    assert_eq!(installed["source"], json!("installed"));
    assert_eq!(egress.requests().len(), 3);
}

#[tokio::test]
async fn builtin_skill_install_url_path_installs_github_tree_subdir_bundle_supporting_files() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let entries = br#"[
        {"type":"file","path":"skills/foo/SKILL.md"},
        {"type":"file","path":"skills/foo/references/tree.md"}
    ]"#
    .to_vec();
    let raw_skill =
        b"---\nname: tree-helper\ndescription: tree skill\n---\nTree prompt.\n".to_vec();
    let raw_reference = b"# Tree\n".to_vec();
    let commit_sha = "abcdef0123456789abcdef0123456789abcdef01";
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(BTreeMap::from([
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/git/matching-refs/heads/release"
                .to_string(),
            (
                200,
                br#"[{"ref":"refs/heads/release/v1"}]"#.to_vec(),
            ),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/commits?sha=release%2Fv1&per_page=1"
                .to_string(),
            (
                200,
                br#"[{"sha":"abcdef0123456789abcdef0123456789abcdef01"}]"#.to_vec(),
            ),
        ),
        (
            format!("https://api.github.com/repos/Pika-Labs/Pika-Skills/contents/skills/foo?ref={commit_sha}"),
            (200, entries),
        ),
        (
            format!("https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/{commit_sha}/skills/foo/SKILL.md"),
            (200, raw_skill),
        ),
        (
            format!("https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/{commit_sha}/skills/foo/references/tree.md"),
            (200, raw_reference),
        ),
    ])));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://github.com/Pika-Labs/Pika-Skills/tree/release/v1/skills/foo"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["name"], json!("tree-helper"));
    assert_eq!(installed["files_installed"], json!(1));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("tree-helper/references/tree.md")).unwrap(),
        "# Tree\n"
    );
    let requests = egress.requests();
    assert_eq!(requests.len(), 5);
    assert!(
        requests
            .iter()
            .all(|request| !request.url.contains("codeload.github.com"))
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.url.contains("release/v1/skills/foo"))
    );
}

#[tokio::test]
async fn builtin_skill_install_url_path_installs_github_tree_tag_subdir_bundle_supporting_files() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let commit_sha = "1234567890abcdef1234567890abcdef12345678";
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(BTreeMap::from([
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/git/matching-refs/heads/release"
                .to_string(),
            (200, b"[]".to_vec()),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/git/matching-refs/tags/release"
                .to_string(),
            (
                200,
                br#"[{"ref":"refs/tags/release/v1"}]"#.to_vec(),
            ),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/commits?sha=release%2Fv1&per_page=1"
                .to_string(),
            (
                200,
                br#"[{"sha":"1234567890abcdef1234567890abcdef12345678"}]"#.to_vec(),
            ),
        ),
        (
            format!("https://api.github.com/repos/Pika-Labs/Pika-Skills/contents/skills/foo?ref={commit_sha}"),
            (
                200,
                br#"[{"type":"file","path":"skills/foo/SKILL.md"}]"#.to_vec(),
            ),
        ),
        (
            format!("https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/{commit_sha}/skills/foo/SKILL.md"),
            (
                200,
                b"---\nname: tagged-tree-helper\ndescription: tree skill\n---\nTree prompt.\n".to_vec(),
            ),
        ),
    ])));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let installed = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://github.com/Pika-Labs/Pika-Skills/tree/release/v1/skills/foo"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(installed["name"], json!("tagged-tree-helper"));
    assert_eq!(egress.requests().len(), 5);
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_github_tree_directory_fanout() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let commit_sha = "abcdef0123456789abcdef0123456789abcdef01";
    let mut responses = BTreeMap::from([
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/git/matching-refs/heads/main"
                .to_string(),
            (200, br#"[{"ref":"refs/heads/main"}]"#.to_vec()),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/commits?sha=main&per_page=1"
                .to_string(),
            (
                200,
                br#"[{"sha":"abcdef0123456789abcdef0123456789abcdef01"}]"#.to_vec(),
            ),
        ),
    ]);
    let mut directory = "skills/foo".to_string();
    for index in 0..64 {
        let next = format!("{directory}/dir-{index}");
        responses.insert(
            format!(
                "https://api.github.com/repos/Pika-Labs/Pika-Skills/contents/{directory}?ref={commit_sha}"
            ),
            (
                200,
                format!(r#"[{{"type":"dir","path":"{next}"}}]"#).into_bytes(),
            ),
        );
        directory = next;
    }
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(responses));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://github.com/Pika-Labs/Pika-Skills/tree/main/skills/foo"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::OutputTooLarge);
    assert_eq!(egress.requests().len(), 24);
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_invalid_zip_bundles() {
    let oversized_entry = vec![b'x'; 2 * 1024 * 1024 + 1];
    let too_many_entries = (0..(ironclaw_skills::MAX_INSTALL_BUNDLE_FILES * 4 + 1))
        .map(|index| (format!("bundle/empty-{index}.txt"), Vec::new()))
        .collect::<Vec<_>>();
    let too_many_directories = (0..(ironclaw_skills::MAX_INSTALL_BUNDLE_FILES * 4 + 1))
        .map(|index| format!("bundle/dir-{index}/"))
        .collect::<Vec<_>>();
    let oversized_total = (0..11)
        .map(|index| {
            (
                format!("bundle/big-{index}.bin"),
                vec![b'x'; 2 * 1024 * 1024],
            )
        })
        .collect::<Vec<_>>();
    let cases = [
        (
            skill_bundle_zip(&[("bundle/references/guide.md", b"# Guide\n")]),
            FailureKind::OperationFailed,
        ),
        (
            skill_bundle_zip(&[
                ("bundle/one/SKILL.md", b"---\nname: one\n---\nOne\n"),
                ("bundle/two/SKILL.md", b"---\nname: two\n---\nTwo\n"),
            ]),
            FailureKind::InputEncode,
        ),
        (
            skill_bundle_zip(&[("../escape/SKILL.md", b"---\nname: escape\n---\nEscape\n")]),
            FailureKind::InputEncode,
        ),
        (
            skill_bundle_zip_owned([("bundle/SKILL.md".to_string(), vec![0xff, 0xfe, 0xfd])]),
            FailureKind::OperationFailed,
        ),
        (
            skill_bundle_zip_owned([("bundle/oversized.bin".to_string(), oversized_entry)]),
            FailureKind::OutputTooLarge,
        ),
        (
            skill_bundle_zip_owned(too_many_entries),
            FailureKind::OutputTooLarge,
        ),
        (
            skill_bundle_zip_with_dirs(
                &too_many_directories,
                std::iter::empty::<(String, Vec<u8>)>(),
            ),
            FailureKind::OutputTooLarge,
        ),
        (
            skill_bundle_zip_owned(
                std::iter::once((
                    "bundle/SKILL.md".to_string(),
                    b"---\nname: oversized-total\n---\nPrompt\n".to_vec(),
                ))
                .chain(oversized_total),
            ),
            FailureKind::OutputTooLarge,
        ),
    ];

    for (body, expected) in cases {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let runtime = runtime_with_filesystem_and_http_egress(
            filesystem,
            Arc::new(RecordingRuntimeHttpEgress::with_body(body)),
        );

        let actual = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": "https://codeload.github.com/Pika-Labs/Pika-Skills/legacy.zip/invalid"}),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(actual, expected);
        assert!(temp.path().read_dir().unwrap().next().is_none());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_truncated_zip_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        b"PK\x03\x04truncated".to_vec(),
    ));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://codeload.github.com/Pika-Labs/Pika-Skills/legacy.zip/truncated"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::OperationFailed);
    assert_eq!(egress.requests().len(), 1);
    assert!(temp.path().read_dir().unwrap().next().is_none());
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_invalid_github_api_responses() {
    let cases = [
        BTreeMap::from([(
            "https://api.github.com/repos/Pika-Labs/Pika-Skills".to_string(),
            (200, b"not json".to_vec()),
        )]),
        BTreeMap::from([(
            "https://api.github.com/repos/Pika-Labs/Pika-Skills".to_string(),
            (200, br#"{"name":"Pika-Skills"}"#.to_vec()),
        )]),
        BTreeMap::from([
            (
                "https://api.github.com/repos/Pika-Labs/Pika-Skills".to_string(),
                (200, br#"{"default_branch":"main"}"#.to_vec()),
            ),
            (
                "https://api.github.com/repos/Pika-Labs/Pika-Skills/commits?sha=main&per_page=1"
                    .to_string(),
                (200, br#"[{"sha":"not-a-sha"}]"#.to_vec()),
            ),
        ]),
    ];

    for responses in cases {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let runtime = runtime_with_filesystem_and_http_egress(
            filesystem,
            Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(responses)),
        );

        let actual = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": "https://github.com/Pika-Labs/Pika-Skills"}),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(actual, FailureKind::OperationFailed);
        assert!(temp.path().read_dir().unwrap().next().is_none());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_github_tree_file_response() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let commit_sha = "abcdef0123456789abcdef0123456789abcdef01";
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(BTreeMap::from([
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/git/matching-refs/heads/main"
                .to_string(),
            (200, br#"[{"ref":"refs/heads/main"}]"#.to_vec()),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/commits?sha=main&per_page=1"
                .to_string(),
            (
                200,
                br#"[{"sha":"abcdef0123456789abcdef0123456789abcdef01"}]"#.to_vec(),
            ),
        ),
        (
            format!("https://api.github.com/repos/Pika-Labs/Pika-Skills/contents/SKILL.md?ref={commit_sha}"),
            (
                200,
                br#"{"type":"file","path":"SKILL.md","download_url":"https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/SKILL.md"}"#.to_vec(),
            ),
        ),
    ])));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://github.com/Pika-Labs/Pika-Skills/tree/main/SKILL.md"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(temp.path().read_dir().unwrap().next().is_none());
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_github_blob_download_url_host_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_url_bodies(BTreeMap::from([
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/git/matching-refs/heads/main"
                .to_string(),
            (200, br#"[{"ref":"refs/heads/main"}]"#.to_vec()),
        ),
        (
            "https://api.github.com/repos/Pika-Labs/Pika-Skills/contents/SKILL.md?ref=main"
                .to_string(),
            (
                200,
                br#"{"type":"file","download_url":"https://api.github.com/repos/Pika-Labs/Pika-Skills/contents/secret"}"#.to_vec(),
            ),
        ),
    ])));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://github.com/Pika-Labs/Pika-Skills/blob/main/SKILL.md"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert_eq!(egress.requests().len(), 2);
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_malformed_github_paths_before_fetch() {
    for url in [
        "https://github.com/%24%7BINJECTION%7D/Pika-Skills",
        "https://github.com/Pika-Labs/Pika-Skills/releases",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
            b"---\nname: fetched-helper\n---\nFetched prompt.\n".to_vec(),
        ));
        let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": url}),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(egress.requests().is_empty());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_ambiguous_content_and_url_before_fetch() {
    for content in [
        json!("---\nname: pasted-helper\n---\nPasted prompt.\n"),
        json!(123),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
            b"---\nname: fetched-helper\n---\nFetched prompt.\n".to_vec(),
        ));
        let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({
                "url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/fetched-helper/SKILL.md",
                "content": content
            }),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(egress.requests().is_empty());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_non_https_url_before_fetch() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        b"---\nname: fetched-helper\n---\nFetched prompt.\n".to_vec(),
    ));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "http://api.example.test/skills/fetched-helper/SKILL.md"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_empty_input_before_fetch() {
    for input in [json!({}), json!({"name": "missing-url"})] {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
            b"---\nname: fetched-helper\n---\nFetched prompt.\n".to_vec(),
        ));
        let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            input,
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(egress.requests().is_empty());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_whitespace_url_before_fetch() {
    for url in ["", "   "] {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
            b"---\nname: fetched-helper\n---\nFetched prompt.\n".to_vec(),
        ));
        let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": url}),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(egress.requests().is_empty());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_credentials_in_url_before_fetch() {
    for url in [
        "https://user@raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/SKILL.md",
        "https://user:pass@raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/SKILL.md",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
            b"---\nname: fetched-helper\n---\nFetched prompt.\n".to_vec(),
        ));
        let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": url}),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(egress.requests().is_empty());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_disallowed_hosts_before_fetch() {
    for url in [
        "https://169.254.169.254/latest/meta-data",
        "https://internal.service.local/skills/SKILL.md",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
            b"---\nname: fetched-helper\n---\nFetched prompt.\n".to_vec(),
        ));
        let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

        let error = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": url}),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(egress.requests().is_empty());
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_fails_closed_when_runtime_egress_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let runtime = runtime_with_filesystem_without_http_egress(filesystem);

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/fetched-helper/SKILL.md"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::NetworkDenied);
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_non_success_url_response() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_status_and_body(
        404,
        b"not found".to_vec(),
    ));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));
    let context = execution_context_with_mounts_and_network(
        [SKILL_INSTALL_CAPABILITY_ID, SKILL_LIST_CAPABILITY_ID],
        mounts,
        http_test_policy(),
    );

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/missing/SKILL.md"}),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::OperationFailed);
    assert_eq!(egress.requests().len(), 1);

    let listed = invoke_with_context(&runtime, SKILL_LIST_CAPABILITY_ID, json!({}), context)
        .await
        .unwrap();
    assert_eq!(listed["count"], json!(0));
}

#[tokio::test]
async fn builtin_skill_install_url_path_rejects_invalid_utf8_url_response() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![0xff, 0xfe]));
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        SKILL_INSTALL_CAPABILITY_ID,
        json!({"url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/binary/SKILL.md"}),
        execution_context_with_mounts_and_network(
            [SKILL_INSTALL_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::OperationFailed);
    assert_eq!(egress.requests().len(), 1);
}

#[tokio::test]
async fn builtin_skill_install_url_path_maps_runtime_egress_errors_by_reason() {
    let cases = [
        (
            RuntimeHttpEgressError::Request {
                reason: "sensitive_header_denied:authorization".to_string(),
                request_bytes: 0,
                response_bytes: 0,
            },
            FailureKind::InputEncode,
        ),
        (
            RuntimeHttpEgressError::Network {
                reason: "policy_denied".to_string(),
                request_bytes: 0,
                response_bytes: 0,
            },
            FailureKind::PolicyDenied,
        ),
        (
            RuntimeHttpEgressError::Network {
                reason: "network_unreachable".to_string(),
                request_bytes: 0,
                response_bytes: 0,
            },
            FailureKind::NetworkDenied,
        ),
        (
            RuntimeHttpEgressError::Response {
                reason: "response_decode_failed".to_string(),
                request_bytes: 4,
                response_bytes: 1024,
            },
            FailureKind::OperationFailed,
        ),
        (
            RuntimeHttpEgressError::Response {
                reason: "response_body_limit_exceeded".to_string(),
                request_bytes: 4,
                response_bytes: 1024,
            },
            FailureKind::OutputTooLarge,
        ),
    ];

    for (error, expected) in cases {
        let temp = tempfile::tempdir().unwrap();
        let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
        let runtime = runtime_with_filesystem_and_http_egress(
            filesystem,
            Arc::new(RecordingRuntimeHttpEgress::with_error(error)),
        );
        let actual = invoke_with_context(
            &runtime,
            SKILL_INSTALL_CAPABILITY_ID,
            json!({"url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/fetched-helper/SKILL.md"}),
            execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn builtin_skill_install_url_path_accounts_wall_clock_time() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_skill_filesystem(temp.path());
    let egress = Arc::new(SleepingRuntimeHttpEgress {
        delay: Duration::from_millis(20),
        body: b"---\nname: slow-helper\n---\nSlow prompt.\n".to_vec(),
    });
    let runtime = runtime_with_filesystem_and_http_egress(filesystem, egress);
    let outcome = runtime
        .invoke_capability((execution_context_with_mounts_and_network(
                [SKILL_INSTALL_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ), capability_id(SKILL_INSTALL_CAPABILITY_ID), ResourceEstimate::default(), json!({"url": "https://raw.githubusercontent.com/Pika-Labs/Pika-Skills/main/slow-helper/SKILL.md"})))
        .await
        .unwrap();

    let RuntimeCapabilityOutcome::Completed(completed) = outcome else {
        panic!("expected completed skill URL install, got {outcome:?}");
    };
    assert!(completed.usage.wall_clock_ms >= 10);
}
