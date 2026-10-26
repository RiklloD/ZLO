//! Computer-use screenshots for vision models: the model port asks for the
//! picture of each `computer_use.*` tool result, and this reads the frame the
//! engine returned with that call (the session keeps the newest few).

use std::sync::Arc;

use base64::Engine as _;
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_loop_host::{HostManagedModelImagePart, LoopToolImagePort};
use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_CAPABILITY_PREFIX, ComputerUseSession,
};

pub(crate) struct ComputerUseToolImages(pub(crate) Arc<ironclaw_computer_use::ComputerUse>);

#[async_trait::async_trait]
impl LoopToolImagePort for ComputerUseToolImages {
    async fn tool_image(
        &self,
        scope: &ResourceScope,
        capability_id: &CapabilityId,
        result_ref: &str,
    ) -> Option<HostManagedModelImagePart> {
        if !capability_id
            .as_str()
            .starts_with(COMPUTER_USE_CAPABILITY_PREFIX)
        {
            return None;
        }
        let session = ComputerUseSession::from_scope(scope)?;
        let frame = self.0.result_frame(&session, result_ref)?;
        image_part(&frame.mime_type, &frame.data)
    }
}

/// The decoded picture of one frame, handed to the model port as an image
/// part; it never becomes tool-result text.
fn image_part(mime_type: &str, data: &str) -> Option<HostManagedModelImagePart> {
    match base64::engine::general_purpose::STANDARD.decode(data.as_bytes()) {
        Ok(bytes) => Some(HostManagedModelImagePart {
            mime_type: mime_type.to_string(),
            bytes,
        }),
        // silent-ok: a frame that does not decode is skipped; the model
        // still has the tool's text result.
        Err(error) => {
            tracing::debug!(%error, "computer-use frame is not valid base64");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::image_part;

    #[test]
    fn a_frame_becomes_an_image_part_and_bad_base64_is_skipped() {
        let part = image_part("image/jpeg", "/9j/4A==").expect("valid frame");
        assert_eq!(part.mime_type, "image/jpeg");
        assert_eq!(part.bytes, [0xff, 0xd8, 0xff, 0xe0]);
        assert!(image_part("image/jpeg", "not base64!").is_none());
    }
}
