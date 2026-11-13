//! A connector whose EDID lands after it connected, on a fallback mode: the
//! scan's `Changed` reads it again, and the config sees the display's name
//! and lays it out by it.

mod common;

use shojiwm_lib::{color::drm_metadata::EdidIdentity, state::set_output_edid_identity};
use shojiwm_rs::prelude::*;

fn setup() {
    COMPOSITOR.output.configure(|context| {
        context
            .connected
            .iter()
            .map(|output| {
                let named = output.make.as_deref() == Some("ASUSTek COMPUTER INC")
                    && output.model.as_deref() == Some("PA278CGV");
                let scale = if named { 2.0 } else { 1.0 };
                (output.name.clone(), Some(OutputConfig::extend(scale)))
            })
            .collect()
    });
}

#[test]
fn an_edid_read_after_the_output_connected_names_it_for_the_config() {
    let mut session = common::Session::new(setup);
    let scale = |session: &common::Session| {
        session
            .state
            .runtime_output_configs
            .get("TEST-1")
            .and_then(|config| config.scale)
    };
    let settle = |session: &mut common::Session| {
        session.state.apply_runtime_display_configuration();
        session.state.notify_runtime_outputs_changed();
        for _ in 0..20 {
            session.roundtrip();
        }
    };
    settle(&mut session);
    assert_eq!(scale(&session), Some(1.0));

    let output = session.state.space.outputs().next().unwrap().clone();
    let identity = EdidIdentity {
        make: "ASUSTek COMPUTER INC".into(),
        model: "PA278CGV".into(),
        serial: "T1LMQS063802".into(),
        pnp_id: "AUS".into(),
    };
    assert!(set_output_edid_identity(&output, identity.clone()));
    assert!(
        !set_output_edid_identity(&output, identity),
        "read again, nothing changed"
    );
    settle(&mut session);

    let snapshot = &session.state.snapshot_outputs()["TEST-1"];
    assert_eq!(
        (
            snapshot.make.as_deref(),
            snapshot.model.as_deref(),
            snapshot.serial.as_deref()
        ),
        (
            Some("ASUSTek COMPUTER INC"),
            Some("PA278CGV"),
            Some("T1LMQS063802")
        )
    );
    assert_eq!(
        scale(&session),
        Some(2.0),
        "the config laid it out by its name"
    );
}
