//! `COMPOSITOR.layer.stacked()` names each layer once, however many outputs
//! there are: the core hands every output's evaluation all the layers.

mod common;

use shojiwm_rs::prelude::*;
use wayland_client::protocol::wl_output;

fn setup() {
    COMPOSITOR.output.create(HeadlessOutputOptions {
        width: 800,
        height: 600,
        ..Default::default()
    });
}

#[test]
fn a_layer_is_stacked_once_with_two_outputs() {
    let mut session = common::Session::new(setup);
    session.roundtrip();
    let outputs = session.client.globals_of("wl_output");
    let first: wl_output::WlOutput = session.bind_global(outputs[0], 1);
    let headless: wl_output::WlOutput = session.bind_global(outputs[1], 1);
    session.map_layer_on("bar", Some(&first));
    session.map_layer_on("wallpaper", Some(&headless));

    let stacked = |session: &mut common::Session| {
        for output in ["TEST-1", "HEADLESS-1", "TEST-1"] {
            session
                .state
                .refresh_layer_effects_for_output(output)
                .unwrap();
        }
        let mut names: Vec<(String, String)> = COMPOSITOR
            .layer
            .stacked()
            .into_iter()
            .map(|layer| (layer.output_name, layer.namespace.unwrap_or_default()))
            .collect();
        names.sort();
        names
    };
    let both = [
        ("HEADLESS-1".to_owned(), "wallpaper".to_owned()),
        ("TEST-1".to_owned(), "bar".to_owned()),
    ];
    assert_eq!(stacked(&mut session), both);

    // A layer more on one output is evaluated for both again.
    session.map_layer_on("osd", Some(&headless));
    let names = stacked(&mut session);
    assert_eq!(names.len(), 3, "{names:?}");
    assert_eq!(names[0].1, "osd");
}
