# html.rs.md (20260820-07-57-23) UTC
# source: src/html.rs [rust]
# modules
# imports
    - L8@anyhow (Context, Result)
    - L9@serde_json (Value)
    - L10@std::path (Path)
    - L1080@super
    - L1081@serde_json (json)
# const
    - L37@TEMPLATE:&str
    - L305@INSIGHTS_TEMPLATE:&str
# funcs
    - L14:8@render_changes_html:String // render the single-file report page. `title` names the report (by
    - L25:8@write_changes_html:Result<()>
    - L30:4@esc:String
    - L281:8@render_insights_html:String // Render the `/insights` page; two modes from one template.
    - L296:8@write_insights_html:Result<()> // Write the self-contained page for static hosting, creating the output
    - L1084:8@embeds_report_and_title
    - L1108:8@static_insights_page_embeds_its_data_and_drops_the_server_controls // A statically exported page has to stand on its own: the analysis is
    - L1135:8@every_template_labels_the_implicit_root_service // Both pages render service names
    - L1150:8@script_breakout_is_defused
# refs
    - render_changes_html@L21 calls L30:4@esc:String
    - write_changes_html@L26 calls L14:8@render_changes_html:String
    - render_insights_html@L290 calls L30:4@esc:String
    - write_insights_html@L301 calls L281:8@render_insights_html:String
    - embeds_report_and_title@L1090 calls L14:8@render_changes_html:String
    - static_insights_page_embeds_its_data_and_drops_the_server_controls@L1115 calls L281:8@render_insights_html:String
    - static_insights_page_embeds_its_data_and_drops_the_server_controls@L1128 calls L281:8@render_insights_html:String
    - script_breakout_is_defused@L1152 calls L14:8@render_changes_html:String
# note
