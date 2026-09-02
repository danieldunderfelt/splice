use splice_core::arrange::{self, Body, Bounds, Rules};
use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, Vec2I};

const RULES: Rules = Rules { min_seam: 160, align_tolerance: 60 };

fn mon(x: i32, y: i32) -> Body {
    Body::new(&[DisplayRect { id: format!("{x},{y}"), x: 0, y: 0, w: 1920, h: 1080, scale: 1.0 }], Vec2I { x, y })
}

fn dump(label: &str, bodies: &[Body], step: &arrange::Step) {
    let placed: Vec<Body> = bodies.iter().zip(&step.deltas).map(|(b, d)| b.shifted(*d)).collect();
    println!("{label}: deltas={:?} side={:?}", step.deltas, step.side);
    for (i, b) in placed.iter().enumerate() {
        println!("   body{i} bounds={:?}", b.bounds().unwrap());
    }
    println!("   components={:?}", arrange::components(&placed, &RULES));
    let overlap = placed.iter().enumerate().any(|(i, a)| placed.iter().skip(i+1).any(|b| {
        a.rects.iter().any(|r| b.rects.iter().any(|o| r.left < o.right && o.left < r.right && r.top < o.bottom && o.top < r.bottom))
    }));
    println!("   overlaps={overlap}");
}

#[test]
fn trace_row_of_three() {
    let bodies = [mon(0,0), mon(1920,0), mon(3840,0)];
    let vacated: Bounds = bodies[1].bounds().unwrap();
    println!("vacated={vacated:?}");
    for dy in [-40, -100, -160, -161, -200, -300, -540, -1080, -1200] {
        let step = arrange::drag_step(&bodies, 1, Vec2I{x:0,y:dy}, vacated, Some(EdgeSide::Left), &RULES).expect("attach");
        dump(&format!("dy={dy}"), &bodies, &step);
    }
}
