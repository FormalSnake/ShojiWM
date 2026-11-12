//! A `non_consuming` binding runs and leaves its key or its mouse button to
//! the client; a binding without it takes the event.

mod common;

use std::cell::RefCell;

use shojiwm_lib::virtual_input::{VirtualButton, VirtualDevice, VirtualInput, VirtualKey};
use shojiwm_rs::prelude::*;
use smithay::{
    backend::input::{ButtonState, InputEvent, KeyState, Keycode},
    input::pointer::MotionEvent,
    utils::SERIAL_COUNTER,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{wl_keyboard, wl_pointer, wl_seat},
};

/// evdev codes.
const ESCAPE: u32 = 1;
const TAB: u32 = 15;
const A: u32 = 30;
const BTN_LEFT: u32 = 0x110;
const BTN_SIDE: u32 = 0x113;

thread_local! {
    static FIRED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    static KEYS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static BUTTONS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

fn fired(name: &'static str) -> impl Fn() + 'static {
    move || FIRED.with(|fired| fired.borrow_mut().push(name))
}

fn take<T>(seen: &'static std::thread::LocalKey<RefCell<Vec<T>>>) -> Vec<T> {
    seen.with(|seen| std::mem::take(&mut *seen.borrow_mut()))
}

fn setup() {
    COMPOSITOR.window.composition(|_| {
        ManagedWindow::new()
            .rect(Rect::new(0.0, 0.0, 300.0, 300.0))
            .child(ClientWindow::new())
    });
    let leaves = || KeyBindOptions::new().non_consuming();
    COMPOSITOR
        .key
        .bind_with("escape", "Escape", leaves(), fired("escape"));
    COMPOSITOR.key.bind("tab", "Tab", fired("tab"));
    COMPOSITOR
        .key
        .bind_with("click", "MouseLeft", leaves(), fired("click"));
    COMPOSITOR.key.bind("side", "MouseBack", fired("side"));
    // What the test asks of a window from outside a callback is collected
    // on the scheduler's next turn, and a timer keeps it turning.
    set_interval(1.0, || {});
}

fn key(session: &mut common::Session, key: u32) {
    for state in [KeyState::Pressed, KeyState::Released] {
        let event = VirtualKey {
            device: VirtualDevice {
                id: "test-keyboard".into(),
                name: "test keyboard".into(),
                keyboard: true,
            },
            time: 0,
            keycode: Keycode::new(key + 8),
            state,
            count: 1,
        };
        session
            .state
            .process_input_event(InputEvent::<VirtualInput>::Keyboard { event });
        session.roundtrip();
    }
}

fn click(session: &mut common::Session, button: u32) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        let event = VirtualButton {
            device: VirtualDevice {
                id: "test-pointer".into(),
                name: "test pointer".into(),
                keyboard: false,
            },
            time: 0,
            button,
            state,
        };
        session
            .state
            .process_input_event(InputEvent::<VirtualInput>::PointerButton { event });
        session.roundtrip();
    }
}

#[test]
fn a_non_consuming_binding_runs_and_the_client_still_gets_the_event() {
    let mut session = common::Session::new(setup);
    session.map_toplevel_with_buffer("a", 300, 300);
    session.state.refresh_window_decorations().unwrap();
    let qh = session.queue.handle();
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    seat.get_keyboard(&qh, ());
    seat.get_pointer(&qh, ());
    COMPOSITOR.window.list()[0].focus();
    session.roundtrip();

    // A key nothing is bound to is the client's.
    key(&mut session, A);
    assert_eq!((take(&FIRED), take(&KEYS)), (vec![], vec![A]));

    key(&mut session, ESCAPE);
    assert_eq!((take(&FIRED), take(&KEYS)), (vec!["escape"], vec![ESCAPE]));

    key(&mut session, TAB);
    assert_eq!((take(&FIRED), take(&KEYS)), (vec!["tab"], vec![]));

    let state = &mut session.state;
    let pointer = state.seat.get_pointer().unwrap();
    let location = (150.0, 150.0).into();
    state.pointer_contents = state.pointer_contents_at(location);
    let under = state.pointer_contents.surface.clone();
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );
    pointer.frame(state);
    session.roundtrip();

    click(&mut session, BTN_LEFT);
    assert_eq!(
        (take(&FIRED), take(&BUTTONS)),
        (vec!["click"], vec![BTN_LEFT])
    );

    click(&mut session, BTN_SIDE);
    assert_eq!((take(&FIRED), take(&BUTTONS)), (vec!["side"], vec![]));
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            KEYS.with(|keys| keys.borrow_mut().push(key));
        }
    }
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
        if let wl_pointer::Event::Button {
            button,
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            BUTTONS.with(|buttons| buttons.borrow_mut().push(button));
        }
    }
}
