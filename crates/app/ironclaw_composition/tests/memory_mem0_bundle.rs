//! The mem0 provider bundle, built from mem0's OWN manifest and asset table.
//!
//! These tests lived in `ironclaw_host_runtime::memory_native_extension`, which
//! compiled mem0's `manifest.toml` in through a cross-crate `include_str!`
//! (§11.2.7, `reborn_cross_crate_include_scan.rs`). The host may not name a
//! memory provider crate (`only_the_sanctioned_residue_names_a_memory_provider`),
//! so the manifest now flows the way mem0's asset table already did:
//! `ironclaw_memory_mem0::{MANIFEST_TOML, MEMORY_ASSETS}` passed in by
//! composition — the one non-binary crate sanctioned to name mem0. This file
//! drives that exact call.
//!
//! Not gated on `memory-mem0`: the bundle builder is always compiled into
//! `ironclaw_host_runtime`, and mem0 is an unconditional dev-dependency here.

#![deny(clippy::let_underscore_must_use)]
use ironclaw_extension_contracts::memory::MemoryLifecycleHook;
use ironclaw_extension_contracts::runtime::ExtensionRuntime;
use ironclaw_extension_registry::CapabilityVisibility;
use ironclaw_host_runtime::memory_native_extension::{
    BundledMemoryProvider, MEM0_MEMORY_EXTENSION_ID, MEM0_MEMORY_PROVIDER_SERVICE,
    mem0_memory_provider_bundle,
};
use ironclaw_memory::{
    MEMORY_READ_CAPABILITY_ID, MEMORY_SEARCH_CAPABILITY_ID, MEMORY_TREE_CAPABILITY_ID,
    MEMORY_WRITE_CAPABILITY_ID, PROFILE_SET_CAPABILITY_ID,
};

fn mem0_bundle() -> BundledMemoryProvider {
    mem0_memory_provider_bundle(
        ironclaw_memory_mem0::MANIFEST_TOML,
        ironclaw_memory_mem0::MEMORY_ASSETS,
    )
    .expect("mem0 bundle builds from its own manifest and asset table")
}

/// The host's mirror of mem0's extension id must agree with the package's.
#[test]
fn host_mirror_of_the_mem0_extension_id_matches_the_package() {
    assert_eq!(
        MEM0_MEMORY_EXTENSION_ID,
        ironclaw_memory_mem0::MEM0_MEMORY_EXTENSION_ID
    );
}

/// mem0's manifest is the source of truth for its own tool surface: the
/// document tools plus profile-set, declared under the reserved stable
/// `ironclaw.memory.*` ids so a backend swap never renames the model's tools,
/// and an honest lifecycle (F5): the long-term retrieval lane and profile reads,
/// no short-term lane, no interaction recording — undeclared hooks are never
/// called by the host.
#[test]
fn mem0_provider_bundle_builds_with_its_honest_lifecycle_and_tools() {
    let bundle = mem0_bundle();
    let manifest = &bundle.package.manifest;
    assert_eq!(manifest.id.as_str(), MEM0_MEMORY_EXTENSION_ID);
    match &manifest.runtime {
        ExtensionRuntime::FirstParty { service } => {
            assert_eq!(service, MEM0_MEMORY_PROVIDER_SERVICE);
        }
        other => panic!("expected first_party runtime, got {other:?}"),
    }
    let ids: Vec<&str> = manifest
        .capabilities
        .iter()
        .map(|capability| capability.id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![
            MEMORY_READ_CAPABILITY_ID,
            MEMORY_WRITE_CAPABILITY_ID,
            MEMORY_SEARCH_CAPABILITY_ID,
            MEMORY_TREE_CAPABILITY_ID,
            PROFILE_SET_CAPABILITY_ID,
        ]
    );
    for capability in &manifest.capabilities {
        assert_eq!(capability.visibility, CapabilityVisibility::Model);
    }
    assert!(bundle.lifecycle.declares(MemoryLifecycleHook::ReadLongTerm));
    assert!(bundle.lifecycle.declares(MemoryLifecycleHook::ProfileRead));
    assert!(
        !bundle
            .lifecycle
            .declares(MemoryLifecycleHook::ReadShortTerm)
    );
    assert!(
        !bundle
            .lifecycle
            .declares(MemoryLifecycleHook::RecordInteraction)
    );
}

/// mem0 deliberately ships no guidance: its recall is search-first, so the
/// native provider's standing-document advice would be wrong under it. Absent
/// must mean "append nothing", never "fall back to native's".
#[test]
fn a_provider_without_a_guidance_declaration_resolves_to_none() {
    let bundle = mem0_bundle();
    assert!(bundle.lifecycle.guidance_doc.is_none());
    assert!(bundle.guidance.is_none());
}

/// A provider that schedules nothing resolves no prompts — absent must mean
/// "nothing is scheduled", never "fall back to another provider's pass".
#[test]
fn a_provider_without_scheduled_ops_resolves_no_pass_prompts() {
    let bundle = mem0_bundle();
    assert!(bundle.lifecycle.scheduled_ops.is_empty());
    assert!(bundle.scheduled_pass_prompts.is_empty());
}
