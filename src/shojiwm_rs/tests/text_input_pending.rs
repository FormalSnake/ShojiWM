//! A text input's state requests wait for its `commit` in the compositor,
//! the last of each kind only: a client that sets state over and over and
//! never commits cannot grow the compositor's memory.

mod common;

use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_seat};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};

/// The process's resident memory in kB. The library sets the allocator, so
/// the test reads what the kernel sees; this binary holds one test so that
/// nothing else runs beside it.
fn resident_kb() -> i64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

/// A client with no focus sets its surrounding text five thousand times,
/// 2 kB each, and never commits.
#[test]
fn a_text_input_that_never_commits_holds_its_last_state_only() {
    let mut session = common::Session::new(|| {});
    let other = session.connect("other");
    let peer = &session.peers[other];
    let seat: wl_seat::WlSeat = peer.bind("wl_seat", 1);
    let manager: zwp_text_input_manager_v3::ZwpTextInputManagerV3 =
        peer.bind("zwp_text_input_manager_v3", 1);
    let text_input = manager.get_text_input(&seat, &peer.queue.handle(), ());
    session.roundtrip();

    let text = "x".repeat(2000);
    let flood = |session: &mut common::Session| {
        for _ in 0..250 {
            for _ in 0..20 {
                text_input.set_surrounding_text(text.clone(), 0, 0);
            }
            session.roundtrip();
        }
    };
    // The first round holds the one request and warms every buffer.
    flood(&mut session);
    let before = resident_kb();
    flood(&mut session);
    let grown = resident_kb() - before;
    assert!(grown < 4096, "the process grew by {grown} kB");

    text_input.commit();
    session.roundtrip();
}

impl Dispatch<zwp_text_input_manager_v3::ZwpTextInputManagerV3, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        _: zwp_text_input_manager_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for common::Client {
    fn event(
        _: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        _: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
