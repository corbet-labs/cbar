use crate::channels::MpscReceiverExt;
use crate::file_watch;
use gtk::ffi::GTK_STYLE_PROVIDER_PRIORITY_USER;
use gtk::{CssProvider, gio};
use std::env;
use std::path::PathBuf;
use tracing::{debug, info};

#[derive(Debug)]
pub enum CssSource {
    String(&'static str),
    File(PathBuf),
}

/// Attempts to load CSS file at the given path
/// and attach if to the current GTK application.
///
/// Installs a file watcher and reloads CSS when
/// write changes are detected on the file.
pub fn load_css(source: &CssSource) {
    let provider = CssProvider::new();

    let path = match source {
        CssSource::String(str) => {
            provider.load_from_string(str);
            debug!("loaded built-in css");
            None
        }
        CssSource::File(style_path) => {
            // file watcher requires absolute path
            let style_path = if style_path.is_absolute() {
                style_path.clone()
            } else {
                env::current_dir().expect("to exist").join(style_path)
            };

            provider.load_from_path(&style_path);
            debug!("loaded css from '{}'", style_path.display());
            Some(style_path)
        }
    };

    // Deprecation warning is an error in gtk-rs bindings
    // <https://github.com/gtk-rs/gtk4-rs/pull/2161>
    #[allow(deprecated)]
    gtk::StyleContext::add_provider_for_display(
        &crate::get_display(),
        &provider,
        GTK_STYLE_PROVIDER_PRIORITY_USER as u32,
    );

    // install file watcher
    if let Some(style_path) = path {
        let rx = file_watch::subscribe(&style_path).expect("Failed to start CSS file watcher");
        debug!("Installed CSS file watcher on '{}'", style_path.display());

        rx.recv_glib((), move |(), ()| {
            info!("Reloading CSS");
            provider.load_from_file(&gio::File::for_path(&style_path));
        });
    }
}
