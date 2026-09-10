use crate::runtime::{BootStatus, Controller};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod state;
#[cfg(target_os = "macos")]
mod promises;
#[cfg(target_os = "macos")]
pub use macos::{attach, NativeSlot};

pub struct FileShelf {
    controller: Controller,
    error: Option<String>,
}

impl FileShelf {
    pub fn new(controller: Controller) -> Self {
        Self { controller, error: None }
    }

    pub fn open(&mut self) {
        if self.controller.status() != BootStatus::Online {
            self.error = Some("Connect Splice before opening the file shelf".into());
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let slot = self.controller.native_files();
            let shelf = slot.read().shelf.clone();
            if let Some(shelf) = shelf {
                shelf.set_visible(true);
                self.error = None;
            } else {
                self.error = Some("The file shelf has not started".into());
            }
        }
        #[cfg(target_os = "linux")]
        {
            self.controller.open_files();
            self.error = None;
        }
    }

    pub fn error(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            self.error.clone().or_else(|| self.controller.native_files().read().error.clone())
        }
        #[cfg(target_os = "linux")]
        {
            self.error.clone()
        }
    }
}
