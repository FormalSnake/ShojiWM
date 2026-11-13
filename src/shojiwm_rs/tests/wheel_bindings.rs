//! A wheel binding runs once per scroll delay, Hyprland's
//! `binds:scroll_event_delay`, and every turn of the wheel spoils a modifier
//! tap, also one the delay keeps from the bindings.

mod common;

use std::{cell::RefCell, time::Duration};

use shojiwm_lib::virtual_input::{
    AxisAmount, AxisFrame, VirtualAxis, VirtualDevice, VirtualInput, VirtualKey,
};
use shojiwm_rs::prelude::*;
use smithay::backend::input::{AxisSource, InputEvent, KeyState, Keycode};

/// evdev code.
const SUPER: u32 = 125;

thread_local! {
    static FIRED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

fn fired(name: &'static str) -> impl Fn() + 'static {
    move || FIRED.with(|fired| fired.borrow_mut().push(name))
}

fn take_fired() -> Vec<&'static str> {
    FIRED.with(|fired| std::mem::take(&mut *fired.borrow_mut()))
}

fn setup() {
    COMPOSITOR
        .key
        .bind("next", "Super+WheelScrollDown", fired("next"));
    COMPOSITOR.key.bind_release("tap", "Super", fired("tap"));
}

fn device(id: &str, keyboard: bool) -> VirtualDevice {
    VirtualDevice {
        id: id.into(),
        name: id.into(),
        keyboard,
    }
}

fn key(session: &mut common::Session, time: u64, state: KeyState) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::Keyboard {
            event: VirtualKey {
                device: device("test-keyboard", true),
                time,
                keycode: Keycode::new(SUPER + 8),
                state,
                count: 1,
            },
        });
    session.roundtrip();
}

/// One wheel notch toward the user.
fn notch(session: &mut common::Session, time: u64) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerAxis {
            event: VirtualAxis {
                device: device("test-pointer", false),
                time,
                frame: AxisFrame {
                    time_msec: time as u32,
                    source: Some(AxisSource::Wheel),
                    horizontal: AxisAmount::default(),
                    vertical: AxisAmount {
                        value: Some(15.0),
                        v120: Some(120.0),
                    },
                },
            },
        });
    session.roundtrip();
}

fn rest() {
    std::thread::sleep(Duration::from_millis(320));
}

#[test]
fn a_wheel_turn_inside_the_delay_spoils_a_tap() {
    let mut session = common::Session::new(setup);
    session.map_toplevel();

    // A bare tap.
    key(&mut session, 1000, KeyState::Pressed);
    key(&mut session, 1050, KeyState::Released);
    assert_eq!(take_fired(), ["tap"]);

    // A plain turn starts the delay; Super and a turn inside it bind
    // nothing, and the release is no tap.
    rest();
    notch(&mut session, 2000);
    key(&mut session, 2050, KeyState::Pressed);
    notch(&mut session, 2100);
    key(&mut session, 2150, KeyState::Released);
    assert_eq!(take_fired(), Vec::<&str>::new());

    // Past the delay the turn binds, once.
    rest();
    key(&mut session, 3000, KeyState::Pressed);
    notch(&mut session, 3050);
    notch(&mut session, 3060);
    key(&mut session, 3100, KeyState::Released);
    assert_eq!(take_fired(), ["next"]);
}
