# session.ts.md (20260921-12-11-30) UTC
# source: extensions/vscode/src/session.ts [typescript]
# modules
# imports
    - L1@node:path (path)
    - L2@vscode (vscode)
    - L3@./client (CccClient, isAborted)
    - L4@./config (Cfg, needsRebuild, needsServerRestart)
    - L5@./enclosing (FileStructureCache, refineFileHints)
    - L6@./log (describe, Log)
    - L7@./model (buildHintIndex, FileHints, HintIndex)
    - L8@./paths (keyOf, relOf)
    - L9@./server (ServerProcess, ServerState)
    - L10@./types (FileStructure, InsightsPayload, ReferencesResult, VulnPayload)
# const
# funcs
    - L39:3@constructor
    - L51:7@index:HintIndex | undefined
    - L55:7@vulnerabilities:VulnPayload | undefined
    - L59:7@serverState:ServerState
    - L63:7@root:vscode.Uri
    - L67:9@ensureStarted:Promise<void>
    - L81:17@waitForHealth:Promise<void> // the listening line lands before the worker threads exist so a request there can be refused
    - L98:3@updateConfig:void
    - L119:3@rebuild:void // rebuild the index from the cached payload - no network, no rescan
    - L129:3@schedule:void // coalesce triggers - the strongest request in the window wins
    - L147:9@refresh:Promise<void>
    - L214:9@hintsFor:Promise<FileHints | undefined> // hints for one file with the second pass applied - one small request per map generation
    - L227:9@structureFor:Promise<FileStructure | undefined> // one file's structure whatever the diff touched - measurements are not diff-driven
    - L236:9@isMapped:Promise<boolean> // whether a file is in the analyser's map at all
    - L243:9@locateExternal:Promise<vscode.Uri | undefined> // URI of a file in a peer repo - undefined when the peer is known only by its surface
    - L261:9@references:Promise<ReferencesResult>
    - L267:7@insightsUrl:string | undefined
    - L272:9@restartServer:Promise<void>
    - L283:3@stopServer:void
    - L293:11@onServerState:void
    - L301:11@startPoll:void
    - L310:11@stopPoll:void
    - L317:3@dispose:void
    - L328:10@sleep:Promise<void>
# refs
    - constructor@L48 calls L293:11@onServerState:void
    - ensureStarted@L74 calls L81:17@waitForHealth:Promise<void>
    - ensureStarted@L76 calls L129:3@schedule:void
    - ensureStarted@L77 calls L301:11@startPoll:void
    - waitForHealth@L93 calls L328:10@sleep:Promise<void>
    - updateConfig@L107 calls L67:9@ensureStarted:Promise<void>
    - updateConfig@L111 calls L129:3@schedule:void
    - updateConfig@L114 calls L119:3@rebuild:void
    - updateConfig@L115 calls L301:11@startPoll:void
    - schedule@L143 calls L147:9@refresh:Promise<void>
    - refresh@L149 calls L67:9@ensureStarted:Promise<void>
    - references@L262 calls L67:9@ensureStarted:Promise<void>
    - restartServer@L280 calls L67:9@ensureStarted:Promise<void>
    - stopServer@L284 calls L310:11@stopPoll:void
    - onServerState@L297 calls L67:9@ensureStarted:Promise<void>
    - startPoll@L302 calls L310:11@stopPoll:void
    - startPoll@L305 calls L129:3@schedule:void
    - dispose@L319 calls L310:11@stopPoll:void
# note
