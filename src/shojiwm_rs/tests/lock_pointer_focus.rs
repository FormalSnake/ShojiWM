//! A lock surface that maps under a still pointer takes it at once, so the
//! locker reads the next move as a motion, and the window under the pointer
//! takes it back as soon as the session unlocks.

mod common;

use std::cell::{Cell, RefCell};

use shojiwm_lib::virtual_input::{
    VirtualDevice, VirtualInput, VirtualMotion, VirtualMotionAbsolute,
};
use shojiwm_rs::prelude::*;
use smithay::backend::input::InputEvent;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_compositor, wl_output, wl_pointer, wl_seat},
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Enter(&'static str),
    Motion(&'static str),
    Leave(&'static str),
}

thread_local! {
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    static LOCK_SIZE: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

fn take() -> Vec<Seen> {
    SEEN.with(|seen| std::mem::take(&mut *seen.borrow_mut()))
}

fn setup() {
    COMPOSITOR.window.composition(|_| {
        ManagedWindow::new()
            .rect(Rect::new(0.0, 0.0, 300.0, 300.0))
            .child(ClientWindow::new())
    });
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

fn nudge(session: &mut common::Session, dx: f64, dy: f64) {
    session
        .state
        .process_input_event(InputEvent::<VirtualInput>::PointerMotion {
            event: VirtualMotion {
                device: device(),
                time: 1,
                dx,
                dy,
            },
        });
    session.roundtrip();
}

#[test]
fn a_lock_surface_takes_a_still_pointer_and_gives_it_back_on_unlock() {
    let mut session = common::Session::new(setup);
    session.map_toplevel_with_buffer("a", 300, 300);
    session.state.refresh_window_decorations().unwrap();
    session.roundtrip();
    let seat: wl_seat::WlSeat = session.bind("wl_seat", 1);
    seat.get_pointer(&session.queue.handle(), ());

    let locker = session.connect("locker");
    let peer = &session.peers[locker];
    let qh = peer.queue.handle();
    let locker_seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    locker_seat.get_pointer(&qh, ());
    let compositor: wl_compositor::WlCompositor = peer.bind("wl_compositor", 1);
    let output: wl_output::WlOutput = peer.bind("wl_output", 1);
    let manager: ext_session_lock_manager_v1::ExtSessionLockManagerV1 =
        peer.bind("ext_session_lock_manager_v1", 1);
    session.roundtrip();

    place(&mut session, 150.0, 150.0);
    assert!(
        matches!(take().last(), Some(Seen::Enter("") | Seen::Motion(""))),
        "the window does not have the pointer"
    );

    let lock = manager.lock(&qh, ());
    let surface = compositor.create_surface(&qh, ());
    lock.get_lock_surface(&surface, &output, &qh, ());
    session.roundtrip();
    assert!(session.state.session_lock_active);
    let (width, height) = LOCK_SIZE.get();
    session.peers[locker].attach_buffer(&surface, width as i32, height as i32);
    session.roundtrip();
    assert_eq!(
        take(),
        [Seen::Leave(""), Seen::Enter("locker")],
        "the lock surface mapped under the still pointer without taking it"
    );

    nudge(&mut session, 10.0, 0.0);
    assert_eq!(
        take(),
        [Seen::Motion("locker")],
        "a move after the lock is not the locker's motion"
    );

    lock.unlock_and_destroy();
    session.roundtrip();
    assert!(!session.state.session_lock_active);
    // Each client's events are read on its own queue, so the two arrive here
    // in no set order.
    let seen = take();
    assert!(
        seen.contains(&Seen::Leave("locker")) && seen.contains(&Seen::Enter("")),
        "the window under the still pointer did not take it back on unlock: {seen:?}"
    );
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
        let seen = match event {
            wl_pointer::Event::Enter { .. } => Seen::Enter(client.name),
            wl_pointer::Event::Motion { .. } => Seen::Motion(client.name),
            wl_pointer::Event::Leave { .. } => Seen::Leave(client.name),
            _ => return,
        };
        SEEN.with(|events| events.borrow_mut().push(seen));
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
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_v1::ExtSessionLockV1
);
