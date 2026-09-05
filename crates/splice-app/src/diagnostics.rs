use crate::runtime::Controller;
use splice_core::{Command, UiState};

pub fn panel(ui: &mut egui::Ui, state: &UiState, controller: &Controller) {
    egui::CollapsingHeader::new("Diagnostics").show(ui, |ui| {
        let build = &state.build;
        ui.label(format!("Splice {} · protocol {}", build.version, build.protocol));
        ui.label(egui::RichText::new(&build.commit).small().monospace());
        if build.dirty { ui.label("Built with uncommitted changes"); }
        ui.label(egui::RichText::new(&build.target).small().weak());
        for (id, peer) in &state.diagnostics.peers {
            let name = state.machines.iter().find(|m| &m.id == id).map(|m| m.hostname.as_str()).unwrap_or(&id.0);
            egui::CollapsingHeader::new(name).id_salt(&id.0).show(ui, |ui| {
                ui.label(format!("{:?}", peer.phase));
                if let Some(address) = peer.address { ui.label(address.to_string()); }
                if let Some(build) = &peer.build {
                    ui.label(format!("Splice {} · protocol {}", build.version, build.protocol));
                    ui.label(egui::RichText::new(&build.commit).small().monospace());
                    if build.dirty { ui.label("Built with uncommitted changes"); }
                }
                if let Some(last) = peer.last_heartbeat_ms {
                    ui.label(format!("Last heartbeat: {} s ago", splice_core::diagnostics::unix_ms().saturating_sub(last) / 1000));
                }
                ui.label(format!("{} connection attempts · {} disconnects", peer.attempts, peer.disconnects));
                if let Some(error) = &peer.last_error { ui.label(egui::RichText::new(format!("Last error: {error}")).color(crate::theme::WARN)); }
                let traffic = &peer.traffic;
                ui.label(format!("{} input frames sent", traffic.input_frames_sent));
                ui.label(format!("Longest input queue wait: {:.2} ms", traffic.max_input_queue_us as f64 / 1000.0));
                ui.label(format!("Longest socket write: {:.2} ms", traffic.max_write_us as f64 / 1000.0));
                ui.label(format!("Peak outgoing queue: {} frames", traffic.max_queue_depth));
            });
        }
        if ui.add_enabled(controller.is_live(), egui::Button::new("Save diagnostic report")).clicked() {
            controller.send(Command::ExportDiagnostics);
        }
        ui.label(egui::RichText::new("Includes machine names, addresses and health. No clipboard contents, typed keys or credentials.").small().weak());
        if let Some(path) = &state.diagnostics.export_path {
            ui.label("Saved report:");
            ui.add(egui::Label::new(path).selectable(true));
            if ui.small_button("Copy report path").clicked() { ui.ctx().copy_text(path.clone()); }
        }
        if let Some(error) = &state.diagnostics.export_error {
            ui.label(egui::RichText::new(error).color(crate::theme::ERR));
        }
    });
}
