# model.rs.md (20260820-07-57-23) UTC
# source: src/model.rs [rust]
# modules
# imports
    - L3@crate::languages (Language)
    - L4@std::path (PathBuf)
    - L221@super
# const
    - L139@Serves:Boundary
    - L141@Calls:Boundary
# funcs
    - L56:12@complexity:usize // cyclomatic-style score: one path, plus one per decision point and loop
    - L60:12@max_loop_depth:usize
    - L66:12@complexity_score:u8 // `complexity` on a 1-10 scale, for anything that has to *show* it rather
    - L145:12@label:&'static str
    - L202:12@counts:Counts
    - L223:8@metrics:FuncMetrics
    - L234:8@the_complexity_band_spans_one_to_ten_and_never_leaves_the_range
    - L281:12@add
# refs
    - complexity_score@L67 calls L56:12@complexity:usize
    - the_complexity_band_spans_one_to_ten_and_never_leaves_the_range@L258 calls L223:8@metrics:FuncMetrics
# note
