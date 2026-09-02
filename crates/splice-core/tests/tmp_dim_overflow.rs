use splice_core::arrange::{drag_step, Body, Rules};
use splice_proto::{DisplayRect, Vec2I};

fn rect(id: &str, x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
    DisplayRect { id: id.into(), x, y, w, h, scale: 1.0 }
}

#[test]
fn huge_dims_overflow() {
    let big = Body::new(&[rect("big", 0, 0, u32::MAX, u32::MAX)], Vec2I::default());
    let small = Body::new(&[rect("small", -100, 0, 100, 100)], Vec2I::default());
    let bodies = vec![small.clone(), big];
    let vacated = small.bounds().unwrap();
    let step = drag_step(&bodies, 0, Vec2I { x: 0, y: 10 }, vacated, None, &Rules::default());
    println!("step = {step:?}");
}
