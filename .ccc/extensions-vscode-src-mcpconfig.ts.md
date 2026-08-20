# mcpconfig.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/mcpconfig.ts [typescript]
# modules
# imports
    - L1@node:fs (promises, fs)
    - L2@node:path (path)
    - L3@vscode (vscode)
    - L4@./log (describe, Log)
    - L5@./server (ServerAddress)
# const
    - L8@SERVER_KEY
    - L18@TARGETS:readonly Target[]
    - L24@MALFORMED
# funcs
    - L28:23@publishMcpConfig:Promise<void> // publish the live endpoint so agents can discover the analyser this window started
    - L47:16@writeOne:Promise<boolean> // returns true when the file was rewritten, false when it already said the right thing
    - L75:16@readJson:Promise<Record<string, unknown> | undefined | typeof MALFORMED>
    - L94:10@asRecord:Record<string, unknown> | undefined
# refs
    - publishMcpConfig@L37 calls L47:16@writeOne:Promise<boolean>
    - writeOne@L48 calls L75:16@readJson:Promise<Record<string, unknown> | undefined | typeof MALFORMED>
    - writeOne@L54 calls L94:10@asRecord:Record<string, unknown> | undefined
    - writeOne@L56 calls L94:10@asRecord:Record<string, unknown> | undefined
    - readJson@L88 calls L94:10@asRecord:Record<string, unknown> | undefined
# note
