use splice_core::arrange::{self, Body, Bounds, Rules};
use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, Vec2I};

const RULES: Rules = Rules { min_seam: 160, align_tolerance: 60 };

fn disp(w: u32, h: u32) -> DisplayRect {
    DisplayRect { id: format!("{w}x{h}"), x: 0, y: 0, w, h, scale: 1.0 }
}

fn body(x: i32, y: i32, w: u32, h: u32) -> Body {
    Body::new(&[disp(w, h)], Vec2I { x, y })
}

fn show(bodies: &[Body]) -> Vec<(i64, i64)> {
    bodies.iter().map(|b| { let r = b.bounds().unwrap(); (r.left, r.top) }).collect()
}

#[test]
fn oneshot() {
    let bodies = [body(0,0,1920,1080), body(1920,0,1920,1080), body(3840,0,1920,1080)];
    let vacated = bodies[1].bounds().unwrap();
    for nudge in [-100, -150, -160, -161, -200, -300, -500, -1000] {
        let step = arrange::drag_step(&bodies, 1, Vec2I{x:0,y:nudge}, vacated, Some(EdgeSide::Left), &RULES).unwrap();
        let placed: Vec<Body> = bodies.iter().zip(&step.deltas).map(|(b,d)| b.shifted(*d)).collect();
        println!("nudge {nudge}: deltas {:?} side {:?} -> {:?} comps {}", step.deltas, step.side, show(&placed), arrange::components(&placed, &RULES).len());
    }
}

#[test]
fn incremental() {
    let mut bodies = vec![body(0,0,1920,1080), body(1920,0,1920,1080), body(3840,0,1920,1080)];
    let vacated = bodies[1].bounds().unwrap();
    let start = Vec2I{x:1920,y:0};
    let mut offset = start;
    let mut side = None;
    let mut total = 0i32;
    for frame in 0..40 {
        total -= 25;
        let goal = Vec2I{ x: start.x + 0 - offset.x, y: start.y + total - offset.y };
        let step = arrange::drag_step(&bodies, 1, goal, vacated, side, &RULES).unwrap();
        side = Some(step.side);
        bodies = bodies.iter().zip(&step.deltas).map(|(b,d)| b.shifted(*d)).collect();
        offset.x += step.deltas[1].x; offset.y += step.deltas[1].y;
        if frame % 4 == 0 || frame < 10 {
            println!("total {total}: {:?} side {:?} comps {}", show(&bodies), step.side, arrange::components(&bodies, &RULES).len());
        }
    }
}

#[test]
fn up_then_back_down() {
    let mut bodies = vec![body(0,0,1920,1080), body(1920,0,1920,1080), body(3840,0,1920,1080)];
    let vacated = bodies[1].bounds().unwrap();
    let start = Vec2I{x:1920,y:0};
    let mut offset = start;
    let mut side = None;
    let mut total = 0i32;
    let mut seq: Vec<i32> = (1..=12).map(|i| -25*i).collect();
    seq.extend((1..=12).rev().map(|i| -25*i));
    seq.push(0);
    for t in seq {
        total = t;
        let goal = Vec2I{ x: start.x - offset.x, y: start.y + total - offset.y };
        let step = arrange::drag_step(&bodies, 1, goal, vacated, side, &RULES).unwrap();
        side = Some(step.side);
        bodies = bodies.iter().zip(&step.deltas).map(|(b,d)| b.shifted(*d)).collect();
        offset.x += step.deltas[1].x; offset.y += step.deltas[1].y;
    }
    println!("back at rest: {:?}", show(&bodies));
}
