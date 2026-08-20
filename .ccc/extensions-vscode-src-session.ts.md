# session.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/session.ts [typescript]
# modules
# imports
    - L1@node:path (path)
    - L2@vscode (vscode)
    - L3@./client (CccClient, isAborted)
    - L4@./config (Cfg, needsRebuild, needsServerRestart)
    - L5@./enclosing (FileStructureCache, refineFileHints)
    - L6@./log (Log)
    - L7@./model (buildHintIndex, FileHints, HintIndex)
    - L8@./paths (keyOf, relOf)
    - L9@./server (ServerProcess, ServerState)
    - L10@./types (FileStructure, InsightsPayload, ReferencesResult)
# const
# funcs
    - L38:3@constructor
    - L48:7@index:HintIndex | undefined
    - L52:7@serverState:ServerState
    - L56:7@root:vscode.Uri
    - L60:9@ensureStarted:Promise<void>
    - L74:17@waitForHealth:Promise<void> // the listening line lands before the worker threads exist so a request there can be refused
    - L91:3@updateConfig:void
    - L112:3@rebuild:void // rebuild the index from the cached payload - no network, no rescan
    - L122:3@schedule:void // coalesce triggers - the strongest request in the window wins
    - L140:9@refresh:Promise<void>
    - L187:9@hintsFor:Promise<FileHints | undefined> // hints for one file with the second pass applied - one small request per map generation
    - L200:9@structureFor:Promise<FileStructure | undefined> // one file's structure whatever the diff touched - measurements are not diff-driven
    - L209:9@isMapped:Promise<boolean> // whether a file is in the analyser's map at all
    - L216:9@locateExternal:Promise<vscode.Uri | undefined> // URI of a file in a peer repo - undefined when the peer is known only by its surface
    - L234:9@references:Promise<ReferencesResult>
    - L240:7@insightsUrl:string | undefined
    - L245:9@restartServer:Promise<void>
    - L256:3@stopServer:void
    - L266:11@onServerState:void
    - L274:11@startPoll:void
    - L283:11@stopPoll:void
    - L290:3@dispose:void
    - L301:10@sleep:Promise<void>
# refs
    - constructor@L45 calls L266:11@onServerState:void
    - ensureStarted@L67 calls L74:17@waitForHealth:Promise<void>
    - ensureStarted@L69 calls L122:3@schedule:void
    - ensureStarted@L70 calls L274:11@startPoll:void
    - waitForHealth@L86 calls L301:10@sleep:Promise<void>
    - updateConfig@L100 calls L60:9@ensureStarted:Promise<void>
    - updateConfig@L104 calls L122:3@schedule:void
    - updateConfig@L107 calls L112:3@rebuild:void
    - updateConfig@L108 calls L274:11@startPoll:void
    - schedule@L136 calls L140:9@refresh:Promise<void>
    - refresh@L142 calls L60:9@ensureStarted:Promise<void>
    - references@L235 calls L60:9@ensureStarted:Promise<void>
    - restartServer@L253 calls L60:9@ensureStarted:Promise<void>
    - stopServer@L257 calls L283:11@stopPoll:void
    - onServerState@L270 calls L60:9@ensureStarted:Promise<void>
    - startPoll@L275 calls L283:11@stopPoll:void
    - startPoll@L278 calls L122:3@schedule:void
    - dispose@L292 calls L283:11@stopPoll:void
# note
