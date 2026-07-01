//! Example AI generated project for demonstrating a generated ContextCodeCache.

mod math;

/// Default radius when none is supplied.
const DEFAULT_RADIUS: f64 = 2.0;

/// Program entry point.
fn main() {
    let area = math::circle_area(DEFAULT_RADIUS);
    report(area);
}

fn report(area: f64) {
    println!("area = {area}");
}
