//! A client's pointer lock holds only while its window has the keyboard: a
//! window that takes the focus unlocks it, and a persistent lock comes back
//! with the focus.

mod common;

use std::cell::RefCell;

use shojiwm_lib::virtual_input::{VirtualDevice, VirtualInput, VirtualMotionAbsolute};
use shojiwm_rs::prelude::*;
use smithay::backend::input::InputEvent;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_pointer, wl_seat},
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};

thread_local! {
    static EVENTS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

fn setup() {
    COMPOSITOR.window.composition(|_| ClientWindow::new());
    // What the test asks of a window from outside a callback is collected
    // on the scheduler's next turn, and a timer keeps it turning.
    set_interval(1.0, || {});
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let name = match event {
            zwp_locked_pointer_v1::Event::Locked => "locked",
            zwp_locked_pointer_v1::Event::Unlocked => "unlocked",
            _ => return,
        };
        EVENTS.with_borrow_mut(|events| events.push(name));
    }
}

impl Dispatch<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        _: zwp_pointer_constraints_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &wl_pointer::WlPointer,
        _: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn events() -> Vec<&'static str> {
    EVENTS.with_borrow(Clone::clone)
}

#[test]
fn a_lock_follows_its_window_keyboard_focus() {
    let mut session = common::Session::new(setup);
    let game = session.map_toplevel_with_buffer("game", 300, 200);
    session.state.refresh_window_decorations().unwrap();
    let game_window = COMPOSITOR.window.list()[0];
    game_window.focus();
    session.roundtrip();
    session.state.refresh_window_decorations().unwrap();

    let window = session.state.space.elements().next().cloned().unwrap();
    let geometry = session.state.space.element_geometry(&window).unwrap();
    let centre = geometry.loc.to_f64() + geometry.size.to_f64().downscale(2.0).to_point();
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerMotionAbsolute {
            event: VirtualMotionAbsolute {
                device: VirtualDevice {
                    id: "test-pointer".into(),
                    name: "test pointer".into(),
                    keyboard: false,
                },
                time: 0,
                x: centre.x / 1024.0,
                y: centre.y / 768.0,
            },
        });

    let qh = session.queue.handle();
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    let pointer = seat.get_pointer(&qh, ());
    let constraints: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1 =
        session.bind("zwp_pointer_constraints_v1", 1);
    constraints.lock_pointer(
        &game,
        &pointer,
        None,
        zwp_pointer_constraints_v1::Lifetime::Persistent,
        &qh,
        (),
    );
    session.roundtrip();
    assert_eq!(events(), ["locked"]);

    session.map_toplevel_with_buffer("other", 200, 200);
    session.state.refresh_window_decorations().unwrap();
    let other = COMPOSITOR
        .window
        .list()
        .into_iter()
        .find(|window| *window != game_window)
        .unwrap();
    other.focus();
    session.roundtrip();
    assert_eq!(
        events(),
        ["locked", "unlocked"],
        "the lock outlived its window losing the keyboard",
    );

    // Focus no window before the older one: the core refuses an older window
    // the focus a newer one holds unless the user used one of them.
    other.unfocus();
    game_window.focus();
    session.roundtrip();
    assert_eq!(events(), ["locked", "unlocked", "locked"]);
}
