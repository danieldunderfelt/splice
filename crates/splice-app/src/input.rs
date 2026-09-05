use crate::runtime::Controller;
use egui::{RichText, Ui};
use splice_core::{
    engine::Command,
    input_settings::CrossingPolicy,
    ui_state::{UiFocus, UiState},
};
use splice_proto::{raw::InputMode, Os};

pub fn panel(ui: &mut Ui, state: &UiState, controller: &Controller) {
    ui.label(RichText::new("Input mode").size(15.5).strong());
    let source_mac = state
        .machines
        .iter()
        .any(|m| m.id == state.self_id && m.os == Os::Macos);
    if source_mac {
        let mut settings = state.input_settings.clone();
        let mut changed = false;
        let mut selected = None;
        for machine in state.machines.iter().filter(|m| m.id != state.self_id) {
            let mut mode = settings.mode(&machine.id);
            ui.label(&machine.hostname);
            egui::ComboBox::from_id_salt(("input-mode", &machine.id.0))
                .selected_text(match mode {
                    InputMode::Desktop => "Desktop",
                    InputMode::Raw => "Raw input",
                })
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut mode, InputMode::Desktop, "Desktop")
                        .changed();
                    ui.add_enabled_ui(machine.os == Os::Linux, |ui| {
                        changed |= ui
                            .selectable_value(&mut mode, InputMode::Raw, "Raw input")
                            .changed();
                    });
                });
            if mode != settings.mode(&machine.id) {
                settings.destinations.insert(machine.id.clone(), mode);
            }
            if ui
                .add_enabled(
                    controller.is_live(),
                    egui::Button::new(format!("Control {}", machine.hostname)),
                )
                .clicked()
            {
                selected = Some(machine.id.clone());
            }
        }
        ui.add_space(8.0);
        ui.label("Edge crossing");
        let mut kind = match settings.crossing {
            CrossingPolicy::Immediate => 0,
            CrossingPolicy::Dwell { .. } => 1,
            CrossingPolicy::Resistance { .. } => 2,
        };
        let before = kind;
        egui::ComboBox::from_id_salt("edge-crossing")
            .selected_text(["Immediate", "Dwell", "Resistance"][kind])
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut kind, 0, "Immediate");
                ui.selectable_value(&mut kind, 1, "Dwell");
                ui.selectable_value(&mut kind, 2, "Resistance");
            });
        if kind != before {
            changed = true;
            settings.crossing = match kind {
                1 => CrossingPolicy::Dwell { milliseconds: 250 },
                2 => CrossingPolicy::Resistance {
                    points: 80.0,
                    decay_per_second: 80.0,
                },
                _ => CrossingPolicy::Immediate,
            };
        }
        match &mut settings.crossing {
            CrossingPolicy::Immediate => {}
            CrossingPolicy::Dwell { milliseconds } => {
                changed |= ui
                    .add(egui::Slider::new(milliseconds, 50..=2000).suffix(" ms"))
                    .changed();
                ui.label(
                    RichText::new("Wait at the edge to cross. Moving away cancels.")
                        .small()
                        .weak(),
                );
            }
            CrossingPolicy::Resistance {
                points,
                decay_per_second,
            } => {
                changed |= ui
                    .add(egui::Slider::new(points, 5.0..=300.0).text("Resistance"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(decay_per_second, 0.0..=300.0).text("Relaxation"))
                    .changed();
                ui.label(RichText::new("Keep pushing outward to cross. Pausing relaxes the edge; pulling back cancels.").small().weak());
            }
        }
        changed |= ui
            .checkbox(&mut settings.focus_lock, "Stay on selected computer")
            .changed();
        ui.label(RichText::new("Ctrl+Alt+F12 switches computers in workspace order. The emergency chord returns control here.").small().weak());
        if settings
            .destinations
            .values()
            .any(|mode| *mode == InputMode::Raw)
        {
            ui.label(RichText::new("Raw input requires focus lock. It forwards physical mouse and keyboard input to Linux. Grant Input Monitoring access on this Mac.").small());
        }
        if changed {
            controller.send(Command::SetInputSettings(settings));
        }
        if let Some(target) = selected {
            controller.send(Command::SelectTarget(target));
        }
        if matches!(state.focus, UiFocus::Remote(_)) && ui.button("Return control here").clicked() {
            controller.send(Command::SelectTarget(state.self_id.clone()));
        }
    } else {
        if state.input_settings.crossing != CrossingPolicy::Immediate
            && ui.button("Use immediate crossing on Linux").clicked()
        {
            let mut settings = state.input_settings.clone();
            settings.crossing = CrossingPolicy::Immediate;
            controller.send(Command::SetInputSettings(settings));
        }
        ui.label(RichText::new("Desktop mode supports every direction. A Mac can send raw mouse and keyboard input to this Linux computer.").small().weak());
    }
    if let Some(crossing) = &state.crossing_progress {
        let name = state
            .machines
            .iter()
            .find(|m| m.id == crossing.to)
            .map(|m| m.hostname.as_str())
            .unwrap_or(&crossing.to.0);
        ui.add(egui::ProgressBar::new(crossing.progress).text(format!("Crossing to {name}")));
    }
    if let Some(target) = &state.preparing_input {
        let name = state
            .machines
            .iter()
            .find(|m| &m.id == target)
            .map(|m| m.hostname.as_str())
            .unwrap_or(&target.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Preparing input on {name}…"));
        });
    } else if state.raw_active {
        ui.label(RichText::new("Raw input active · focus locked").strong());
    }
    if let Some(error) = &state.input_error {
        ui.colored_label(crate::theme::ERR, error);
    }
}
