# render.rs.md (20260820-07-57-23) UTC
# source: src/render.rs [rust]
# modules
# imports
    - L4@crate::model (Counts, FileCache)
    - L5@std::fmt (Write, _)
    - L6@std::path (Path)
    - L209@super
# const
# funcs
    - L9:8@now_ts:String // current UTC timestamp formatted as `yyyymmdd-hh-mm-ss`
    - L14:8@render_file:String // render a single per-file cache entry
    - L106:8@render_index:String // render the CCC index for the whole project
    - L190:8@strip_timestamps:String // replace embedded generation timestamps with a fixed token so freshness
    - L194:4@strip_ts_line:String
    - L212:8@timestamps_are_normalized_away
# refs
# note
