//! A key's release goes where its press went. A key a binding or an input
//! grab took never reaches a client, not even its release after the focus
//! or the grab changed, and a key a client got down always comes back up in
//! it, also when its keyboard goes away or the session leaves its VT. The
//! client's modifiers follow the seat's whether or not the key that changed
//! them reached it. Under the session lock only the locker hears a key: no
//! keyboard grab, no input method, and no key pressed in the locker reaches
//! anything else after the unlock.

mod common;

use std::{
    cell::{Cell, RefCell},
    io::Write,
    os::fd::AsFd,
};

use shojiwm_lib::virtual_input::{VirtualButton, VirtualDevice, VirtualInput, VirtualKey};
use shojiwm_rs::prelude::*;
use smithay::{
    backend::input::{ButtonState, InputEvent, KeyState, Keycode},
    input::pointer::MotionEvent,
    utils::SERIAL_COUNTER,
    wayland::input_method::InputMethodSeat,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_seat},
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_wm_base};
use wayland_protocols_misc::{
    zwp_input_method_v2::client::{
        zwp_input_method_keyboard_grab_v2, zwp_input_method_manager_v2, zwp_input_method_v2,
        zwp_input_popup_surface_v2,
    },
    zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    },
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
const F1: u32 = 59;
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

/// What the locker's text input was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Text {
    Enter,
    CommitString(String),
    PreeditString,
    DeleteSurroundingText,
    /// With its serial.
    Done(u32),
}

thread_local! {
    static FIRED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    /// What the keyboard grab of an input method not the locker's was sent.
    static IM_SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    /// What the locker's keyboard was sent.
    static LOCKER_SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    /// The size of each keymap the locker's keyboard was sent.
    static LOCKER_KEYMAPS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    /// The lock surface's last configure: width and height.
    static LOCK_SIZE: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
    static GRAB: RefCell<Option<InputGrab>> = const { RefCell::new(None) };
    static BUTTON_SERIAL: Cell<u32> = const { Cell::new(0) };
    /// Every event the keyboard grab of an input method not the locker's was
    /// sent, by name.
    static IM_GRAB_EVENTS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    /// What the locker's text input was sent.
    static LOCKER_TEXT: RefCell<Vec<Text>> = const { RefCell::new(Vec::new()) };
    /// What the input methods not the locker's were sent, by name.
    static IM_EVENTS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    /// What the locker's input method's keyboard grab was sent.
    static LOCKER_IM_SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    /// What the locker's input method was sent, by name.
    static LOCKER_IM_EVENTS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
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
        "repeat",
        "F1",
        KeyBindOptions::new().repeat(),
        fired("repeat"),
    );
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

/// The session locked by a client of its own with a lock surface on the
/// output, which has the keyboard. Returns the lock.
fn lock(session: &mut common::Session) -> ext_session_lock_v1::ExtSessionLockV1 {
    let locker = session.connect("locker");
    let peer = &session.peers[locker];
    let qh = peer.queue.handle();
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    seat.get_keyboard(&qh, ());
    let compositor: wl_compositor::WlCompositor = peer.bind("wl_compositor", 1);
    let output: wl_output::WlOutput = peer.bind("wl_output", 1);
    let manager: ext_session_lock_manager_v1::ExtSessionLockManagerV1 =
        peer.bind("ext_session_lock_manager_v1", 1);
    let lock = manager.lock(&qh, ());
    let surface = compositor.create_surface(&qh, ());
    lock.get_lock_surface(&surface, &output, &qh, ());
    session.roundtrip();
    assert!(session.state.session_lock_active);
    let (width, height) = LOCK_SIZE.get();
    session.peers[locker].attach_buffer(&surface, width as i32, height as i32);
    session.roundtrip();
    let seen = take(&LOCKER_SEEN);
    assert!(
        matches!(seen[0], Seen::Enter(_)),
        "the locker has no keyboard: {seen:?}"
    );
    take(&LOCKER_KEYMAPS);
    lock
}

fn unlock(session: &mut common::Session, lock: ext_session_lock_v1::ExtSessionLockV1) {
    lock.unlock_and_destroy();
    session.roundtrip();
    assert!(!session.state.session_lock_active);
}

/// An input method, a client of its own, that grabbed the keyboard.
fn input_method(session: &mut common::Session) -> (usize, zwp_input_method_v2::ZwpInputMethodV2) {
    let im = session.connect("input-method");
    let peer = &session.peers[im];
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        peer.bind("zwp_input_method_manager_v2", 1);
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let input_method = manager.get_input_method(&seat, &peer.queue.handle(), ());
    input_method.grab_keyboard(&peer.queue.handle(), ());
    session.roundtrip();
    // The grab starts with the seat's modifiers.
    assert_eq!(take(&IM_SEEN), vec![Seen::Mods(0)]);
    (im, input_method)
}

/// What fcitx5 and ibus do when they are activated: grab the keyboard again.
fn grab_again(
    session: &mut common::Session,
    im: usize,
    input_method: &zwp_input_method_v2::ZwpInputMethodV2,
) {
    input_method.grab_keyboard(&session.peers[im].queue.handle(), ());
    session.roundtrip();
}

/// One key, `k`, on keycode 9, like wtype's or wvkbd's keymap.
const VIRTUAL_KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "(unnamed)" { minimum = 8; maximum = 9; <K1> = 9; };
xkb_types "(unnamed)" { include "complete" };
xkb_compat "(unnamed)" { include "complete" };
xkb_symbols "(unnamed)" { key <K1> {[k]}; };
};
"#;
/// The key of the keymap above, as the virtual keyboard sends it.
const VIRTUAL_K: u32 = 1;

fn virtual_keyboard(session: &mut common::Session, peer: usize) -> ZwpVirtualKeyboardV1 {
    let peer = &session.peers[peer];
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let manager: ZwpVirtualKeyboardManagerV1 = peer.bind("zwp_virtual_keyboard_manager_v1", 1);
    let keyboard = manager.create_virtual_keyboard(&seat, &peer.queue.handle(), ());
    send_keymap(session, &keyboard);
    keyboard
}

fn send_keymap(session: &mut common::Session, keyboard: &ZwpVirtualKeyboardV1) {
    let mut file = common::tempfile("keymap");
    file.write_all(VIRTUAL_KEYMAP.as_bytes()).unwrap();
    file.write_all(&[0]).unwrap();
    keyboard.keymap(1, file.as_fd(), VIRTUAL_KEYMAP.len() as u32 + 1);
    session.roundtrip();
}

fn tap_virtual_k(session: &mut common::Session, keyboard: &ZwpVirtualKeyboardV1) {
    keyboard.key(0, VIRTUAL_K, 1);
    keyboard.key(0, VIRTUAL_K, 0);
    session.roundtrip();
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
fn a_modifier_held_across_the_unlock_comes_up_in_the_window_without_its_key() {
    let mut session = focused_session();

    let locked = lock(&mut session);
    press(&mut session, SHIFT);
    unlock(&mut session, locked);
    release(&mut session, SHIFT);
    assert_eq!(
        take(&LOCKER_SEEN),
        vec![
            Seen::Mods(SHIFT_MASK),
            Seen::Down(SHIFT),
            Seen::Up(SHIFT),
            Seen::Leave
        ]
    );
    // The window never saw Shift go down: it learns of it as a modifier, and
    // of its release the same way.
    assert_eq!(
        take(&SEEN),
        vec![
            Seen::Leave,
            Seen::Enter(0),
            Seen::Mods(SHIFT_MASK),
            Seen::Mods(0)
        ]
    );
}

#[test]
fn a_key_held_across_the_unlock_reaches_neither_the_window_nor_an_input_method() {
    let mut session = focused_session();
    let (_im, _input_method) = input_method(&mut session);

    let locked = lock(&mut session);
    take(&SEEN);
    press(&mut session, K);
    unlock(&mut session, locked);
    release(&mut session, K);
    assert_eq!(
        take(&LOCKER_SEEN),
        vec![Seen::Down(K), Seen::Up(K), Seen::Leave]
    );
    assert_eq!(take(&IM_SEEN), vec![]);
    assert!(
        !take(&SEEN)
            .iter()
            .any(|seen| matches!(seen, Seen::Down(K) | Seen::Up(K) | Seen::Enter(1..))),
    );

    // Its next press is an ordinary one.
    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&SEEN), vec![Seen::Down(K), Seen::Up(K)]);
}

#[test]
fn an_input_method_hears_no_key_while_the_session_is_locked() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);

    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(take(&IM_SEEN), vec![Seen::Down(A), Seen::Up(A)]);

    let locked = lock(&mut session);
    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&IM_SEEN), vec![]);
    assert_eq!(take(&LOCKER_SEEN), vec![Seen::Down(K), Seen::Up(K)]);

    // The grab it held went with the lock; it hears keys again once it grabs
    // again.
    unlock(&mut session, locked);
    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(take(&IM_SEEN), vec![]);
    grab_again(&mut session, im, &input_method);
    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(
        take(&IM_SEEN),
        vec![Seen::Mods(0), Seen::Down(A), Seen::Up(A)]
    );
}

#[test]
fn an_input_method_that_grabs_the_keyboard_under_the_lock_hears_no_key() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);

    let locked = lock(&mut session);
    grab_again(&mut session, im, &input_method);
    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&IM_SEEN), vec![]);
    assert_eq!(take(&LOCKER_SEEN), vec![Seen::Down(K), Seen::Up(K)]);
    unlock(&mut session, locked);
}

#[test]
fn an_input_method_that_grabs_again_under_the_lock_hears_no_release_the_lock_makes() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);

    let locked = lock(&mut session);
    press(&mut session, K);
    grab_again(&mut session, im, &input_method);
    // The session leaves its VT with K down in the locker.
    session.state.pause_input_session();
    session.roundtrip();
    session.state.resume_input_session();
    session.roundtrip();
    assert_eq!(take(&IM_SEEN), vec![]);
    assert_eq!(take(&LOCKER_SEEN), vec![Seen::Down(K), Seen::Up(K)]);
    unlock(&mut session, locked);
}

#[test]
fn an_input_method_never_hears_a_key_with_modifiers_from_before_the_lock() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);

    press(&mut session, SHIFT);
    // Smithay sends an input method's grab the key, then the modifiers.
    assert_eq!(
        take(&IM_SEEN),
        vec![Seen::Down(SHIFT), Seen::Mods(SHIFT_MASK)]
    );
    let locked = lock(&mut session);
    release(&mut session, SHIFT);
    unlock(&mut session, locked);
    // Its grab from before the lock would read this `a` as shifted.
    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(take(&IM_SEEN), vec![]);

    // A new grab starts with the seat's modifiers.
    grab_again(&mut session, im, &input_method);
    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(
        take(&IM_SEEN),
        vec![Seen::Mods(0), Seen::Down(A), Seen::Up(A)]
    );
}

#[test]
fn an_input_method_grabbing_again_after_the_unlock_hears_no_release_from_the_locker() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);

    let locked = lock(&mut session);
    take(&SEEN);
    press(&mut session, K);
    unlock(&mut session, locked);
    grab_again(&mut session, im, &input_method);
    release(&mut session, K);
    assert_eq!(take(&IM_SEEN), vec![Seen::Mods(0)]);
    assert_eq!(
        take(&LOCKER_SEEN),
        vec![Seen::Down(K), Seen::Up(K), Seen::Leave]
    );
    assert!(
        !take(&SEEN)
            .iter()
            .any(|seen| matches!(seen, Seen::Down(K) | Seen::Up(K))),
    );

    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&IM_SEEN), vec![Seen::Down(K), Seen::Up(K)]);
}

/// Smithay tells an input method's grab of the modifiers only after a key it
/// forwards; Shift going down under an input grab reaches it before `a`.
#[test]
fn an_input_method_hears_a_modifier_a_grab_took_before_the_next_key() {
    let mut session = focused_session();
    let (_im, _input_method) = input_method(&mut session);

    press(&mut session, SUPER);
    press(&mut session, G);
    release(&mut session, G);
    release(&mut session, SUPER);
    press(&mut session, SHIFT);
    press(&mut session, ESCAPE);
    release(&mut session, ESCAPE);
    assert!(session.state.runtime_input_grab.is_none());
    press(&mut session, A);
    release(&mut session, A);
    release(&mut session, SHIFT);
    assert_eq!(
        take(&IM_SEEN),
        vec![
            Seen::Down(SUPER),
            Seen::Mods(SUPER_MASK),
            Seen::Up(SUPER),
            Seen::Mods(0),
            Seen::Mods(SHIFT_MASK),
            Seen::Down(A),
            Seen::Up(A),
            Seen::Mods(0)
        ]
    );
}

#[test]
fn an_input_methods_virtual_keyboard_cannot_type_into_the_lock() {
    let mut session = focused_session();
    let (im, _input_method) = input_method(&mut session);
    let keyboard = virtual_keyboard(&mut session, im);

    let locked = lock(&mut session);
    keyboard.modifiers(SHIFT_MASK, 0, 0, 0);
    keyboard.key(0, VIRTUAL_K, 1);
    keyboard.key(0, VIRTUAL_K, 0);
    keyboard.modifiers(0, 0, 0, 0);
    session.roundtrip();
    assert_eq!(take(&LOCKER_SEEN), vec![]);
    unlock(&mut session, locked);
}

/// A client stays an input method for the lock after it destroys its
/// `zwp_input_method_v2`: its virtual keyboard would otherwise type into the
/// locker as an on-screen keyboard.
#[test]
fn an_input_method_that_destroyed_itself_cannot_type_into_the_lock() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);
    let keyboard = virtual_keyboard(&mut session, im);

    let locked = lock(&mut session);
    input_method.destroy();
    session.roundtrip();
    assert!(!session.state.seat.input_method().has_instance());
    send_keymap(&mut session, &keyboard);
    keyboard.modifiers(SHIFT_MASK, 0, 0, 0);
    tap_virtual_k(&mut session, &keyboard);
    keyboard.modifiers(0, 0, 0, 0);
    session.roundtrip();
    assert_eq!(take(&LOCKER_SEEN), vec![]);
    assert_eq!(take(&LOCKER_KEYMAPS), Vec::<u32>::new());
    unlock(&mut session, locked);
}

/// The core knows an input method by its client: a virtual keyboard it opens
/// on a second connection is a client like wvkbd, and types into the locker.
#[test]
fn an_input_methods_virtual_keyboard_on_a_second_connection_types_into_the_lock() {
    let mut session = focused_session();
    let (_im, _input_method) = input_method(&mut session);
    let second = session.connect("input-method-second-connection");
    let keyboard = virtual_keyboard(&mut session, second);

    let locked = lock(&mut session);
    tap_virtual_k(&mut session, &keyboard);
    let seen = take(&LOCKER_SEEN);
    assert!(
        seen.ends_with(&[Seen::Down(VIRTUAL_K), Seen::Up(VIRTUAL_K)]),
        "{seen:?}"
    );
    unlock(&mut session, locked);
}

#[test]
fn a_keymap_sent_under_the_lock_reaches_the_locker_only_from_an_on_screen_keyboard() {
    let mut session = focused_session();
    let (im, _input_method) = input_method(&mut session);
    let im_keyboard = virtual_keyboard(&mut session, im);
    let osk = session.connect("on-screen-keyboard");
    let osk_keyboard = virtual_keyboard(&mut session, osk);

    let locked = lock(&mut session);
    send_keymap(&mut session, &im_keyboard);
    tap_virtual_k(&mut session, &im_keyboard);
    assert_eq!(take(&LOCKER_SEEN), vec![]);
    assert_eq!(take(&LOCKER_KEYMAPS), Vec::<u32>::new());

    send_keymap(&mut session, &osk_keyboard);
    tap_virtual_k(&mut session, &osk_keyboard);
    let seen = take(&LOCKER_SEEN);
    assert!(
        seen.ends_with(&[Seen::Down(VIRTUAL_K), Seen::Up(VIRTUAL_K)]),
        "{seen:?}"
    );
    assert_eq!(take(&LOCKER_KEYMAPS).len(), 1);
    unlock(&mut session, locked);
}

/// An on-screen keyboard such as wvkbd: a virtual keyboard of a client that
/// is no input method types into the lock like a keyboard.
#[test]
fn an_on_screen_keyboard_types_into_the_lock() {
    let mut session = focused_session();
    let osk = session.connect("on-screen-keyboard");
    let keyboard = virtual_keyboard(&mut session, osk);

    let locked = lock(&mut session);
    keyboard.key(0, VIRTUAL_K, 1);
    keyboard.key(0, VIRTUAL_K, 0);
    session.roundtrip();
    let seen = take(&LOCKER_SEEN);
    assert!(
        seen.ends_with(&[Seen::Down(VIRTUAL_K), Seen::Up(VIRTUAL_K)]),
        "{seen:?}"
    );
    unlock(&mut session, locked);
}

#[test]
fn a_virtual_keyboard_destroyed_with_a_key_down_lets_it_go() {
    let mut session = focused_session();
    let osk = session.connect("on-screen-keyboard");
    let keyboard = virtual_keyboard(&mut session, osk);

    keyboard.key(0, VIRTUAL_K, 1);
    session.roundtrip();
    keyboard.destroy();
    session.roundtrip();
    let seen = take(&SEEN);
    let down = seen.iter().position(|seen| *seen == Seen::Down(VIRTUAL_K));
    let up = seen.iter().position(|seen| *seen == Seen::Up(VIRTUAL_K));
    assert!(down.is_some() && up > down, "{seen:?}");
}

#[test]
fn a_repeating_bind_repeats_on_while_another_keyboard_taps_its_key() {
    let mut session = focused_session();
    session.state.keyboard_repeat_info = (25, 600);

    send_from(&mut session, "first", F1, KeyState::Pressed);
    assert_eq!(take(&FIRED), vec!["repeat"]);
    assert!(session.state.key_binding_repeat.is_some());
    send_from(&mut session, "second", F1, KeyState::Pressed);
    send_from(&mut session, "second", F1, KeyState::Released);
    assert!(session.state.key_binding_repeat.is_some());
    send_from(&mut session, "first", F1, KeyState::Released);
    assert!(session.state.key_binding_repeat.is_none());
    assert_eq!(take(&FIRED), Vec::<&str>::new());
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

struct Window {
    compositor: wl_compositor::WlCompositor,
    shell: xdg_wm_base::XdgWmBase,
    seat: wl_seat::WlSeat,
    xdg: xdg_surface::XdgSurface,
}

/// A focused toplevel of the session's own client with the pointer over it.
fn window_under_pointer(session: &mut common::Session) -> Window {
    let qh = session.queue.handle();
    let compositor: wl_compositor::WlCompositor = session.bind("wl_compositor", 1);
    let shell: xdg_wm_base::XdgWmBase = session.bind("xdg_wm_base", 1);
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    let window = compositor.create_surface(&qh, ());
    let xdg = shell.get_xdg_surface(&window, &qh, ());
    xdg.get_toplevel(&qh, ());
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
    Window {
        compositor,
        shell,
        seat,
        xdg,
    }
}

/// A click opens a menu, which grabs with the click's serial.
fn open_menu(session: &mut common::Session, window: &Window) {
    let qh = session.queue.handle();
    button(session, ButtonState::Pressed);
    let positioner = window.shell.create_positioner(&qh, ());
    positioner.set_size(50, 50);
    positioner.set_anchor_rect(0, 0, 10, 10);
    let menu = window.compositor.create_surface(&qh, ());
    let menu_xdg = window.shell.get_xdg_surface(&menu, &qh, ());
    let popup = menu_xdg.get_popup(Some(&window.xdg), &positioner, &qh, ());
    popup.grab(&window.seat, BUTTON_SERIAL.get());
    menu.commit();
    session.roundtrip();
    session.attach_buffer(&menu, 50, 50);
    button(session, ButtonState::Released);
}

#[test]
fn a_menu_that_grabbed_the_keyboard_hears_no_key_while_the_session_is_locked() {
    let mut session = common::Session::new(setup);
    let window = window_under_pointer(&mut session);
    open_menu(&mut session, &window);
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

/// The input method's grab went with the lock, and the input method has not
/// grabbed again yet: nothing holds the keyboard, so the menu may.
#[test]
fn a_menu_opened_after_the_unlock_grabs_the_keyboard_before_the_input_method_grabs_again() {
    let mut session = common::Session::new(setup);
    let window = window_under_pointer(&mut session);
    let (_im, _input_method) = input_method(&mut session);
    let locked = lock(&mut session);
    unlock(&mut session, locked);

    open_menu(&mut session, &window);
    assert!(session.state.seat.get_keyboard().unwrap().is_grabbed());
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

/// The locker's text input, enabled on its lock surface.
fn locker_text_input(session: &mut common::Session) -> zwp_text_input_v3::ZwpTextInputV3 {
    let locker = session
        .peers
        .iter()
        .position(|peer| peer.client.name == "locker")
        .unwrap();
    let peer = &session.peers[locker];
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let manager: zwp_text_input_manager_v3::ZwpTextInputManagerV3 =
        peer.bind("zwp_text_input_manager_v3", 1);
    let text_input = manager.get_text_input(&seat, &peer.queue.handle(), ());
    session.roundtrip();
    text_input.enable();
    text_input.commit();
    session.roundtrip();
    text_input
}

fn locker(session: &common::Session) -> usize {
    session
        .peers
        .iter()
        .position(|peer| peer.client.name == "locker")
        .unwrap()
}

/// An input method of the locker's own.
fn locker_input_method(session: &mut common::Session) -> zwp_input_method_v2::ZwpInputMethodV2 {
    let peer = &session.peers[locker(session)];
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        peer.bind("zwp_input_method_manager_v2", 1);
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let input_method = manager.get_input_method(&seat, &peer.queue.handle(), ());
    session.roundtrip();
    input_method
}

/// The serial an input method's next commit carries: the `done` events it
/// was sent since its last `unavailable`, where smithay counts from 0 again.
fn dones(events: &[&str]) -> u32 {
    let since = events
        .iter()
        .rposition(|event| *event == "unavailable")
        .map_or(0, |at| at + 1);
    events[since..]
        .iter()
        .filter(|event| **event == "done")
        .count() as u32
}

/// An input method that held the keyboard before the lock, and one asked
/// for under it, poll the modifiers through fresh grabs while the user types
/// the password with Shift.
#[test]
fn an_input_method_gets_no_keyboard_grab_under_the_lock() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);
    let locked = lock(&mut session);
    take(&IM_GRAB_EVENTS);

    press(&mut session, SHIFT);
    for _ in 0..3 {
        grab_again(&mut session, im, &input_method);
    }
    let peer = &session.peers[im];
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        peer.bind("zwp_input_method_manager_v2", 1);
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let second = manager.get_input_method(&seat, &peer.queue.handle(), ());
    second.grab_keyboard(&peer.queue.handle(), ());
    session.roundtrip();
    release(&mut session, SHIFT);
    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&IM_GRAB_EVENTS), Vec::<&str>::new());
    assert_eq!(take(&IM_SEEN), vec![]);
    unlock(&mut session, locked);

    // Activated again after the unlock, it grabs as usual.
    grab_again(&mut session, im, &input_method);
    press(&mut session, A);
    release(&mut session, A);
    assert_eq!(
        take(&IM_SEEN),
        vec![Seen::Mods(0), Seen::Down(A), Seen::Up(A)]
    );
}

/// Another client's input method commits text while the locker's password
/// field is active with the locker's own input method.
#[test]
fn an_input_method_cannot_type_into_the_lockers_text_input() {
    let mut session = focused_session();
    let locked = lock(&mut session);
    let locker_im = locker_input_method(&mut session);
    let text_input = locker_text_input(&mut session);
    assert_eq!(take(&LOCKER_TEXT), vec![Text::Enter]);
    let mut heard = take(&LOCKER_IM_EVENTS);
    assert!(heard.contains(&"activate"), "{heard:?}");

    let im = session.connect("input-method");
    let peer = &session.peers[im];
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        peer.bind("zwp_input_method_manager_v2", 1);
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let input_method = manager.get_input_method(&seat, &peer.queue.handle(), ());
    session.roundtrip();
    // Smithay enters the focused text input again for each new input method,
    // and tells the one it keeps that it is unavailable.
    assert_eq!(take(&LOCKER_TEXT), vec![Text::Enter]);
    heard.extend(take(&LOCKER_IM_EVENTS));
    assert_eq!(heard.last(), Some(&"unavailable"));
    input_method.set_preedit_string("hun".into(), 0, 3);
    input_method.commit_string("hunter2".into());
    input_method.delete_surrounding_text(4, 0);
    input_method.commit(dones(&heard));
    session.roundtrip();
    assert_eq!(take(&LOCKER_TEXT), vec![]);

    // The locker's own input method still types into it.
    locker_im.commit_string("a".into());
    locker_im.commit(dones(&heard));
    session.roundtrip();
    assert_eq!(
        take(&LOCKER_TEXT),
        vec![Text::CommitString("a".into()), Text::Done(1)]
    );
    text_input.set_surrounding_text("a".into(), 1, 1);
    text_input.commit();
    session.roundtrip();
    heard.extend(take(&LOCKER_IM_EVENTS));
    locker_im.commit_string("b".into());
    locker_im.commit(dones(&heard));
    session.roundtrip();
    assert_eq!(
        take(&LOCKER_TEXT),
        vec![Text::CommitString("b".into()), Text::Done(2)]
    );
    unlock(&mut session, locked);
}

/// The locker's password field holds `enable` and its text, not committed
/// yet, while its own input method is the seat's. Another client creates an
/// input method and destroys it, which makes the seat forget the locker's,
/// and creates the one the seat takes next.
#[test]
fn a_replacement_input_method_hears_nothing_the_locker_left_pending() {
    let mut session = focused_session();
    let locked = lock(&mut session);
    let _locker_im = locker_input_method(&mut session);
    let peer = &session.peers[locker(&session)];
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let manager: zwp_text_input_manager_v3::ZwpTextInputManagerV3 =
        peer.bind("zwp_text_input_manager_v3", 1);
    let text_input = manager.get_text_input(&seat, &peer.queue.handle(), ());
    session.roundtrip();
    text_input.enable();
    text_input.set_content_type(
        zwp_text_input_v3::ContentHint::SensitiveData,
        zwp_text_input_v3::ContentPurpose::Password,
    );
    text_input.set_surrounding_text("hunter2".into(), 7, 7);
    session.roundtrip();

    let im = session.connect("input-method");
    let peer = &session.peers[im];
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        peer.bind("zwp_input_method_manager_v2", 1);
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let first = manager.get_input_method(&seat, &peer.queue.handle(), ());
    session.roundtrip();
    first.destroy();
    session.roundtrip();
    assert!(!session.state.seat.input_method().has_instance());
    let peer = &session.peers[im];
    let _replacement = manager.get_input_method(&seat, &peer.queue.handle(), ());
    session.roundtrip();
    assert!(session.state.seat.input_method().has_instance());
    take(&IM_EVENTS);

    text_input.commit();
    session.roundtrip();
    let heard = take(&IM_EVENTS);
    assert!(
        !heard
            .iter()
            .any(|event| matches!(*event, "activate" | "surrounding_text" | "content_type")),
        "{heard:?}"
    );

    // Nor does a whole new state.
    text_input.enable();
    text_input.set_surrounding_text("hunter2".into(), 7, 7);
    text_input.commit();
    session.roundtrip();
    let heard = take(&IM_EVENTS);
    assert!(
        !heard
            .iter()
            .any(|event| matches!(*event, "activate" | "surrounding_text" | "content_type")),
        "{heard:?}"
    );
    unlock(&mut session, locked);
}

/// The locker's own input method grabs the keyboard under the lock and
/// hears the keys, where a grab another client held before the lock went
/// at the first key.
#[test]
fn the_lockers_input_method_keeps_its_keyboard_grab_under_the_lock() {
    let mut session = focused_session();
    let (_im, _input_method) = input_method(&mut session);
    let locked = lock(&mut session);
    press(&mut session, K);
    release(&mut session, K);
    assert_eq!(take(&IM_SEEN), vec![]);
    assert_eq!(take(&LOCKER_SEEN), vec![Seen::Down(K), Seen::Up(K)]);

    let locker_im = locker_input_method(&mut session);
    locker_im.grab_keyboard(&session.peers[locker(&session)].queue.handle(), ());
    session.roundtrip();
    assert_eq!(take(&LOCKER_IM_SEEN), vec![Seen::Mods(0)]);
    for _ in 0..2 {
        press(&mut session, K);
        release(&mut session, K);
        assert_eq!(take(&LOCKER_IM_SEEN), vec![Seen::Down(K), Seen::Up(K)]);
    }
    assert!(session.state.seat.get_keyboard().unwrap().is_grabbed());
    assert_eq!(take(&LOCKER_SEEN), vec![]);
    assert_eq!(take(&IM_SEEN), vec![]);
    unlock(&mut session, locked);
}

/// An input method refused a grab under the lock while the locker's own
/// input method holds the keyboard, and an on-screen keyboard presses Shift.
#[test]
fn an_input_method_refused_a_grab_hears_no_modifiers() {
    let mut session = focused_session();
    let osk = session.connect("on-screen-keyboard");
    let keyboard = virtual_keyboard(&mut session, osk);
    let locked = lock(&mut session);

    let im = session.connect("input-method");
    let peer = &session.peers[im];
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 =
        peer.bind("zwp_input_method_manager_v2", 1);
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let input_method = manager.get_input_method(&seat, &peer.queue.handle(), ());
    input_method.grab_keyboard(&peer.queue.handle(), ());
    session.roundtrip();
    let locker_im = locker_input_method(&mut session);
    locker_im.grab_keyboard(&session.peers[locker(&session)].queue.handle(), ());
    session.roundtrip();
    assert_eq!(take(&LOCKER_IM_SEEN), vec![Seen::Mods(0)]);

    keyboard.modifiers(SHIFT_MASK, 0, 0, 0);
    session.roundtrip();
    keyboard.modifiers(0, 0, 0, 0);
    session.roundtrip();
    assert_eq!(take(&IM_GRAB_EVENTS), Vec::<&str>::new());
    assert_eq!(
        take(&LOCKER_IM_SEEN),
        vec![Seen::Mods(SHIFT_MASK), Seen::Mods(0)]
    );
    unlock(&mut session, locked);
}

/// Another client's input method maps its candidate popup while the
/// locker's text input is enabled: it would show over the lock screen.
#[test]
fn an_input_method_popup_does_not_map_over_the_lock() {
    let mut session = focused_session();
    let (im, input_method) = input_method(&mut session);
    let locked = lock(&mut session);
    let _text_input = locker_text_input(&mut session);

    let peer = &session.peers[im];
    let compositor: wl_compositor::WlCompositor = peer.bind("wl_compositor", 1);
    let surface = compositor.create_surface(&peer.queue.handle(), ());
    input_method.get_input_popup_surface(&surface, &peer.queue.handle(), ());
    session.roundtrip();
    session.peers[im].attach_buffer(&surface, 40, 20);
    session.roundtrip();
    let lock_surface = session
        .state
        .session_lock_surfaces
        .values()
        .next()
        .unwrap()
        .wl_surface()
        .clone();
    assert_eq!(
        smithay::desktop::PopupManager::popups_for_surface(&lock_surface).count(),
        0,
        "an input method popup is tracked over the lock surface"
    );
    unlock(&mut session, locked);
}

/// The locker's password field sends its state on: the input method of
/// another client hears none of it, and after the unlock it starts again
/// from a deactivate.
#[test]
fn an_input_method_hears_nothing_of_the_lockers_text_input() {
    let mut session = focused_session();
    let (_im, _input_method) = input_method(&mut session);
    let qh = session.queue.handle();
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    let manager: zwp_text_input_manager_v3::ZwpTextInputManagerV3 =
        session.bind("zwp_text_input_manager_v3", 1);
    let window_text = manager.get_text_input(&seat, &qh, ());
    session.roundtrip();
    window_text.enable();
    window_text.commit();
    session.roundtrip();
    let heard = take(&IM_EVENTS);
    assert!(heard.contains(&"activate"), "{heard:?}");

    let locked = lock(&mut session);
    take(&IM_EVENTS);
    let text_input = locker_text_input(&mut session);
    text_input.set_content_type(
        zwp_text_input_v3::ContentHint::SensitiveData,
        zwp_text_input_v3::ContentPurpose::Password,
    );
    text_input.set_surrounding_text("hunter2".into(), 7, 7);
    text_input.set_text_change_cause(zwp_text_input_v3::ChangeCause::Other);
    text_input.set_cursor_rectangle(10, 10, 1, 20);
    text_input.commit();
    session.roundtrip();
    let heard = take(&IM_EVENTS);
    assert!(
        !heard.iter().any(|event| matches!(
            *event,
            "activate" | "surrounding_text" | "content_type" | "text_change_cause"
        )),
        "{heard:?}"
    );

    unlock(&mut session, locked);
    let heard = take(&IM_EVENTS);
    assert_eq!(heard.first(), Some(&"deactivate"), "{heard:?}");
    assert!(!heard.contains(&"activate"), "{heard:?}");
    window_text.enable();
    window_text.set_surrounding_text("abc".into(), 3, 3);
    window_text.commit();
    session.roundtrip();
    let heard = take(&IM_EVENTS);
    assert_eq!(heard.first(), Some(&"activate"), "{heard:?}");
    assert!(heard.contains(&"surrounding_text"), "{heard:?}");
}

fn seen_key(key: u32, state: WEnum<wl_keyboard::KeyState>) -> Seen {
    match state {
        WEnum::Value(wl_keyboard::KeyState::Pressed) => Seen::Down(key),
        _ => Seen::Up(key),
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for common::Client {
    fn event(
        client: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { size, .. } = event {
            if client.name == "locker" {
                LOCKER_KEYMAPS.with(|keymaps| keymaps.borrow_mut().push(size));
            }
            return;
        }
        let seen = match event {
            wl_keyboard::Event::Key { key, state, .. } => seen_key(key, state),
            wl_keyboard::Event::Enter { keys, .. } => Seen::Enter(keys.len() / 4),
            wl_keyboard::Event::Leave { .. } => Seen::Leave,
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => Seen::Mods(mods_depressed),
            _ => return,
        };
        let list = match client.name {
            "" => &SEEN,
            "locker" => &LOCKER_SEEN,
            _ => return,
        };
        list.with(|seen_list| seen_list.borrow_mut().push(seen));
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
        client: &mut Self,
        _: &zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
        event: zwp_input_method_keyboard_grab_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let lockers = client.name == "locker";
        if !lockers {
            IM_GRAB_EVENTS.with(|events| {
                events.borrow_mut().push(match &event {
                    zwp_input_method_keyboard_grab_v2::Event::Keymap { .. } => "keymap",
                    zwp_input_method_keyboard_grab_v2::Event::RepeatInfo { .. } => "repeat_info",
                    zwp_input_method_keyboard_grab_v2::Event::Modifiers { .. } => "modifiers",
                    zwp_input_method_keyboard_grab_v2::Event::Key { .. } => "key",
                    _ => "other",
                })
            });
        }
        let seen = match event {
            zwp_input_method_keyboard_grab_v2::Event::Key { key, state, .. } => {
                seen_key(key, state)
            }
            zwp_input_method_keyboard_grab_v2::Event::Modifiers { mods_depressed, .. } => {
                Seen::Mods(mods_depressed)
            }
            _ => return,
        };
        let list = if lockers { &LOCKER_IM_SEEN } else { &IM_SEEN };
        list.with(|seen_list| seen_list.borrow_mut().push(seen));
    }
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for common::Client {
    fn event(
        client: &mut Self,
        _: &zwp_input_method_v2::ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let name = match event {
            zwp_input_method_v2::Event::Activate => "activate",
            zwp_input_method_v2::Event::Deactivate => "deactivate",
            zwp_input_method_v2::Event::SurroundingText { .. } => "surrounding_text",
            zwp_input_method_v2::Event::TextChangeCause { .. } => "text_change_cause",
            zwp_input_method_v2::Event::ContentType { .. } => "content_type",
            zwp_input_method_v2::Event::Done => "done",
            zwp_input_method_v2::Event::Unavailable => "unavailable",
            _ => return,
        };
        let list = if client.name == "locker" {
            &LOCKER_IM_EVENTS
        } else {
            &IM_EVENTS
        };
        list.with(|events| events.borrow_mut().push(name));
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for common::Client {
    fn event(
        client: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if client.name != "locker" {
            return;
        }
        let text = match event {
            zwp_text_input_v3::Event::Enter { .. } => Text::Enter,
            zwp_text_input_v3::Event::CommitString { text } => {
                Text::CommitString(text.unwrap_or_default())
            }
            zwp_text_input_v3::Event::PreeditString { .. } => Text::PreeditString,
            zwp_text_input_v3::Event::DeleteSurroundingText { .. } => Text::DeleteSurroundingText,
            zwp_text_input_v3::Event::Done { serial } => Text::Done(serial),
            _ => return,
        };
        LOCKER_TEXT.with(|seen| seen.borrow_mut().push(text));
    }
}

impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ()> for common::Client {
    fn event(
        _: &mut Self,
        surface: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            surface.ack_configure(serial);
            LOCK_SIZE.set((width, height));
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
    zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
    zwp_text_input_manager_v3::ZwpTextInputManagerV3
);
