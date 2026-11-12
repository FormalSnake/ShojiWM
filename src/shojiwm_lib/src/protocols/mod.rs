use std::os::fd::OwnedFd;
use std::sync::Arc;

use smithay::reexports::wayland_server::backend::{
    ClientId, Handle, ObjectData, ObjectId, protocol::Message,
};

use crate::state::ShojiWM;

pub mod color_management;
pub mod gamma_control;
pub mod hyprland_toplevel_export;
pub mod input_method;
pub mod output_power;
pub mod plasma_shell;
pub mod screencopy;
pub mod tearing_control;
pub mod text_input;
pub mod virtual_keyboard;
pub mod virtual_pointer;

/// Hands a request an object data filter let through to smithay's object
/// data, `inner`. Smithay reads its user data back from the backend while it
/// dispatches, so `inner` stands in for `filter` until it returns.
pub(crate) fn forward_request(
    filter: Arc<dyn ObjectData<ShojiWM>>,
    inner: Arc<dyn ObjectData<ShojiWM>>,
    handle: &Handle,
    state: &mut ShojiWM,
    client_id: ClientId,
    msg: Message<ObjectId, OwnedFd>,
) -> Option<Arc<dyn ObjectData<ShojiWM>>> {
    let id = msg.sender_id.clone();
    let _ = handle.set_object_data(id.clone(), inner.clone());
    let new_object = inner.request(handle, state, client_id, msg);
    // A destructor has already removed the object.
    let _ = handle.set_object_data(id, filter);
    new_object
}
