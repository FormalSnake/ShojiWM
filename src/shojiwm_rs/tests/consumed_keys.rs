//! A key's release goes where its press went. A key a binding or an input
//! grab took never reaches a client, not even its release after the focus
//! or the grab changed, and a key a client got down always comes back up in
//! it, also when its keyboard goes away or the session leaves its VT. The
//! client's modifiers follow the seat's whether or not the key that changed
//! them reached it.

mod common;

use std::cell::{Cell, RefCell};

use shojiwm_lib::virtual_input::{VirtualButton, VirtualDevice, VirtualInput, VirtualKey};
use shojiwm_rs::prelude::*;
use smithay::{
    backend::input::{ButtonState, InputEvent, KeyState, Keycode},
    input::pointer::MotionEvent,
    utils::SERIAL_COUNTER,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{wl_compositor, wl_keyboard, wl_pointer, wl_seat},
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1, ext_session_lock_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_wm_base};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2, zwp_input_method_manager_v2, zwp_input_method_v2,
};

/// evdev codes.
const ESCAPE: u32 = 1;
const A: u32 = 30;
const G: u32 = 34;
const H: u32 = 35;
const K: u32 = 37;
const L: u32 = 38;
const X: u32 = 45;
const SHIFT: u32 = 42;
const SUPER: u32 = 125;
const BTN_LEFT: u32 = 0x110;

/// Depressed modifier masks of the default keymap.
const SHIFT_MASK: u32 = 1;
const SUPER_MASK: u32 = 64;

const KEYBOARD: &str = "test-keyboard";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Down(u32),
    Up(u32),
    /// The keyboard entered a surface holding these keys down.
    Enter(usize),
    /// The depressed modifiers the client was told.
    Mods(u32),
    Leave,
}

thread_local! {
    static FIRED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    /// Keys the input method's keyboard grab was sent.
    static IM_SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    static GRAB: RefCell<Option<InputGrab>> = const { RefCell::new(None) };
    static BUTTON_SERIAL: Cell<u32> = const { Cell::new(0) };
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

fn device(id: &str) -> VirtualDevice {
    VirtualDevice {
        id: id.into(),
        name: "test keyboard".into(),
        keyboard: true,
    }
}

fn send(session: &mut common::Session, key: u32, state: KeyState) {
    send_from(session, KEYBOARD, key, state);
}

fn send_from(session: &mut common::Session, keyboard: &str, key: u32, state: KeyState) {
    let event = VirtualKey {
        device: device(keyboard),
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

fn unplug(session: &mut common::Session, keyboard: &str) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::DeviceRemoved {
            device: device(keyboard),
        });
    session.roundtrip();
}

fn lock(session: &mut common::Session) -> ext_session_lock_v1::ExtSessionLockV1 {
    let manager: ext_session_lock_manager_v1::ExtSessionLockManagerV1 =
        session.bind("ext_session_lock_manager_v1", 1);
    let lock = manager.lock(&session.queue.handle(), ());
    session.roundtrip();
    assert!(session.state.session_lock_active);
    lock
}

fn unlock(session: &mut common::Session, lock: ext_session_lock_v1::ExtSessionLockV1) {
    lock.unlock_and_destroy();
    session.roundtrip();
    assert!(!session.state.session_lock_active);
}

/// An input method that grabs the keyboard.
fn grab_keyboard_for_input_method(session: &mut common::Session) {
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        session.bind("zwp_input_method_manager_v2", 1);
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    let input_method = manager.get_input_method(&seat, &session.queue.handle(), ());
    input_method.grab_keyboard(&session.queue.handle(), ());
    session.roundtrip();
    take(&IM_SEEN);
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
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Mods(0),
            Seen::Up(SUPER)
        ]
    );

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
        vec![
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Leave,
            Seen::Enter(1),
            Seen::Mods(SUPER_MASK),
            Seen::Mods(0),
            Seen::Up(SUPER)
        ]
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
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Down(X),
            Seen::Up(X),
            Seen::Mods(0),
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
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Mods(0),
            Seen::Up(SUPER)
        ]
    );
}

#[test]
fn a_modifier_an_input_grab_took_is_let_go_in_the_client() {
    let mut session = focused_session();

    press(&mut session, SUPER);
    press(&mut session, G);
    release(&mut session, G);
    // Shift goes down under the grab, and Super comes up while it is held.
    press(&mut session, SHIFT);
    release(&mut session, SUPER);
    press(&mut session, ESCAPE);
    release(&mut session, ESCAPE);
    assert!(session.state.runtime_input_grab.is_none());
    release(&mut session, SHIFT);
    press(&mut session, A);
    release(&mut session, A);

    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Mods(SUPER_MASK | SHIFT_MASK),
            Seen::Mods(SHIFT_MASK),
            Seen::Up(SUPER),
            Seen::Mods(0),
            Seen::Down(A),
            Seen::Up(A)
        ]
    );
}

#[test]
fn the_client_has_the_seats_modifiers_after_the_lock() {
    let mut session = focused_session();

    press(&mut session, SUPER);
    press(&mut session, L);
    press(&mut session, SHIFT);
    let locked = lock(&mut session);
    release(&mut session, L);
    release(&mut session, SUPER);
    unlock(&mut session, locked);
    assert_eq!(take(&FIRED), vec!["taken"]);
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Mods(SUPER_MASK | SHIFT_MASK),
            Seen::Down(SHIFT),
            Seen::Leave,
            Seen::Enter(1),
            Seen::Mods(SHIFT_MASK)
        ]
    );

    release(&mut session, SHIFT);
    assert_eq!(take(&SEEN), vec![Seen::Mods(0), Seen::Up(SHIFT)]);
}

#[test]
fn an_input_method_hears_no_key_while_the_session_is_locked() {
    let mut session = focused_session();
    grab_keyboard_for_input_method(&mut session);

    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(take(&IM_SEEN), vec![Seen::Down(A), Seen::Up(A)]);

    let locked = lock(&mut session);
    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&IM_SEEN), vec![]);

    unlock(&mut session, locked);
    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(take(&IM_SEEN), vec![Seen::Down(A), Seen::Up(A)]);
}

#[test]
fn an_input_method_that_grabs_the_keyboard_under_the_lock_hears_no_key() {
    let mut session = focused_session();

    let locked = lock(&mut session);
    grab_keyboard_for_input_method(&mut session);
    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&IM_SEEN), vec![]);

    unlock(&mut session, locked);
    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(take(&IM_SEEN), vec![Seen::Down(A), Seen::Up(A)]);
}

fn button(session: &mut common::Session, state: ButtonState) {
    let event = VirtualButton {
        device: VirtualDevice {
            id: "test-pointer".into(),
            name: "test pointer".into(),
            keyboard: false,
        },
        time: 0,
        button: BTN_LEFT,
        state,
    };
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerButton { event });
    session.roundtrip();
}

#[test]
fn a_menu_that_grabbed_the_keyboard_hears_no_key_while_the_session_is_locked() {
    let mut session = common::Session::new(setup);
    let qh = session.queue.handle();
    let compositor: wl_compositor::WlCompositor = session.bind("wl_compositor", 1);
    let shell: xdg_wm_base::XdgWmBase = session.bind("xdg_wm_base", 1);
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    let window = compositor.create_surface(&qh, ());
    let window_xdg = shell.get_xdg_surface(&window, &qh, ());
    window_xdg.get_toplevel(&qh, ());
    window.commit();
    session.roundtrip();
    session.attach_buffer(&window, 300, 300);
    session.state.refresh_window_decorations().unwrap();
    seat.get_keyboard(&qh, ());
    seat.get_pointer(&qh, ());
    COMPOSITOR.window.list()[0].focus();
    session.roundtrip();

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

    // A click opens the menu, which grabs with the click's serial.
    button(&mut session, ButtonState::Pressed);
    let positioner = shell.create_positioner(&qh, ());
    positioner.set_size(50, 50);
    positioner.set_anchor_rect(0, 0, 10, 10);
    let menu = compositor.create_surface(&qh, ());
    let menu_xdg = shell.get_xdg_surface(&menu, &qh, ());
    let popup = menu_xdg.get_popup(Some(&window_xdg), &positioner, &qh, ());
    popup.grab(&seat, BUTTON_SERIAL.get());
    menu.commit();
    session.roundtrip();
    session.attach_buffer(&menu, 50, 50);
    button(&mut session, ButtonState::Released);
    assert!(session.state.seat.get_keyboard().unwrap().is_grabbed());
    take(&SEEN);

    let locked = lock(&mut session);
    press(&mut session, K);
    release(&mut session, K);
    let seen = take(&SEEN);
    assert!(
        !seen.contains(&Seen::Down(K)) && !seen.contains(&Seen::Up(K)),
        "{seen:?}"
    );

    unlock(&mut session, locked);
    press(&mut session, A);
    release(&mut session, A);
    let seen = take(&SEEN);
    assert!(seen.ends_with(&[Seen::Down(A), Seen::Up(A)]), "{seen:?}");
}

#[test]
fn a_keyboard_unplugged_with_keys_down_lets_go_of_the_ones_its_client_has() {
    let mut session = focused_session();

    send_from(&mut session, "unplugged", A, KeyState::Pressed);
    send_from(&mut session, "unplugged", SUPER, KeyState::Pressed);
    send_from(&mut session, "unplugged", L, KeyState::Pressed);
    unplug(&mut session, "unplugged");
    assert_eq!(take(&FIRED), vec!["taken"]);
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Down(A),
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Up(A),
            Seen::Mods(0),
            Seen::Up(SUPER)
        ]
    );

    // The same keys on the next keyboard are new presses.
    press(&mut session, A);
    press(&mut session, SUPER);
    press(&mut session, L);
    assert_eq!(take(&FIRED), vec!["taken"]);
    assert_eq!(
        take(&SEEN),
        vec![Seen::Down(A), Seen::Mods(SUPER_MASK), Seen::Down(SUPER)]
    );
}

#[test]
fn a_key_two_keyboards_hold_comes_up_when_both_let_go() {
    let mut session = focused_session();

    send_from(&mut session, "first", A, KeyState::Pressed);
    send_from(&mut session, "second", A, KeyState::Pressed);
    send_from(&mut session, "second", A, KeyState::Released);
    assert_eq!(take(&SEEN), vec![Seen::Down(A)]);
    send_from(&mut session, "first", A, KeyState::Released);
    assert_eq!(take(&SEEN), vec![Seen::Up(A)]);
}

#[test]
fn keys_held_across_a_vt_switch_are_let_go_and_pressed_anew() {
    let mut session = focused_session();

    press(&mut session, SUPER);
    press(&mut session, L);
    // On another VT the session hears none of the releases.
    session.state.pause_input_session();
    session.roundtrip();
    session.state.resume_input_session();
    session.roundtrip();
    assert_eq!(take(&FIRED), vec!["taken"]);
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Mods(0),
            Seen::Up(SUPER)
        ]
    );

    press(&mut session, SUPER);
    press(&mut session, L);
    release(&mut session, L);
    release(&mut session, SUPER);
    assert_eq!(take(&FIRED), vec!["taken"]);
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Mods(SUPER_MASK),
            Seen::Down(SUPER),
            Seen::Mods(0),
            Seen::Up(SUPER)
        ]
    );
}

fn seen_key(key: u32, state: WEnum<wl_keyboard::KeyState>) -> Seen {
    match state {
        WEnum::Value(wl_keyboard::KeyState::Pressed) => Seen::Down(key),
        _ => Seen::Up(key),
    }
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
            wl_keyboard::Event::Key { key, state, .. } => seen_key(key, state),
            wl_keyboard::Event::Enter { keys, .. } => Seen::Enter(keys.len() / 4),
            wl_keyboard::Event::Leave { .. } => Seen::Leave,
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => Seen::Mods(mods_depressed),
            _ => return,
        };
        SEEN.with(|seen_list| seen_list.borrow_mut().push(seen));
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
        if let wl_pointer::Event::Button { serial, .. } = event {
            BUTTON_SERIAL.set(serial);
        }
    }
}

impl Dispatch<zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2, ()>
    for common::Client
{
    fn event(
        _: &mut Self,
        _: &zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
        event: zwp_input_method_keyboard_grab_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_input_method_keyboard_grab_v2::Event::Key { key, state, .. } = event {
            IM_SEEN.with(|seen| seen.borrow_mut().push(seen_key(key, state)));
        }
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for common::Client {
            fn event(_: &mut Self, _: &$proxy, _: <$proxy as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    xdg_positioner::XdgPositioner,
    xdg_popup::XdgPopup,
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_v1::ExtSessionLockV1,
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::ZwpInputMethodV2
);
