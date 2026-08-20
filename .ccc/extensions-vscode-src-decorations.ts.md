# decorations.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/decorations.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./config (Cfg)
    - L3@./model (Anchor, FileHints, HintIndex, HintKind, LineHint)
    - L4@./types (FileStructure)
# const
    - L6@KINDS:HintKind[]
    - L10@GUTTER_ICON:Partial<Record<HintKind, string>>
    - L17@BADGE_COLOR:Record<HintKind, string>
    - L29@COMPLEXITY_GLYPH
    - L33@COMPLEXITY_COLOR
# funcs
    - L53:3@constructor
    - L60:3@reload:void
    - L66:11@create:void
    - L106:11@icon:vscode.Uri
    - L112:3@apply:void // every type is set on every apply including the empty ones
    - L144:3@applyComplexity:void
    - L176:3@clear:void
    - L181:11@disposeTypes:void
    - L188:3@dispose:void
    - L193:17@anchorToRange:vscode.Range | undefined
# refs
    - constructor@L57 calls L66:11@create:void
    - reload@L62 calls L181:11@disposeTypes:void
    - reload@L63 calls L66:11@create:void
    - create@L93 calls L106:11@icon:vscode.Uri
    - create@L94 calls L106:11@icon:vscode.Uri
    - apply@L118 calls L193:17@anchorToRange:vscode.Range | undefined
    - applyComplexity@L155 calls L193:17@anchorToRange:vscode.Range | undefined
    - dispose@L189 calls L181:11@disposeTypes:void
# note
