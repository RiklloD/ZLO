//! The app's asset source: gpui-kit's default component icons plus the few
//! extra Lucide icons the inbox and goal strip use (a few KB, instead of
//! embedding the whole 1,830-icon catalog).

use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, SharedString};

icon_assets!(
    ExtraIcons,
    [
        Pin,
        PinOff,
        AlarmClock,
        GitBranch,
        ListFilter,
        Target,
        Archive,
        ArchiveRestore,
        Pencil,
        Bell
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// The extra icons by name, so screens never spell asset paths.
pub use gpui_kit::assets::IconName as ExtraIcon;
