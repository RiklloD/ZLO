use std::collections::BTreeMap;

use super::*;

mod leaks;
mod read_only;
mod tail;

/// Built at runtime so upstream secret scanners do not match the literal.
fn slack_like() -> String {
    format!("xoxb-{}-{}", "4242424242", "diagnosticsBundleLeakCanary")
}

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

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

fn run_bundle(
    context: &RebornCliContext,
    env: &[(String, String)],
    include_logs: bool,
) -> (tempfile::TempDir, PathBuf, anyhow::Result<BundleReport>) {
    let out_dir = tempfile::tempdir().expect("out dir");
    let out = out_dir.path().join("bundle.zip");
    let result = write_bundle_with(
        context,
        &BundleOptions {
            out: out.clone(),
            include_logs,
            force: false,
        },
        &BundleInputs {
            service: ServiceStateDto::Unknown,
            env,
        },
    );
    (out_dir, out, result)
}

/// Every needle, checked in every entry's bytes, every entry's name, and the
/// lenient decodings of every entry (so an encoded leak fails too).
fn assert_no_leaks(entries: &BTreeMap<String, Vec<u8>>, needles: &[String]) {
    let probe = redact::Scrubber::new(needles.iter().filter(|n| n.len() >= 8).cloned());
    for (name, bytes) in entries {
        for needle in needles {
            assert!(
                !contains(bytes, needle.as_bytes()) && !name.contains(needle.as_str()),
                "{name} leaks {needle:?}:\n{}",
                String::from_utf8_lossy(bytes)
            );
        }
        assert!(
            !probe.contains_known_secret(bytes),
            "{name} leaks an encoded needle:\n{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
