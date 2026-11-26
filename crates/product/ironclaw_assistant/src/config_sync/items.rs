//! What the itemized domains (`skills`, `memory`) share: how a page is cut
//! and how items are hashed, so both ends agree on "the same".

use std::collections::BTreeMap;

use base64::Engine as _;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncItem, ConfigSyncItemFile, ItemizedDomainPayload,
};
use sha2::{Digest as _, Sha256};

use super::{ConfigSyncError, hex_lower};

/// What one page may weigh once encoded (keys, hashes, base64 content and
/// their JSON), leaving room for the snapshot and apply envelopes under the
/// snapshot limit.
pub const PAGE_ENCODED_BYTES: usize = 3 * 1024 * 1024;
/// An item larger than this (before base64) is not carried at all: alone
/// in a page it still fits.
pub const ITEM_CONTENT_BYTES: usize = 2 * 1024 * 1024;
/// Lines a page carries about what it left out; the rest are counted.
pub const PAGE_NOTES: usize = 60;
/// Items listed in one page, with or without content.
pub const PAGE_ITEMS: usize = 2_000;

/// An item's files: path inside the item, and bytes.
pub type Files = Vec<(String, Vec<u8>)>;

/// How one export call cuts its page.
#[derive(Debug, Clone, Default)]
pub struct PageRequest {
    pub index_only: bool,
    pub have: BTreeMap<String, String>,
    pub after: Option<String>,
    /// `have` says nothing about keys after this one: the page ends here.
    pub until: Option<String>,
    /// The installation the page is for.
    pub peer: Option<String>,
}

/// A file name under which a key, a certificate bundle, a cluster sign-in
/// or a program's environment is conventionally kept. Judged on the
/// lowercased name. A template (`.env.example`) is an ordinary file.
pub fn key_file_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let (stem, extensions) = lower.split_once('.').unwrap_or((lower.as_str(), ""));
    const STEMS: [&str; 6] = [
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "kubeconfig",
        "netrc",
        "npmrc",
    ];
    const EXTENSIONS: [&str; 16] = [
        "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "ppk", "gpg", "asc", "der", "p8",
        "netrc", "npmrc", "pypirc", "env",
    ];
    const TEMPLATES: [&str; 5] = ["example", "sample", "template", "dist", "md"];
    if extensions
        .rsplit('.')
        .next()
        .is_some_and(|last| TEMPLATES.contains(&last))
    {
        return false;
    }
    STEMS.contains(&stem)
        || stem.starts_with("id_ed25519")
        || extensions.split('.').any(|ext| EXTENSIONS.contains(&ext))
}

/// The value written after a field name at `from`: past an optional
/// closing quote, a `:` or `=`, and an optional opening quote, up to the
/// next quote, whitespace or line end.
fn value_after(bytes: &[u8], from: usize) -> &[u8] {
    let rest = bytes.get(from..).unwrap_or_default();
    let skip = |rest: &[u8], what: &[u8]| -> usize {
        rest.iter().take_while(|b| what.contains(b)).count()
    };
    let mut at = skip(rest, b"\"' \t");
    if !rest.get(at).is_some_and(|b| matches!(b, b':' | b'=')) {
        return &[];
    }
    at += 1;
    at += skip(&rest[at..], b" \t");
    at += skip(&rest[at..], b"\"'");
    let len = rest[at..]
        .iter()
        .take_while(|b| !matches!(b, b'"' | b'\'' | b' ' | b'\t' | b'\r' | b'\n' | b','))
        .count();
    &rest[at..at + len]
}

/// A value that is a real secret rather than a placeholder in an example:
/// long, and made only of the characters keys and tokens are made of.
fn looks_real(value: &[u8]) -> bool {
    const PLACEHOLDERS: [&[u8]; 5] = [
        b"your",
        b"example",
        b"placeholder",
        b"redacted",
        b"changeme",
    ];
    value.len() >= 20
        && value
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+/=:~".contains(b))
        && !value
            .windows(3)
            .any(|w| w == b"..." || w.eq_ignore_ascii_case(b"xxx"))
        && !PLACEHOLDERS.iter().any(|word| {
            value
                .windows(word.len())
                .any(|w| w.eq_ignore_ascii_case(word))
        })
}

/// Where each occurrence of `mark` in `bytes` ends.
fn found<'a>(bytes: &'a [u8], mark: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    bytes
        .windows(mark.len())
        .enumerate()
        .filter(move |(_, window)| *window == mark)
        .map(move |(at, _)| at + mark.len())
}

/// Bytes that hold a secret in one of the forms tools write them: a
/// private key block with a body, or a sign-in field (a cluster's, a
/// cloud's, a stored token, a provider key in an environment file) with a
/// real value. A document that only names such a field, or shows a
/// placeholder, is an ordinary file.
pub fn secret_bytes(bytes: &[u8]) -> bool {
    const BLOCK: &[u8] = b"PRIVATE KEY-----";
    const FIELDS: [&[u8]; 12] = [
        b"client-key-data",
        b"aws_secret_access_key",
        b"access_token",
        b"refresh_token",
        b"accessToken",
        b"refreshToken",
        b"client_secret",
        b"private_key",
        b"API_KEY",
        b"_TOKEN",
        b"_SECRET",
        b"api_key",
    ];
    // A key block: the header, then key material. An encrypted key has
    // header lines (`Proc-Type`, `DEK-Info`) before it, so the material is
    // looked for anywhere in the stretch that follows.
    let block = found(bytes, BLOCK).any(|after| {
        let stretch = bytes.get(after..).unwrap_or_default();
        let stretch = stretch.get(..stretch.len().min(4096)).unwrap_or_default();
        stretch
            .split(|b| !(b.is_ascii_alphanumeric() || b"+/=".contains(b)))
            .any(|run| run.len() >= 40)
    });
    block
        || FIELDS
            .iter()
            .any(|field| found(bytes, field).any(|after| looks_real(value_after(bytes, after))))
}

/// `sha256:<hex>` of bytes.
pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_lower(&Sha256::digest(bytes)))
}

/// A bundle's digest: its files by path, each by content.
pub fn files_hash(files: &[(String, Vec<u8>)]) -> String {
    let mut sorted: Vec<&(String, Vec<u8>)> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (path, bytes) in sorted {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(Sha256::digest(bytes));
    }
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

/// [`domain_hash`] of a domain that also has something to say about what
/// it left out. The notes are part of the digest, so the two sides differ
/// while there is something to tell and a pass is made that tells it.
pub fn domain_hash_with_notes(items: &BTreeMap<String, String>, notes: &[String]) -> String {
    if notes.is_empty() {
        return domain_hash(items);
    }
    let mut hasher = Sha256::new();
    hasher.update(domain_hash(items).as_bytes());
    for note in notes {
        hasher.update([0]);
        hasher.update(note.as_bytes());
    }
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

/// The domain's digest: every item's key and hash, in key order.
pub fn domain_hash(items: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (key, hash) in items {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        hasher.update(hash.as_bytes());
        hasher.update([0]);
    }
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

pub fn encode(files: Vec<(String, Vec<u8>)>) -> Vec<ConfigSyncItemFile> {
    files
        .into_iter()
        .map(|(path, bytes)| ConfigSyncItemFile {
            path,
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .collect()
}

pub fn decode(
    domain: ConfigSyncDomain,
    item: &ConfigSyncItem,
) -> Result<Option<Files>, ConfigSyncError> {
    let Some(files) = &item.files else {
        return Ok(None);
    };
    files
        .iter()
        .map(|file| {
            base64::engine::general_purpose::STANDARD
                .decode(&file.data_b64)
                .map(|bytes| (file.path.clone(), bytes))
                .map_err(|error| {
                    ConfigSyncError::validation(
                        format!("{domain}.items"),
                        format!("item `{}` is not valid base64: {error}", item.key),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Shared payload checks: bounded, decodable.
pub fn validate(
    domain: ConfigSyncDomain,
    page: &ItemizedDomainPayload,
) -> Result<(), ConfigSyncError> {
    if page.items.len() > PAGE_ITEMS {
        return Err(ConfigSyncError::validation(
            format!("{domain}.items"),
            format!("more than {PAGE_ITEMS} items in one page"),
        ));
    }
    // In key order, each once: a page is cut that way, and a key twice
    // would be judged twice against one reading of what is held.
    if page.items.windows(2).any(|pair| pair[0].key >= pair[1].key) {
        return Err(ConfigSyncError::validation(
            format!("{domain}.items"),
            "items are not in key order, or a key appears twice",
        ));
    }
    if page.not_carried.len() > PAGE_NOTES + 2 {
        return Err(ConfigSyncError::validation(
            format!("{domain}.not_carried"),
            format!("more than {PAGE_NOTES} notes in one page"),
        ));
    }
    for item in &page.items {
        if let Some(files) = decode(domain, item)? {
            let bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
            if bytes > ITEM_CONTENT_BYTES {
                return Err(ConfigSyncError::validation(
                    format!("{domain}.items"),
                    format!(
                        "item `{}` is larger than {ITEM_CONTENT_BYTES} bytes",
                        item.key
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Cuts a page from a domain's items (key to hash, all of them) under
/// `request`. `load` reads one item's files; an item it cannot give a
/// content for (`None`: gone meanwhile) is left out.
pub async fn page<F, Fut>(
    all: &BTreeMap<String, String>,
    request: &PageRequest,
    mut load: F,
) -> Result<ItemizedDomainPayload, ConfigSyncError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Files>, ConfigSyncError>>,
{
    let mut out = ItemizedDomainPayload {
        hash: domain_hash(all),
        items: Vec::new(),
        next: None,
        not_carried: Vec::new(),
    };
    // The page's weight as it will be sent: each item is measured in its
    // JSON form, so a key the encoding inflates counts for what it costs.
    let mut weight = 0usize;
    let mut unsaid = 0usize;
    let mut say = |out: &mut ItemizedDomainPayload, weight: &mut usize, line: String| {
        if out.not_carried.len() < PAGE_NOTES {
            *weight += 2 * line.len() + 4;
            out.not_carried.push(line);
        } else {
            unsaid += 1;
        }
    };
    let sent =
        |item: &ConfigSyncItem| serde_json::to_vec(item).map_or(usize::MAX / 2, |b| b.len() + 1);
    // The last key dealt with, carried or not: where the next page starts.
    let mut consumed: Option<String> = None;
    for (key, hash) in all {
        if request.after.as_ref().is_some_and(|after| key <= after) {
            continue;
        }
        if let Some(until) = &request.until
            && key > until
        {
            out.next = Some(until.clone());
            break;
        }
        let mut item = ConfigSyncItem {
            key: key.clone(),
            hash: hash.clone(),
            files: None,
            base: None,
        };
        if out.items.len() >= PAGE_ITEMS || weight + sent(&item) > PAGE_ENCODED_BYTES {
            out.next = consumed.clone();
            break;
        }
        if !(request.index_only || request.have.get(key) == Some(hash)) {
            let Some(files) = load(key.clone()).await? else {
                // Gone or changed since it was listed: said, so the pass
                // is not taken for a clean one.
                say(
                    &mut out,
                    &mut weight,
                    format!("{key} (changed while syncing)"),
                );
                consumed = Some(key.clone());
                continue;
            };
            let bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
            if bytes > ITEM_CONTENT_BYTES {
                let line = format!("{key} (too large, {} KiB)", bytes.div_ceil(1024));
                say(&mut out, &mut weight, line);
                consumed = Some(key.clone());
                continue;
            }
            item.files = Some(encode(files));
            let cost = sent(&item);
            // Alone it does not fit a request at all (its key and paths
            // inflate too far): said, and the page moves on past it.
            if cost > PAGE_ENCODED_BYTES {
                say(
                    &mut out,
                    &mut weight,
                    format!("{key} (too large once encoded)"),
                );
                consumed = Some(key.clone());
                continue;
            }
            // The first item of a page always goes: the notes ahead of it
            // are few and short, and a page must move the cursor on.
            if !out.items.is_empty() && weight + cost > PAGE_ENCODED_BYTES {
                out.next = consumed.clone();
                break;
            }
        }
        weight += sent(&item);
        consumed = Some(key.clone());
        out.items.push(item);
    }
    if unsaid > 0 {
        out.not_carried.push(format!("{unsaid} more items"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(keys: &[&str]) -> BTreeMap<String, String> {
        keys.iter()
            .map(|key| ((*key).to_string(), format!("h-{key}")))
            .collect()
    }

    async fn cut(all: &BTreeMap<String, String>, request: PageRequest) -> ItemizedDomainPayload {
        page(all, &request, |key| async move {
            Ok((key != "gone").then(|| vec![(String::new(), key.into_bytes())]))
        })
        .await
        .expect("page")
    }

    fn keys(page: &ItemizedDomainPayload) -> Vec<&str> {
        page.items.iter().map(|item| item.key.as_str()).collect()
    }

    #[tokio::test]
    async fn a_page_carries_content_only_for_what_the_receiver_lacks() {
        let all = index(&["a", "b", "c"]);
        let request = PageRequest {
            have: BTreeMap::from([("b".to_string(), "h-b".to_string())]),
            ..PageRequest::default()
        };
        let page = cut(&all, request).await;
        assert_eq!(keys(&page), ["a", "b", "c"]);
        assert!(page.items[0].files.is_some());
        assert!(page.items[1].files.is_none(), "the receiver holds b");
        assert!(page.items[2].files.is_some());
        assert_eq!(page.next, None);
    }

    #[tokio::test]
    async fn a_page_stops_where_the_receivers_index_window_ends_and_resumes_there() {
        let all = index(&["a", "b", "c", "d"]);
        let first = cut(
            &all,
            PageRequest {
                until: Some("b".to_string()),
                ..PageRequest::default()
            },
        )
        .await;
        assert_eq!(keys(&first), ["a", "b"]);
        assert_eq!(first.next.as_deref(), Some("b"));
        let rest = cut(
            &all,
            PageRequest {
                after: first.next,
                ..PageRequest::default()
            },
        )
        .await;
        assert_eq!(keys(&rest), ["c", "d"]);
        assert_eq!(rest.next, None);
        // A window that covers everything leaves no cursor behind.
        let whole = cut(
            &all,
            PageRequest {
                until: Some("z".to_string()),
                ..PageRequest::default()
            },
        )
        .await;
        assert_eq!(whole.next, None);
    }

    #[tokio::test]
    async fn an_index_lists_every_item_without_content() {
        let all = index(&["a", "b"]);
        let page = cut(
            &all,
            PageRequest {
                index_only: true,
                ..PageRequest::default()
            },
        )
        .await;
        assert!(page.items.iter().all(|item| item.files.is_none()));
        assert_eq!(page.hash, domain_hash(&all));
    }

    #[tokio::test]
    async fn an_item_that_went_away_is_named_and_long_keys_cut_the_page() {
        let all = index(&["a", "gone"]);
        let page = cut(&all, PageRequest::default()).await;
        assert_eq!(keys(&page), ["a"]);
        assert_eq!(page.not_carried, ["gone (changed while syncing)"]);

        // Two thousand items with long keys and a kilobyte each stay under
        // the item count and the content size, and would not fit a request.
        let many: BTreeMap<String, String> = (0..2_000)
            .map(|n| {
                (
                    format!("{n:05}-{}", "k".repeat(740)),
                    "sha256:h".to_string(),
                )
            })
            .collect();
        let first = super::page(&many, &PageRequest::default(), |_| async {
            Ok(Some(vec![(String::new(), vec![7u8; 1_048])]))
        })
        .await
        .expect("page");
        assert!(first.next.is_some(), "the page was cut");
        let encoded = serde_json::to_vec(&first).expect("json").len();
        assert!(encoded < 4 * 1024 * 1024 - 64 * 1024, "{encoded}");

        // Keys JSON inflates sixfold, and more notes than a page says.
        let odd: BTreeMap<String, String> = (0..1_000)
            .map(|n| {
                (
                    format!("{n:04}{}", "\u{1}".repeat(700)),
                    "sha256:h".to_string(),
                )
            })
            .collect();
        let page = super::page(&odd, &PageRequest::default(), |key| async move {
            Ok((!key.starts_with("00")).then(|| vec![(String::new(), vec![7u8; 64])]))
        })
        .await
        .expect("page");
        let encoded = serde_json::to_vec(&page).expect("json").len();
        assert!(encoded < 4 * 1024 * 1024 - 64 * 1024, "{encoded}");
        assert_eq!(page.not_carried.len(), PAGE_NOTES + 1);
        assert!(page.not_carried[PAGE_NOTES].ends_with("more items"));
        assert!(validate(ConfigSyncDomain::Memory, &page).is_ok());
    }

    #[tokio::test]
    async fn a_page_that_skips_its_first_items_still_moves_the_cursor_on() {
        // Two items go away, then two that each fill most of a page.
        let all = index(&["a-gone", "b-gone", "c", "d"]);
        let load = |key: String| async move {
            Ok((!key.ends_with("gone")).then(|| vec![(String::new(), vec![7u8; 1_500_000])]))
        };
        let first = super::page(&all, &PageRequest::default(), load)
            .await
            .expect("page");
        assert_eq!(keys(&first), ["c"]);
        assert_eq!(first.next.as_deref(), Some("c"));
        // And when nothing at all could be carried before the cut.
        let none: BTreeMap<String, String> = (0..PAGE_ITEMS + 5)
            .map(|n| (format!("{n:05}-gone"), "h".to_string()))
            .collect();
        let page = super::page(&none, &PageRequest::default(), load)
            .await
            .expect("page");
        assert!(page.items.is_empty());
        assert_eq!(page.next, None, "every key was dealt with");
    }

    #[test]
    fn a_secret_is_a_field_with_a_real_value_not_the_mention_of_one() {
        for secret in [
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtz\n",
            "users:\n- user:\n    client-key-data: LS0tLS1CRUdJTiBSU0EgUFJJVkFURSBLRVktLS0tLQ==\n",
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,2F1A9C0B7D3E4F5A6B7C8D9E0F1A2B3C\n\nMIIEpAIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\n",
            "{\"access_token\": \"ya29.a0AfH6SMBx7r3kQ9vT2uYp1LzWqE8\"}",
            "OPENAI_API_KEY=sk-proj-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c\n",
            "export GITHUB_TOKEN='ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8'\n",
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYzQ9v8u7t6s5r4\n",
        ] {
            assert!(secret_bytes(secret.as_bytes()), "{secret}");
        }
        for ordinary in [
            "The response has an `access_token` and a `refresh_token`.",
            "{\"access_token\": \"<token>\"}",
            "{\"access_token\": \"YOUR_ACCESS_TOKEN_GOES_HERE_PLEASE\"}",
            "OPENAI_API_KEY=your-key-here\n",
            "OPENAI_API_KEY=\n",
            "Paste the block that starts with -----BEGIN PRIVATE KEY----- into the field.",
            "set API_KEY to the value from the dashboard",
        ] {
            assert!(!secret_bytes(ordinary.as_bytes()), "{ordinary}");
        }
        for (name, kept) in [
            (".env", true),
            ("prod.env", true),
            (".env.example", false),
            ("env.md", false),
            ("deploy.pem", true),
            ("id_ed25519", true),
            ("environment.md", false),
        ] {
            assert_eq!(key_file_name(name), kept, "{name}");
        }
    }

    #[test]
    fn a_page_with_a_key_twice_or_out_of_order_is_refused() {
        let item = |key: &str| ConfigSyncItem {
            key: key.into(),
            hash: "h".into(),
            files: None,
            base: None,
        };
        let page = |keys: &[&str]| ItemizedDomainPayload {
            hash: String::new(),
            items: keys.iter().map(|key| item(key)).collect(),
            next: None,
            not_carried: Vec::new(),
        };
        assert!(validate(ConfigSyncDomain::Memory, &page(&["a", "b"])).is_ok());
        assert!(validate(ConfigSyncDomain::Memory, &page(&["a", "a"])).is_err());
        assert!(validate(ConfigSyncDomain::Memory, &page(&["b", "a"])).is_err());
    }
}
