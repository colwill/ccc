# binary.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/binary.ts [typescript]
# modules
# imports
    - L1@node:child_process (execFile)
    - L2@node:fs (fs)
    - L3@node:path (path)
    - L4@vscode (vscode)
    - L5@./config (Cfg)
    - L6@./log (Log)
# const
    - L26@EXE
# funcs
    - L17:3@constructor
    - L29:23@resolveCccBinary:Promise<BinaryResolution> // find a usable `ccc` binary - a broken `ccc.binaryPath` errors rather than silently falling through
    - L77:10@probe:Promise<string | undefined> // run `<bin> --version`; undefined means "not a usable ccc binary"
# refs
    - resolveCccBinary@L37 calls L77:10@probe:Promise<string | undefined>
    - resolveCccBinary@L50 calls L77:10@probe:Promise<string | undefined>
    - resolveCccBinary@L63 calls L77:10@probe:Promise<string | undefined>
# note
