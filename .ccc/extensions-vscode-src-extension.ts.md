# extension.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/extension.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./binary (CccBinaryError)
    - L3@./codelens (CccCodeLensProvider)
    - L4@./commands (CommandHost, registerCommands)
    - L5@./config (Cfg, needsDecorationReload, readConfig)
    - L6@./decorations (DecorationSet)
    - L7@./hover (CccHoverProvider)
    - L8@./log (Log)
    - L9@./paths (isSupportedDocument, keyOf)
    - L10@./session (WorkspaceSession)
    - L11@./status (ActiveFileState, StatusBar)
    - L12@./complexitypanel (ComplexityPanel)
    - L13@./testpanel (TestTriggerPanel)
# const
    - L16@DIRTY_DEBOUNCE_MS
    - L18@FOCUS_COOLDOWN_MS
# funcs
    - L22:23@activate:Promise<void>
    - L27:23@deactivate:Promise<void>
    - L49:3@constructor
    - L82:9@start:Promise<void>
    - L148:17@sessionFor:Promise<WorkspaceSession | undefined> // sessions start lazily so a twelve-folder workspace does not spawn twelve analysers
    - L171:17@wake:Promise<void> // the lazy path starts the analyser from the active editor, which leaves the panels dead
    - L179:3@activeSession:WorkspaceSession | undefined
    - L192:3@sessions:WorkspaceSession[]
    - L196:11@reportStartFailure:void
    - L214:17@onConfigChanged:Promise<void>
    - L232:17@onActiveEditor:Promise<void>
    - L238:11@onSave:void
    - L252:11@onEdit:void // the analyser reads disk not the buffer so an unsaved edit only dims the hints
    - L267:11@onWindowState:void // catches git checkouts, codegen and edits made outside the editor
    - L277:11@onFoldersChanged:void
    - L287:9@render:Promise<void>
    - L339:11@lensSignature:string // everything a CodeLens is drawn from - equal signature means identical lenses
    - L352:11@clearAllDecorations:void
    - L356:3@refreshAll:void
    - L363:9@toggleHints:Promise<void>
    - L370:9@shutdown:Promise<void>
# refs
    - constructor@L73 calls L192:3@sessions:WorkspaceSession[]
    - constructor@L74 calls L171:17@wake:Promise<void>
    - constructor@L77 calls L192:3@sessions:WorkspaceSession[]
    - constructor@L78 calls L171:17@wake:Promise<void>
    - start@L99 calls L356:3@refreshAll:void
    - start@L132 calls L214:17@onConfigChanged:Promise<void>
    - start@L134 calls L232:17@onActiveEditor:Promise<void>
    - start@L135 calls L287:9@render:Promise<void>
    - start@L136 calls L238:11@onSave:void
    - start@L137 calls L252:11@onEdit:void
    - start@L138 calls L267:11@onWindowState:void
    - start@L139 calls L277:11@onFoldersChanged:void
    - start@L142 calls L232:17@onActiveEditor:Promise<void>
    - sessionFor@L159 calls L287:9@render:Promise<void>
    - sessionFor@L164 calls L196:11@reportStartFailure:void
    - wake@L176 calls L148:17@sessionFor:Promise<WorkspaceSession | undefined>
    - onConfigChanged@L227 calls L352:11@clearAllDecorations:void
    - onConfigChanged@L229 calls L287:9@render:Promise<void>
    - onActiveEditor@L234 calls L148:17@sessionFor:Promise<WorkspaceSession | undefined>
    - onActiveEditor@L235 calls L287:9@render:Promise<void>
    - onSave@L242 calls L287:9@render:Promise<void>
    - onEdit@L259 calls L287:9@render:Promise<void>
    - onEdit@L262 calls L287:9@render:Promise<void>
    - onFoldersChanged@L284 calls L287:9@render:Promise<void>
    - render@L288 calls L339:11@lensSignature:string
    - render@L324 calls L179:3@activeSession:WorkspaceSession | undefined
# note
