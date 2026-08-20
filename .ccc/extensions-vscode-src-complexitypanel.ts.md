# complexitypanel.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/complexitypanel.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./model (ARITIES, HintIndex, SCORE_DESCRIPTION)
    - L3@./paths (absOf)
    - L4@./session (WorkspaceSession)
    - L5@./types (Arity, ComplexityRow)
# const
    - L212@ARITY_DESCRIPTION:Record<Arity, string>
    - L220@GLYPH
# funcs
    - L17:3@constructor
    - L33:3@refresh:void // rebuild only when the analysis moved so expansion state survives typing
    - L44:9@filterByName:Promise<void>
    - L55:9@filterByArity:Promise<void>
    - L73:9@filterByScore:Promise<void>
    - L74:11@pick:Promise<number | undefined>
    - L94:9@toggleTests:Promise<void>
    - L99:3@clearFilters:void
    - L108:11@apply:void
    - L114:11@publishFilterState:void
    - L118:11@isFiltered:boolean
    - L123:11@filterSignature:string
    - L128:3@getTreeItem:vscode.TreeItem
    - L132:3@getChildren:Node[]
    - L152:11@matching:ComplexityRow[]
    - L165:11@emptyMessage:Node // an empty list has two causes a reader cannot tell apart - nothing measured, or all filtered out
    - L177:11@describe:string
    - L192:3@dispose:void
    - L223:10@bands:Node[] // grouped by band rather than listed flat
    - L243:10@functionNode:Node
    - L264:10@folder:Node
    - L270:10@message:Node
# refs
    - constructor@L25 calls L114:11@publishFilterState:void
    - refresh@L36 calls L123:11@filterSignature:string
    - refresh@L41 calls L177:11@describe:string
    - filterByName@L52 calls L108:11@apply:void
    - filterByArity@L70 calls L108:11@apply:void
    - filterByScore@L84 calls L74:11@pick:Promise<number | undefined>
    - filterByScore@L86 calls L74:11@pick:Promise<number | undefined>
    - filterByScore@L91 calls L108:11@apply:void
    - toggleTests@L96 calls L108:11@apply:void
    - clearFilters@L105 calls L108:11@apply:void
    - apply@L109 calls L114:11@publishFilterState:void
    - apply@L111 calls L33:3@refresh:void
    - publishFilterState@L115 calls L118:11@isFiltered:boolean
    - getChildren@L135 calls L270:10@message:Node
    - getChildren@L141 calls L270:10@message:Node
    - getChildren@L144 calls L152:11@matching:ComplexityRow[]
    - getChildren@L145 calls L223:10@bands:Node[]
    - getChildren@L145 calls L165:11@emptyMessage:Node
    - getChildren@L146 calls L264:10@folder:Node
    - emptyMessage@L166 calls L270:10@message:Node
    - emptyMessage@L167 calls L118:11@isFiltered:boolean
    - emptyMessage@L168 calls L270:10@message:Node
    - emptyMessage@L174 calls L270:10@message:Node
    - describe@L180 calls L152:11@matching:ComplexityRow[]
    - describe@L188 calls L118:11@isFiltered:boolean
    - bands@L239 calls L243:10@functionNode:Node
# note
