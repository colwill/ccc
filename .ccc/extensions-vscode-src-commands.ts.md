# commands.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/commands.ts [typescript]
# modules
# imports
    - L1@node:child_process (execFile)
    - L2@vscode (vscode)
    - L3@./log (Log)
    - L4@./model (Coverage, InboundRef, missingTestPhrase, OutboundRef, TestLink)
    - L5@./paths (absOf)
    - L6@./session (WorkspaceSession)
# const
# funcs
    - L18:17@registerCommands:vscode.Disposable[]
    - L265:16@openRemote:Promise<void> // a peer's handler has no URI here unless it is checked out - try the local path first
    - L284:10@readTests:TestLink[]
    - L290:10@readCoverage:Coverage | undefined
    - L296:10@readOutbound:OutboundRef[]
    - L302:10@readInbound:InboundRef[]
    - L308:10@readLocation:{ file: string; line: number } | undefined
    - L317:10@readSymbol:string | undefined
    - L324:10@gitRefs:Promise<string[]>
# refs
    - registerCommands@L52 calls L308:10@readLocation:{ file: string; line: number } | undefined
    - registerCommands@L71 calls L317:10@readSymbol:string | undefined
    - registerCommands@L101 calls L284:10@readTests:TestLink[]
    - registerCommands@L134 calls L290:10@readCoverage:Coverage | undefined
    - registerCommands@L152 calls L296:10@readOutbound:OutboundRef[]
    - registerCommands@L159 calls L265:16@openRemote:Promise<void>
    - registerCommands@L169 calls L296:10@readOutbound:OutboundRef[]
    - registerCommands@L178 calls L302:10@readInbound:InboundRef[]
    - registerCommands@L241 calls L324:10@gitRefs:Promise<string[]>
# note
