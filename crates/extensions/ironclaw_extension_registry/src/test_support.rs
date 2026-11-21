//! Dev-only seams, compiled only for this crate's tests and the
//! `test-support` feature (never in a production build).

use crate::ExtensionPackage;

/// Explicit test construction: a package whose source needs consent but
/// whose files the test itself wrote, run without any content binding. A
/// production build has no way to construct such a package — a third-party
/// package reaches a lane only with the pins of its consent grant (the
/// lifecycle's publication and the generic host's loader attach them).
pub trait UnverifiedContentsForTest {
    fn with_unverified_contents_for_test(self) -> Self;
}

impl UnverifiedContentsForTest for ExtensionPackage {
    fn with_unverified_contents_for_test(mut self) -> Self {
        self.content_pins = Some(crate::package::unverified_test_pins());
        self
    }
}
