//! The server's tool definitions, verbatim from its `tools/list`
//! (`catalog/tools.json`). Static so offering the tools never starts the
//! process; a tool the installed build lacks fails at call time.

use std::sync::OnceLock;

use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_CAPABILITY_PREFIX, ComputerUseToolSpec,
};
use serde::Deserialize;

const TOOLS_JSON: &str = include_str!("../catalog/tools.json");

#[derive(Deserialize)]
struct Catalog {
    tools: Vec<ComputerUseToolSpec>,
}

/// Every tool, in catalog order. Empty only if the embedded file is broken,
/// which the crate tests rule out.
pub fn computer_use_tools() -> &'static [ComputerUseToolSpec] {
    static TOOLS: OnceLock<Vec<ComputerUseToolSpec>> = OnceLock::new();
    TOOLS.get_or_init(|| match serde_json::from_str::<Catalog>(TOOLS_JSON) {
        Ok(catalog) => catalog.tools,
        Err(error) => {
            tracing::error!(%error, "computer-use tool catalog is unreadable");
            Vec::new()
        }
    })
}

pub fn computer_use_tool(name: &str) -> Option<&'static ComputerUseToolSpec> {
    computer_use_tools().iter().find(|tool| tool.name == name)
}

/// The tool behind a `computer_use.<tool>` capability id.
pub fn tool_for_capability(capability_id: &str) -> Option<&'static ComputerUseToolSpec> {
    capability_id
        .strip_prefix(COMPUTER_USE_CAPABILITY_PREFIX)
        .and_then(computer_use_tool)
}
