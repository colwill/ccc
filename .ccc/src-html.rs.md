# html.rs.md (20260729-22-00-57) UTC
# source: src/html.rs [rust]
# const
    - L37@TEMPLATE:&str
# funcs
    - L14:8@render_surf_html:String // render the single-file report page. `title` names the report (by
    - L25:8@write_surf_html:Result<()>
    - L30:4@esc:String
    - L259:8@embeds_report_and_title
    - L281:8@script_breakout_is_defused
# refs
    - render_surf_html@L21 calls L30:4@esc:String
    - write_surf_html@L26 calls L14:8@render_surf_html:String
    - embeds_report_and_title@L265 calls L14:8@render_surf_html:String
    - script_breakout_is_defused@L283 calls L14:8@render_surf_html:String
# note
