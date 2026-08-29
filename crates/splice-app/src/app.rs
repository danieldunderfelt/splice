//! eframe App: arrangement canvas (draggable machine cards, snapping, green/red edges),
//! side panel (sensitivity, clipboard, panic chord, health), header (master switch,
//! disconnect all). Renders purely from the latest UiState; mutates only via Commands.

use crate::runtime::{BootStatus, Controller};
use crate::theme;
use crate::tray::{Tray, TrayAction};
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Pos2, Rect,
    RichText, Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2, pos2, vec2,
};
use splice_core::ui_state::{UiConnection, UiMachine};
use splice_core::{Command, UiState};
use splice_proto::{DisplayRect, LayoutDoc, MachineId, Os, Vec2I};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CARD_PAD_X: f32 = 14.0;
const CARD_PAD_TOP: f32 = 46.0;
const CARD_PAD_BOTTOM: f32 = 34.0;
const SNAP_PX: f32 = 8.0;

pub struct SpliceApp {
    ctrl: Controller,
    tray: Tray,
    tray_actions: mpsc::Receiver<TrayAction>,
    drag: Option<Drag>,
    allow_close: bool,
    exit_at: Option<Instant>,
}

struct Drag {
    id: MachineId,
    /// Accumulated pointer movement, screen px.
    accum: Vec2,
}

impl SpliceApp {
    pub fn new(
        ctrl: Controller,
        tray: Tray,
        tray_actions: mpsc::Receiver<TrayAction>,
        exit_after: Option<f64>,
    ) -> Self {
        SpliceApp {
            ctrl,
            tray,
            tray_actions,
            drag: None,
            allow_close: false,
            exit_at: exit_after.map(|secs| Instant::now() + Duration::from_secs_f64(secs)),
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, state: &UiState) {
        let dark = ui.style().visuals.dark_mode;
        egui::Panel::top("header")
            .exact_size(54.0)
            .frame(egui::Frame::new().fill(theme::panel_fill(dark)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new("Splice").size(20.0).strong());
                    ui.add_space(6.0);
                    status_chip(ui, self.ctrl.status());

                    let live = self.ctrl.is_live();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(12.0);
                        let mut master = state.master_enabled;
                        if switch(ui, &mut master, live).changed() {
                            self.ctrl.send(Command::SetMasterEnabled(master));
                        }
                        ui.label(
                            RichText::new(if master { "Enabled" } else { "Disabled" })
                                .small()
                                .weak(),
                        );
                        ui.add_space(8.0);
                        let button = egui::Button::new(RichText::new("Disconnect all").color(theme::ERR));
                        if ui
                            .add_enabled(live, button)
                            .on_hover_text("Release all captured input everywhere (panic)")
                            .clicked()
                        {
                            self.ctrl.send(Command::Panic);
                        }
                    });
                });
            });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, state: &UiState) {
        let dark = ui.style().visuals.dark_mode;
        egui::Panel::right("side")
            .default_size(280.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::panel_fill(dark))
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(err) = &state.tailscale_error {
                            banner(ui, theme::ERR, &format!("Tailscale: {err}"));
                            ui.add_space(8.0);
                        }
                        if let Some(hint) = self.tray.hint() {
                            banner(ui, theme::WARN, &hint);
                            ui.add_space(8.0);
                        }

                        self.sensitivity_section(ui, state);
                        ui.add_space(14.0);
                        ui.separator();
                        ui.add_space(10.0);

                        self.clipboard_row(ui, state);
                        ui.add_space(14.0);
                        ui.separator();
                        ui.add_space(10.0);

                        panic_chord_section(ui, state);
                        ui.add_space(14.0);
                        ui.separator();
                        ui.add_space(10.0);

                        health_section(ui, state);
                        ui.add_space(8.0);
                    });
            });
    }

    fn sensitivity_section(&mut self, ui: &mut egui::Ui, state: &UiState) {
        ui.label(RichText::new("Link sensitivity").size(15.5).strong());
        ui.label(RichText::new("Pointer speed when crossing onto each machine.").small().weak());
        ui.add_space(6.0);

        let live = self.ctrl.is_live();
        let peers: Vec<&UiMachine> = state
            .machines
            .iter()
            .filter(|m| m.id != state.self_id)
            .collect();
        if peers.is_empty() {
            ui.label(RichText::new("No other machines yet.").weak());
            return;
        }
        for machine in peers {
            let link_key = LayoutDoc::link_key(&state.self_id, &machine.id);
            let mut factor = state.sensitivity.get(&link_key).copied().unwrap_or(1.0);
            ui.label(&machine.hostname);
            let slider = egui::Slider::new(&mut factor, 0.25..=4.0)
                .logarithmic(true)
                .fixed_decimals(2)
                .suffix("×");
            if ui.add_enabled(live, slider).changed() {
                self.ctrl.send(Command::SetSensitivity { link_key, factor });
            }
            ui.add_space(4.0);
        }
    }

    fn clipboard_row(&mut self, ui: &mut egui::Ui, state: &UiState) {
        let live = self.ctrl.is_live();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Clipboard sync").size(15.5).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut on = state.clipboard_sync;
                if switch(ui, &mut on, live).changed() {
                    self.ctrl.send(Command::SetClipboardSync(on));
                }
            });
        });
        ui.label(RichText::new("Share text and images between machines.").small().weak());
    }

    fn central(&mut self, ui: &mut egui::Ui, state: &UiState) {
        let dark = ui.style().visuals.dark_mode;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::canvas_fill(dark))
                    .inner_margin(Margin::same(10)),
            )
            .show(ui, |ui| {
                if let BootStatus::Offline(err) = self.ctrl.status() {
                    offline_banner(ui, &err, &self.ctrl);
                    ui.add_space(8.0);
                }
                self.canvas(ui, state);
            });
    }

    fn canvas(&mut self, ui: &mut egui::Ui, state: &UiState) {
        let dark = ui.visuals().dark_mode;
        let (bg_response, painter) = ui.allocate_painter(ui.available_size(), Sense::hover());
        let area = bg_response.rect;

        let bounds = content_bounds(state);
        let Some((min, max)) = bounds else {
            painter.text(
                area.center(),
                Align2::CENTER_CENTER,
                "No machines yet — Splice discovers tailnet peers automatically.",
                FontId::proportional(15.0),
                ui.visuals().weak_text_color(),
            );
            return;
        };

        let avail = area.shrink(36.0);
        let content_w = (max.x - min.x).max(1.0);
        let content_h = (max.y - min.y).max(1.0);
        let scale = (avail.width() / content_w)
            .min(avail.height() / content_h)
            .clamp(0.004, 0.5);
        let origin = avail.center()
            - vec2((min.x + max.x) / 2.0 * scale, (min.y + max.y) / 2.0 * scale);
        let to_screen = |cx: f32, cy: f32| origin + vec2(cx * scale, cy * scale);

        // Pass 1: interactions (drag bookkeeping needs the previous frame's rects).
        let mut toggles: Vec<(MachineId, bool)> = Vec::new();
        for machine in &state.machines {
            let card = card_rect(
                machine,
                self.drag
                    .as_ref()
                    .filter(|d| d.id == machine.id)
                    .map(|d| d.accum)
                    .unwrap_or(Vec2::ZERO),
                &to_screen,
                scale,
            );
            let response = ui.interact(
                card,
                ui.id().with(("card", &machine.id.0)),
                Sense::drag(),
            );
            if response.drag_started() {
                self.drag = Some(Drag {
                    id: machine.id.clone(),
                    accum: Vec2::ZERO,
                });
            }
            if response.dragged() {
                if let Some(drag) = &mut self.drag {
                    if drag.id == machine.id {
                        drag.accum += response.drag_delta();
                    }
                }
            }
            if response.drag_stopped() {
                if let Some(drag) = self.drag.take() {
                    if drag.id == machine.id {
                        self.commit_placement(state, machine, drag.accum, scale);
                    }
                }
            }

            let mut enabled = machine.enabled;
            let toggle_rect = Rect::from_min_size(
                pos2(card.right() - 10.0 - 38.0, card.bottom() - 8.0 - 22.0),
                vec2(38.0, 22.0),
            );
            ui.scope_builder(
                UiBuilder::new().max_rect(toggle_rect).layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    if switch(ui, &mut enabled, self.ctrl.is_live()).changed() {
                        toggles.push((machine.id.clone(), enabled));
                    }
                },
            );
        }
        for (id, enabled) in toggles {
            self.ctrl.send(Command::SetMachineEnabled(id, enabled));
        }

        // Pass 2: paint.
        for machine in &state.machines {
            let live_offset = self
                .drag
                .as_ref()
                .filter(|d| d.id == machine.id)
                .map(|d| d.accum)
                .unwrap_or(Vec2::ZERO);
            draw_card(
                &painter,
                machine,
                live_offset,
                &to_screen,
                scale,
                machine.id == state.self_id,
                dark,
            );
        }
        for edge in &state.edges {
            let color = if edge.crossable { theme::OK } else { theme::ERR };
            painter.line_segment(
                [
                    to_screen(edge.x1 as f32, edge.y1 as f32),
                    to_screen(edge.x2 as f32, edge.y2 as f32),
                ],
                Stroke::new(4.0, color),
            );
        }
    }

    fn commit_placement(&mut self, state: &UiState, machine: &UiMachine, delta_px: Vec2, scale: f32) {
        let proposed = Vec2I {
            x: machine.offset.x + (delta_px.x / scale).round() as i32,
            y: machine.offset.y + (delta_px.y / scale).round() as i32,
        };
        let others: Vec<(&[DisplayRect], Vec2I)> = state
            .machines
            .iter()
            .filter(|m| m.id != machine.id)
            .map(|m| (m.displays.as_slice(), m.offset))
            .collect();
        let tolerance = ((SNAP_PX / scale).ceil() as i32).max(1);
        let snapped = splice_core::layout::snap_offset(&machine.displays, proposed, &others, tolerance);
        if snapped != machine.offset {
            self.ctrl.send(Command::SetPlacement(machine.id.clone(), snapped));
        }
    }
}

impl eframe::App for SpliceApp {
    /// Non-drawing work: smoke-run deadline, close→hide, tray event pump. Also runs
    /// while the window is hidden, so the tray keeps working then too.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // SPLICE_UI_EXIT_AFTER (preview smoke runs): close cleanly after N seconds.
        if let Some(deadline) = self.exit_at {
            if Instant::now() >= deadline {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        // Window close = hide; the app keeps running. Tray Open re-shows, Quit exits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        self.tray.poll();
        self.tray.sync(&self.ctrl.state());
        while let Ok(action) = self.tray_actions.try_recv() {
            match action {
                TrayAction::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayAction::Quit => {
                    self.ctrl.send(Command::Panic);
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                TrayAction::DisconnectAll => self.ctrl.send(Command::Panic),
                TrayAction::ToggleMachine(id) => {
                    let current = self
                        .ctrl
                        .state()
                        .machines
                        .iter()
                        .find(|m| m.id == id)
                        .map(|m| m.enabled);
                    if let Some(enabled) = current {
                        self.ctrl.send(Command::SetMachineEnabled(id, !enabled));
                    }
                }
            }
        }

        // While hidden, egui runs no passes on its own; keep `logic` ticking at a
        // low rate so tray events and engine state changes are still pumped.
        if ctx.input(|i| i.viewport().visible()) == Some(false) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let state = self.ctrl.state();
        self.header(ui, &state);
        self.side_panel(ui, &state);
        self.central(ui, &state);
    }
}

/// Union of all machines' display rects in canvas coordinates.
fn content_bounds(state: &UiState) -> Option<(Pos2, Pos2)> {
    let mut min = pos2(f32::MAX, f32::MAX);
    let mut max = pos2(f32::MIN, f32::MIN);
    let mut any = false;
    for machine in &state.machines {
        for display in &machine.displays {
            if display.w == 0 || display.h == 0 {
                continue;
            }
            any = true;
            let x0 = (machine.offset.x + display.x) as f32;
            let y0 = (machine.offset.y + display.y) as f32;
            let x1 = x0 + display.w as f32;
            let y1 = y0 + display.h as f32;
            min = pos2(min.x.min(x0), min.y.min(y0));
            max = pos2(max.x.max(x1), max.y.max(y1));
        }
    }
    any.then_some((min, max))
}

/// Card rect (screen px): the machine's display union plus header/toggle margins.
fn card_rect(
    machine: &UiMachine,
    live_offset: Vec2,
    to_screen: &impl Fn(f32, f32) -> Pos2,
    scale: f32,
) -> Rect {
    let mut union = Rect::NOTHING;
    for display in &machine.displays {
        let top_left = to_screen(
            (machine.offset.x + display.x) as f32,
            (machine.offset.y + display.y) as f32,
        ) + live_offset;
        union = union.union(Rect::from_min_size(
            top_left,
            vec2(display.w as f32 * scale, display.h as f32 * scale),
        ));
    }
    if union == Rect::NOTHING {
        union = Rect::from_min_size(to_screen(machine.offset.x as f32, machine.offset.y as f32), vec2(40.0, 30.0));
    }
    Rect::from_min_max(
        union.min - vec2(CARD_PAD_X, CARD_PAD_TOP),
        union.max + vec2(CARD_PAD_X, CARD_PAD_BOTTOM),
    )
}

fn draw_card(
    painter: &egui::Painter,
    machine: &UiMachine,
    live_offset: Vec2,
    to_screen: &impl Fn(f32, f32) -> Pos2,
    scale: f32,
    is_self: bool,
    dark: bool,
) {
    let offline = matches!(machine.connection, UiConnection::Offline);
    let opacity = if offline { 0.4 } else { 1.0 };
    let shade = |color: Color32| {
        let color = if machine.enabled {
            color
        } else {
            theme::desaturate(color, 0.75)
        };
        theme::ghost(color, opacity)
    };

    let card = card_rect(machine, live_offset, to_screen, scale);
    let fill = shade(theme::card_fill(dark));
    painter.rect_filled(card, CornerRadius::same(12), fill);

    let border = if is_self {
        theme::ACCENT
    } else {
        theme::card_border(dark)
    };
    painter.rect_stroke(
        card,
        CornerRadius::same(12),
        Stroke::new(if is_self { 2.0 } else { 1.0 }, shade(border)),
        StrokeKind::Inside,
    );

    // Displays, drawn to scale inside the card.
    for display in &machine.displays {
        let top_left = to_screen(
            (machine.offset.x + display.x) as f32,
            (machine.offset.y + display.y) as f32,
        ) + live_offset;
        let rect = Rect::from_min_size(
            top_left,
            vec2(display.w as f32 * scale, display.h as f32 * scale),
        );
        painter.rect_filled(rect, CornerRadius::same(5), shade(theme::display_fill(dark)));
        painter.rect_stroke(
            rect,
            CornerRadius::same(5),
            Stroke::new(1.0, shade(theme::card_border(dark))),
            StrokeKind::Inside,
        );
    }

    let style = painter.ctx().style_of(painter.ctx().theme());
    let ink = style.visuals.strong_text_color();
    let weak = style.visuals.weak_text_color();

    // OS glyph + hostname.
    let glyph_rect = Rect::from_min_size(card.min + vec2(12.0, 10.0), vec2(18.0, 18.0));
    draw_os_glyph(painter, glyph_rect, machine.os, shade(ink), fill);
    painter.text(
        pos2(glyph_rect.right() + 8.0, card.top() + 11.0),
        Align2::LEFT_TOP,
        &machine.hostname,
        FontId::proportional(14.5),
        shade(ink),
    );

    // Connection badge.
    let badge_y = card.top() + 33.0;
    let dot_center = pos2(card.left() + 16.0, badge_y + 5.0);
    let text_pos = pos2(card.left() + 26.0, badge_y);
    match &machine.connection {
        UiConnection::SelfMachine => {
            painter.text(
                text_pos,
                Align2::LEFT_TOP,
                "this machine",
                FontId::proportional(11.5),
                shade(weak),
            );
        }
        UiConnection::Direct { rtt_ms } | UiConnection::Derp { rtt_ms } => {
            let derp = matches!(machine.connection, UiConnection::Derp { .. });
            let color = if derp { theme::WARN } else { theme::OK };
            painter.circle(dot_center, 4.0, shade(color), Stroke::NONE);
            let text = if derp {
                format!("{rtt_ms:.1} ms · relay")
            } else {
                format!("{rtt_ms:.1} ms")
            };
            painter.text(
                text_pos,
                Align2::LEFT_TOP,
                text,
                FontId::proportional(11.5),
                shade(if derp { color } else { weak }),
            );
        }
        UiConnection::Connecting => {
            painter.circle_stroke(dot_center, 4.0, Stroke::new(1.5, shade(weak)));
            painter.text(
                text_pos,
                Align2::LEFT_TOP,
                "connecting…",
                FontId::proportional(11.5),
                shade(weak),
            );
        }
        UiConnection::Offline => {
            painter.circle_stroke(dot_center, 4.0, Stroke::new(1.5, shade(weak)));
            painter.text(
                text_pos,
                Align2::LEFT_TOP,
                "offline",
                FontId::proportional(11.5),
                shade(weak),
            );
        }
    }

    // SOURCE chip.
    if machine.is_source {
        let galley = painter.layout_no_wrap(
            "SOURCE".into(),
            FontId::proportional(10.0),
            Color32::WHITE,
        );
        let chip = Rect::from_min_size(
            pos2(card.right() - 12.0 - galley.size().x - 14.0, card.top() + 11.0),
            galley.size() + vec2(14.0, 6.0),
        );
        painter.rect_filled(chip, CornerRadius::same(8), shade(theme::ACCENT));
        painter.galley(chip.min + vec2(7.0, 3.0), galley, Color32::WHITE);
    }
}

/// Simple vector OS glyphs (no icon assets).
fn draw_os_glyph(painter: &egui::Painter, rect: Rect, os: Os, ink: Color32, bg: Color32) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0;
    match os {
        Os::Macos => {
            painter.circle(c + vec2(0.0, r * 0.15), r * 0.78, ink, Stroke::NONE);
            painter.circle(c + vec2(r * 0.6, -r * 0.1), r * 0.42, bg, Stroke::NONE);
            painter.circle(c + vec2(r * 0.3, -r * 0.75), r * 0.3, ink, Stroke::NONE);
        }
        Os::Linux => {
            painter.circle(c + vec2(0.0, r * 0.1), r * 0.8, ink, Stroke::NONE);
            painter.circle(c + vec2(0.0, r * 0.35), r * 0.45, bg, Stroke::NONE);
            painter.circle(c + vec2(-r * 0.25, -r * 0.35), r * 0.13, bg, Stroke::NONE);
            painter.circle(c + vec2(r * 0.25, -r * 0.35), r * 0.13, bg, Stroke::NONE);
            painter.add(Shape::convex_polygon(
                vec![
                    c + vec2(-r * 0.15, -r * 0.12),
                    c + vec2(r * 0.15, -r * 0.12),
                    c + vec2(0.0, r * 0.08),
                ],
                theme::WARN,
                Stroke::NONE,
            ));
        }
        Os::Other => {
            let monitor = Rect::from_center_size(c + vec2(0.0, -r * 0.15), vec2(r * 1.5, r * 1.0));
            painter.rect_stroke(monitor, CornerRadius::same(2), Stroke::new(1.5, ink), StrokeKind::Inside);
            painter.line_segment(
                [c + vec2(0.0, monitor.bottom() - c.y + r * 0.15), c + vec2(0.0, r * 0.6)],
                Stroke::new(1.5, ink),
            );
            painter.line_segment(
                [c + vec2(-r * 0.4, r * 0.6), c + vec2(r * 0.4, r * 0.6)],
                Stroke::new(1.5, ink),
            );
        }
    }
}

/// Rounded toggle switch (custom-drawn; not a stock checkbox).
fn switch(ui: &mut egui::Ui, on: &mut bool, enabled: bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if response.clicked() && enabled {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let dark = ui.visuals().dark_mode;
        let mut t = ui.ctx().animate_bool_responsive(response.id, *on);
        if !enabled {
            t = 0.0;
        }
        let alpha = if enabled { 1.0 } else { 0.45 };
        let track_off = theme::mix(theme::display_fill(dark), theme::card_border(dark), 0.35);
        let track = theme::ghost(theme::mix(track_off, theme::ACCENT, t), alpha);
        ui.painter().rect_filled(rect, CornerRadius::same(11), track);
        let knob_x = rect.left() + 11.0 + t * (rect.width() - 22.0);
        let center = pos2(knob_x, rect.center().y);
        ui.painter().circle(
            center + vec2(0.0, 1.0),
            8.5,
            theme::ghost(Color32::from_black_alpha(40), alpha),
            Stroke::NONE,
        );
        ui.painter()
            .circle(center, 8.0, theme::ghost(Color32::WHITE, alpha), Stroke::NONE);
    }
    response
}

fn status_chip(ui: &mut egui::Ui, status: BootStatus) {
    let (color, text) = match status {
        BootStatus::Online => (theme::OK, "engine online"),
        BootStatus::Starting => (theme::WARN, "starting…"),
        BootStatus::Offline(_) => (theme::ERR, "engine offline"),
        BootStatus::Preview => (theme::ACCENT, "preview"),
    };
    let (rect, _) = ui.allocate_exact_size(vec2(6.0, 6.0), Sense::hover());
    ui.painter().circle(rect.center(), 3.0, color, Stroke::NONE);
    ui.label(RichText::new(text).small().weak());
}

fn banner(ui: &mut egui::Ui, color: Color32, text: &str) {
    let fill = theme::mix(ui.visuals().window_fill, color, 0.12);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .stroke(Stroke::new(1.0, theme::mix(ui.visuals().window_fill, color, 0.45)))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).color(color).size(12.5));
        });
}

fn offline_banner(ui: &mut egui::Ui, err: &str, ctrl: &Controller) {
    let color = theme::ERR;
    let fill = theme::mix(ui.visuals().window_fill, color, 0.12);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .stroke(Stroke::new(1.0, theme::mix(ui.visuals().window_fill, color, 0.45)))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("engine offline: {err}"))
                        .color(color)
                        .size(12.5),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Retry").clicked() {
                        ctrl.retry();
                    }
                });
            });
        });
}

fn panic_chord_section(ui: &mut egui::Ui, state: &UiState) {
    ui.label(RichText::new("Panic chord").size(15.5).strong());
    ui.label(RichText::new("Immediately releases all captured input.").small().weak());
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.label(RichText::new(&state.panic_chord).monospace());
        });
}

fn health_section(ui: &mut egui::Ui, state: &UiState) {
    ui.label(RichText::new("Health").size(15.5).strong());
    ui.add_space(4.0);

    let rows = health_rows(state);
    if rows.is_empty() {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle(rect.center(), 4.0, theme::OK, Stroke::NONE);
            ui.label(RichText::new("All systems nominal.").weak());
        });
        return;
    }
    for (title, detail, hint) in rows {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle(rect.center(), 4.0, theme::WARN, Stroke::NONE);
            ui.label(RichText::new(title).strong());
        });
        ui.label(RichText::new(detail).small().weak());
        ui.label(RichText::new(hint).small().color(theme::WARN));
        ui.add_space(6.0);
    }
}

/// (title, detail, fix-it hint) for every active health problem.
fn health_rows(state: &UiState) -> Vec<(String, String, String)> {
    let macos = cfg!(target_os = "macos");
    let mut rows = Vec::new();
    let health = &state.health;
    if let Some(detail) = &health.capture {
        let hint = if macos {
            "Grant Accessibility: System Settings → Privacy & Security → Accessibility → Splice."
        } else {
            "The InputCapture portal session failed; log out and back in, then restart Splice."
        };
        rows.push(("Input capture".into(), detail.clone(), hint.into()));
    }
    if let Some(detail) = &health.emulate {
        let hint = if macos {
            "Event posting needs Accessibility too — check the Splice toggle."
        } else {
            "Re-grant the RemoteDesktop portal permission when prompted."
        };
        rows.push(("Input injection".into(), detail.clone(), hint.into()));
    }
    if let Some(detail) = &health.secure_input {
        rows.push((
            "Secure Input active".into(),
            format!("Enabled by {detail}."),
            "Quit that app or leave its password field — input capture is suspended meanwhile."
                .into(),
        ));
    }
    if let Some(detail) = &health.activity_monitor {
        rows.push((
            "Physical-input monitor".into(),
            detail.clone(),
            "Add your user to the input group: sudo usermod -aG input $USER — then re-login."
                .into(),
        ));
    }
    if let Some(detail) = &health.clipboard {
        let hint = if macos {
            "Pasteboard access is degraded; relaunch Splice."
        } else {
            "Update xdg-desktop-portal and re-login to restore clipboard sync."
        };
        rows.push(("Clipboard sync".into(), detail.clone(), hint.into()));
    }
    rows
}
