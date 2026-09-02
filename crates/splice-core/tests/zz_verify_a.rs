use splice_core::arrange::{self, Body, Rules};
use splice_proto::{DisplayRect, Vec2I};

fn disp(id: &str, x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
    DisplayRect { id: id.into(), x, y, w, h, scale: 1.0 }
}

fn machine(off: (i32, i32)) -> Body {
    Body::new(
        &[disp("a", 0, 0, 1920, 1080), disp("b", 1920, 0, 1920, 1080)],
        Vec2I { x: off.0, y: off.1 },
    )
}

#[test]
fn trace() {
    let rules = Rules { min_seam: 160, align_tolerance: 60 };
    let offs = [(0, 0), (3840, -773), (3840, 920), (-3680, -1080)];
    let mut bodies: Vec<Body> = offs.iter().map(|&o| machine(o)).collect();
    println!("components at start: {:?}", arrange::components(&bodies, &rules));
    let vacated = bodies[0].bounds().unwrap();
    let start = Vec2I { x: 0, y: 0 };
    let mut offset = start;
    // absolute pointer target relative to start
    let target = Vec2I { x: -2201, y: 1499 };
    let mut side = None;
    for frame in 0..6 {
        let goal = Vec2I { x: start.x + target.x - offset.x, y: start.y + target.y - offset.y };
        let step = arrange::drag_step(&bodies, 0, goal, vacated, side, &rules).expect("attach");
        side = Some(step.side);
        println!("frame {frame}: goal={goal:?} deltas={:?} side={:?}", step.deltas, step.side);
        bodies = bodies.iter().zip(&step.deltas).map(|(b, d)| b.shifted(*d)).collect();
        offset.x += step.deltas[0].x;
        offset.y += step.deltas[0].y;
        println!("   body0 offset now {offset:?} bounds {:?}", bodies[0].bounds());
        for (i, b) in bodies.iter().enumerate() { println!("   body{i} {:?}", b.bounds().unwrap()); }
        println!("   components: {:?}", arrange::components(&bodies, &rules).len());
    }
}
