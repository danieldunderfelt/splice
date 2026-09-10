use crate::linux::backends::Driven;
use crate::linux::Shared;
use crate::raw::RawEmulate;
use crate::{Capture, EdgeSide, EdgeSpec, PlatformEvent};
use splice_proto::raw::{RawEvent, RawReport};
use splice_proto::Vec2;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test]
#[ignore = "moves the real Wayland pointer; requires layer-shell and /dev/uinput"]
async fn native_raw_pointer_reaches_every_boundary_without_capture_or_entry_bounce() {
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        raw_destination: Default::default(),
        raw_boundary: Default::default(),
        capture_control: Default::default(),
        emission: parking_lot::Mutex::new(()),
        tx,
        health: parking_lot::Mutex::new(crate::HealthReport::default()),
        displays: parking_lot::RwLock::new(Vec::new()),
        epoch: Instant::now(),
        last_injection: Default::default(),
        injected_keys: parking_lot::Mutex::new(std::collections::VecDeque::new()),
    });
    let displays = super::super::displays::spawn(shared.clone()).unwrap();
    *shared.displays.write() = displays.clone();
    let display = displays[0].clone();
    let (capture, _, capture_stop) =
        match super::create(shared.clone(), vec![42, 54, 1], Arc::new(Driven::default())).await {
            Ok(handles) => handles,
            Err(error) => panic!("overlay setup failed: {error}"),
        };
    let (emulate, emulate_stop) = match super::super::uinput::create(shared.clone(), None).await {
        Ok(handles) => handles,
        Err(error) => {
            capture_stop.stop();
            panic!("uinput setup failed: {error}");
        }
    };
    let raw = super::super::raw::RelativeInput::new(shared);
    if let Err(error) = raw.prepare().await {
        capture_stop.stop();
        emulate_stop.stop();
        panic!("raw input setup failed: {error}");
    }
    let center = Vec2 {
        x: f64::from(display.x) + f64::from(display.w) / 2.0,
        y: f64::from(display.y) + f64::from(display.h) / 2.0,
    };
    let outcome: anyhow::Result<()> = async {
        for (index, side) in [
            EdgeSide::Left,
            EdgeSide::Left,
            EdgeSide::Right,
            EdgeSide::Right,
            EdgeSide::Top,
            EdgeSide::Top,
            EdgeSide::Bottom,
            EdgeSide::Bottom,
        ]
        .into_iter()
        .enumerate()
        {
            let session = index as u64 + 1;
            let (at, from, to, landing, motion) = match side {
                EdgeSide::Left => (
                    display.x,
                    display.y,
                    display.y + display.h as i32,
                    Vec2 {
                        x: f64::from(display.x) + 8.0,
                        y: center.y,
                    },
                    (-8, 0),
                ),
                EdgeSide::Right => (
                    display.x + display.w as i32,
                    display.y,
                    display.y + display.h as i32,
                    Vec2 {
                        x: f64::from(display.x) + f64::from(display.w) - 8.0,
                        y: center.y,
                    },
                    (8, 0),
                ),
                EdgeSide::Top => (
                    display.y,
                    display.x,
                    display.x + display.w as i32,
                    Vec2 {
                        x: center.x,
                        y: f64::from(display.y) + 8.0,
                    },
                    (0, -8),
                ),
                EdgeSide::Bottom => (
                    display.y + display.h as i32,
                    display.x,
                    display.x + display.w as i32,
                    Vec2 {
                        x: center.x,
                        y: f64::from(display.y) + f64::from(display.h) - 8.0,
                    },
                    (0, 8),
                ),
            };
            capture
                .set_edges(vec![EdgeSpec {
                    id: (index / 2) as u32,
                    side,
                    at,
                    from,
                    to,
                }])
                .await?;
            raw.begin(session)?;
            raw.boundary_policy(session, true)?;
            emulate.enter(landing).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            while let Ok(event) = events.try_recv() {
                anyhow::ensure!(
                    !matches!(
                        event,
                        PlatformEvent::RawBoundary { .. } | PlatformEvent::Capture(_)
                    ),
                    "cursor placement triggered local handling: {event:?}"
                );
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut sequence = 0;
            loop {
                let captured_us = crate::raw::clock::now_us();
                raw.inject(
                    session,
                    &RawReport {
                        device: 1,
                        sequence,
                        captured_us,
                        events: vec![RawEvent::Motion {
                            x: motion.0,
                            y: motion.1,
                        }],
                    },
                    captured_us,
                )?;
                sequence += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
                let mut boundary = false;
                while let Ok(event) = events.try_recv() {
                    match event {
                        PlatformEvent::RawBoundary {
                            session: actual,
                            edge: observed,
                            along,
                        } => {
                            anyhow::ensure!(
                                actual == session
                                    && observed.id == (index / 2) as u32
                                    && along > f64::from(from)
                                    && along < f64::from(to),
                                "wrong native boundary {actual} {observed:?} {along}"
                            );
                            boundary = true;
                        }
                        PlatformEvent::Capture(event) => {
                            anyhow::bail!("raw destination acquired local handling: {event:?}")
                        }
                        _ => {}
                    }
                }
                if boundary {
                    break;
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "no native raw boundary for {side:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            while let Ok(event) = events.try_recv() {
                anyhow::ensure!(
                    !matches!(
                        event,
                        PlatformEvent::RawBoundary { .. } | PlatformEvent::Capture(_)
                    ),
                    "native boundary bounced or captured locally: {event:?}"
                );
            }
            raw.end(session)?;
        }
        Ok(())
    }
    .await;
    let mut cleanup_errors = Vec::new();
    if let Err(error) = capture.set_edges(Vec::new()).await {
        cleanup_errors.push(format!("clear edges: {error}"));
    }
    for session in 1..=8 {
        if let Err(error) = raw.end(session) {
            cleanup_errors.push(format!("end raw session {session}: {error}"));
        }
    }
    if let Err(error) = emulate.enter(center).await {
        cleanup_errors.push(format!("center pointer: {error}"));
    }
    if let Err(error) = emulate.leave().await {
        cleanup_errors.push(format!("leave uinput session: {error}"));
    }
    capture_stop.stop();
    emulate_stop.stop();
    match outcome {
        Ok(()) if cleanup_errors.is_empty() => {}
        Ok(()) => panic!(
            "native boundary cleanup failed: {}",
            cleanup_errors.join("; ")
        ),
        Err(error) if cleanup_errors.is_empty() => panic!("{error}"),
        Err(error) => panic!("{error}; cleanup failed: {}", cleanup_errors.join("; ")),
    }
}
