# testpanel.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/testpanel.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./model (HintIndex, missingTestPhrase)
    - L3@./paths (absOf)
    - L4@./session (WorkspaceSession)
    - L5@./types (TestRun, TestTarget, TriggerAdd)
# const
# funcs
    - L16:3@constructor
    - L31:3@refresh:void // rebuild only when the analysis moved - firing the event would reset expansion state
    - L51:3@getTreeItem:vscode.TreeItem
    - L55:3@getChildren:Node[]
    - L89:3@dispose:void
    - L101:10@groupsFor:Node[]
    - L165:10@testNode:Node
    - L187:10@gapNode:Node
    - L205:10@splitTarget:[string, string]
    - L210:10@group:Node
    - L221:10@folder:Node
    - L227:10@message:Node
    - L234:10@describe:string
# refs
    - refresh@L48 calls L234:10@describe:string
    - getChildren@L58 calls L227:10@message:Node
    - getChildren@L64 calls L227:10@message:Node
    - getChildren@L69 calls L227:10@message:Node
    - getChildren@L79 calls L101:10@groupsFor:Node[]
    - getChildren@L81 calls L221:10@folder:Node
    - groupsFor@L108 calls L210:10@group:Node
    - groupsFor@L111 calls L165:10@testNode:Node
    - groupsFor@L121 calls L210:10@group:Node
    - groupsFor@L124 calls L187:10@gapNode:Node
    - groupsFor@L133 calls L210:10@group:Node
    - groupsFor@L155 calls L227:10@message:Node
    - gapNode@L188 calls L205:10@splitTarget:[string, string]
# note
