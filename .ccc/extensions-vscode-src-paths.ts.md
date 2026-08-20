# paths.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/paths.ts [typescript]
# modules
# imports
    - L1@node:path (path)
    - L2@vscode (vscode)
    - L3@./pathkeys (keyOfPath)
# const
# funcs
    - L8:17@relOf:string | undefined // a repo-relative, '/'-separated path as the analyser emits them - undefined outside the folder
    - L17:17@absOf:vscode.Uri // resolve a repo-relative analyser path back to an absolute URI
    - L22:17@keyOf:string // the index key for a document
    - L27:17@isSupportedDocument:boolean // only real files on disk get hints - `untitled:` is never in the map and `git:` is another revision
# refs
# note
