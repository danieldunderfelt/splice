//! Edge drop panels: a thin borderless panel sits along each armed screen edge and accepts
//! native file drags. Releasing files on one offers them to the machine across that edge,
//! the same offer a drop into the shelf makes, so a Mac source can send files with a single
//! drag toward the boundary. While a drag hovers a panel the event tap suppresses edge
//! crossing (see [`super::file_drag_at_edge`]) so the drag is never taken for a cursor cross.

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{define_class, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingDestination, NSDraggingInfo, NSPanel,
    NSPasteboardTypeFileURL, NSTextField, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSArray, NSCopying, NSPoint, NSRect, NSSize, NSString};
use splice_proto::DisplayRect;
use std::cell::Cell;
use std::sync::Arc;

use super::files::ShelfCtx;
use crate::file_shelf::{EdgeTarget, FileEvent, Recipient, SourceGesture};
use crate::EdgeSide;

const THICK: f64 = 44.0;

pub struct EdgeDropIvars {
    ctx: Arc<ShelfCtx>,
    recipient: Recipient,
    enabled: Cell<bool>,
    label: Retained<NSTextField>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpliceEdgeDropView"]
    #[ivars = EdgeDropIvars]
    pub struct EdgeDropView;

    unsafe impl NSObjectProtocol for EdgeDropView {}

    impl EdgeDropView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }

    unsafe impl NSDraggingDestination for EdgeDropView {
        #[unsafe(method(draggingEntered:))]
        fn entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let iv = self.ivars();
            let pb = sender.draggingPasteboard();
            let has_files = super::shelf::pasteboard_has_files(&pb);
            let copy = sender.draggingSourceOperationMask().contains(NSDragOperation::Copy);
            if iv.enabled.get() && has_files && copy {
                super::set_file_drag_at_edge(true);
                if let Some(window) = self.window() {
                    window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(0.23, 0.51, 0.96, 0.6)));
                }
                iv.label
                    .setStringValue(&NSString::from_str(&format!("Release to send to {}", iv.recipient.name)));
                NSDragOperation::Copy
            } else {
                NSDragOperation::None
            }
        }

        #[unsafe(method(draggingUpdated:))]
        fn updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let iv = self.ivars();
            let pb = sender.draggingPasteboard();
            if iv.enabled.get()
                && super::shelf::pasteboard_has_files(&pb)
                && sender.draggingSourceOperationMask().contains(NSDragOperation::Copy)
            {
                NSDragOperation::Copy
            } else {
                NSDragOperation::None
            }
        }

        #[unsafe(method(draggingExited:))]
        fn exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            super::set_file_drag_at_edge(false);
            self.clear_feedback();
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            let iv = self.ivars();
            let pb = sender.draggingPasteboard();
            iv.enabled.get()
                && super::shelf::pasteboard_has_files(&pb)
                && sender.draggingSourceOperationMask().contains(NSDragOperation::Copy)
        }

        #[unsafe(method(performDragOperation:))]
        fn perform(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            super::set_file_drag_at_edge(false);
            self.clear_feedback();
            let iv = self.ivars();
            let selection = if iv.enabled.get() {
                super::files::read_file_urls(&sender.draggingPasteboard())
            } else {
                None
            };
            match selection {
                Some(selection) => iv.ctx.emit(FileEvent::SourceSelected {
                    paths: selection.paths,
                    recipient: iv.recipient.id.clone(),
                    gesture: SourceGesture::NativeDrop,
                    lease: selection.lease,
                }),
                None => false,
            }
        }

        #[unsafe(method(concludeDragOperation:))]
        fn conclude(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            super::set_file_drag_at_edge(false);
            self.clear_feedback();
        }
    }
);

impl EdgeDropView {
    fn clear_feedback(&self) {
        self.ivars().label.setStringValue(&NSString::from_str(""));
        if let Some(window) = self.window() {
            window.setBackgroundColor(Some(&NSColor::clearColor()));
        }
    }

    fn new(mtm: MainThreadMarker, ctx: Arc<ShelfCtx>, recipient: Recipient, frame: NSRect) -> Retained<Self> {
        let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setTextColor(Some(&NSColor::whiteColor()));
        let this = Self::alloc(mtm).set_ivars(EdgeDropIvars {
            ctx,
            recipient,
            enabled: Cell::new(true),
            label: label.clone(),
        });
        let this: Retained<Self> = unsafe {
            objc2::msg_send![super(this), initWithFrame: frame]
        };
        let inset = NSRect::new(NSPoint::new(6.0, 4.0), NSSize::new((frame.size.width - 12.0).max(0.0), 16.0));
        label.setFrame(inset);
        this.addSubview(&label);
        let types = [unsafe { NSPasteboardTypeFileURL }.copy()];
        this.registerForDraggedTypes(&NSArray::from_retained_slice(&types));
        this
    }
}

/// Panels currently shown, one per edge target, rebuilt whenever the set changes.
#[derive(Default)]
pub struct EdgeDropPanels {
    current: Vec<EdgeTarget>,
    panels: Vec<Retained<NSPanel>>,
}

impl EdgeDropPanels {
    pub fn apply(&mut self, mtm: MainThreadMarker, ctx: &Arc<ShelfCtx>, targets: Vec<EdgeTarget>) {
        if targets == self.current {
            return;
        }
        for panel in self.panels.drain(..) {
            panel.orderOut(None);
        }
        self.current = targets.clone();
        let displays = super::displays::snapshot();
        let Some(main_height) = main_height(&displays) else {
            return;
        };
        for target in targets {
            let Some(frame) = frame_for(&target, main_height) else {
                continue;
            };
            let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                frame,
                NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                NSBackingStoreType::Buffered,
                false,
            );
            unsafe { panel.setReleasedWhenClosed(false) };
            panel.setOpaque(false);
            panel.setBackgroundColor(Some(&NSColor::clearColor()));
            panel.setHasShadow(false);
            panel.setHidesOnDeactivate(false);
            panel.setLevel(objc2_app_kit::NSFloatingWindowLevel);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
            let recipient = Recipient { id: target.recipient.id.clone(), name: target.recipient.name.clone() };
            let view = EdgeDropView::new(mtm, ctx.clone(), recipient, NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
            panel.setContentView(Some(&view));
            panel.orderFrontRegardless();
            self.panels.push(panel);
        }
    }
}

fn main_height(displays: &[DisplayRect]) -> Option<f64> {
    displays
        .iter()
        .find(|d| d.x == 0 && d.y == 0)
        .map(|d| f64::from(d.h))
}

fn frame_for(target: &EdgeTarget, main_height: f64) -> Option<NSRect> {
    let from = f64::from(target.from);
    let to = f64::from(target.to);
    let at = f64::from(target.at);
    if to <= from {
        return None;
    }
    let span = to - from;
    let rect = match target.side {
        EdgeSide::Left => NSRect::new(NSPoint::new(at, main_height - to), NSSize::new(THICK, span)),
        EdgeSide::Right => NSRect::new(NSPoint::new(at - THICK, main_height - to), NSSize::new(THICK, span)),
        EdgeSide::Top => NSRect::new(NSPoint::new(from, main_height - at - THICK), NSSize::new(span, THICK)),
        EdgeSide::Bottom => NSRect::new(NSPoint::new(from, main_height - at), NSSize::new(span, THICK)),
    };
    Some(rect)
}
