//! `xdg_wm_dialog_v1` (staging xdg-dialog-v1.xml) global.
//!
//! GTK 4 and Qt 6 mark their dialogs with it and set the modal hint on the
//! ones that block their parent. The hint is recorded in smithay's toplevel
//! role data (`XdgToplevelSurfaceRoleAttributes::dialog_hint`). It does not
//! classify a window: the protocol gives it no effect on a toplevel without a
//! parent, and a parent already makes one float.
//!
//! This stands in for smithay's `XdgDialogState`, which keeps acting on a
//! dialog object after its toplevel is destroyed (writing the hint into a
//! later toplevel on the same `wl_surface`) and unwraps the lookup of that
//! toplevel. Here the object goes inert with its toplevel, as the protocol
//! asks.

use std::sync::Mutex;

use smithay::reexports::wayland_protocols::xdg::dialog::v1::server::{
    xdg_dialog_v1::{self, XdgDialogV1},
    xdg_wm_dialog_v1::{self, XdgWmDialogV1},
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::{
    ToplevelSurface, XdgShellHandler, XdgToplevelSurfaceData, dialog::ToplevelDialogHint,
};

const VERSION: u32 = 1;

#[cfg(test)]
mod tests;

/// Manager state for the `xdg_wm_dialog_v1` global.
#[derive(Debug)]
pub struct XdgDialogState;

impl XdgDialogState {
    /// Create and advertise the `xdg_wm_dialog_v1` global.
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<XdgWmDialogV1, ()>,
        D: Dispatch<XdgWmDialogV1, ()>,
        D: Dispatch<XdgDialogV1, XdgDialogData>,
        D: XdgShellHandler,
        D: 'static,
    {
        display.create_global::<D, XdgWmDialogV1, _>(VERSION, ());
        Self
    }
}

/// Per-`xdg_dialog_v1` data: its toplevel, `None` when that was already gone
/// when the object was made.
#[derive(Debug)]
pub struct XdgDialogData {
    toplevel: Option<ToplevelSurface>,
}

impl XdgDialogData {
    fn live_toplevel(&self) -> Option<&ToplevelSurface> {
        self.toplevel.as_ref().filter(|toplevel| toplevel.alive())
    }
}

/// The dialog object last made for the toplevel on a `wl_surface`, to refuse
/// a second one for the same toplevel.
#[derive(Debug, Default)]
struct SurfaceDialog(Mutex<Option<(XdgToplevel, XdgDialogV1)>>);

fn surface_dialog<T>(
    toplevel: &ToplevelSurface,
    f: impl FnOnce(&mut Option<(XdgToplevel, XdgDialogV1)>) -> T,
) -> T {
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(SurfaceDialog::default);
        let dialog = states.data_map.get::<SurfaceDialog>().unwrap();
        f(&mut dialog
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    })
}

fn set_hint(toplevel: &ToplevelSurface, hint: ToplevelDialogHint) {
    with_states(toplevel.wl_surface(), |states| {
        if let Some(role) = states.data_map.get::<XdgToplevelSurfaceData>() {
            role.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .dialog_hint = hint;
        }
    });
}

impl<D> GlobalDispatch<XdgWmDialogV1, (), D> for XdgDialogState
where
    D: GlobalDispatch<XdgWmDialogV1, ()>,
    D: Dispatch<XdgWmDialogV1, ()>,
    D: Dispatch<XdgDialogV1, XdgDialogData>,
    D: XdgShellHandler,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        manager: New<XdgWmDialogV1>,
        _data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }
}

impl<D> Dispatch<XdgWmDialogV1, (), D> for XdgDialogState
where
    D: Dispatch<XdgWmDialogV1, ()>,
    D: Dispatch<XdgDialogV1, XdgDialogData>,
    D: XdgShellHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &XdgWmDialogV1,
        request: xdg_wm_dialog_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let xdg_wm_dialog_v1::Request::GetXdgDialog { id, toplevel } = request else {
            return;
        };
        let toplevel = state.xdg_shell_state().get_toplevel(&toplevel);
        // The object is made even on the error path, to consume the id.
        let dialog = data_init.init(
            id,
            XdgDialogData {
                toplevel: toplevel.clone(),
            },
        );
        let Some(toplevel) = toplevel else {
            return;
        };
        let already_used = surface_dialog(&toplevel, |current| {
            let used = current.as_ref().is_some_and(|(owner, existing)| {
                owner == toplevel.xdg_toplevel() && existing.is_alive()
            });
            if !used {
                *current = Some((toplevel.xdg_toplevel().clone(), dialog));
            }
            used
        });
        if already_used {
            manager.post_error(
                xdg_wm_dialog_v1::Error::AlreadyUsed,
                "the toplevel already has an xdg_dialog_v1",
            );
            return;
        }
        set_hint(&toplevel, ToplevelDialogHint::Dialog);
    }
}

impl<D> Dispatch<XdgDialogV1, XdgDialogData, D> for XdgDialogState
where
    D: Dispatch<XdgDialogV1, XdgDialogData>,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _dialog: &XdgDialogV1,
        request: xdg_dialog_v1::Request,
        data: &XdgDialogData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let Some(toplevel) = data.live_toplevel() else {
            return;
        };
        match request {
            xdg_dialog_v1::Request::SetModal => set_hint(toplevel, ToplevelDialogHint::Modal),
            xdg_dialog_v1::Request::UnsetModal => set_hint(toplevel, ToplevelDialogHint::Dialog),
            xdg_dialog_v1::Request::Destroy => set_hint(toplevel, ToplevelDialogHint::Unknown),
            _ => {}
        }
    }
}

/// Delegate the `xdg_wm_dialog_v1` global to [`XdgDialogState`].
#[macro_export]
macro_rules! delegate_xdg_dialog {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::xdg::dialog::v1::server::xdg_wm_dialog_v1::XdgWmDialogV1: ()
        ] => $crate::protocols::xdg_dialog::XdgDialogState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::xdg::dialog::v1::server::xdg_wm_dialog_v1::XdgWmDialogV1: ()
        ] => $crate::protocols::xdg_dialog::XdgDialogState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::xdg::dialog::v1::server::xdg_dialog_v1::XdgDialogV1: $crate::protocols::xdg_dialog::XdgDialogData
        ] => $crate::protocols::xdg_dialog::XdgDialogState);
    };
}
