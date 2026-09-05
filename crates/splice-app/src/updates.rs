use crate::runtime::Controller;
use splice_core::{Command, UiState};
use splice_update::{control::Action, Phase};

pub fn panel(ui: &mut egui::Ui, state: &UiState, controller: &Controller) {
    egui::CollapsingHeader::new("Updates")
        .default_open(true)
        .show(ui, |ui| {
            if state.updates.is_empty() {
                ui.label("Update service is starting.");
                return;
            }
            for (id, update) in &state.updates {
                if update.state.is_none() && !state.machines.iter().any(|m| &m.id == id) {
                    continue;
                }
                let name = state
                    .machines
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.hostname.as_str())
                    .unwrap_or(&id.0);
                ui.label(
                    egui::RichText::new(if id == &state.self_id {
                        format!("{name} · this computer")
                    } else {
                        name.into()
                    })
                    .strong(),
                );
                if let Some(version) = &update.expected_version {
                    ui.spinner();
                    ui.label(format!("Waiting for Splice {version} to restart…"));
                }
                if let Some(error) = &update.error {
                    ui.label(egui::RichText::new(error).color(crate::theme::ERR));
                }
                let Some(status) = &update.state else {
                    continue;
                };
                ui.label(format!("Installed: {}", status.build.version));
                if let Some(message) = &status.message {
                    ui.label(message);
                }
                let mut action = None;
                ui.add_enabled_ui(
                    controller.is_live() && update.expected_version.is_none(),
                    |ui| match status.phase {
                        Phase::Idle | Phase::Current | Phase::Failed => {
                            if status.phase == Phase::Current {
                                ui.label("Up to date");
                            }
                            if ui.button("Check for updates").clicked() {
                                action = Some(Action::Check);
                            }
                        }
                        Phase::Available => {
                            if let Some(version) = &status.version {
                                if ui.button(format!("Download {version}")).clicked() {
                                    action = Some(Action::Prepare {
                                        version: version.clone(),
                                    });
                                }
                            }
                        }
                        Phase::Ready => {
                            if let Some(version) = &status.version {
                                ui.label(format!("Splice {version} is verified and ready."));
                                if ui
                                    .button("Install and restart this computer's Splice")
                                    .clicked()
                                {
                                    action = Some(Action::Install {
                                        version: version.clone(),
                                    });
                                }
                            }
                        }
                        Phase::Checking => {
                            ui.spinner();
                            ui.label("Checking signed releases…");
                        }
                        Phase::Downloading => {
                            if status.total > 0 {
                                ui.add(
                                    egui::ProgressBar::new(
                                        status.downloaded as f32 / status.total as f32,
                                    )
                                    .show_percentage(),
                                );
                            } else {
                                ui.spinner();
                            }
                            ui.label("Downloading and verifying…");
                        }
                        Phase::Restarting => {
                            ui.spinner();
                            ui.label("Restarting Splice…");
                        }
                        Phase::Unsupported => {}
                    },
                );
                if let Some(action) = action {
                    controller.send(Command::Update {
                        machine: id.clone(),
                        action,
                    });
                }
                ui.add_space(8.0);
            }
        });
}
