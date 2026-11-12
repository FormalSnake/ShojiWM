//! Under the session lock the pointer reaches a lock surface or nothing: no
//! window, layer or popup underneath, no grab a client held when the lock
//! went up, no drag. A button held in a window when the lock or an input grab
//! takes the pointer comes up in that window at once, and its own release goes
//! nowhere. A press an input grab took comes up in the grab, or nowhere once
//! the grab is gone. While an input grab owns the keyboard, nothing else
//! takes the keyboard focus until it ends. Under the lock no other client
//! takes a grab, the keyboard or the cursor, and the unlock takes the pointer
//! from the locker the way the lock took it from the session.

mod common;

use std::cell::{Cell, RefCell};

use shojiwm_lib::virtual_input::{
    AxisAmount, AxisFrame, VirtualAxis, VirtualButton, VirtualDevice, VirtualInput, VirtualKey,
    VirtualMotion, VirtualMotionAbsolute,
};
use shojiwm_rs::prelude::*;
use smithay::backend::input::{AxisSource, ButtonState, InputEvent, KeyState, Keycode};
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    backend::ObjectId,
    protocol::{
        wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
        wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_surface,
    },
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::KeyboardInteractivity;

const BTN_LEFT: u32 = 0x110;
/// evdev codes.
const ESCAPE: u32 = 1;
const K: u32 = 37;
const F2: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Enter(&'static str),
    Leave(&'static str),
    Motion(&'static str),
    Down(&'static str),
    Up(&'static str),
    Scroll(&'static str),
    Relative(&'static str),
}

thread_local! {
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    /// The surfaces the session's own client got the keyboard on.
    static KEYBOARD: RefCell<Vec<ObjectId>> = const { RefCell::new(Vec::new()) };
    /// What the data source of a drag was told.
    static SOURCE: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    /// What the window's pointer lock was told.
    static CONSTRAINT: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    static BUTTON_SERIAL: Cell<u32> = const { Cell::new(0) };
    static LOCK_SIZE: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
    static GRAB: RefCell<Option<InputGrab>> = const { RefCell::new(None) };
    static GRAB_MOTIONS: Cell<usize> = const { Cell::new(0) };
    static GRAB_BUTTONS: Cell<usize> = const { Cell::new(0) };
    /// The keys the session's own client got.
    static KEYS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static POPUP_DONE: Cell<usize> = const { Cell::new(0) };
    /// The serial of the last pointer enter of the session's own client and
    /// of the locker.
    static DESKTOP_ENTER: Cell<u32> = const { Cell::new(0) };
    static LOCKER_ENTER: Cell<u32> = const { Cell::new(0) };
    static ACTIVATIONS: Cell<usize> = const { Cell::new(0) };
    static TOKEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn take<T>(seen: &'static std::thread::LocalKey<RefCell<Vec<T>>>) -> Vec<T> {
    seen.with(|seen| std::mem::take(&mut *seen.borrow_mut()))
}

/// What the session's own client (the window, its layers) got.
fn window_seen() -> Vec<Seen> {
    take(&SEEN)
        .into_iter()
        .filter(|seen| !matches!(seen, Seen::Enter("locker") | Seen::Leave("locker")))
        .filter(|seen| {
            !matches!(
                seen,
                Seen::Motion("locker")
                    | Seen::Down("locker")
                    | Seen::Up("locker")
                    | Seen::Scroll("locker")
            )
        })
        .collect()
}

/// The presses and releases seen, without the motion a press resends.
fn buttons() -> Vec<Seen> {
    take(&SEEN)
        .into_iter()
        .filter(|seen| matches!(seen, Seen::Down(_) | Seen::Up(_)))
        .collect()
}

fn setup() {
    COMPOSITOR.window.composition(|_| {
        ManagedWindow::new()
            .rect(Rect::new(0.0, 0.0, 300.0, 300.0))
            .child(ClientWindow::new())
    });
    // An overview: opened by a key, closed with Escape, and the top layers
    // keep the pointer under it.
    COMPOSITOR.key.bind("overview", "F2", || {
        let grab = COMPOSITOR.input.grab(
            InputGrabOptions::new()
                .leave_layers_the_pointer()
                .on_pointer_motion(|_| GRAB_MOTIONS.set(GRAB_MOTIONS.get() + 1))
                .on_pointer_button(|_| GRAB_BUTTONS.set(GRAB_BUTTONS.get() + 1))
                .on_key(|event| {
                    if event.key == "Escape"
                        && event.state == InputGrabState::Pressed
                        && let Some(grab) = GRAB.with(|grab| grab.borrow_mut().take())
                    {
                        grab.release();
                    }
                }),
        );
        GRAB.with(|current| *current.borrow_mut() = Some(grab));
    });
    COMPOSITOR
        .event
        .on_window_activate_request(|_, _| ACTIVATIONS.set(ACTIVATIONS.get() + 1));
    set_interval(1.0, || {});
}

fn setup_two_outputs() {
    setup();
    COMPOSITOR.output.create(HeadlessOutputOptions {
        width: 800,
        height: 600,
        ..Default::default()
    });
}

fn device(id: &str, keyboard: bool) -> VirtualDevice {
    VirtualDevice {
        id: id.into(),
        name: id.into(),
        keyboard,
    }
}

/// Puts the pointer at `x`, `y` of the output layout.
fn place(session: &mut common::Session, x: f64, y: f64) {
    let bounds = session.state.output_layout_bounds().unwrap();
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerMotionAbsolute {
            event: VirtualMotionAbsolute {
                device: device("test-pointer", false),
                time: 0,
                x: (x - bounds.loc.x as f64) / bounds.size.w as f64,
                y: (y - bounds.loc.y as f64) / bounds.size.h as f64,
            },
        });
    session.roundtrip();
}

fn nudge(session: &mut common::Session) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerMotion {
            event: VirtualMotion {
                device: device("test-pointer", false),
                time: 1,
                dx: 3.0,
                dy: 2.0,
            },
        });
    session.roundtrip();
}

fn button(session: &mut common::Session, state: ButtonState) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerButton {
            event: VirtualButton {
                device: device("test-pointer", false),
                time: 0,
                button: BTN_LEFT,
                state,
            },
        });
    session.roundtrip();
}

fn scroll(session: &mut common::Session) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerAxis {
            event: VirtualAxis {
                device: device("test-pointer", false),
                time: 0,
                frame: AxisFrame {
                    time_msec: 0,
                    source: Some(AxisSource::Finger),
                    horizontal: AxisAmount::default(),
                    vertical: AxisAmount {
                        value: Some(10.0),
                        v120: None,
                    },
                },
            },
        });
    session.roundtrip();
}

fn tap_key(session: &mut common::Session, key: u32) {
    for state in [KeyState::Pressed, KeyState::Released] {
        session
            .state
            .process_input_event(InputEvent::<VirtualInput>::Keyboard {
                event: VirtualKey {
                    device: device("test-keyboard", true),
                    time: 0,
                    keycode: Keycode::new(key + 8),
                    state,
                    count: 1,
                },
            });
        session.roundtrip();
    }
}

/// A window of the session's own client with its pointer and keyboard, and
/// the pointer over it.
fn window_session() -> (
    common::Session,
    wl_surface::WlSurface,
    wl_pointer::WlPointer,
) {
    let (session, desktop) = desktop_session(setup);
    (session, desktop.surface, desktop.pointer)
}

/// The window of [`window_session`] and what its client opens menus with.
struct Desktop {
    compositor: wl_compositor::WlCompositor,
    shell: xdg_wm_base::XdgWmBase,
    seat: wl_seat::WlSeat,
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    pointer: wl_pointer::WlPointer,
}

fn desktop_session(setup: fn()) -> (common::Session, Desktop) {
    let mut session = common::Session::new(setup);
    let qh = session.queue.handle();
    let compositor: wl_compositor::WlCompositor = session.bind("wl_compositor", 1);
    let shell: xdg_wm_base::XdgWmBase = session.bind("xdg_wm_base", 1);
    let surface = compositor.create_surface(&qh, ());
    let xdg = shell.get_xdg_surface(&surface, &qh, ());
    xdg.get_toplevel(&qh, ()).set_app_id("a".into());
    surface.commit();
    session.roundtrip();
    session.attach_buffer(&surface, 300, 300);
    session.state.refresh_window_decorations().unwrap();
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    let pointer = seat.get_pointer(&qh, ());
    seat.get_keyboard(&qh, ());
    COMPOSITOR.window.list()[0].focus();
    session.roundtrip();
    place(&mut session, 150.0, 150.0);
    let seen = take(&SEEN);
    assert!(seen.contains(&Seen::Enter("")), "{seen:?}");
    take(&KEYBOARD);
    (
        session,
        Desktop {
            compositor,
            shell,
            seat,
            surface,
            xdg,
            pointer,
        },
    )
}

struct Locker {
    peer: usize,
    lock: ext_session_lock_v1::ExtSessionLockV1,
    surface: wl_surface::WlSurface,
    lock_surface: ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
    pointer: wl_pointer::WlPointer,
}

/// The session locked by a client of its own with a pointer, its lock
/// surface not mapped yet.
fn lock(session: &mut common::Session) -> Locker {
    let peer = session.connect("locker");
    let client = &session.peers[peer];
    let qh = client.queue.handle();
    let seat: wl_seat::WlSeat = client.bind("wl_seat", 1);
    let pointer = seat.get_pointer(&qh, ());
    let compositor: wl_compositor::WlCompositor = client.bind("wl_compositor", 1);
    let output: wl_output::WlOutput = client.bind("wl_output", 1);
    let manager: ext_session_lock_manager_v1::ExtSessionLockManagerV1 =
        client.bind("ext_session_lock_manager_v1", 1);
    session.roundtrip();
    let lock = manager.lock(&qh, ());
    let surface = compositor.create_surface(&qh, ());
    let lock_surface = lock.get_lock_surface(&surface, &output, &qh, ());
    session.roundtrip();
    assert!(session.state.session_lock_active);
    Locker {
        peer,
        lock,
        surface,
        lock_surface,
        pointer,
    }
}

/// Maps the lock surface over the whole output; with `input`, only that
/// rectangle of it takes input.
fn map_lock_surface(
    session: &mut common::Session,
    locker: &Locker,
    input: Option<(i32, i32, i32, i32)>,
) {
    let client = &session.peers[locker.peer];
    if let Some((x, y, width, height)) = input {
        let compositor: wl_compositor::WlCompositor = client.bind("wl_compositor", 1);
        let region = compositor.create_region(&client.queue.handle(), ());
        region.add(x, y, width, height);
        locker.surface.set_input_region(Some(&region));
    }
    let (width, height) = LOCK_SIZE.get();
    session.peers[locker.peer].attach_buffer(&locker.surface, width as i32, height as i32);
    session.roundtrip();
}

fn unlock(session: &mut common::Session, locker: Locker) {
    locker.lock.unlock_and_destroy();
    session.roundtrip();
    assert!(!session.state.session_lock_active);
}

/// The locker's own events in `seen`.
fn of_locker(seen: &[Seen]) -> Vec<Seen> {
    seen.iter()
        .copied()
        .filter(|seen| {
            matches!(
                seen,
                Seen::Enter("locker")
                    | Seen::Leave("locker")
                    | Seen::Down("locker")
                    | Seen::Up("locker")
            )
        })
        .collect()
}

#[test]
fn under_the_lock_the_pointer_reaches_a_lock_surface_or_nothing() {
    let (mut session, _window, _pointer) = window_session();

    // No lock surface yet: nothing under the pointer may have it.
    let locker = lock(&mut session);
    place(&mut session, 160.0, 160.0);
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    scroll(&mut session);
    assert_eq!(window_seen(), [Seen::Leave("")]);

    // A lock surface that takes input only in its top left corner: the
    // window under the rest of it stays out of reach.
    map_lock_surface(&mut session, &locker, Some((0, 0, 100, 100)));
    place(&mut session, 170.0, 170.0);
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    scroll(&mut session);
    assert_eq!(take(&SEEN), []);

    place(&mut session, 50.0, 50.0);
    let seen = take(&SEEN);
    assert_eq!(seen.first(), Some(&Seen::Enter("locker")), "{seen:?}");
    unlock(&mut session, locker);
}

#[test]
fn locking_lets_go_of_a_button_held_in_a_window() {
    let (mut session, _window, _pointer) = window_session();
    button(&mut session, ButtonState::Pressed);
    assert!(take(&SEEN).ends_with(&[Seen::Down("")]));

    let locker = lock(&mut session);
    assert_eq!(take(&SEEN), [Seen::Up(""), Seen::Leave("")]);
    map_lock_surface(&mut session, &locker, None);
    nudge(&mut session);
    scroll(&mut session);
    button(&mut session, ButtonState::Released);
    let seen = take(&SEEN);
    assert!(
        seen.iter().all(|seen| matches!(
            seen,
            Seen::Enter("locker") | Seen::Motion("locker") | Seen::Scroll("locker")
        )),
        "{seen:?}"
    );
    assert!(seen.contains(&Seen::Enter("locker")), "{seen:?}");

    // The next click is a click of the locker's.
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    assert_eq!(buttons(), [Seen::Down("locker"), Seen::Up("locker")]);
    unlock(&mut session, locker);
}

#[test]
fn locking_cancels_a_drag() {
    let (mut session, window, _pointer) = window_session();
    let qh = session.queue.handle();
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    let manager: wl_data_device_manager::WlDataDeviceManager =
        session.bind("wl_data_device_manager", 3);
    let data_device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    source.offer("text/plain".into());
    source.set_actions(wl_data_device_manager::DndAction::Copy);
    button(&mut session, ButtonState::Pressed);
    data_device.start_drag(Some(&source), &window, None, BUTTON_SERIAL.get());
    session.roundtrip();
    nudge(&mut session);
    assert!(
        session.state.seat.get_pointer().unwrap().is_grabbed(),
        "the drag did not start"
    );
    take(&SEEN);

    // The drag ends with the lock, not with the release over the locker.
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    assert_eq!(take(&SOURCE), ["cancelled"]);
    button(&mut session, ButtonState::Released);
    assert_eq!(take(&SOURCE), Vec::<&str>::new());
    let seen = take(&SEEN);
    assert!(!seen.contains(&Seen::Up("locker")), "{seen:?}");
    unlock(&mut session, locker);
}

#[test]
fn locking_unlocks_a_locked_pointer_and_its_relative_motion() {
    let (mut session, window, pointer) = window_session();
    let qh = session.queue.handle();
    let constraints: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1 =
        session.bind("zwp_pointer_constraints_v1", 1);
    constraints.lock_pointer(
        &window,
        &pointer,
        None,
        zwp_pointer_constraints_v1::Lifetime::Persistent,
        &qh,
        (),
    );
    let relative: zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1 =
        session.bind("zwp_relative_pointer_manager_v1", 1);
    relative.get_relative_pointer(&pointer, &qh, ());
    session.roundtrip();
    nudge(&mut session);
    assert_eq!(take(&CONSTRAINT), ["locked"]);
    assert!(take(&SEEN).contains(&Seen::Relative("")));

    let locker = lock(&mut session);
    nudge(&mut session);
    map_lock_surface(&mut session, &locker, None);
    nudge(&mut session);
    assert_eq!(take(&CONSTRAINT), ["unlocked"]);
    let seen = window_seen();
    assert_eq!(seen, [Seen::Leave("")]);
    unlock(&mut session, locker);
}

#[test]
fn a_window_holding_a_button_when_an_input_grab_starts_loses_the_pointer() {
    let (mut session, _window, _pointer) = window_session();
    button(&mut session, ButtonState::Pressed);
    assert!(take(&SEEN).ends_with(&[Seen::Down("")]));

    tap_key(&mut session, F2);
    assert!(session.state.runtime_input_grab.is_some());
    assert_eq!(take(&SEEN), [Seen::Up(""), Seen::Leave("")]);
    let motions = GRAB_MOTIONS.get();
    nudge(&mut session);
    scroll(&mut session);
    button(&mut session, ButtonState::Released);
    assert_eq!(take(&SEEN), []);
    assert!(
        GRAB_MOTIONS.get() > motions,
        "the grab did not get the pointer"
    );
    assert_eq!(
        GRAB_BUTTONS.get(),
        0,
        "the grab heard a release it never had the press of"
    );
}

#[test]
fn a_press_an_input_grab_took_comes_up_nowhere_once_the_grab_ended() {
    let (mut session, _window, _pointer) = window_session();
    tap_key(&mut session, F2);
    assert!(session.state.runtime_input_grab.is_some());
    take(&SEEN);

    button(&mut session, ButtonState::Pressed);
    tap_key(&mut session, ESCAPE);
    assert!(session.state.runtime_input_grab.is_none());
    button(&mut session, ButtonState::Released);
    let seen = take(&SEEN);
    assert!(
        !seen
            .iter()
            .any(|seen| matches!(seen, Seen::Up(_) | Seen::Down(_))),
        "{seen:?}"
    );
    assert_eq!(GRAB_BUTTONS.get(), 1);

    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    assert_eq!(buttons(), [Seen::Down(""), Seen::Up("")]);
}

#[test]
fn the_keyboard_focus_waits_for_an_input_grab_to_end() {
    let (mut session, window, _pointer) = window_session();
    let (layer, _on_demand) = session.map_keyboard_layer("panel", KeyboardInteractivity::OnDemand);
    COMPOSITOR.window.list()[0].focus();
    session.roundtrip();
    take(&KEYBOARD);

    // A click on an on-demand layer under the grab.
    tap_key(&mut session, F2);
    place(&mut session, 500.0, 20.0);
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    assert!(
        take(&SEEN).contains(&Seen::Down("")),
        "the layer did not get its click"
    );
    assert_eq!(take(&KEYBOARD), Vec::<ObjectId>::new());
    tap_key(&mut session, ESCAPE);
    assert_eq!(take(&KEYBOARD), [layer.id()]);

    // An exclusive layer mapped under the grab.
    COMPOSITOR.window.list()[0].focus();
    session.roundtrip();
    assert_eq!(take(&KEYBOARD), [window.id()]);
    tap_key(&mut session, F2);
    let (exclusive, _layer) =
        session.map_keyboard_layer("prompt", KeyboardInteractivity::Exclusive);
    assert_eq!(take(&KEYBOARD), Vec::<ObjectId>::new());
    tap_key(&mut session, ESCAPE);
    assert_eq!(take(&KEYBOARD), [exclusive.id()]);
}

fn cursor_is_wait(session: &common::Session) -> bool {
    matches!(
        session.state.cursor_status,
        CursorImageStatus::Named(CursorIcon::Wait)
    )
}

fn cursor_is_a_surface(session: &common::Session) -> bool {
    matches!(session.state.cursor_status, CursorImageStatus::Surface(_))
}

/// Two clients: the session's own with a window, and the locker. The window's
/// client asks for a menu grab under the lock, sets the cursor while it would
/// hold it, destroys the menu, and the user moves and types.
#[test]
fn a_desktop_client_takes_no_popup_grab_under_the_lock() {
    let (mut session, desktop) = desktop_session(setup);
    let qh = session.queue.handle();
    let shapes: wp_cursor_shape_manager_v1::WpCursorShapeManagerV1 =
        session.bind("wp_cursor_shape_manager_v1", 1);
    let shape = shapes.get_pointer(&desktop.pointer, &qh, ());
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    place(&mut session, 160.0, 160.0);
    take(&SEEN);
    take(&KEYBOARD);
    take(&KEYS);

    let positioner = desktop.shell.create_positioner(&qh, ());
    positioner.set_size(50, 50);
    positioner.set_anchor_rect(0, 0, 10, 10);
    let menu = desktop.compositor.create_surface(&qh, ());
    let menu_xdg = desktop.shell.get_xdg_surface(&menu, &qh, ());
    let popup = menu_xdg.get_popup(Some(&desktop.xdg), &positioner, &qh, ());
    popup.grab(&desktop.seat, DESKTOP_ENTER.get());
    menu.commit();
    session.roundtrip();
    session.attach_buffer(&menu, 50, 50);

    shape.set_shape(DESKTOP_ENTER.get(), wp_cursor_shape_device_v1::Shape::Wait);
    session.roundtrip();
    let shaped = cursor_is_wait(&session);
    let cursor = desktop.compositor.create_surface(&qh, ());
    desktop
        .pointer
        .set_cursor(DESKTOP_ENTER.get(), Some(&cursor), 0, 0);
    session.roundtrip();
    let surfaced = cursor_is_a_surface(&session);

    popup.destroy();
    menu_xdg.destroy();
    menu.destroy();
    session.roundtrip();
    // What the event loop does after each dispatch.
    session.state.cleanup_popups_with_debug("test");
    nudge(&mut session);
    tap_key(&mut session, K);
    assert_eq!(take(&KEYBOARD), Vec::<ObjectId>::new());
    assert_eq!(take(&KEYS), Vec::<u32>::new());
    assert_eq!(window_seen(), []);
    assert!(!shaped, "the window's client set the cursor shape");
    assert!(!surfaced, "the window's client set a cursor surface");
    assert_eq!(POPUP_DONE.get(), 1, "the menu was not dismissed");
    unlock(&mut session, locker);
}

/// Smithay starts a drag on any serial a pointer grab has, whoever asks.
#[test]
fn a_desktop_client_cannot_start_a_drag_on_the_lockers_click() {
    let (mut session, desktop) = desktop_session(setup);
    let qh = session.queue.handle();
    let manager: wl_data_device_manager::WlDataDeviceManager =
        session.bind("wl_data_device_manager", 3);
    let data_device = manager.get_data_device(&desktop.seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    source.offer("text/plain".into());
    source.set_actions(wl_data_device_manager::DndAction::Copy);
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    place(&mut session, 160.0, 160.0);
    button(&mut session, ButtonState::Pressed);
    assert!(take(&SEEN).ends_with(&[Seen::Down("locker")]));

    data_device.start_drag(Some(&source), &desktop.surface, None, BUTTON_SERIAL.get());
    session.roundtrip();
    nudge(&mut session);
    button(&mut session, ButtonState::Released);
    assert_eq!(take(&SOURCE), ["cancelled"]);
    assert_eq!(of_locker(&take(&SEEN)), [Seen::Up("locker")]);
    unlock(&mut session, locker);
}

#[test]
fn unlocking_lets_go_of_a_button_held_on_the_lock_surface() {
    let (mut session, _window, _pointer) = window_session();
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    place(&mut session, 160.0, 160.0);
    button(&mut session, ButtonState::Pressed);
    assert!(take(&SEEN).ends_with(&[Seen::Down("locker")]));

    let lock_surface = locker.lock_surface.clone();
    let surface = locker.surface.clone();
    unlock(&mut session, locker);
    lock_surface.destroy();
    surface.destroy();
    session.roundtrip();
    let at_unlock = take(&SEEN);

    place(&mut session, 170.0, 170.0);
    button(&mut session, ButtonState::Released);
    let seen = take(&SEEN);
    assert!(
        !seen
            .iter()
            .any(|seen| matches!(seen, Seen::Up(_) | Seen::Down(_))),
        "{seen:?}"
    );
    assert_eq!(
        of_locker(&at_unlock),
        [Seen::Up("locker"), Seen::Leave("locker")],
        "{at_unlock:?}"
    );
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    assert_eq!(buttons(), [Seen::Down(""), Seen::Up("")]);
}

#[test]
fn a_lock_surface_destroyed_under_the_lock_leaves_the_pointer_nowhere() {
    let (mut session, _window, _pointer) = window_session();
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    place(&mut session, 160.0, 160.0);
    button(&mut session, ButtonState::Pressed);
    take(&SEEN);

    locker.lock_surface.destroy();
    locker.surface.destroy();
    session.roundtrip();
    assert!(session.state.session_lock_active);
    nudge(&mut session);
    button(&mut session, ButtonState::Released);
    scroll(&mut session);
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    tap_key(&mut session, K);
    assert_eq!(window_seen(), []);
    assert_eq!(take(&KEYBOARD), Vec::<ObjectId>::new());
    assert_eq!(take(&KEYS), Vec::<u32>::new());
}

#[test]
fn with_two_outputs_the_one_without_a_lock_surface_reaches_nothing() {
    let (mut session, _desktop) = desktop_session(setup_two_outputs);
    let outputs = session.client.globals_of("wl_output");
    let headless: wl_output::WlOutput = session.bind_global(outputs[1], 1);
    let layer = session.map_layer_on("bar", Some(&headless));
    session.attach_buffer(&layer, 800, 40);
    let geometry = session
        .state
        .space
        .outputs()
        .find(|output| output.name() == "HEADLESS-1")
        .and_then(|output| session.state.space.output_geometry(output))
        .unwrap();
    let (x, y) = (geometry.loc.x as f64 + 100.0, geometry.loc.y as f64 + 20.0);
    place(&mut session, x, y);
    let seen = take(&SEEN);
    assert!(
        seen.contains(&Seen::Enter("")),
        "the bar is not under {x},{y}: {seen:?}"
    );

    // The locker's one lock surface is on the first output.
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    take(&SEEN);
    place(&mut session, x + 10.0, y);
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    scroll(&mut session);
    assert_eq!(take(&SEEN), []);

    place(&mut session, 160.0, 160.0);
    let seen = take(&SEEN);
    assert_eq!(seen.first(), Some(&Seen::Enter("locker")), "{seen:?}");
    unlock(&mut session, locker);
}

#[test]
fn a_pointer_lock_asked_for_under_the_lock_does_not_engage() {
    let (mut session, window, pointer) = window_session();
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    place(&mut session, 160.0, 160.0);
    take(&SEEN);

    let qh = session.queue.handle();
    let constraints: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1 =
        session.bind("zwp_pointer_constraints_v1", 1);
    constraints.lock_pointer(
        &window,
        &pointer,
        None,
        zwp_pointer_constraints_v1::Lifetime::Persistent,
        &qh,
        (),
    );
    let relative: zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1 =
        session.bind("zwp_relative_pointer_manager_v1", 1);
    relative.get_relative_pointer(&pointer, &qh, ());
    session.roundtrip();
    nudge(&mut session);
    nudge(&mut session);
    assert_eq!(take(&CONSTRAINT), Vec::<&str>::new());
    assert_eq!(window_seen(), []);
    unlock(&mut session, locker);
}

#[test]
fn under_the_lock_only_the_locker_sets_the_cursor() {
    let (mut session, desktop) = desktop_session(setup);
    let qh = session.queue.handle();
    let shapes: wp_cursor_shape_manager_v1::WpCursorShapeManagerV1 =
        session.bind("wp_cursor_shape_manager_v1", 1);
    let shape = shapes.get_pointer(&desktop.pointer, &qh, ());
    let cursor = desktop.compositor.create_surface(&qh, ());
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    place(&mut session, 160.0, 160.0);
    take(&SEEN);

    for serial in [DESKTOP_ENTER.get(), LOCKER_ENTER.get()] {
        shape.set_shape(serial, wp_cursor_shape_device_v1::Shape::Wait);
        desktop.pointer.set_cursor(serial, Some(&cursor), 0, 0);
        session.roundtrip();
        assert!(!cursor_is_wait(&session));
        assert!(!cursor_is_a_surface(&session));
    }

    let client = &session.peers[locker.peer];
    let shapes: wp_cursor_shape_manager_v1::WpCursorShapeManagerV1 =
        client.bind("wp_cursor_shape_manager_v1", 1);
    let shape = shapes.get_pointer(&locker.pointer, &client.queue.handle(), ());
    shape.set_shape(LOCKER_ENTER.get(), wp_cursor_shape_device_v1::Shape::Wait);
    session.roundtrip();
    assert!(
        cursor_is_wait(&session),
        "the locker could not set the cursor"
    );
    unlock(&mut session, locker);
}

#[test]
fn a_top_layer_holding_a_button_keeps_it_under_an_input_grab() {
    let (mut session, _window, _pointer) = window_session();
    session.map_keyboard_layer("panel", KeyboardInteractivity::None);
    place(&mut session, 500.0, 20.0);
    button(&mut session, ButtonState::Pressed);
    assert!(take(&SEEN).ends_with(&[Seen::Down("")]));

    tap_key(&mut session, F2);
    assert!(session.state.runtime_input_grab.is_some());
    nudge(&mut session);
    button(&mut session, ButtonState::Released);
    let seen = take(&SEEN);
    assert!(seen.contains(&Seen::Motion("")), "{seen:?}");
    assert_eq!(buttons_in(&seen), [Seen::Up("")]);
    assert_eq!(GRAB_BUTTONS.get(), 0, "the grab heard the layer's release");
    tap_key(&mut session, ESCAPE);
}

fn buttons_in(seen: &[Seen]) -> Vec<Seen> {
    seen.iter()
        .copied()
        .filter(|seen| matches!(seen, Seen::Down(_) | Seen::Up(_)))
        .collect()
}

#[test]
fn a_button_held_across_a_vt_switch_comes_up_once() {
    let (mut session, _window, _pointer) = window_session();
    button(&mut session, ButtonState::Pressed);
    assert!(take(&SEEN).ends_with(&[Seen::Down("")]));

    session.state.pause_input_session();
    session.roundtrip();
    assert_eq!(take(&SEEN), [Seen::Up(""), Seen::Leave("")]);
    // Released on the other VT: the release never comes.
    session.state.resume_input_session();
    place(&mut session, 170.0, 170.0);
    button(&mut session, ButtonState::Pressed);
    button(&mut session, ButtonState::Released);
    assert_eq!(buttons(), [Seen::Down(""), Seen::Up("")]);

    // Still held when the session is back: its release goes nowhere.
    button(&mut session, ButtonState::Pressed);
    session.state.pause_input_session();
    session.roundtrip();
    session.state.resume_input_session();
    place(&mut session, 160.0, 160.0);
    take(&SEEN);
    button(&mut session, ButtonState::Released);
    assert_eq!(buttons(), []);
}

/// A window that asks to be activated under the lock: the config hears
/// nothing, and nothing raises it or hands it the keyboard at unlock.
#[test]
fn an_activation_under_the_lock_is_refused() {
    let (mut session, desktop) = desktop_session(setup);
    let qh = session.queue.handle();
    let activation: xdg_activation_v1::XdgActivationV1 = session.bind("xdg_activation_v1", 1);
    let activate = |session: &mut common::Session| {
        let token = activation.get_activation_token(&qh, ());
        token.set_serial(DESKTOP_ENTER.get(), &desktop.seat);
        token.set_surface(&desktop.surface);
        token.commit();
        session.roundtrip();
        let token = TOKEN.with(|token| token.borrow_mut().take()).unwrap();
        activation.activate(token, &desktop.surface);
        session.roundtrip();
    };
    activate(&mut session);
    assert_eq!(ACTIVATIONS.get(), 1);

    let locker = lock(&mut session);
    activate(&mut session);
    assert_eq!(
        ACTIVATIONS.get(),
        1,
        "the config heard an activation under the lock"
    );
    unlock(&mut session, locker);
}

/// A panel mapped behind the lock screen asks for the keyboard on demand:
/// the unlock puts the user back in the window they were in.
#[test]
fn an_on_demand_layer_mapped_under_the_lock_does_not_take_the_keyboard_at_unlock() {
    let (mut session, window, _pointer) = window_session();
    let locker = lock(&mut session);
    map_lock_surface(&mut session, &locker, None);
    session.map_keyboard_layer("panel", KeyboardInteractivity::OnDemand);
    take(&KEYBOARD);
    unlock(&mut session, locker);
    assert_eq!(take(&KEYBOARD), [window.id()]);
}

fn push(seen: Seen) {
    SEEN.with(|events| events.borrow_mut().push(seen));
}

impl Dispatch<wl_pointer::WlPointer, ()> for common::Client {
    fn event(
        client: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let name = client.name;
        match event {
            wl_pointer::Event::Enter { serial, .. } => {
                if name.is_empty() {
                    DESKTOP_ENTER.set(serial);
                } else {
                    LOCKER_ENTER.set(serial);
                }
                push(Seen::Enter(name));
            }
            wl_pointer::Event::Leave { .. } => push(Seen::Leave(name)),
            wl_pointer::Event::Motion { .. } => push(Seen::Motion(name)),
            wl_pointer::Event::Button { serial, state, .. } => {
                BUTTON_SERIAL.set(serial);
                push(match state {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => Seen::Down(name),
                    _ => Seen::Up(name),
                });
            }
            wl_pointer::Event::Axis { .. } => push(Seen::Scroll(name)),
            _ => {}
        }
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
        if !client.name.is_empty() {
            return;
        }
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                KEYBOARD.with(|entered| entered.borrow_mut().push(surface.id()));
            }
            wl_keyboard::Event::Key { key, .. } => KEYS.with(|keys| keys.borrow_mut().push(key)),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let name = match event {
            wl_data_source::Event::Cancelled => "cancelled",
            wl_data_source::Event::DndDropPerformed => "dropped",
            _ => return,
        };
        SOURCE.with(|events| events.borrow_mut().push(name));
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &wl_data_device::WlDataDevice,
        _: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    wayland_client::event_created_child!(common::Client, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
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
        CONSTRAINT.with(|events| events.borrow_mut().push(name));
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for common::Client {
    fn event(
        client: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { .. } = event {
            push(Seen::Relative(client.name));
        }
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

impl Dispatch<xdg_popup::XdgPopup, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_popup::Event::PopupDone = event {
            POPUP_DONE.set(POPUP_DONE.get() + 1);
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            TOKEN.with(|current| *current.borrow_mut() = Some(token));
        }
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for common::Client {
            fn event(_: &mut Self, _: &$proxy, _: <$proxy as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    wl_region::WlRegion,
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::WlDataOffer,
    zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_v1::ExtSessionLockV1,
    xdg_positioner::XdgPositioner,
    xdg_activation_v1::XdgActivationV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1
);
