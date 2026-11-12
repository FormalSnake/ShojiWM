//! `zwp_text_input_manager_v3`, created here instead of by smithay's
//! `TextInputManagerState` so the requests of each text input pass a filter
//! before smithay handles them.
//!
//! Smithay passes a text input's state to the seat's input method on
//! `commit`: `activate`, the content type, the surrounding text (all of a
//! password field) and the cursor rectangle. Under the session lock an input
//! method that is not the locker's must hear none of it, so the requests that
//! set that state are dropped. `commit` still goes through, with nothing
//! pending: smithay counts commits for the serial of its `done` events.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3, zwp_text_input_v3,
};
use smithay::reexports::wayland_server::backend::{
    ClientId, Handle, ObjectData, ObjectId, protocol::Message,
};
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, New, Resource, ResourceData,
};
use smithay::wayland::{GlobalData, GlobalDispatch2};

use super::forward_request;
use crate::state::ShojiWM;

/// The version smithay implements.
const VERSION: u32 = 1;

pub struct TextInputManagerGlobal;

impl TextInputManagerGlobal {
    pub fn create(dh: &DisplayHandle) {
        dh.create_global::<ShojiWM, ZwpTextInputManagerV3, _>(VERSION, Self);
    }
}

impl GlobalDispatch2<ZwpTextInputManagerV3, ShojiWM> for TextInputManagerGlobal {
    fn bind(
        &self,
        _state: &mut ShojiWM,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpTextInputManagerV3>,
        data_init: &mut DataInit<'_, ShojiWM>,
    ) {
        let manager = data_init.init(resource, GlobalData);
        let Some(inner) = manager.object_data().cloned().and_then(|data| {
            data.downcast::<ResourceData<ZwpTextInputManagerV3, GlobalData>>()
                .ok()
        }) else {
            return;
        };
        data_init.custom_init(New::wrap(manager), Arc::new(ManagerFilter { inner }));
    }
}

/// The manager's object data: puts [`TextInputFilter`] in front of each text
/// input smithay makes.
struct ManagerFilter {
    inner: Arc<ResourceData<ZwpTextInputManagerV3, GlobalData>>,
}

impl ObjectData<ShojiWM> for ManagerFilter {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ShojiWM>>> {
        let inner = self.inner.clone();
        forward_request(self, inner, handle, state, client_id, msg)
            .map(|inner| Arc::new(TextInputFilter { inner }) as Arc<dyn ObjectData<ShojiWM>>)
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

/// A `zwp_text_input_v3`'s object data, in front of smithay's.
struct TextInputFilter {
    inner: Arc<dyn ObjectData<ShojiWM>>,
}

impl ObjectData<ShojiWM> for TextInputFilter {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut ShojiWM,
        client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ShojiWM>>> {
        if state.session_lock_active
            && !state.input_method_is_lockers()
            && matches!(
                msg.opcode,
                zwp_text_input_v3::REQ_ENABLE_OPCODE
                    | zwp_text_input_v3::REQ_DISABLE_OPCODE
                    | zwp_text_input_v3::REQ_SET_SURROUNDING_TEXT_OPCODE
                    | zwp_text_input_v3::REQ_SET_TEXT_CHANGE_CAUSE_OPCODE
                    | zwp_text_input_v3::REQ_SET_CONTENT_TYPE_OPCODE
                    | zwp_text_input_v3::REQ_SET_CURSOR_RECTANGLE_OPCODE
            )
        {
            return None;
        }
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
        self.inner
            .clone()
            .destroyed(handle, state, client_id, object_id);
    }
}
