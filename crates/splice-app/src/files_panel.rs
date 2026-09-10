use crate::{runtime::{BootStatus, Controller}, theme};
use splice_core::files::{FileCommand, OfferState, TransferDirection, TransferState};
use splice_core::UiState;

pub fn panel(ui: &mut egui::Ui, state: &UiState, controller: &Controller) -> bool {
    let waiting = state
        .files
        .offers
        .iter()
        .filter(|offer| offer.recipient == state.self_id && offer.state == OfferState::Available)
        .count();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Files").size(15.5).strong());
        if waiting > 0 {
            ui.label(
                egui::RichText::new(waiting.to_string())
                    .color(theme::ACCENT)
                    .strong(),
            );
        }
    });
    ui.label(
        egui::RichText::new("Drop files into the shelf, then pick them up on another computer.")
            .small()
            .weak(),
    );
    let mut enabled = state.files.enabled;
    if ui.add_enabled(controller.status() == BootStatus::Online, egui::Checkbox::new(&mut enabled, "Share files")).changed() {
        controller.send(splice_core::Command::Files(FileCommand::SetEnabled(enabled)));
    }
    ui.add_space(4.0);
    let title = match waiting {
        0 => "Open file shelf".to_owned(),
        1 => "Open shelf · 1 offer waiting".to_owned(),
        count => format!("Open shelf · {count} offers waiting"),
    };
    let open = ui
        .add_enabled(
            controller.status() == BootStatus::Online,
            egui::Button::new(title).min_size(egui::vec2(ui.available_width(), 34.0)),
        )
        .clicked();
    for transfer in state
        .files
        .transfers
        .iter()
        .filter(|transfer| {
            matches!(
                transfer.state,
                TransferState::Preparing
                    | TransferState::Committed
                    | TransferState::Receiving
                    | TransferState::Verifying
            )
        })
        .take(2)
    {
        let peer = state
            .machines
            .iter()
            .find(|machine| machine.id == transfer.peer)
            .map(|machine| machine.hostname.as_str())
            .unwrap_or(&transfer.peer.0);
        let label = match transfer.state {
            TransferState::Preparing => "Preparing files".to_owned(),
            TransferState::Verifying => "Verifying files".to_owned(),
            _ => match transfer.direction {
                TransferDirection::Send => format!("Sending to {peer}"),
                TransferDirection::Receive => format!("Receiving from {peer}"),
            },
        };
        let progress = if transfer.total_bytes == 0 {
            0.0
        } else {
            (transfer.bytes as f64 / transfer.total_bytes as f64).clamp(0.0, 1.0) as f32
        };
        ui.add(egui::ProgressBar::new(progress).text(label));
    }
    if let Some(error) = &state.files.error {
        ui.label(egui::RichText::new(error).small().color(theme::ERR));
    }
    for issue in state.files.recovery_errors.iter().take(3) {
        ui.label(egui::RichText::new(&issue.message).small().color(theme::ERR));
    }
    if state.files.recovery_error_count > 3 {
        ui.label(egui::RichText::new(format!(
            "{} more stored transfer issues",
            state.files.recovery_error_count - 3,
        )).small().color(theme::ERR));
    }
    open
}
