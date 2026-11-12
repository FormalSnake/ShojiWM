//! A key's release goes where its press went. A key a binding or an input
//! grab took never reaches a client, not even its release after the focus
//! or the grab changed, and a key a client got down always comes back up in
//! it.

mod common;

use std::cell::RefCell;

use shojiwm_lib::virtual_input::{VirtualDevice, VirtualInput, VirtualKey};
use shojiwm_rs::prelude::*;
use smithay::backend::input::{InputEvent, KeyState, Keycode};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{wl_keyboard, wl_seat},
};

/// evdev codes.
const ESCAPE: u32 = 1;
const A: u32 = 30;
const G: u32 = 34;
const H: u32 = 35;
const K: u32 = 37;
const L: u32 = 38;
const X: u32 = 45;
const SUPER: u32 = 125;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Down(u32),
    Up(u32),
    /// The keyboard entered a surface holding these keys down.
    Enter(usize),
}

thread_local! {
    static FIRED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    static GRAB: RefCell<Option<InputGrab>> = const { RefCell::new(None) };
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
    COMPOSITOR.key.bind("focus", "Super+H", || {
        fired("focus")();
        COMPOSITOR.window.list()[1].focus();
    });
    COMPOSITOR.key.bind("taken", "Super+L", fired("taken"));
    COMPOSITOR.key.bind_with(
        "on-release",
        "Super+X",
        KeyBindOptions::new().on_release(),
        fired("on-release"),
    );
    COMPOSITOR.key.bind("grab", "Super+G", || {
        let grab = COMPOSITOR
            .input
            .grab(InputGrabOptions::new().on_key(|event| {
                if event.key == "Escape"
                    && event.state == InputGrabState::Pressed
                    && let Some(grab) = GRAB.with(|grab| grab.borrow_mut().take())
                {
                    grab.release();
                }
            }));
        GRAB.with(|current| *current.borrow_mut() = Some(grab));
    });
    // What the test asks of a window from outside a callback is collected
    // on the scheduler's next turn, and a timer keeps it turning.
    set_interval(1.0, || {});
}

fn send(session: &mut common::Session, key: u32, state: KeyState) {
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

fn press(session: &mut common::Session, key: u32) {
    send(session, key, KeyState::Pressed);
}

fn release(session: &mut common::Session, key: u32) {
    send(session, key, KeyState::Released);
}

fn focused_session() -> common::Session {
    let mut session = common::Session::new(setup);
    session.map_toplevel_with_buffer("a", 300, 300);
    session.map_toplevel_with_buffer("b", 300, 300);
    session.state.refresh_window_decorations().unwrap();
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    seat.get_keyboard(&session.queue.handle(), ());
    COMPOSITOR.window.list()[0].focus();
    session.roundtrip();
    take(&SEEN);
    session
}

#[test]
fn a_bound_key_reaches_no_client_down_or_up() {
    let mut session = focused_session();

    press(&mut session, SUPER);
    press(&mut session, L);
    release(&mut session, L);
    release(&mut session, SUPER);
    assert_eq!(take(&FIRED), vec!["taken"]);
    assert_eq!(take(&SEEN), vec![Seen::Down(SUPER), Seen::Up(SUPER)]);

    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(take(&SEEN), vec![Seen::Down(A), Seen::Up(A)]);
}

#[test]
fn a_bound_key_that_moves_the_focus_is_not_released_in_the_new_window() {
    let mut session = focused_session();

    press(&mut session, SUPER);
    press(&mut session, H);
    release(&mut session, H);
    release(&mut session, SUPER);
    assert_eq!(take(&FIRED), vec!["focus"]);
    // The new window enters with Super held, then sees it go up.
    assert_eq!(
        take(&SEEN),
        vec![Seen::Down(SUPER), Seen::Enter(1), Seen::Up(SUPER)]
    );
}

#[test]
fn a_release_binding_leaves_the_key_it_saw_go_down_to_come_up() {
    let mut session = focused_session();

    press(&mut session, SUPER);
    press(&mut session, X);
    release(&mut session, X);
    release(&mut session, SUPER);
    assert_eq!(take(&FIRED), vec!["on-release"]);
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Down(SUPER),
            Seen::Down(X),
            Seen::Up(X),
            Seen::Up(SUPER)
        ]
    );
}

#[test]
fn keys_an_input_grab_took_are_not_released_in_the_client() {
    let mut session = focused_session();

    // The key that opens the grab, let go under it.
    press(&mut session, SUPER);
    press(&mut session, G);
    assert!(session.state.runtime_input_grab.is_some());
    release(&mut session, G);
    release(&mut session, SUPER);
    // A key pressed under the grab and let go after it ended.
    press(&mut session, K);
    press(&mut session, ESCAPE);
    release(&mut session, ESCAPE);
    assert!(session.state.runtime_input_grab.is_none());
    release(&mut session, K);

    assert_eq!(take(&FIRED), Vec::<&str>::new());
    assert_eq!(take(&SEEN), vec![Seen::Down(SUPER), Seen::Up(SUPER)]);
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
        let seen = match event {
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(state),
                ..
            } => match state {
                wl_keyboard::KeyState::Pressed => Seen::Down(key),
                _ => Seen::Up(key),
            },
            wl_keyboard::Event::Enter { keys, .. } => Seen::Enter(keys.len() / 4),
            _ => return,
        };
        SEEN.with(|seen_list| seen_list.borrow_mut().push(seen));
    }
}
