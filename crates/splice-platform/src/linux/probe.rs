//! What this session makes available, so the supervisor can resolve `Auto` preferences
//! and the UI can grey out impossible choices.

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Availability {
    /// zwlr_layer_shell_v1 + zwp_pointer_constraints_v1 + zwp_relative_pointer_manager_v1.
    pub overlay: bool,
    /// ext_data_control_manager_v1 or zwlr_data_control_manager_v1.
    pub data_control: bool,
    /// /dev/uinput opens read-write.
    pub uinput: bool,
    /// org.freedesktop.portal.InputCapture is implemented by a portal backend.
    pub portal_capture: bool,
    /// org.freedesktop.portal.RemoteDesktop is implemented by a portal backend.
    pub portal_inject: bool,
    /// Number of wl_output globals (COSMIC's absolute-pointer caveat is per output).
    pub outputs: usize,
}

pub const OVERLAY_GLOBALS: [&str; 3] = [
    "zwlr_layer_shell_v1",
    "zwp_pointer_constraints_v1",
    "zwp_relative_pointer_manager_v1",
];
pub const DATA_CONTROL_GLOBALS: [&str; 2] =
    ["ext_data_control_manager_v1", "zwlr_data_control_manager_v1"];

pub async fn run(conn: Option<&zbus::Connection>) -> Availability {
    let globals = tokio::task::spawn_blocking(wayland_globals)
        .await
        .unwrap_or_default();
    let (portal_capture, portal_inject) = match conn {
        Some(conn) => (
            portal_version(conn, "org.freedesktop.portal.InputCapture").await > 0,
            portal_version(conn, "org.freedesktop.portal.RemoteDesktop").await > 0,
        ),
        None => (false, false),
    };
    Availability {
        overlay: OVERLAY_GLOBALS.iter().all(|g| globals.iter().any(|have| have == g)),
        data_control: DATA_CONTROL_GLOBALS.iter().any(|g| globals.iter().any(|have| have == g)),
        uinput: uinput_accessible(),
        portal_capture,
        portal_inject,
        outputs: globals.iter().filter(|g| g.as_str() == "wl_output").count(),
    }
}

pub fn uinput_accessible() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/uinput")
        .is_ok()
}

async fn portal_version(conn: &zbus::Connection, iface: &'static str) -> u32 {
    match super::portal::proxy(conn, iface).await {
        Ok(proxy) => super::portal::version(&proxy).await,
        Err(_) => 0,
    }
}

struct Probe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn wayland_globals() -> Vec<String> {
    let Ok(conn) = Connection::connect_to_env() else {
        return Vec::new();
    };
    let Ok((globals, _queue)) = registry_queue_init::<Probe>(&conn) else {
        return Vec::new();
    };
    globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|global| global.interface)
        .collect()
}
