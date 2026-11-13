//! `xdg_wm_dialog_v1` (staging xdg-dialog-v1.xml), through smithay's
//! implementation.
//!
//! GTK 4 and Qt 6 mark their dialogs with it and set the modal hint on the
//! ones that block their parent. smithay keeps the hint in the toplevel's
//! role data; the window snapshot reads it into `suggests_float`.

use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::shell::xdg::{
        ToplevelSurface,
        dialog::{ToplevelDialogHint, XdgDialogHandler, XdgDialogState},
    },
};

use crate::{handlers::xdg_shell::mark_toplevel_metadata_dirty, state::ShojiWM};

pub fn init_xdg_dialog(display: &DisplayHandle) {
    XdgDialogState::new::<ShojiWM>(display);
}

impl XdgDialogHandler for ShojiWM {
    fn dialog_hint_changed(&mut self, toplevel: ToplevelSurface, _hint: ToplevelDialogHint) {
        mark_toplevel_metadata_dirty(self, &toplevel, "xdg-dialog-hint-changed");
    }
}
