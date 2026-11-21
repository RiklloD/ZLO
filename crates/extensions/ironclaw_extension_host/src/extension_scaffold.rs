//! `zlo extension new <id>`: the files of a minimal, valid third-party
//! package ("plugin").
//!
//! The example runs on the MCP-over-HTTP runtime: it is the one runtime a
//! third party can serve today without a build step (external stdio MCP is
//! refused by the MCP lane, and a WASM package needs a compiled component).
//! Templates live as files under `templates/plugin/` and are filled with the
//! extension id and a display name; nothing here writes to disk.

use ironclaw_host_api::ids::ExtensionId;

const MANIFEST_TEMPLATE: &str = include_str!("../templates/plugin/manifest.toml");
const INPUT_SCHEMA_TEMPLATE: &str = include_str!("../templates/plugin/echo.input.v1.json");
const PROMPT_TEMPLATE: &str = include_str!("../templates/plugin/echo.md");
const README_TEMPLATE: &str = include_str!("../templates/plugin/README.md");

/// `(relative path, bytes)` for every file of a fresh package with this id.
pub fn scaffold_extension_package(id: &ExtensionId) -> Vec<(String, Vec<u8>)> {
    let name = display_name(id.as_str());
    let fill = |template: &str| {
        template
            .replace("{{id}}", id.as_str())
            .replace("{{name}}", &name)
            .into_bytes()
    };
    vec![
        ("manifest.toml".to_string(), fill(MANIFEST_TEMPLATE)),
        (
            format!("schemas/{}/echo.input.v1.json", id.as_str()),
            fill(INPUT_SCHEMA_TEMPLATE),
        ),
        (
            format!("prompts/{}/echo.md", id.as_str()),
            fill(PROMPT_TEMPLATE),
        ),
        ("README.md".to_string(), fill(README_TEMPLATE)),
    ]
}

/// `sample-notes` → `Sample Notes`.
fn display_name(id: &str) -> String {
    id.split(['-', '_', '.'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_extension_registry::{ExtensionRuntimeV2, ManifestSource};

    #[test]
    fn scaffold_is_a_valid_third_party_package() {
        let id = ExtensionId::new("sample-notes").expect("id");
        let files = scaffold_extension_package(&id);
        let manifest = files
            .iter()
            .find(|(path, _)| path == "manifest.toml")
            .map(|(_, bytes)| String::from_utf8(bytes.clone()).expect("utf8"))
            .expect("manifest");
        assert!(!manifest.contains("{{"), "every placeholder is filled");
        let record = crate::parse_imported_manifest(&manifest, ManifestSource::InstalledLocal)
            .expect("the scaffold manifest validates as an installed-local package");
        assert_eq!(record.manifest().id.as_str(), "sample-notes");
        assert!(matches!(
            record.manifest().runtime,
            ExtensionRuntimeV2::Mcp { .. }
        ));
        let tool = record
            .resolved()
            .tools
            .iter()
            .find(|tool| tool.id.as_str() == "sample-notes.echo")
            .expect("example tool");
        for declared in [
            tool.input_schema_ref.as_str().to_string(),
            tool.prompt_doc_ref
                .as_ref()
                .map(|doc| doc.as_str().to_string())
                .expect("prompt doc"),
        ] {
            assert!(
                files.iter().any(|(path, _)| *path == declared),
                "scaffold ships the declared asset {declared}"
            );
        }
        assert_eq!(display_name("sample-notes"), "Sample Notes");
    }
}
