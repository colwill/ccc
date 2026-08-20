# client.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/client.ts [typescript]
# modules
# imports
    - L1@node:http (http)
    - L2@./log (Log)
    - L3@./server (ServerAddress)
    - L4@./types (FileStructure, Health, InsightsPayload, ReferencesResult, RefreshResult)
# const
    - L28@TIMEOUT_FAST_MS
    - L30@TIMEOUT_SLOW_MS
# funcs
    - L7:3@constructor
    - L18:3@constructor
    - L24:17@isAborted:boolean
    - L36:3@constructor
    - L45:3@health:Promise<Health>
    - L49:3@insights:Promise<InsightsPayload>
    - L55:9@file:Promise<FileStructure | undefined> // pass the full repo-relative path - the server suffix-matches so a bare `money.rs` can mis-resolve
    - L68:3@references:Promise<ReferencesResult>
    - L76:3@refresh:Promise<RefreshResult>
    - L80:11@getJson:Promise<T>
    - L84:11@request:Promise<T>
    - L122:13@onAbort
    - L127:13@cleanup
    - L142:3@dispose:void
# refs
    - health@L46 calls L80:11@getJson:Promise<T>
    - insights@L51 calls L80:11@getJson:Promise<T>
    - references@L69 calls L80:11@getJson:Promise<T>
    - refresh@L77 calls L84:11@request:Promise<T>
    - getJson@L81 calls L84:11@request:Promise<T>
    - request@L105 calls L127:13@cleanup
    - onAbort@L124 calls L127:13@cleanup
    - request@L134 calls L127:13@cleanup
# note
