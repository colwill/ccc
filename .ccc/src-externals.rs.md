# externals.rs.md (20260820-07-57-23) UTC
# source: src/externals.rs [rust]
# modules
# imports
    - L20@crate::model (Boundary, FileCache)
    - L21@crate (scan)
    - L22@anyhow (bail, Context, Result)
    - L23@serde (Deserialize, Serialize)
    - L24@std::collections (BTreeMap, BTreeSet)
    - L25@std::path (Path, PathBuf)
    - L26@std::process (Command)
# const
    - L28@SURFACE_SCHEMA:&str
    - L30@SURFACE_NAME:&str
    - L32@MAX_SURFACE_BYTES:u64
    - L33@FETCH_TIMEOUT_SECS:u64
# funcs
    - L69:12@from_caches:Surface // derive a surface from an already-parsed tree
    - L109:8@parse:Result<Surface>
    - L154:12@json:serde_json::Value
    - L172:8@resolve_all:Vec<ExternalService> // Resolve every peer named in the config. Errors are captured per peer.
    - L182:4@resolve_one:ExternalService
    - L231:4@resolve_path:PathBuf
    - L241:4@surface_from_dir:Result<Surface> // Parse a peer checkout and reduce it to its surface.
    - L262:4@load_surface:Result<Surface> // Read a surface from a file, a directory holding one, or a URL.
    - L286:4@fetch:Result<String> // fetch over HTTP by shelling out to curl.
    - L326:4@read_auth:Result<String> // Only `env:NAME` is accepted: a literal token in a file that belongs in git
    - L350:8@index_by_key:BTreeMap<&str, Vec<&Endpoint>> // index a peers endpoints by key
    - L359:8@norm_key:String // normalise a key for matching
# refs
    - resolve_all@L178 calls L182:4@resolve_one:ExternalService
    - resolve_one@L194 calls L231:4@resolve_path:PathBuf
    - resolve_one@L197 calls L241:4@surface_from_dir:Result<Surface>
    - resolve_one@L212 calls L262:4@load_surface:Result<Surface>
    - load_surface@L264 calls L286:4@fetch:Result<String>
    - load_surface@L267 calls L231:4@resolve_path:PathBuf
    - fetch@L288 calls L326:4@read_auth:Result<String>
# note
