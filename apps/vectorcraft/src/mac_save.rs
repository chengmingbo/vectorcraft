//! Save sheets with independent directory and filename settings. rfd 0.17 joins these before
//! setting `directoryURL`, so AppKit receives a nonexistent directory and reuses an old location.
#![allow(unsafe_code)]

use std::sync::mpsc::{Receiver, channel};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSModalResponseOK, NSSavePanel, NSView};
use objc2_foundation::{NSArray, NSString, NSURL};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use vectorcraft_ui_egui::FilePick;

/// Begin a nonblocking sheet on the app's window. The receiver answers once, including Cancel.
pub fn show(window: &winit::window::Window, pick: &FilePick) -> Option<Receiver<Vec<String>>> {
    let mtm = MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else { return None };
    // SAFETY: winit owns this live window's content view; all AppKit work stays on the main thread.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let parent = view.window()?;
    let panel = NSSavePanel::savePanel(mtm);
    panel.setCanCreateDirectories(true);
    let extensions: Vec<_> = pick.filters.iter().flat_map(|(_, exts)| exts.iter()).map(|ext| NSString::from_str(ext)).collect();
    if !extensions.is_empty() {
        // Match the existing save panel's union of supported types; format follows the extension.
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&extensions)));
    }
    if let Some(folder) = &pick.folder {
        let folder = std::path::absolute(folder).ok()?;
        let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&folder.to_string_lossy()), true);
        panel.setDirectoryURL(Some(&url));
    }
    panel.setNameFieldStringValue(&NSString::from_str(&pick.name));
    let (tx, rx) = channel();
    let finished = panel.clone();
    let completion = block2::RcBlock::new(move |response| {
        let paths = if response == NSModalResponseOK {
            finished.URL().and_then(|url| url.path()).map(|p| vec![p.to_string()]).unwrap_or_default()
        } else {
            Vec::new()
        };
        // Closing the app while a sheet is open can drop its receiver.
        let _ = tx.send(paths);
    });
    panel.beginSheetModalForWindow_completionHandler(&parent, &completion);
    Some(rx)
}
