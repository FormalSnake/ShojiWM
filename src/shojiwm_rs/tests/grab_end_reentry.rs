//! A config handler for the end of a window move runs while the compositor
//! may still hold the pointer. Taking an input grab from it used to deadlock
//! on the pointer's lock.

mod common;

use std::{sync::mpsc, time::Duration};

use shojiwm_lib::grabs::move_grab::MoveSurfaceGrab;
use shojiwm_rs::{
    prelude::*,
    ssd::{WindowMovePhaseSnapshot, WindowMoveSourceSnapshot},
};
use smithay::{
    backend::input::ButtonState,
    input::pointer::{ButtonEvent, Focus, GrabStartData},
    utils::{Rectangle, SERIAL_COUNTER},
};

const BTN_LEFT: u32 = 0x110;

fn setup() {
    COMPOSITOR.event.on_window_move(|_, event| {
        if event.phase == WindowMovePhaseSnapshot::End {
            COMPOSITOR.input.grab(InputGrabOptions::new());
        }
    });
}

#[test]
fn input_grab_from_window_move_end_does_not_deadlock() {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let mut session = common::Session::new(setup);
        session.map_toplevel();
        let state = &mut session.state;

        let window = state
            .space
            .elements()
            .next()
            .cloned()
            .expect("toplevel mapped");
        let location = state.space.element_location(&window).unwrap();
        let pointer = state.seat.get_pointer().unwrap();
        let mut grab = MoveSurfaceGrab::start(
            GrabStartData {
                focus: None,
                button: BTN_LEFT,
                location: location.to_f64(),
            },
            window,
            location,
            Rectangle::new(location, (100, 100).into()),
            WindowMoveSourceSnapshot::Modifier,
        );
        grab.notify_start(state);
        pointer.set_grab(state, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
        pointer.button(
            state,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
                button: BTN_LEFT,
                state: ButtonState::Released,
            },
        );
        session
            .event_loop
            .dispatch(Some(Duration::ZERO), &mut session.state)
            .unwrap();
        done.send(session.state.runtime_input_grab.is_some())
            .unwrap();
    });

    let grabbed = finished
        .recv_timeout(Duration::from_secs(10))
        .expect("the compositor deadlocked releasing the move grab");
    assert!(grabbed, "the End handler's input grab was not applied");
}
