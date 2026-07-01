# math.rs.md (20260701-13-08-47) UTC
# source: example/src/math.rs [rust]
# const
    - L4@PI:f64
# funcs
    - L7:8@square:f64 // Square a number.
    - L12:8@circle_area:f64 // Area of a circle with the given radius.
# refs
    - circle_area@L14 calls L7:8@square:f64
# note
    - @L13 NOTE: uses the truncated PI above, so results are approximate.
