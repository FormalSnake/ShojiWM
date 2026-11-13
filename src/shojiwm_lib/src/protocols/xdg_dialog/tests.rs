use std::{collections::HashMap, os::unix::net::UnixStream, sync::Arc};

use smithay::{
    utils::Serial,
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData, dialog::ToplevelDialogHint,
        },
    },
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    protocol::{wl_callback, wl_compositor, wl_registry, wl_surface},
};
use wayland_protocols::xdg::{
    dialog::v1::client::{xdg_dialog_v1::XdgDialogV1, xdg_wm_dialog_v1::XdgWmDialogV1},
    shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use wayland_server::{Client, Display, backend::ClientData, protocol as server};

use super::XdgDialogState;

struct TestClient {
    display: Display<Server>,
    server: Server,
    connection: Connection,
    queue: EventQueue<ClientState>,
    client: ClientState,
    compositor: wl_compositor::WlCompositor,
    shell: xdg_wm_base::XdgWmBase,
    dialogs: XdgWmDialogV1,
}

impl TestClient {
    fn new() -> Self {
        let mut display = Display::new().unwrap();
        let mut handle = display.handle();
        let mut server = Server {
            compositor: CompositorState::new::<Server>(&handle),
            shell: XdgShellState::new::<Server>(&handle),
            toplevels: Vec::new(),
        };
        XdgDialogState::new::<Server>(&handle);
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        client_socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        handle
            .insert_client(server_socket, Arc::new(ServerClient::default()))
            .unwrap();
        let connection = Connection::from_socket(client_socket).unwrap();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut client = ClientState::default();
        connection.display().sync(&qh, ());
        connection.flush().unwrap();
        display.dispatch_clients(&mut server).unwrap();
        display.flush_clients().unwrap();
        queue.prepare_read().unwrap().read().unwrap();
        queue.dispatch_pending(&mut client).unwrap();
        let compositor = registry.bind(client.globals["wl_compositor"], 1, &qh, ());
        let shell = registry.bind(client.globals["xdg_wm_base"], 1, &qh, ());
        let dialogs = registry.bind(client.globals["xdg_wm_dialog_v1"], 1, &qh, ());
        Self {
            display,
            server,
            connection,
            queue,
            client,
            compositor,
            shell,
            dialogs,
        }
    }

    fn sync(&mut self) {
        self.connection.display().sync(&self.queue.handle(), ());
        self.connection.flush().unwrap();
        self.display.dispatch_clients(&mut self.server).unwrap();
        self.display.flush_clients().unwrap();
        self.queue.prepare_read().unwrap().read().unwrap();
        self.queue.dispatch_pending(&mut self.client).unwrap();
    }

    fn hint(&self, index: usize) -> ToplevelDialogHint {
        with_states(self.server.toplevels[index].wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .dialog_hint
        })
    }
}

#[test]
fn a_dialog_goes_inert_with_its_toplevel() {
    let mut test = TestClient::new();
    let qh = test.queue.handle();
    let surface = test.compositor.create_surface(&qh, ());
    let xdg = test.shell.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    let dialog = test.dialogs.get_xdg_dialog(&toplevel, &qh, ());
    dialog.set_modal();
    test.sync();
    assert_eq!(test.hint(0), ToplevelDialogHint::Modal);

    // The same wl_surface takes a new xdg_toplevel while the old dialog
    // object lives on.
    toplevel.destroy();
    xdg.destroy();
    let xdg = test.shell.get_xdg_surface(&surface, &qh, ());
    let replacement = xdg.get_toplevel(&qh, ());
    test.sync();
    assert_eq!(test.hint(1), ToplevelDialogHint::Unknown);

    dialog.set_modal();
    test.sync();
    assert_eq!(
        test.hint(1),
        ToplevelDialogHint::Unknown,
        "the old dialog object reached the replacement toplevel"
    );
    dialog.destroy();
    test.sync();
    assert_eq!(test.hint(1), ToplevelDialogHint::Unknown);

    let dialog = test.dialogs.get_xdg_dialog(&replacement, &qh, ());
    test.sync();
    assert_eq!(test.hint(1), ToplevelDialogHint::Dialog);
    dialog.set_modal();
    test.sync();
    assert_eq!(test.hint(1), ToplevelDialogHint::Modal);
    dialog.destroy();
    test.sync();
    assert_eq!(test.hint(1), ToplevelDialogHint::Unknown);
}

struct Server {
    compositor: CompositorState,
    shell: XdgShellState,
    toplevels: Vec<ToplevelSurface>,
}

#[derive(Default)]
struct ServerClient(CompositorClientState);
impl ClientData for ServerClient {}

impl CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ServerClient>().unwrap().0
    }
    fn commit(&mut self, _: &server::wl_surface::WlSurface) {}
}

impl XdgShellHandler for Server {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.toplevels.push(surface);
    }
    fn new_popup(&mut self, _: PopupSurface, _: PositionerState) {}
    fn grab(&mut self, _: PopupSurface, _: server::wl_seat::WlSeat, _: Serial) {}
    fn reposition_request(&mut self, _: PopupSurface, _: PositionerState, _: u32) {}
}
smithay::delegate_dispatch2!(Server);
crate::delegate_xdg_dialog!(Server);

#[derive(Default)]
struct ClientState {
    globals: HashMap<String, u32>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            state.globals.insert(interface, name);
        }
    }
}
wayland_client::delegate_noop!(ClientState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ClientState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(ClientState: ignore wl_callback::WlCallback);
wayland_client::delegate_noop!(ClientState: ignore xdg_wm_base::XdgWmBase);
wayland_client::delegate_noop!(ClientState: ignore xdg_surface::XdgSurface);
wayland_client::delegate_noop!(ClientState: ignore xdg_toplevel::XdgToplevel);
wayland_client::delegate_noop!(ClientState: ignore XdgWmDialogV1);
wayland_client::delegate_noop!(ClientState: ignore XdgDialogV1);
