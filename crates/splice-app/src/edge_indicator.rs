use objc2::{rc::Retained, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFont, NSPanel, NSTextField, NSTitlePosition,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use splice_core::{ui_state::UiCrossing, UiState};
use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, Vec2};

const WIDTH: f64 = 184.0;
const HEIGHT: f64 = 44.0;
const MARGIN: f64 = 10.0;

pub struct EdgeIndicator {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
    fill: Retained<NSBox>,
}

impl EdgeIndicator {
    pub fn new() -> Self {
        let mtm = MainThreadMarker::new().expect("edge indicator requires the AppKit main thread");
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, WIDTH, HEIGHT),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setIgnoresMouseEvents(true);
        panel.setHidesOnDeactivate(false);
        panel.setLevel(25);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        let background = colored_box(mtm, rect(0.0, 0.0, WIDTH, HEIGHT), 0.10, 0.12, 0.16, 10.0);
        panel.setContentView(Some(&background));
        let content = background
            .contentView()
            .expect("custom box has a content view");
        let label = NSTextField::labelWithString(&NSString::new(), mtm);
        label.setFrame(rect(10.0, 19.0, WIDTH - 20.0, 17.0));
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        content.addSubview(&label);
        let track = colored_box(
            mtm,
            rect(10.0, 8.0, WIDTH - 20.0, 4.0),
            0.26,
            0.29,
            0.34,
            2.0,
        );
        content.addSubview(&track);
        let fill = colored_box(mtm, rect(10.0, 8.0, 0.0, 4.0), 0.36, 0.55, 0.94, 2.0);
        content.addSubview(&fill);
        Self { panel, label, fill }
    }

    pub fn sync(&self, state: &UiState) {
        let frame = state.crossing_progress.as_ref().and_then(|crossing| {
            let source = state.machines.iter().find(|m| m.id == crossing.from)?;
            let local = state.machines.iter().find(|m| m.id == state.self_id)?;
            let target = state.machines.iter().find(|m| m.id == crossing.to)?;
            let displays = splice_platform::macos::displays::snapshot();
            let pointer = objc2_app_kit::NSEvent::mouseLocation();
            let main_height = main_height(&displays)?;
            let point = Vec2 {
                x: pointer.x,
                y: main_height - pointer.y,
            };
            let anchor = anchor(
                crossing,
                &source.displays,
                &displays,
                source.id == local.id,
                point,
            )?;
            let frame = panel_frame(anchor.0, anchor.1, crossing.side, main_height)?;
            Some((frame, crossing.progress, crossing.side, &target.hostname))
        });
        let Some((frame, progress, side, hostname)) = frame else {
            self.panel.orderOut(None);
            return;
        };
        let arrow = match side {
            EdgeSide::Left => "←",
            EdgeSide::Right => "→",
            EdgeSide::Top => "↑",
            EdgeSide::Bottom => "↓",
        };
        self.label
            .setStringValue(&NSString::from_str(&format!("{arrow} {hostname}")));
        self.fill.setFrame(rect(
            10.0,
            8.0,
            (WIDTH - 20.0) * f64::from(progress.clamp(0.0, 1.0)),
            4.0,
        ));
        self.panel.setFrame_display(frame, true);
        if !self.panel.isVisible() {
            self.panel.orderFrontRegardless();
        }
    }
}

fn colored_box(
    mtm: MainThreadMarker,
    frame: NSRect,
    r: f64,
    g: f64,
    b: f64,
    radius: f64,
) -> Retained<NSBox> {
    let view = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    view.setBoxType(NSBoxType::Custom);
    view.setTitlePosition(NSTitlePosition::NoTitle);
    view.setBorderWidth(0.0);
    view.setCornerRadius(radius);
    view.setContentViewMargins(NSSize::new(0.0, 0.0));
    view.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 0.96));
    view
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn main_height(displays: &[DisplayRect]) -> Option<f64> {
    displays
        .iter()
        .find(|d| d.x == 0 && d.y == 0)
        .map(|d| f64::from(d.h))
}

fn contains(display: &DisplayRect, point: Vec2) -> bool {
    point.x >= f64::from(display.x)
        && point.x <= f64::from(display.x) + f64::from(display.w)
        && point.y >= f64::from(display.y)
        && point.y <= f64::from(display.y) + f64::from(display.h)
}

fn anchor<'a>(
    crossing: &UiCrossing,
    source: &[DisplayRect],
    local: &'a [DisplayRect],
    is_local: bool,
    pointer: Vec2,
) -> Option<(&'a DisplayRect, Vec2)> {
    if !crossing.position.x.is_finite()
        || !crossing.position.y.is_finite()
        || !crossing.progress.is_finite()
    {
        return None;
    }
    if is_local {
        return local
            .iter()
            .find(|d| contains(d, crossing.position))
            .map(|d| (d, crossing.position));
    }
    let from = source.iter().find(|d| contains(d, crossing.position))?;
    let to = local.iter().find(|d| contains(d, pointer))?;
    if from.w == 0 || from.h == 0 {
        return None;
    }
    let x = (crossing.position.x - f64::from(from.x)) / f64::from(from.w);
    let y = (crossing.position.y - f64::from(from.y)) / f64::from(from.h);
    Some((
        to,
        Vec2 {
            x: f64::from(to.x) + x * f64::from(to.w),
            y: f64::from(to.y) + y * f64::from(to.h),
        },
    ))
}

fn panel_frame(
    display: &DisplayRect,
    at: Vec2,
    side: EdgeSide,
    main_height: f64,
) -> Option<NSRect> {
    let left = f64::from(display.x) + MARGIN;
    let top = f64::from(display.y) + MARGIN;
    let right = f64::from(display.x) + f64::from(display.w) - MARGIN - WIDTH;
    let bottom = f64::from(display.y) + f64::from(display.h) - MARGIN - HEIGHT;
    if right < left || bottom < top {
        return None;
    }
    let x = match side {
        EdgeSide::Left => left,
        EdgeSide::Right => right,
        _ => (at.x - WIDTH / 2.0).clamp(left, right),
    };
    let y = match side {
        EdgeSide::Top => top,
        EdgeSide::Bottom => bottom,
        _ => (at.y - HEIGHT / 2.0).clamp(top, bottom),
    };
    Some(rect(x, main_height - y - HEIGHT, WIDTH, HEIGHT))
}

impl Drop for EdgeIndicator {
    fn drop(&mut self) {
        self.panel.orderOut(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splice_proto::MachineId;

    fn display(x: i32, y: i32, w: u32, h: u32, scale: f64) -> DisplayRect {
        DisplayRect {
            id: format!("{x},{y}"),
            x,
            y,
            w,
            h,
            scale,
        }
    }

    #[test]
    fn every_edge_stays_inside_scaled_and_negative_origin_displays() {
        for d in [
            display(0, 0, 2560, 1440, 2.0),
            display(-1920, -1080, 1920, 1080, 1.0),
        ] {
            for side in [
                EdgeSide::Left,
                EdgeSide::Right,
                EdgeSide::Top,
                EdgeSide::Bottom,
            ] {
                for at in [
                    Vec2 {
                        x: f64::from(d.x),
                        y: f64::from(d.y),
                    },
                    Vec2 {
                        x: f64::from(d.x) + f64::from(d.w),
                        y: f64::from(d.y) + f64::from(d.h),
                    },
                ] {
                    let frame = panel_frame(&d, at, side, 1440.0).unwrap();
                    let cg_y = 1440.0 - frame.origin.y - frame.size.height;
                    assert!(frame.origin.x >= f64::from(d.x));
                    assert!(frame.origin.x + WIDTH <= f64::from(d.x) + f64::from(d.w));
                    assert!(cg_y >= f64::from(d.y));
                    assert!(cg_y + HEIGHT <= f64::from(d.y) + f64::from(d.h));
                    assert_eq!(frame.size, NSSize::new(WIDTH, HEIGHT));
                }
            }
        }
    }

    #[test]
    fn remote_crossing_maps_to_the_display_holding_the_physical_pointer() {
        let source = [display(0, 0, 1920, 1080, 1.0)];
        let local = [
            display(0, 0, 2560, 1440, 2.0),
            display(-1280, 0, 1280, 720, 1.0),
        ];
        let crossing = UiCrossing {
            from: MachineId("linux".into()),
            to: MachineId("mac".into()),
            side: EdgeSide::Right,
            position: Vec2 {
                x: 1920.0,
                y: 540.0,
            },
            progress: 0.5,
        };
        let (d, point) = anchor(
            &crossing,
            &source,
            &local,
            false,
            Vec2 {
                x: -640.0,
                y: 100.0,
            },
        )
        .unwrap();
        assert_eq!(d.id, local[1].id);
        assert_eq!(point, Vec2 { x: 0.0, y: 360.0 });
        let frame = panel_frame(d, point, crossing.side, 1440.0).unwrap();
        assert_eq!(frame.origin.x, -WIDTH - MARGIN);
    }

    #[test]
    fn changed_layout_and_invalid_progress_hide_the_indicator() {
        let local = [display(0, 0, 2560, 1440, 2.0)];
        let mut crossing = UiCrossing {
            from: MachineId("mac".into()),
            to: MachineId("linux".into()),
            side: EdgeSide::Right,
            position: Vec2 {
                x: -1280.0,
                y: 540.0,
            },
            progress: 0.5,
        };
        assert!(anchor(&crossing, &local, &local, true, Vec2 { x: 0.0, y: 0.0 }).is_none());
        crossing.position.x = 2560.0;
        crossing.progress = f32::NAN;
        assert!(anchor(&crossing, &local, &local, true, Vec2 { x: 0.0, y: 0.0 }).is_none());
    }
}
