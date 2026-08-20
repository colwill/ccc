# config.ts.md (20260820-07-57-23) UTC
# source: extensions/vscode/src/config.ts [typescript]
# modules
# imports
    - L1@vscode (vscode)
    - L2@./log (TraceLevel)
# const
# funcs
    - L56:17@readConfig:Cfg // read the config for a scope - settings are per workspace folder so multi-root folders can differ
    - L107:17@needsServerRestart:boolean // settings that can only be honoured by restarting the analyser process
    - L118:17@needsRebuild:boolean // settings that change the hint index but not the payload - a rebuild from the cache is enough
    - L127:17@needsDecorationReload:boolean // settings that require the decoration types themselves to be recreated
    - L135:10@clampInt:number
# refs
    - readConfig@L66 calls L135:10@clampInt:number
    - readConfig@L67 calls L135:10@clampInt:number
    - readConfig@L68 calls L135:10@clampInt:number
    - readConfig@L88 calls L135:10@clampInt:number
    - readConfig@L94 calls L135:10@clampInt:number
    - readConfig@L99 calls L135:10@clampInt:number
    - readConfig@L100 calls L135:10@clampInt:number
# note
