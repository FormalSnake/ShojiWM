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
    ClientId, Handle, ObjectData, ObjectId,
    protocol::{Argument, Message},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, New, Resource, ResourceData,
};
use smithay::wayland::{Dispatch2, GlobalData, GlobalDispatch2};

use super::forward_request;
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

fn new_object_id(msg: &Message<ObjectId, OwnedFd>) -> Option<ObjectId> {
    msg.args.iter().find_map(|arg| match arg {
        Argument::NewId(id) => Some(id.clone()),
        _ => None,
    })
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
        let new_id = new_object_id(&msg);
        let inner = self.inner.clone();
        let input_method = forward_request(self, inner, handle, state, client_id, msg)?;
        // Smithay's seat keeps the first live input method; a later one only
        // tells it `unavailable`.
        if state.input_method_object.is_none() {
            state.input_method_object = new_id;
        }
        Some(Arc::new(InputMethodShim {
            inner: input_method,
        }))
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
        if msg.opcode == zwp_input_method_v2::REQ_GRAB_KEYBOARD_OPCODE {
            state.input_method_grab = new_object_id(&msg);
            return forward_request(self, inner, handle, state, client_id, msg)
                .map(|inner| Arc::new(GrabShim { inner }) as Arc<dyn ObjectData<ShojiWM>>);
        }
        forward_request(self, inner, handle, state, client_id, msg)
    }

    fn destroyed(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        object_id: ObjectId,
    ) {
        // Smithay's seat forgets its input method when any of them goes.
        state.input_method_object = None;
        self.inner
            .clone()
            .destroyed(handle, state, client_id, object_id);
    }
}

/// A `zwp_input_method_keyboard_grab_v2` smithay made, in front of its
/// object data. Every grab object of the seat shares smithay's one grab, and
/// smithay's destructor clears it and unsets the seat's keyboard grab
/// whichever object goes. Only the destruction of the grab the keys go to
/// reaches it, and only while that grab holds the keyboard: an older grab's
/// `release` would end the live one.
struct GrabShim {
    inner: Arc<dyn ObjectData<ShojiWM>>,
}

impl ObjectData<ShojiWM> for GrabShim {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ShojiWM>>> {
        let inner = self.inner.clone();
        forward_request(self, inner, handle, state, client_id, msg)
    }

    fn destroyed(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        object_id: ObjectId,
    ) {
        if state.input_method_grab.as_ref() != Some(&object_id) {
            return;
        }
        state.input_method_grab = None;
        // The lock may have taken the keyboard from this grab, and the seat
        // may hold another grab by now.
        if state.input_method_holds_keyboard() {
            self.inner
                .clone()
                .destroyed(handle, state, client_id, object_id);
        }
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
