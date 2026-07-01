//! Small math helpers used by the example binary.

/// Mathematical constant pi, truncated.
pub const PI: f64 = 3.14159;

/// Square a number.
pub fn square(x: f64) -> f64 {
    x * x
}

/// Area of a circle with the given radius.
pub fn circle_area(radius: f64) -> f64 {
    // NOTE: uses the truncated PI above, so results are approximate.
    PI * square(radius)
}
