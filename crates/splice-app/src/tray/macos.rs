use super::{icon_rgba, machine_menu_label, AppAction};
use crate::runtime::Controller;
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{NSMenu, NSMenuDidBeginTrackingNotification, NSMenuDidEndTrackingNotification};
use objc2_foundation::{
    NSArray, NSDefaultRunLoopMode, NSNotificationCenter, NSObjectProtocol, NSRunLoop,
};
use splice_core::UiState;
use splice_proto::MachineId;
use std::cell::RefCell;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use tray_icon::menu::{CheckMenuItem, ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem};

const OPEN_ID: &str = "splice.open";
const FILES_ID: &str = "splice.files";
const DISCONNECT_ID: &str = "splice.disconnect";
const QUIT_ID: &str = "splice.quit";
const MACHINE_PREFIX: &str = "splice.machine.";

pub struct MacTray {
    tracking: MenuTracking,
    pub(crate) _tray: tray_icon::TrayIcon,
    menu: Menu,
    machines: RefCell<Vec<(MachineId, CheckMenuItem)>>,
}

impl MacTray {
    pub fn new(
        ctrl: &Controller,
        actions: mpsc::Sender<AppAction>,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        let menu = Menu::new();
        menu.append_items(&[
            &MenuItem::with_id(OPEN_ID, "Open Splice", true, None),
            &MenuItem::with_id(FILES_ID, "Open file shelf", true, None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(DISCONNECT_ID, "Disconnect all", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(QUIT_ID, "Quit Splice", true, None),
        ])
        .map_err(|error| error.to_string())?;
        let tracking = MenuTracking::new(&menu, ctx.clone());
        let icon = tray_icon::Icon::from_rgba(icon_rgba(64), 64, 64)
            .map_err(|error| format!("icon: {error}"))?;
        let tray = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("Splice")
            .with_icon(icon)
            .with_menu_on_left_click(true)
            .build()
            .map_err(|error| error.to_string())?;
        let result = Self {
            _tray: tray,
            menu,
            machines: RefCell::new(Vec::new()),
            tracking,
        };
        result.sync(&ctrl.state())?;
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = match event.id().0.as_str() {
                OPEN_ID => Some(AppAction::Open),
                FILES_ID => Some(AppAction::Files),
                DISCONNECT_ID => Some(AppAction::DisconnectAll),
                QUIT_ID => Some(AppAction::Quit),
                id => id
                    .strip_prefix(MACHINE_PREFIX)
                    .map(|id| AppAction::ToggleMachine(MachineId(id.to_owned()))),
            };
            if let Some(action) = action {
                let _ = actions.send(action);
                ctx.request_repaint();
            }
        }));
        Ok(result)
    }

    pub fn sync(&self, state: &UiState) -> Result<(), String> {
        if self.tracking.active.load(Ordering::SeqCst) != 0 {
            return Ok(());
        }
        let desired: Vec<_> = state
            .machines
            .iter()
            .filter(|machine| machine.id != state.self_id)
            .collect();
        let mut items = self.machines.borrow_mut();
        for index in (0..items.len()).rev() {
            if !desired.iter().any(|machine| machine.id == items[index].0) {
                self.menu
                    .remove(&items[index].1)
                    .map_err(|error| error.to_string())?;
                items.remove(index);
            }
        }
        for (index, machine) in desired.into_iter().enumerate() {
            let existing = items.iter().position(|(id, _)| *id == machine.id);
            match existing {
                Some(position) if position != index => {
                    let item = &items[position].1;
                    self.menu.remove(item).map_err(|error| error.to_string())?;
                    self.menu
                        .insert(item, index + 3)
                        .map_err(|error| error.to_string())?;
                    let item = items.remove(position);
                    items.insert(index, item);
                }
                Some(_) => {}
                None => {
                    let item = CheckMenuItem::with_id(
                        format!("{MACHINE_PREFIX}{}", machine.id.0),
                        machine_menu_label(machine),
                        true,
                        machine.enabled,
                        None,
                    );
                    self.menu
                        .insert(&item, index + 3)
                        .map_err(|error| error.to_string())?;
                    items.insert(index, (machine.id.clone(), item));
                }
            }
            let item = &items[index].1;
            let label = machine_menu_label(machine);
            if item.text() != label {
                item.set_text(label);
            }
            if item.is_checked() != machine.enabled {
                item.set_checked(machine.enabled);
            }
        }
        Ok(())
    }
}

struct MenuTracking {
    active: Arc<AtomicUsize>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl MenuTracking {
    fn new(menu: &Menu, ctx: egui::Context) -> Self {
        let active = Arc::new(AtomicUsize::new(0));
        let center = NSNotificationCenter::defaultCenter();
        let native = unsafe { &*menu.ns_menu().cast::<NSMenu>() };
        let started = active.clone();
        let begin = block2::RcBlock::new(move |_| {
            started.fetch_add(1, Ordering::SeqCst);
        });
        let ended = active.clone();
        let end = block2::RcBlock::new(move |_| {
            let active = ended.clone();
            let ctx = ctx.clone();
            let finished = block2::RcBlock::new(move || {
                let _ = active.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    Some(count.saturating_sub(1))
                });
                ctx.request_repaint();
            });
            unsafe {
                NSRunLoop::mainRunLoop()
                    .performInModes_block(&NSArray::from_slice(&[NSDefaultRunLoopMode]), &finished);
            }
        });
        let observers = unsafe {
            vec![
                center.addObserverForName_object_queue_usingBlock(
                    Some(NSMenuDidBeginTrackingNotification),
                    Some(native),
                    None,
                    &begin,
                ),
                center.addObserverForName_object_queue_usingBlock(
                    Some(NSMenuDidEndTrackingNotification),
                    Some(native),
                    None,
                    &end,
                ),
            ]
        };
        Self { active, observers }
    }
}

impl Drop for MenuTracking {
    fn drop(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        for observer in &self.observers {
            unsafe {
                center.removeObserver((**observer).as_ref());
            }
        }
    }
}
