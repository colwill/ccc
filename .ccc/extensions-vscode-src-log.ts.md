# log.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/log.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
# const
    - L5@RANK:Record<TraceLevel, number>
# funcs
    - L12:3@constructor
    - L16:3@setLevel:void
    - L20:3@show:void
    - L25:3@error:void // always written
    - L30:3@warn:void // always written
    - L35:3@info:void // written at `messages` and above
    - L40:3@trace:void // written at `verbose` only
    - L45:3@server:void // raw analyser output prefixed so it is distinguishable from our own lines
    - L54:11@write:void
    - L58:3@dispose:void
    - L63:10@stamp:string
    - L65:9@p
    - L69:17@describe:string
# refs
    - error@L26 calls L69:17@describe:string
    - error@L26 calls L54:11@write:void
    - warn@L31 calls L54:11@write:void
    - info@L36 calls L54:11@write:void
    - trace@L41 calls L54:11@write:void
    - server@L49 calls L54:11@write:void
    - server@L50 calls L40:3@trace:void
    - write@L55 calls L63:10@stamp:string
    - stamp@L66 calls L65:9@p
# note
