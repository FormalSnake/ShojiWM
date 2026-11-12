//! `zwp_input_method_manager_v2`, created here instead of by smithay's
//! `InputMethodManagerState` so the compositor knows which clients run an
//! input method. Smithay keeps the input method's client to itself, and keys
//! that client sends back through its own virtual keyboard must not go into
//! its keyboard grab again (see [`crate::virtual_keyboard`]).
//!
//! Every request is still handled by smithay, except that under the session
//! lock an input method of any client but the locker gets no keyboard grab
//! and cannot write into the locker's text input. Smithay installs a grab and
//! sends it the keymap and the modifiers as the request arrives, and sends
//! the text straight on, so the requests are refused before smithay sees
//! them: each `zwp_input_method_v2` gets [`InputMethodShim`] for object data,
//! which hands smithay the requests it lets through.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use smithay::reexports::wayland_protocols_misc::zwp_input_method_v2::server::{
    zwp_input_method_manager_v2::{self, ZwpInputMethodManagerV2},
    zwp_input_method_v2,
};
use smithay::reexports::wayland_server::backend::{
    ClientId, Handle, ObjectData, ObjectId, protocol::Message,
};
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, New, Resource, ResourceData,
};
use smithay::wayland::{Dispatch2, GlobalData, GlobalDispatch2};

use crate::state::ShojiWM;

/// The version smithay implements.
const VERSION: u32 = 1;

pub struct InputMethodManagerGlobal;

pub struct InputMethodManagerData;

impl InputMethodManagerGlobal {
    pub fn create(dh: &DisplayHandle) {
        dh.create_global::<ShojiWM, ZwpInputMethodManagerV2, _>(VERSION, Self);
    }
}

impl GlobalDispatch2<ZwpInputMethodManagerV2, ShojiWM> for InputMethodManagerGlobal {
    fn bind(
        &self,
        _state: &mut ShojiWM,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpInputMethodManagerV2>,
        data_init: &mut DataInit<'_, ShojiWM>,
    ) {
        let manager = data_init.init(resource, InputMethodManagerData);
        let Some(inner) = manager.object_data().cloned().and_then(|data| {
            data.downcast::<ResourceData<ZwpInputMethodManagerV2, InputMethodManagerData>>()
                .ok()
        }) else {
            return;
        };
        data_init.custom_init(New::wrap(manager), Arc::new(ManagerShim { inner }));
    }
}

/// Smithay's dispatch of a request reads the object data back from the
/// backend, so `inner` stands in for `shim` while it runs.
fn forward(
    shim: Arc<dyn ObjectData<ShojiWM>>,
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
    let _ = handle.set_object_data(id, shim);
    new_object
}

/// The manager's object data: puts [`InputMethodShim`] in front of each
/// input method smithay makes.
struct ManagerShim {
    inner: Arc<ResourceData<ZwpInputMethodManagerV2, InputMethodManagerData>>,
}

impl ObjectData<ShojiWM> for ManagerShim {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ShojiWM>>> {
        let inner = self.inner.clone();
        forward(self, inner, handle, state, client_id, msg)
            .map(|inner| Arc::new(InputMethodShim { inner }) as Arc<dyn ObjectData<ShojiWM>>)
    }

    fn destroyed(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        object_id: ObjectId,
    ) {
        self.inner
            .clone()
            .destroyed(handle, state, client_id, object_id);
    }
}

/// A `zwp_input_method_v2`'s object data, in front of smithay's.
struct InputMethodShim {
    inner: Arc<dyn ObjectData<ShojiWM>>,
}

impl ObjectData<ShojiWM> for InputMethodShim {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ShojiWM>>> {
        if state.session_lock_active && !state.client_holds_session_lock(&client_id) {
            match msg.opcode {
                // The grab object exists for the client, and hears nothing:
                // fcitx5 and ibus grab again when they are next activated.
                zwp_input_method_v2::REQ_GRAB_KEYBOARD_OPCODE => {
                    tracing::info!("refused an input method keyboard grab under the session lock");
                    return Some(Arc::new(RefusedGrab));
                }
                zwp_input_method_v2::REQ_COMMIT_STRING_OPCODE
                | zwp_input_method_v2::REQ_SET_PREEDIT_STRING_OPCODE
                | zwp_input_method_v2::REQ_DELETE_SURROUNDING_TEXT_OPCODE
                | zwp_input_method_v2::REQ_COMMIT_OPCODE => return None,
                _ => {}
            }
        }
        let inner = self.inner.clone();
        forward(self, inner, handle, state, client_id, msg)
    }

    fn destroyed(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        object_id: ObjectId,
    ) {
        self.inner
            .clone()
            .destroyed(handle, state, client_id, object_id);
    }
}

/// A `zwp_input_method_keyboard_grab_v2` asked for under the lock. Its only
/// request is `release`.
struct RefusedGrab;

impl ObjectData<ShojiWM> for RefusedGrab {
    fn request(
        self: Arc<Self>,
        _: &Handle,
        _: &mut ShojiWM,
        _: ClientId,
        _: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ShojiWM>>> {
        None
    }

    fn destroyed(self: Arc<Self>, _: &Handle, _: &mut ShojiWM, _: ClientId, _: ObjectId) {}
}

impl Dispatch2<ZwpInputMethodManagerV2, ShojiWM> for InputMethodManagerData {
    fn request(
        &self,
        state: &mut ShojiWM,
        client: &Client,
        manager: &ZwpInputMethodManagerV2,
        request: zwp_input_method_manager_v2::Request,
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, ShojiWM>,
    ) {
        if matches!(
            request,
            zwp_input_method_manager_v2::Request::GetInputMethod { .. }
        ) {
            let backend = dh.backend_handle();
            state
                .input_method_clients
                .retain(|id| backend.get_client_data(id.clone()).is_ok());
            state.input_method_clients.insert(client.id());
        }
        GlobalData.request(state, client, manager, request, dh, data_init);
    }
}
