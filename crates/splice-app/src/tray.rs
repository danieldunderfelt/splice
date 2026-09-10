//! Tray: cfg split per OS.
//!
//! macOS: `tray-icon` (no default features) + its muda menu API, created on the main
//! thread inside the eframe creation callback; menu events are polled from the UI loop.
//! Linux: `ksni` StatusNotifierItem owned by the `splice service` process (service.rs),
//! so it outlives windows. If no StatusNotifierWatcher owns
//! `org.kde.StatusNotifierWatcher`, spawn fails and the window shows a hint instead.

#[cfg(not(target_os = "linux"))]
use parking_lot::Mutex;
use splice_core::ui_state::{UiConnection, UiMachine};
use splice_core::UiState;
use splice_proto::MachineId;
use std::sync::mpsc;
#[cfg(not(target_os = "linux"))]
use std::sync::Arc;

use crate::runtime::Controller;

/// Actions requested from outside the UI loop (tray menu, service), drained by the app
/// every frame.
#[derive(Clone, Debug)]
pub enum AppAction {
    Open,
    Files,
    Quit,
    ToggleMachine(MachineId),
    DisconnectAll,
}

pub struct Tray {
    #[cfg(not(target_os = "linux"))]
    hint: Arc<Mutex<Option<String>>>,
    #[cfg(target_os = "linux")]
    ctrl: Controller,
    #[cfg(target_os = "macos")]
    macos: Option<macos::MacTray>,
}

impl Tray {
    /// Create the platform tray. Never fails hard: problems become `hint()` text.
    #[cfg_attr(target_os = "linux", allow(unused_variables))]
    pub fn new(ctrl: &Controller, tx: mpsc::Sender<AppAction>, ctx: egui::Context) -> Self {
        #[cfg(target_os = "linux")]
        {
            Tray { ctrl: ctrl.clone() }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let hint = Arc::new(Mutex::new(None));
            #[cfg(target_os = "macos")]
            let macos = match macos::MacTray::new(ctrl, tx.clone(), ctx) {
                Ok(tray) => Some(tray),
                Err(err) => {
                    tracing::warn!("tray unavailable: {err}");
                    *hint.lock() = Some(format!("menu bar icon unavailable: {err}"));
                    None
                }
            };
            #[cfg(not(target_os = "macos"))]
            {
                let _ = tx;
                *hint.lock() = Some("system tray unsupported on this OS".into());
            }
            Tray {
                hint,
                #[cfg(target_os = "macos")]
                macos,
            }
        }
    }

    /// A user-visible tray problem (rendered in the side panel), if any.
    pub fn hint(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            self.ctrl.tray_hint()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.hint.lock().clone()
        }
    }

    /// Reflect the latest state in the tray menu/icon. Cheap when nothing changed.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    pub fn sync(&self, state: &UiState) {
        #[cfg(target_os = "macos")]
        if let Some(macos) = &self.macos {
            if let Err(error) = macos.sync(state) {
                tracing::warn!(%error, "cannot update menu bar items");
                *self.hint.lock() = Some(error);
            }
        }
    }
}

/// Tray menu label for one machine: hostname plus a compact connection suffix.
fn machine_menu_label(machine: &UiMachine) -> String {
    let suffix = match &machine.connection {
        UiConnection::SelfMachine => return machine.hostname.clone(),
        UiConnection::Direct { rtt_ms } => format!("{rtt_ms:.1} ms"),
        UiConnection::Derp { rtt_ms } => format!("{rtt_ms:.1} ms (relay)"),
        UiConnection::Connecting => "connecting…".into(),
        UiConnection::Offline => "offline".into(),
    };
    format!("{} — {suffix}", machine.hostname)
}

/// Generated tray icon (no asset files): accent rounded square with two overlapping
/// white "display" rectangles. Returns RGBA8.
fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];

    let rounded = |x: f32, y: f32, min: (f32, f32), max: (f32, f32), r: f32| -> f32 {
        let cx = (min.0 + max.0) / 2.0;
        let cy = (min.1 + max.1) / 2.0;
        let bx = (max.0 - min.0) / 2.0 - r;
        let by = (max.1 - min.1) / 2.0 - r;
        let qx = (x - cx).abs() - bx;
        let qy = (y - cy).abs() - by;
        let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
        (0.5 - d).clamp(0.0, 1.0)
    };

    for y in 0..size {
        for x in 0..size {
            let xf = x as f32 + 0.5;
            let yf = y as f32 + 0.5;
            let idx = ((y * size + x) * 4) as usize;

            let bg = rounded(xf, yf, (s * 0.05, s * 0.05), (s * 0.95, s * 0.95), s * 0.24);
            let (mut r, mut g, mut b, a) = (0x5B as f32, 0x8D as f32, 0xEF as f32, bg);

            for (min, max) in [((0.17, 0.40), (0.50, 0.80)), ((0.50, 0.20), (0.83, 0.60))] {
                let cov = rounded(
                    xf,
                    yf,
                    (s * min.0, s * min.1),
                    (s * max.0, s * max.1),
                    s * 0.06,
                ) * a;
                r = r + (255.0 - r) * cov;
                g = g + (255.0 - g) * cov;
                b = b + (255.0 - b) * cov;
            }

            out[idx] = r as u8;
            out[idx + 1] = g as u8;
            out[idx + 2] = b as u8;
            out[idx + 3] = (a * 255.0) as u8;
        }
    }
    out
}

#[cfg(target_os = "macos")]
pub fn set_activation_policy_accessory() {
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
    }
}

#[cfg(target_os = "macos")]
#[path = "tray/macos.rs"]
pub(crate) mod macos;

#[cfg(target_os = "linux")]
pub mod linux {
    use super::{icon_rgba, machine_menu_label, AppAction};
    use parking_lot::{Mutex, RwLock};
    use splice_core::UiState;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    pub struct LinuxTray {
        slot: Arc<Mutex<Option<ksni::Handle<SpliceTray>>>>,
        tokio: tokio::runtime::Handle,
        menu_sig: Mutex<u64>,
        available: Arc<AtomicBool>,
    }

    /// Spawn the StatusNotifierItem on the runtime. A missing StatusNotifierWatcher makes
    /// ksni's spawn fail; `available()` then stays false and windows show a hint.
    pub fn spawn(
        state: Arc<RwLock<UiState>>,
        actions: mpsc::UnboundedSender<AppAction>,
        tokio: tokio::runtime::Handle,
    ) -> LinuxTray {
        let tray = SpliceTray { state, actions };
        let slot: Arc<Mutex<Option<ksni::Handle<SpliceTray>>>> = Arc::new(Mutex::new(None));
        let available = Arc::new(AtomicBool::new(false));
        let task_slot = slot.clone();
        let task_available = available.clone();
        tokio.spawn(async move {
            use ksni::TrayMethods;
            match tray.spawn().await {
                Ok(handle) => {
                    *task_slot.lock() = Some(handle);
                    task_available.store(true, Ordering::Release);
                }
                Err(err) => {
                    tracing::info!("no status notifier host; running without a tray icon: {err}");
                }
            }
        });
        LinuxTray {
            slot,
            tokio,
            menu_sig: Mutex::new(0),
            available,
        }
    }

    impl LinuxTray {
        pub fn available(&self) -> bool {
            self.available.load(Ordering::Acquire)
        }

        /// Menus are generated from state on demand; nudge ksni to re-read them.
        pub fn sync(&self, state: &UiState) {
            let Some(handle) = self.slot.lock().clone() else {
                return;
            };
            let mut hasher = DefaultHasher::new();
            state.master_enabled.hash(&mut hasher);
            for machine in &state.machines {
                machine.id.0.hash(&mut hasher);
                machine.enabled.hash(&mut hasher);
                machine_menu_label(machine).hash(&mut hasher);
            }
            let sig = hasher.finish();
            let mut stored = self.menu_sig.lock();
            if *stored == sig {
                return;
            }
            *stored = sig;
            drop(stored);
            self.tokio.spawn(async move {
                handle.update(|_| ()).await;
            });
        }
    }

    struct SpliceTray {
        state: Arc<RwLock<UiState>>,
        actions: mpsc::UnboundedSender<AppAction>,
    }
    impl ksni::Tray for SpliceTray {
        fn id(&self) -> String {
            "splice".into()
        }

        fn title(&self) -> String {
            "Splice".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let rgba = icon_rgba(24);
            let mut data = Vec::with_capacity(rgba.len());
            for px in rgba.as_chunks::<4>().0 {
                // ARGB32, network byte order.
                data.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
            }
            vec![ksni::Icon {
                width: 24,
                height: 24,
                data,
            }]
        }

        fn menu(&self) -> Vec<ksni::menu::MenuItem<Self>> {
            use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
            let state = self.state.read().clone();
            let mut items: Vec<MenuItem<Self>> = vec![
                StandardItem {
                    label: "Open Splice".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.actions.send(AppAction::Open);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Open file shelf".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.actions.send(AppAction::Files);
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
            ];
            for machine in state.machines.iter().filter(|m| m.id != state.self_id) {
                let id = machine.id.clone();
                items.push(
                    CheckmarkItem {
                        label: machine_menu_label(machine),
                        checked: machine.enabled,
                        activate: Box::new(move |tray: &mut Self| {
                            let _ = tray.actions.send(AppAction::ToggleMachine(id.clone()));
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            items.extend([
                MenuItem::Separator,
                StandardItem {
                    label: "Disconnect all".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.actions.send(AppAction::DisconnectAll);
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit Splice".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.actions.send(AppAction::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]);
            items
        }
    }
}
