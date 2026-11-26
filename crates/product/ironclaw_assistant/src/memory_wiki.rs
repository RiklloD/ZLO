//! The local knowledge wiki (Karpathy's "LLM wiki", as Hermes' `llm-wiki`
//! skill keeps it), inside the caller's local memory under `wiki/`.
//!
//! This module only scaffolds and reports: `SCHEMA.md`, `index.md` and
//! `log.md` from `prompts/memory_wiki/`. Agents maintain the pages through
//! their memory tools, guided by the bundled `llm-wiki` skill. A scaffold
//! write is an atomic create, so an existing page is never overwritten.

use ironclaw_product_contracts::memory_page::{
    MEMORY_WIKI_ROOT, MemoryDocumentsSource, MemoryPageWriteOutcome, MemoryWikiInitResponse,
    MemoryWikiStatus, MemoryWriteRequest,
};
use ironclaw_product_contracts::surface::{ProductSurfaceCaller, ProductSurfaceError};

const SCHEMA: &str = include_str!("../prompts/memory_wiki/SCHEMA.md");
const INDEX: &str = include_str!("../prompts/memory_wiki/index.md");
const LOG: &str = include_str!("../prompts/memory_wiki/log.md");

fn scaffold(today: &str) -> [(String, String); 3] {
    [
        (format!("{MEMORY_WIKI_ROOT}/SCHEMA.md"), SCHEMA.to_string()),
        (format!("{MEMORY_WIKI_ROOT}/index.md"), INDEX.to_string()),
        (
            format!("{MEMORY_WIKI_ROOT}/log.md"),
            LOG.replace("{{date}}", today),
        ),
    ]
}

pub(crate) async fn status(
    documents: Option<&dyn MemoryDocumentsSource>,
    caller: &ProductSurfaceCaller,
) -> Result<MemoryWikiStatus, ProductSurfaceError> {
    let Some(documents) = documents else {
        return Ok(MemoryWikiStatus {
            available: false,
            initialized: false,
            pages: 0,
        });
    };
    let prefix = format!("{MEMORY_WIKI_ROOT}/");
    let tree = documents.tree(caller).await?;
    let pages = tree.iter().filter(|e| e.path.starts_with(&prefix)).count();
    // Built means every scaffold page is there: an interrupted build stays
    // "not built", so the Build action comes back and finishes it.
    let initialized = scaffold("")
        .iter()
        .all(|(path, _)| tree.iter().any(|e| &e.path == path));
    Ok(MemoryWikiStatus {
        available: true,
        initialized,
        pages,
    })
}

pub(crate) async fn init(
    documents: Option<&dyn MemoryDocumentsSource>,
    caller: &ProductSurfaceCaller,
) -> Result<MemoryWikiInitResponse, ProductSurfaceError> {
    let source = documents.ok_or_else(|| ProductSurfaceError::unavailable(false))?;
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut created = Vec::new();
    let mut kept = Vec::new();
    for (path, content) in scaffold(&today) {
        let response = source
            .write(
                caller,
                MemoryWriteRequest {
                    path: path.clone(),
                    content,
                    expected_content_hash: None,
                },
            )
            .await?;
        match response.outcome {
            MemoryPageWriteOutcome::Written => created.push(path),
            MemoryPageWriteOutcome::Conflict => kept.push(path),
        }
    }
    let status = status(documents, caller).await?;
    if !status.initialized {
        // The writes reported success but the read-back cannot see the
        // schema: do not claim a wiki exists.
        return Err(ProductSurfaceError::unavailable(true));
    }
    Ok(MemoryWikiInitResponse {
        created,
        kept,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_dates_the_log_and_keeps_paths_under_the_root() {
        let files = scaffold("2026-11-26");
        assert!(files.iter().all(|(path, _)| path.starts_with("wiki/")));
        let log = &files[2].1;
        assert!(log.contains("2026-11-26: wiki created"));
        assert!(!log.contains("{{date}}"));
        assert!(files[0].1.contains("## Tag taxonomy"));
    }
}
