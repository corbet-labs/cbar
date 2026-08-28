//! The launcher is another window of cbar's existing GTK application.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone)]
pub struct Launcher {
    application: gtk::Application,
    ui: Rc<RefCell<Option<cbar_launcher::LauncherUi>>>,
    display: Rc<RefCell<Option<gtk::gdk::Display>>>,
    initializing: Rc<Cell<bool>>,
    desired_visible: Rc<Cell<bool>>,
}

impl std::fmt::Debug for Launcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Launcher")
            .field("initialized", &self.ui.borrow().is_some())
            .finish_non_exhaustive()
    }
}

impl Launcher {
    pub fn new(application: &gtk::Application) -> Self {
        Self {
            application: application.clone(),
            ui: Rc::new(RefCell::new(None)),
            display: Rc::new(RefCell::new(None)),
            initializing: Rc::new(Cell::new(false)),
            desired_visible: Rc::new(Cell::new(false)),
        }
    }

    /// Warm the configured launcher only after cbar's bars have been constructed. An ordinary
    /// Ironbar setup therefore pays no launcher disk/theme work, and even configured setups get
    /// their bar first. Explicit show remains able to initialize on demand if it wins the race.
    pub fn warm(&self) {
        let launcher = self.clone();
        let prepared = crate::Ironbar::runtime()
            .handle()
            .spawn_blocking(cbar_launcher::prepare_if_configured);
        gtk::glib::spawn_future_local(async move {
            if let Ok(Some(prepared)) = prepared.await {
                let Some(display) = launcher.launcher_display() else {
                    eprintln!("cbar launcher: unable to open a GDK display");
                    return;
                };
                let theme = cbar_launcher::capture_icon_theme(&display);
                let prepared = crate::Ironbar::runtime()
                    .handle()
                    .spawn_blocking(move || cbar_launcher::prepare_icons(prepared, theme));
                if let Ok(prepared) = prepared.await {
                    launcher.ensure_initialized_with(prepared, &display);
                }
            }
        });
    }

    /// Initialize on the blocking runtime when an IPC action beats speculative warmup. The action
    /// returns immediately, leaving cbar's bars responsive; no config or mutable-state file is read
    /// on GTK. The same path reloads an existing launcher: a changed config replaces the complete UI
    /// so fields captured by GTK construction (theme, keymap, terminal and focus policy included)
    /// cannot become partially/stickily applied. Repeated actions coalesce onto one preparation.
    fn initialize_async(&self) {
        if self.initializing.replace(true) {
            return;
        }
        let launcher = self.clone();
        let prepared = crate::Ironbar::runtime()
            .handle()
            .spawn_blocking(cbar_launcher::prepare);
        gtk::glib::spawn_future_local(async move {
            match prepared.await {
                Ok(prepared) => {
                    let Some(display) = launcher.launcher_display() else {
                        eprintln!("cbar launcher: unable to open a GDK display");
                        launcher.initializing.set(false);
                        return;
                    };
                    let theme = cbar_launcher::capture_icon_theme(&display);
                    let prepared = crate::Ironbar::runtime()
                        .handle()
                        .spawn_blocking(move || cbar_launcher::prepare_icons(prepared, theme));
                    match prepared.await {
                        Ok(prepared) => launcher.ensure_initialized_with(prepared, &display),
                        Err(error) => {
                            eprintln!("cbar launcher: icon preparation worker failed: {error}")
                        }
                    }
                }
                Err(error) => eprintln!("cbar launcher: preparation worker failed: {error}"),
            }
            launcher.initializing.set(false);
        });
    }

    fn launcher_display(&self) -> Option<gtk::gdk::Display> {
        if let Some(display) = self.display.borrow().as_ref() {
            return Some(display.clone());
        }
        // A separate connection gives the embedded Golden Master a separate CSS provider domain;
        // the default display is retained only as a portability fallback when a backend refuses a
        // second connection. No display is opened at all for an unconfigured launcher.
        let display = gtk::gdk::Display::open(None).or_else(gtk::gdk::Display::default)?;
        self.display.replace(Some(display.clone()));
        Some(display)
    }

    fn ensure_initialized_with(
        &self,
        mut prepared: cbar_launcher::PreparedLauncher,
        display: &gtk::gdk::Display,
    ) {
        let current = self.ui.borrow().as_ref().cloned();
        match current {
            None => {
                self.ui
                    .replace(Some(cbar_launcher::LauncherUi::attach_prepared(
                        &self.application,
                        crate::Ironbar::runtime().handle(),
                        prepared,
                        display,
                    )));
            }
            Some(current) if current.prepare_replacement(&mut prepared) => {
                let replacement = cbar_launcher::LauncherUi::attach_prepared(
                    &self.application,
                    crate::Ironbar::runtime().handle(),
                    prepared,
                    display,
                );
                self.ui.replace(Some(replacement));
                current.retire();
            }
            Some(current) => current.refresh(),
        }
        self.apply_desired_visibility();
    }

    fn apply_desired_visibility(&self) {
        let ui = self.ui.borrow().as_ref().cloned();
        let Some(ui) = ui else {
            return;
        };
        if self.desired_visible.get() && !ui.is_visible() {
            ui.show();
        } else if !self.desired_visible.get() && ui.is_visible() {
            ui.hide();
        }
    }

    pub fn show(&self) -> Result<(), String> {
        self.desired_visible.set(true);
        // A coherent resident UI is the launcher's local last-known-good state. Map it before
        // asking the blocking pool to check config/state again: pool pressure, a slow filesystem,
        // or a half-written declarative update must never make an already-resident launcher look
        // dead. A prepared replacement is still swapped atomically when it arrives.
        self.apply_desired_visibility();
        self.initialize_async();
        Ok(())
    }

    pub fn hide(&self) {
        self.desired_visible.set(false);
        self.apply_desired_visibility();
    }

    pub fn toggle(&self) -> Result<(), String> {
        let visible = self
            .ui
            .borrow()
            .as_ref()
            .map_or_else(|| self.desired_visible.get(), |ui| ui.is_visible());
        self.desired_visible.set(!visible);
        if visible {
            self.apply_desired_visibility();
        } else {
            self.apply_desired_visibility();
            self.initialize_async();
        }
        Ok(())
    }

    pub fn refresh(&self) -> Result<(), String> {
        self.initialize_async();
        Ok(())
    }

    pub fn status(&self) -> String {
        match self.ui.borrow().as_ref() {
            Some(ui) if ui.is_visible() => "visible".to_string(),
            Some(_) => "resident".to_string(),
            None => "uninitialized".to_string(),
        }
    }

    pub fn owns_window(&self, window: &gtk::Window) -> bool {
        self.ui
            .borrow()
            .as_ref()
            .is_some_and(|ui| ui.owns_window(window))
    }
}
