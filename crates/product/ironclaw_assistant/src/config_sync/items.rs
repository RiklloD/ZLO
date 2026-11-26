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
/// JSON around one item or file (field names, quotes, separators).
const ITEM_OVERHEAD_BYTES: usize = 64;
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
    // Estimated encoded weight of the page so far. A key is counted
    // twice over: JSON may escape it.
    let mut weight = 0usize;
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
        let listed = 2 * key.len() + hash.len() + ITEM_OVERHEAD_BYTES;
        if out.items.len() >= PAGE_ITEMS || weight + listed > PAGE_ENCODED_BYTES {
            out.next = out.items.last().map(|item| item.key.clone());
            break;
        }
        let known = request.index_only || request.have.get(key) == Some(hash);
        let files = if known {
            None
        } else {
            let Some(files) = load(key.clone()).await? else {
                // Gone or changed since it was listed: said, so the pass
                // is not taken for a clean one.
                out.not_carried
                    .push(format!("{key} (changed while syncing)"));
                continue;
            };
            let bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
            if bytes > ITEM_CONTENT_BYTES {
                out.not_carried
                    .push(format!("{key} (too large, {} KiB)", bytes.div_ceil(1024)));
                continue;
            }
            let encoded: usize = files
                .iter()
                .map(|(path, b)| 2 * path.len() + b.len().div_ceil(3) * 4 + ITEM_OVERHEAD_BYTES)
                .sum();
            if !out.items.is_empty() && weight + listed + encoded > PAGE_ENCODED_BYTES {
                out.next = out.items.last().map(|item| item.key.clone());
                break;
            }
            weight += encoded;
            Some(encode(files))
        };
        weight += listed;
        out.items.push(ConfigSyncItem {
            key: key.clone(),
            hash: hash.clone(),
            files,
        });
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
    }
}
