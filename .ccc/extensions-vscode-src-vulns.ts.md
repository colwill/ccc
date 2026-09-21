# vulns.ts.md (20260921-12-11-30) UTC
# source: extensions/vscode/src/vulns.ts [typescript]
# modules
# imports
    - L1@node:path (path)
    - L2@vscode (vscode)
    - L3@./types (VulnFinding, VulnPayload)
# const
    - L6@ORDER
    - L20@MAX_LISTED
# funcs
    - L8:10@rank:number
    - L54:3@constructor
    - L64:3@update:boolean // returns true when the marks actually changed
    - L89:3@applyAll:void // draw every visible editor that has marks
    - L93:3@apply:void
    - L131:11@hoverFor:vscode.Hover | undefined
    - L157:3@dispose:void
    - L164:10@sorted:VulnFinding[]
    - L169:10@packagesOf:string[] // the packages a line is answerable for, named so the badge says what is wrong
    - L173:10@badgeText:string
    - L182:10@headline:string
    - L192:10@signatureOf:string // identity of the whole mark set, so an unchanged payload redraws nothing
# refs
    - constructor@L58 calls L131:11@hoverFor:vscode.Hover | undefined
    - update@L81 calls L192:10@signatureOf:string
    - applyAll@L90 calls L93:3@apply:void
    - apply@L120 calls L173:10@badgeText:string
    - hoverFor@L139 calls L182:10@headline:string
    - hoverFor@L141 calls L164:10@sorted:VulnFinding[]
    - sorted@L165 calls L8:10@rank:number
    - badgeText@L174 calls L169:10@packagesOf:string[]
    - headline@L185 calls L169:10@packagesOf:string[]
# note
