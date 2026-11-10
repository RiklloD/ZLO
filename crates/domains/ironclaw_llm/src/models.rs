//! Provider-neutral discovered-model capability types.

/// Provider-neutral model input/output modality discovered from a model catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelModality {
    Text,
    Image,
    Audio,
    Video,
    Embedding,
}

impl ModelModality {
    /// Normalize a provider-supplied modality while ignoring unknown future
    /// values. Unknown values must not make model discovery fail.
    pub(crate) fn from_provider_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "image" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            "embedding" | "embeddings" => Some(Self::Embedding),
            _ => None,
        }
    }
}

/// Provider-neutral entry returned by detailed model discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub input_modalities: Vec<ModelModality>,
    pub output_modalities: Vec<ModelModality>,
}

impl DiscoveredModel {
    /// Preserve compatibility for providers that only expose model IDs.
    pub fn from_id(id: String) -> Self {
        Self {
            id,
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
        }
    }
}
