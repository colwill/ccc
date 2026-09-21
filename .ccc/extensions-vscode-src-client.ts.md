# client.ts.md (20260921-12-11-30) UTC
# source: extensions/vscode/src/client.ts [typescript]
# modules
# imports
    - L1@node:http (http)
    - L2@./log (Log)
    - L3@./server (ServerAddress)
    - L4@./types (FileStructure, Health, InsightsPayload, ReferencesResult, RefreshResult, VulnPayload)
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
    - L55:3@vulnerabilities:Promise<VulnPayload> // dependency advisories; the analyser caches per map generation so this is cheap to re-ask
    - L60:9@file:Promise<FileStructure | undefined> // pass the full repo-relative path - the server suffix-matches so a bare `money.rs` can mis-resolve
    - L73:3@references:Promise<ReferencesResult>
    - L81:3@refresh:Promise<RefreshResult>
    - L85:11@getJson:Promise<T>
    - L89:11@request:Promise<T>
    - L127:13@onAbort
    - L132:13@cleanup
    - L147:3@dispose:void
# refs
    - health@L46 calls L85:11@getJson:Promise<T>
    - insights@L51 calls L85:11@getJson:Promise<T>
    - vulnerabilities@L56 calls L85:11@getJson:Promise<T>
    - references@L74 calls L85:11@getJson:Promise<T>
    - refresh@L82 calls L89:11@request:Promise<T>
    - getJson@L86 calls L89:11@request:Promise<T>
    - request@L110 calls L132:13@cleanup
    - onAbort@L129 calls L132:13@cleanup
    - request@L139 calls L132:13@cleanup
# note
