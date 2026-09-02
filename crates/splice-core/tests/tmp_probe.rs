use splice_core::arrange::{self, Body, Rules};
use splice_proto::{DisplayRect, Vec2I};

fn d(id: &str, x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
    DisplayRect { id: id.into(), x, y, w, h, scale: 1.0 }
}
fn at(x: i32, y: i32) -> Vec2I { Vec2I { x, y } }

fn preview() -> Vec<Body> {
    vec![
        Body::new(&[d("1",0,0,1512,982), d("2",1512,0,1920,1080)], at(0,0)),
        Body::new(&[d("1",0,0,2560,1440)], at(3432,0)),
        Body::new(&[d("1",0,0,3840,1080)], at(3432,1440)),
        Body::new(&[d("1",0,0,1920,1080)], at(0,1080)),
    ]
}

#[test]
fn probe() {
    let rules = Rules { min_seam: arrange::MIN_SEAM, align_tolerance: 60 };
    let bodies = preview();
    println!("components: {:?}", arrange::components(&bodies, &rules));
    for dragged in 0..bodies.len() {
        for goal in [at(0,-1), at(0,-10), at(1,0), at(0,10), at(-10,0), at(0,-100)] {
            let vacated = bodies[dragged].bounds().unwrap();
            match arrange::drag_step(&bodies, dragged, goal, vacated, None, &rules) {
                Some(step) => {
                    let max_other = step.deltas.iter().enumerate()
                        .filter(|(i,_)| *i != dragged)
                        .map(|(_,d)| (f64::from(d.x).hypot(f64::from(d.y))) as i64)
                        .max().unwrap_or(0);
                    println!("drag {dragged} goal {:?} -> {:?} maxother={max_other}", (goal.x,goal.y), step.deltas.iter().map(|d|(d.x,d.y)).collect::<Vec<_>>());
                }
                None => println!("drag {dragged} goal {:?} -> None", (goal.x, goal.y)),
            }
        }
    }
}
