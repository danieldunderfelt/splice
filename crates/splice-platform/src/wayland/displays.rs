//! Pre-consent display discovery using wl_output + xdg-output logical geometry.
//!
//! InputCapture GetZones remains authoritative once its session exists, but tying
//! initial MachineInfo to that permission creates a deadlock: a peer cannot drive
//! this machine to approve the prompt because it has no display to target.

use std::sync::Arc;

use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    output::{OutputHandler, OutputInfo, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use splice_proto::DisplayRect;
use wayland_client::{
    globals::registry_queue_init, protocol::wl_output, Connection, QueueHandle,
};

use super::WaylandShared;
use crate::{PlatformError, PlatformEvent, Result};

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    changed: bool,
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.changed = true;
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.changed = true;
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.changed = true;
    }
}

delegate_output!(State);
delegate_registry!(State);

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers! {
        OutputState,
    }
}

fn scale(info: &OutputInfo, logical: (i32, i32)) -> f64 {
    let Some(mode) = info.modes.iter().find(|mode| mode.current) else {
        return f64::from(info.scale_factor.max(1));
    };
    let (pw, ph) = (mode.dimensions.0.unsigned_abs() as f64, mode.dimensions.1.unsigned_abs() as f64);
    let (lw, lh) = (logical.0.unsigned_abs() as f64, logical.1.unsigned_abs() as f64);
    let direct = (pw / lw, ph / lh);
    let rotated = (pw / lh, ph / lw);
    let pair = if (direct.0 - direct.1).abs() <= (rotated.0 - rotated.1).abs() {
        direct
    } else {
        rotated
    };
    ((pair.0 + pair.1) / 2.0).max(0.01)
}

fn snapshot(state: &State) -> Result<Vec<DisplayRect>> {
    let mut displays = Vec::new();
    for output in state.output_state.outputs() {
        let info = state.output_state.info(&output).ok_or_else(|| {
            PlatformError::Unavailable("Wayland output has no completed metadata".into())
        })?;
        let position = info.logical_position.ok_or_else(|| {
            PlatformError::Unavailable(
                "compositor did not publish xdg-output logical_position".into(),
            )
        })?;
        let size = info.logical_size.ok_or_else(|| {
            PlatformError::Unavailable(
                "compositor did not publish xdg-output logical_size".into(),
            )
        })?;
        if size.0 <= 0 || size.1 <= 0 {
            return Err(PlatformError::Unavailable(format!(
                "compositor published invalid logical output size {}x{}",
                size.0, size.1
            )));
        }
        let display_scale = scale(&info, size);
        displays.push(DisplayRect {
            id: info.name.unwrap_or_else(|| format!("wl-output-{}", info.id)),
            x: position.0,
            y: position.1,
            w: size.0 as u32,
            h: size.1 as u32,
            scale: display_scale,
        });
    }
    if displays.is_empty() {
        return Err(PlatformError::Unavailable(
            "Wayland compositor published no outputs".into(),
        ));
    }
    displays.sort_by(|a, b| (&a.id, a.x, a.y).cmp(&(&b.id, b.x, b.y)));
    Ok(displays)
}

fn run(shared: Arc<WaylandShared>, ready: std::sync::mpsc::SyncSender<Result<Vec<DisplayRect>>>) {
    let initialized = (|| -> Result<_> {
        let conn = Connection::connect_to_env().map_err(|err| {
            PlatformError::Unavailable(format!("cannot connect to Wayland display: {err}"))
        })?;
        let (globals, mut queue) = registry_queue_init(&conn).map_err(|err| {
            PlatformError::Unavailable(format!("cannot initialize Wayland registry: {err}"))
        })?;
        let qh = queue.handle();
        let mut state = State {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            changed: false,
        };
        queue.roundtrip(&mut state).map_err(|err| {
            PlatformError::Unavailable(format!("cannot read Wayland outputs: {err}"))
        })?;
        let initial = snapshot(&state)?;
        Ok((queue, state, initial))
    })();

    let (mut queue, mut state, initial) = match initialized {
        Ok(parts) => parts,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    if ready.send(Ok(initial.clone())).is_err() {
        return;
    }
    tracing::info!(displays = ?initial, "Wayland display geometry discovered");
    let mut last = initial;
    loop {
        state.changed = false;
        if let Err(err) = queue.blocking_dispatch(&mut state) {
            tracing::warn!(error = %err, "Wayland output monitor stopped");
            return;
        }
        if state.changed {
            match snapshot(&state) {
                Ok(displays) if displays != last => {
                    last = displays.clone();
                    shared.emit(PlatformEvent::DisplaysChanged { displays });
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, "ignoring incomplete Wayland output update"),
            }
        }
    }
}

pub fn spawn(shared: Arc<WaylandShared>) -> Result<Vec<DisplayRect>> {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("splice-wayland-outputs".into())
        .spawn(move || run(shared, ready_tx))
        .map_err(|err| {
            PlatformError::Unavailable(format!("cannot start Wayland output monitor: {err}"))
        })?;
    ready_rx.recv().map_err(|_| {
        PlatformError::Unavailable("Wayland output monitor exited during startup".into())
    })?
}
