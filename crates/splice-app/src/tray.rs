//! Tray: cfg split per OS.
//!
//! macOS: `tray-icon` (no default features) + its muda menu API, created on the main
//! thread inside the eframe creation callback; menu events are polled from the UI loop.
//! Linux: `ksni` StatusNotifierItem spawned on the background tokio runtime; if no
//! StatusNotifierWatcher owns `org.kde.StatusNotifierWatcher`, spawn fails and we surface
//! a UI hint instead of failing the app.

use crate::runtime::Controller;
use parking_lot::Mutex;
use splice_core::UiState;
use splice_core::ui_state::{UiConnection, UiMachine};
use splice_proto::MachineId;
use std::sync::{Arc, mpsc};

/// Actions requested from the tray menu, drained by the app every frame.
#[derive(Clone, Debug)]
pub enum TrayAction {
    Open,
    Quit,
    ToggleMachine(MachineId),
    DisconnectAll,
}

pub struct Tray {
    hint: Arc<Mutex<Option<String>>>,
    #[cfg(target_os = "macos")]
    macos: Option<macos::MacTray>,
    #[cfg(target_os = "linux")]
    linux: Option<linux::LinuxTray>,
}

impl Tray {
    /// Create the platform tray. Never fails hard: problems become `hint()` text.
    pub fn new(ctrl: &Controller) -> (Self, mpsc::Receiver<TrayAction>) {
        let (tx, rx) = mpsc::channel();
        let hint = Arc::new(Mutex::new(None));

        #[cfg(target_os = "macos")]
        let macos = match macos::MacTray::new(ctrl, tx.clone()) {
            Ok(tray) => Some(tray),
            Err(err) => {
                tracing::warn!("tray unavailable: {err}");
                *hint.lock() = Some(format!("menu bar icon unavailable: {err}"));
                None
            }
        };
        #[cfg(target_os = "linux")]
        let linux = linux::spawn(ctrl, tx.clone(), hint.clone());

        #[allow(unused_mut)]
        let mut tray = Tray {
            hint,
            #[cfg(target_os = "macos")]
            macos,
            #[cfg(target_os = "linux")]
            linux,
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = tx;
            *tray.hint.lock() = Some("system tray unsupported on this OS".into());
        }
        (tray, rx)
    }

    /// A user-visible tray problem (rendered in the side panel), if any.
    pub fn hint(&self) -> Option<String> {
        self.hint.lock().clone()
    }

    /// Drain pending native menu events into the action channel (macOS). Per-frame.
    pub fn poll(&self) {
        #[cfg(target_os = "macos")]
        if let Some(macos) = &self.macos {
            macos.poll();
        }
    }

    /// Reflect the latest state in the tray menu/icon. Cheap when nothing changed.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    pub fn sync(&self, state: &UiState) {
        #[cfg(target_os = "macos")]
        if let Some(macos) = &self.macos {
            macos.sync(state);
        }
        #[cfg(target_os = "linux")]
        if let Some(linux) = &self.linux {
            linux.sync(state);
        }
    }
}

/// Tray menu label for one machine: hostname plus a compact connection suffix.
#[cfg(any(target_os = "macos", target_os = "linux"))]
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
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

            for (min, max) in [
                ((0.17, 0.40), (0.50, 0.80)),
                ((0.50, 0.20), (0.83, 0.60)),
            ] {
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
mod macos {
    use super::{TrayAction, icon_rgba, machine_menu_label};
    use crate::runtime::Controller;
    use splice_core::UiState;
    use splice_proto::MachineId;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::{Mutex, mpsc};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};

    const OPEN_ID: &str = "splice.open";
    const DISCONNECT_ID: &str = "splice.disconnect";
    const QUIT_ID: &str = "splice.quit";
    const MACHINE_PREFIX: &str = "splice.machine.";

    pub struct MacTray {
        tray: tray_icon::TrayIcon,
        actions: mpsc::Sender<TrayAction>,
        menu_sig: Mutex<u64>,
    }

    impl MacTray {
        pub fn new(ctrl: &Controller, actions: mpsc::Sender<TrayAction>) -> Result<Self, String> {
            let icon = tray_icon::Icon::from_rgba(icon_rgba(64), 64, 64)
                .map_err(|err| format!("icon: {err}"))?;
            let tray = tray_icon::TrayIconBuilder::new()
                .with_menu(Box::new(build_menu(&ctrl.state())))
                .with_tooltip("Splice")
                .with_icon(icon)
                .with_menu_on_left_click(true)
                .build()
                .map_err(|err| format!("{err}"))?;
            Ok(MacTray {
                tray,
                actions,
                menu_sig: Mutex::new(0),
            })
        }

        /// Forward native menu events into the action channel.
        pub fn poll(&self) {
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let id = event.id().0.clone();
                let action = match id.as_str() {
                    OPEN_ID => Some(TrayAction::Open),
                    DISCONNECT_ID => Some(TrayAction::DisconnectAll),
                    QUIT_ID => Some(TrayAction::Quit),
                    _ => id
                        .strip_prefix(MACHINE_PREFIX)
                        .map(|mid| TrayAction::ToggleMachine(MachineId(mid.to_owned()))),
                };
                if let Some(action) = action {
                    let _ = self.actions.send(action);
                }
            }
        }

        /// Rebuild the menu when the machine set or labels changed.
        pub fn sync(&self, state: &UiState) {
            let mut hasher = DefaultHasher::new();
            state.master_enabled.hash(&mut hasher);
            for machine in &state.machines {
                machine.id.0.hash(&mut hasher);
                machine.enabled.hash(&mut hasher);
                machine_menu_label(machine).hash(&mut hasher);
            }
            let sig = hasher.finish();

            let mut stored = self.menu_sig.lock().unwrap_or_else(|e| e.into_inner());
            if *stored != sig {
                *stored = sig;
                drop(stored);
                self.tray.set_menu(Some(Box::new(build_menu(state))));
            }
        }
    }

    fn build_menu(state: &UiState) -> Menu {
        let menu = Menu::new();
        let _ = menu.append(&MenuItem::with_id(OPEN_ID, "Open Splice", true, None));
        let _ = menu.append(&PredefinedMenuItem::separator());
        for machine in state.machines.iter().filter(|m| m.id != state.self_id) {
            let _ = menu.append(&CheckMenuItem::with_id(
                format!("{MACHINE_PREFIX}{}", machine.id.0),
                machine_menu_label(machine),
                true,
                machine.enabled,
                None,
            ));
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            DISCONNECT_ID,
            "Disconnect all",
            true,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(QUIT_ID, "Quit Splice", true, None));
        menu
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{TrayAction, icon_rgba, machine_menu_label};
    use crate::runtime::Controller;
    use parking_lot::{Mutex, RwLock};
    use splice_core::UiState;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::{Arc, mpsc};

    pub struct LinuxTray {
        slot: Arc<Mutex<Option<ksni::Handle<SpliceTray>>>>,
        tokio: tokio::runtime::Handle,
        menu_sig: Mutex<u64>,
    }

    /// Spawn the StatusNotifierItem on the background runtime. A missing
    /// StatusNotifierWatcher makes ksni's spawn fail; that becomes a UI hint.
    pub fn spawn(
        ctrl: &Controller,
        actions: mpsc::Sender<TrayAction>,
        hint: Arc<Mutex<Option<String>>>,
    ) -> Option<LinuxTray> {
        let tokio = ctrl.tokio.clone()?;
        let tray = SpliceTray {
            state: ctrl.shared_state(),
            actions,
        };
        let slot: Arc<Mutex<Option<ksni::Handle<SpliceTray>>>> = Arc::new(Mutex::new(None));
        let task_slot = slot.clone();
        tokio.spawn(async move {
            use ksni::TrayMethods;
            match tray.spawn().await {
                Ok(handle) => {
                    *task_slot.lock() = Some(handle);
                }
                Err(err) => {
                    tracing::warn!("ksni tray failed: {err}");
                    *hint.lock() = Some(format!(
                        "no system tray found ({err}); on GNOME enable the AppIndicator extension"
                    ));
                }
            }
        });
        Some(LinuxTray { slot, tokio, menu_sig: Mutex::new(0) })
    }

    impl LinuxTray {
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
        actions: mpsc::Sender<TrayAction>,
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
            for px in rgba.chunks_exact(4) {
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
                        let _ = tray.actions.send(TrayAction::Open);
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
                            let _ = tray.actions.send(TrayAction::ToggleMachine(id.clone()));
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
                        let _ = tray.actions.send(TrayAction::DisconnectAll);
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit Splice".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.actions.send(TrayAction::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]);
            items
        }
    }
}
