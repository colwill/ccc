# status.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/status.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./model (HintIndex)
    - L3@./server (ServerState)
# const
# funcs
    - L24:3@constructor
    - L30:3@hide:void
    - L37:3@update:void // writes only on a real change - assigning to a StatusBarItem re-renders it and strobes any open hover
    - L49:3@dispose:void
    - L54:10@render:{ text: string; tooltip: vscode.MarkdownString; background?: string; }
# refs
    - update@L38 calls L54:10@render:{ text: string; tooltip: vscode.MarkdownString; background?: string; }
# note
