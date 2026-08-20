# codelens.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/codelens.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./config (Cfg)
    - L3@./model (addTestPhrase, Coverage, FileHints, Hot, InboundRef, LineHint, OutboundRef)
# const
# funcs
    - L16:3@constructor
    - L21:3@updateConfig:void
    - L27:3@refresh:void // announce new lenses - the signature no-ops an unchanged analysis so the lenses do not twitch
    - L33:9@provideCodeLenses:Promise<vscode.CodeLens[]>
    - L56:3@dispose:void
    - L61:10@lensesFor:vscode.Command[]
    - L79:10@coverageLens:vscode.Command | undefined
    - L103:10@outboundLenses:vscode.Command[]
    - L147:10@inboundLens:vscode.Command | undefined
    - L163:10@hotLens:vscode.Command | undefined
# refs
    - updateConfig@L23 calls L27:3@refresh:void
    - provideCodeLenses@L49 calls L61:10@lensesFor:vscode.Command[]
    - lensesFor@L66 calls L79:10@coverageLens:vscode.Command | undefined
    - lensesFor@L69 calls L103:10@outboundLenses:vscode.Command[]
    - lensesFor@L70 calls L147:10@inboundLens:vscode.Command | undefined
    - lensesFor@L73 calls L163:10@hotLens:vscode.Command | undefined
# note
