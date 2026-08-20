# build.rs.md (20260820-07-57-23) UTC
# source: build.rs [rust]
# modules
# imports
    - L17@std::path (Path, PathBuf)
    - L18@std::process (Command)
    - L19@std::time (SystemTime)
# const
    - L23@INPUTS:&[&str]
    - L34@OUTPUT:&str
# funcs
    - L36:4@main
    - L62:4@skip_reason:Option<String>
    - L81:4@package:Result<(), String>
    - L123:4@stamp // Date the vsix by its newest source, not by the moment it was written.
    - L134:4@newest_mtime:Option<SystemTime>
    - L145:4@run:Result<(), String>
    - L169:4@npm:&'static str
    - L178:4@which:Option<PathBuf>
# refs
    - main@L51 calls L62:4@skip_reason:Option<String>
    - main@L56 calls L81:4@package:Result<(), String>
    - skip_reason@L75 calls L178:4@which:Option<PathBuf>
    - package@L90 calls L145:4@run:Result<(), String>
    - package@L93 calls L145:4@run:Result<(), String>
    - package@L94 calls L145:4@run:Result<(), String>
    - package@L108 calls L123:4@stamp
    - stamp@L126 calls L134:4@newest_mtime:Option<SystemTime>
    - newest_mtime@L141 calls L134:4@newest_mtime:Option<SystemTime>
    - run@L146 calls L169:4@npm:&'static str
# note
