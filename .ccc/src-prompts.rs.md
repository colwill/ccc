# prompts.rs.md (20260921-12-11-30) UTC
# source: src/prompts.rs [rust]
# modules
# imports
    - L16@anyhow (Context, Result)
    - L17@serde (Serialize)
    - L18@serde_json (Value)
    - L19@std::collections (BTreeMap, BTreeSet)
    - L20@std (fs)
    - L21@std::path (Path, PathBuf)
    - L949@super
# const
    - L23@SCHEMA:&str
    - L26@LEDGER_NAME:&str
    - L31@EDIT_TOOLS:&[&str]
    - L35@PROMPT_CAP:usize
    - L39@MAX_TEMPORAL_GAP_SECS:i64
    - L42@MAX_PARENT_HOPS:usize
# funcs
    - L193:15@claude_slug:String // claude names a project directory after its path with the separators flattened
    - L361:4@condense:String // condense a request to something a report can carry
    - L504:4@parse_claude:Vec<Turn> // one claude transcript -> the requests it contains, each with its edits
    - L625:4@parse_copilot:Vec<Turn> // VS Code stores a chat session as an initial snapshot plus a patch log:
    - L764:8@attribute:Attribution // Tie each changed file to the requests that produced it.
    - L955:20@new:Dir
    - L961:20@path:&std::path::Path
    - L966:16@drop
    - L972:8@write:PathBuf
    - L984:8@user:Value
    - L992:8@assistant:Value
    - L1000:8@tool_use:Value
    - L1005:8@an_edit_is_credited_to_the_request_that_led_to_it_across_a_tool_result_gap
    - L1039:8@harness_noise_is_never_reported_as_a_request
    - L1067:8@a_request_that_changed_nothing_is_still_reported
    - L1080:8@copilots_patch_log_replays_into_the_requests_it_recorded
    - L1108:8@turn:Turn
    - L1122:8@evidence_is_graded_from_the_text_still_being_there_down_to_mere_timing
    - L1191:8@a_file_wide_reference_covers_every_span_and_a_pinned_one_does_not
    - L1212:8@the_slug_flattens_the_path_the_way_claude_writes_it
    - L1226:8@a_long_request_is_condensed_rather_than_carried_whole
# refs
    - parse_copilot@L669 calls L361:4@condense:String
    - an_edit_is_credited_to_the_request_that_led_to_it_across_a_tool_result_gap@L1008 calls L972:8@write:PathBuf
    - an_edit_is_credited_to_the_request_that_led_to_it_across_a_tool_result_gap@L1012 calls L984:8@user:Value
    - an_edit_is_credited_to_the_request_that_led_to_it_across_a_tool_result_gap@L1013 calls L992:8@assistant:Value
    - an_edit_is_credited_to_the_request_that_led_to_it_across_a_tool_result_gap@L1017 calls L984:8@user:Value
    - an_edit_is_credited_to_the_request_that_led_to_it_across_a_tool_result_gap@L1020 calls L992:8@assistant:Value
    - an_edit_is_credited_to_the_request_that_led_to_it_across_a_tool_result_gap@L1028 calls L504:4@parse_claude:Vec<Turn>
    - harness_noise_is_never_reported_as_a_request@L1041 calls L972:8@write:PathBuf
    - harness_noise_is_never_reported_as_a_request@L1046 calls L984:8@user:Value
    - harness_noise_is_never_reported_as_a_request@L1055 calls L984:8@user:Value
    - harness_noise_is_never_reported_as_a_request@L1061 calls L504:4@parse_claude:Vec<Turn>
    - a_request_that_changed_nothing_is_still_reported@L1069 calls L972:8@write:PathBuf
    - a_request_that_changed_nothing_is_still_reported@L1072 calls L984:8@user:Value
    - a_request_that_changed_nothing_is_still_reported@L1074 calls L504:4@parse_claude:Vec<Turn>
    - copilots_patch_log_replays_into_the_requests_it_recorded@L1082 calls L972:8@write:PathBuf
    - copilots_patch_log_replays_into_the_requests_it_recorded@L1099 calls L625:4@parse_copilot:Vec<Turn>
    - evidence_is_graded_from_the_text_still_being_there_down_to_mere_timing@L1169 calls L764:8@attribute:Attribution
    - a_long_request_is_condensed_rather_than_carried_whole@L1228 calls L361:4@condense:String
# note
