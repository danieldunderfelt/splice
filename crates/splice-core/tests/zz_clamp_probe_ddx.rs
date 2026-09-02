use splice_core::arrange::{self, Body, Rules};
use splice_proto::{DisplayRect, Vec2I};

fn display(w: u32, h: u32) -> DisplayRect {
    DisplayRect { id: "d".into(), x: 0, y: 0, w, h, scale: 1.0 }
}

#[test]
fn clamp_probe() {
    let rules = Rules { min_seam: 20, align_tolerance: 0 };
    let moving = Body::new(&[display(100, 100)], Vec2I { x: i32::MIN, y: 0 });
    let fixed = Body::new(&[display(100, 100)], Vec2I { x: i32::MAX, y: 0 });
    let p = arrange::resolve(&moving, Vec2I::default(), &[fixed.clone()], &rules, None);
    println!("placement = {p:?}");
    let p = p.expect("some");
    let landed = moving.shifted(p.delta);
    println!("landed bounds = {:?} fixed bounds = {:?}", landed.bounds(), fixed.bounds());
    println!("touching = {}", arrange::touching(&landed, &fixed, &rules));
    println!("components = {:?}", arrange::components(&[landed, fixed], &rules).len());
}

#[test]
fn realistic_extents() {
    let rules = Rules::default();
    let moving = Body::new(&[display(3840, 2160)], Vec2I { x: -50_000, y: 0 });
    let fixed = Body::new(&[display(3840, 2160)], Vec2I { x: 50_000, y: 0 });
    let p = arrange::resolve(&moving, Vec2I::default(), &[fixed.clone()], &rules, None).expect("some");
    let landed = moving.shifted(p.delta);
    println!("delta {:?} touching {}", p.delta, arrange::touching(&landed, &fixed, &rules));
}
