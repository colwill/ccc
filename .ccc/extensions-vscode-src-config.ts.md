# config.ts.md (20260921-12-11-30) UTC
# source: extensions/vscode/src/config.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./log (TraceLevel)
# const
# funcs
    - L58:17@readConfig:Cfg // read the config for a scope - settings are per workspace folder so multi-root folders can differ
    - L110:17@needsServerRestart:boolean // settings that can only be honoured by restarting the analyser process
    - L121:17@needsRebuild:boolean // settings that change the hint index but not the payload - a rebuild from the cache is enough
    - L130:17@needsDecorationReload:boolean // settings that require the decoration types themselves to be recreated
    - L138:10@clampInt:number
# refs
    - readConfig@L69 calls L138:10@clampInt:number
    - readConfig@L70 calls L138:10@clampInt:number
    - readConfig@L71 calls L138:10@clampInt:number
    - readConfig@L91 calls L138:10@clampInt:number
    - readConfig@L97 calls L138:10@clampInt:number
    - readConfig@L102 calls L138:10@clampInt:number
    - readConfig@L103 calls L138:10@clampInt:number
# note
