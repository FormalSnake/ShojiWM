//! A grab that leaves the layers the pointer: over a top layer surface the
//! client has it, over a window the grab does, and a press the grab took
//! keeps the pointer the grab's until it is let go.

mod common;

use std::cell::{Cell, RefCell};

use shojiwm_lib::virtual_input::{
    VirtualButton, VirtualDevice, VirtualInput, VirtualMotionAbsolute,
};
use shojiwm_rs::prelude::*;
use smithay::backend::input::{ButtonState, InputEvent};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::ObjectId,
    protocol::{wl_pointer, wl_seat},
};

const BTN_LEFT: u32 = 0x110;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Enter(ObjectId),
    Leave(ObjectId),
    Button,
}

thread_local! {
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    static GRAB_MOTIONS: Cell<usize> = const { Cell::new(0) };
    static GRAB_BUTTONS: Cell<usize> = const { Cell::new(0) };
}

fn take() -> Vec<Seen> {
    SEEN.with(|seen| std::mem::take(&mut *seen.borrow_mut()))
}

fn grabbed() -> (usize, usize) {
    (GRAB_MOTIONS.get(), GRAB_BUTTONS.get())
}

fn setup() {
    COMPOSITOR.window.composition(|_| {
        ManagedWindow::new()
            .rect(Rect::new(0.0, 0.0, 300.0, 300.0))
            .child(ClientWindow::new())
    });
    COMPOSITOR.input.grab(
        InputGrabOptions::new()
            .leave_layers_the_pointer()
            .on_pointer_motion(|_| GRAB_MOTIONS.set(GRAB_MOTIONS.get() + 1))
            .on_pointer_button(|_| GRAB_BUTTONS.set(GRAB_BUTTONS.get() + 1)),
    );
    set_interval(1.0, || {});
}

fn device() -> VirtualDevice {
    VirtualDevice {
        id: "test-pointer".into(),
        name: "test pointer".into(),
        keyboard: false,
    }
}

fn place(session: &mut common::Session, x: f64, y: f64) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerMotionAbsolute {
            event: VirtualMotionAbsolute {
                device: device(),
                time: 0,
                x: x / 1024.0,
                y: y / 768.0,
            },
        });
    session.roundtrip();
}

fn button(session: &mut common::Session, state: ButtonState) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerButton {
            event: VirtualButton {
                device: device(),
                time: 0,
                button: BTN_LEFT,
                state,
            },
        });
    session.roundtrip();
}

#[test]
fn a_grab_leaves_a_top_layer_surface_the_pointer_and_keeps_the_windows() {
    let mut session = common::Session::new(setup);
    session.map_toplevel_with_buffer("a", 300, 300);
    session.state.refresh_window_decorations().unwrap();
    let bar = session.map_layer("bar");
    session.attach_buffer(&bar, 1024, 40);
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    seat.get_pointer(&session.queue.handle(), ());
    session.roundtrip();
    assert!(
        session.state.runtime_input_grab.is_some(),
        "the grab did not start"
    );

    place(&mut session, 150.0, 150.0);
    // Where the pointer started, the bar may have had it.
    let seen = take();
    assert!(
        !seen.iter().any(|seen| matches!(seen, Seen::Enter(_))),
        "the window under the grab got the pointer: {seen:?}"
    );
    let (motions, _) = grabbed();
    assert!(motions > 0, "the grab did not hear a move over the window");

    place(&mut session, 500.0, 20.0);
    assert_eq!(
        take(),
        [Seen::Enter(bar.id())],
        "the bar did not get the pointer"
    );
    assert_eq!(grabbed().0, motions, "the grab heard a move over the bar");

    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    assert_eq!(
        take(),
        [Seen::Button, Seen::Button],
        "the bar did not get its click"
    );
    assert_eq!(grabbed().1, 0, "the grab heard a click on the bar");

    place(&mut session, 150.0, 150.0);
    assert_eq!(
        take(),
        [Seen::Leave(bar.id())],
        "the bar kept the pointer off it"
    );
    assert!(
        grabbed().0 > motions,
        "the grab did not get the pointer back"
    );

    // A press the grab took keeps the pointer the grab's over the bar.
    button(&mut session, ButtonState::Pressed);
    place(&mut session, 500.0, 20.0);
    button(&mut session, ButtonState::Released);
    assert_eq!(take(), [], "the bar got a drag the grab started");
    assert_eq!(
        grabbed().1,
        2,
        "the grab did not hear its press and release"
    );

    place(&mut session, 510.0, 20.0);
    assert_eq!(
        take(),
        [Seen::Enter(bar.id())],
        "the bar did not get the pointer once the grab's button was up"
    );
}

impl Dispatch<wl_pointer::WlPointer, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let seen = match event {
            wl_pointer::Event::Enter { surface, .. } => Seen::Enter(surface.id()),
            wl_pointer::Event::Leave { surface, .. } => Seen::Leave(surface.id()),
            wl_pointer::Event::Button { .. } => Seen::Button,
            _ => return,
        };
        SEEN.with(|events| events.borrow_mut().push(seen));
    }
}
