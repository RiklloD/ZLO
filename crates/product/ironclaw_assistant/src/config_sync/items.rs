//! What the itemized domains (`skills`, `memory`) share: how a page is cut
//! and how items are hashed, so both ends agree on "the same".

use std::collections::BTreeMap;

use base64::Engine as _;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncItem, ConfigSyncItemFile, ItemizedDomainPayload,
};
use sha2::{Digest as _, Sha256};

use super::{ConfigSyncError, hex_lower};

/// Content carried in one page before it is cut (before base64). A page and
/// its envelope stay under the snapshot limit.
pub const PAGE_CONTENT_BYTES: usize = 2 * 1024 * 1024;
/// An item larger than this is not carried at all.
pub const ITEM_CONTENT_BYTES: usize = PAGE_CONTENT_BYTES;
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
        too_large: Vec::new(),
    };
    let mut carried = 0usize;
    for (key, hash) in all {
        if request.after.as_ref().is_some_and(|after| key <= after) {
            continue;
        }
        if out.items.len() >= PAGE_ITEMS || carried >= PAGE_CONTENT_BYTES {
            out.next = out.items.last().map(|item| item.key.clone());
            break;
        }
        let known = request.index_only || request.have.get(key) == Some(hash);
        let files = if known {
            None
        } else {
            let Some(files) = load(key.clone()).await? else {
                continue;
            };
            let bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
            if bytes > ITEM_CONTENT_BYTES {
                out.too_large
                    .push(format!("{key} ({} KiB)", bytes.div_ceil(1024)));
                continue;
            }
            if carried > 0 && carried + bytes > PAGE_CONTENT_BYTES {
                out.next = out.items.last().map(|item| item.key.clone());
                break;
            }
            carried += bytes;
            Some(encode(files))
        };
        out.items.push(ConfigSyncItem {
            key: key.clone(),
            hash: hash.clone(),
            files,
        });
    }
    Ok(out)
}
