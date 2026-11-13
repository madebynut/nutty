//! Embedded application artwork alongside GPUI Kit's component assets.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

#[cfg(target_os = "macos")]
#[path = "branding/macos.rs"]
mod macos;

/// Give direct GUI launches a real bundle so LaunchServices can resolve the process icon.
pub(crate) fn prepare_launch() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    macos::prepare_launch()?;
    Ok(())
}

pub(crate) const WORKSPACE_LOGO: &str = "nutty/workspace-logo.png";
/// The acorn alone, as a single-colour mask the interface tints to its theme.
pub(crate) const LOGO_MARK: &str = "nutty/logo-mark.svg";
/// The prompt cut out of [`LOGO_MARK`], tinted in the contrasting colour.
pub(crate) const LOGO_PROMPT: &str = "nutty/logo-prompt.svg";

pub(crate) struct Assets;

/// Register native identity for both bundled and directly launched builds.
pub(crate) fn register_application() {
    #[cfg(target_os = "macos")]
    {
        use objc2::{AnyThread as _, MainThreadMarker};
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::{NSData, NSProcessInfo, NSString};

        NSProcessInfo::processInfo().setProcessName(&NSString::from_str("Nutty"));
        let Some(main_thread) = MainThreadMarker::new() else {
            tracing::error!("native application identity requires the main thread");
            return;
        };
        let bytes = NSData::with_bytes(include_bytes!("../assets/Nutty.icns"));
        if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &bytes) {
            // SAFETY: AppKit is initialized, this is the main thread and `icon` is a
            // live NSImage decoded from the application's embedded ICNS resource.
            unsafe {
                NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&icon));
            }
        } else {
            tracing::error!("could not decode application icon");
        }
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        // The image loader caches these resources; rendering never reads the source tree.
        if path == WORKSPACE_LOGO {
            Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/Nutty-workspace.png"
            ))))
        } else if path == LOGO_MARK {
            Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/Nutty-mark.svg"
            ))))
        } else if path == LOGO_PROMPT {
            Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/Nutty-prompt.svg"
            ))))
        } else {
            gpui_kit::assets::AllAssets.load(path)
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut assets = gpui_kit::assets::AllAssets.list(path)?;
        for asset in [WORKSPACE_LOGO, LOGO_MARK, LOGO_PROMPT] {
            if asset.starts_with(path) {
                assets.push(asset.into());
            }
        }
        Ok(assets)
    }
}
