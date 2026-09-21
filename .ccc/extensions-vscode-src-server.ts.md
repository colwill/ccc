# server.ts.md (20260921-12-11-30) UTC
# source: extensions/vscode/src/server.ts [typescript]
# modules
# imports
    - L1@node:child_process (ChildProcessByStdio, spawn)
    - L2@node:stream (Readable)
    - L3@vscode (vscode)
    - L4@./binary (CccBinaryError, resolveCccBinary)
    - L5@./config (Cfg)
    - L6@./log (describe, Log)
    - L7@./mcpconfig (publishMcpConfig)
# const
    - L23@LISTENING
    - L28@BACKOFF_MS
    - L29@MAX_FAILURES
    - L30@FAILURE_WINDOW_MS
    - L31@STABLE_UPTIME_MS
    - L32@STDERR_TAIL
# funcs
    - L48:3@constructor
    - L61:7@state:ServerState
    - L65:7@address:ServerAddress | undefined
    - L69:3@updateConfig:void
    - L74:9@start:Promise<ServerAddress> // idempotent - returns the existing address when already running
    - L83:9@restart:Promise<ServerAddress>
    - L91:3@stop:void
    - L97:17@spawnAndWait:Promise<ServerAddress>
    - L145:11@awaitListening:Promise<ServerAddress>
    - L150:13@finish
    - L213:11@wireExit:void // restart with backoff when a healthy process dies unexpectedly
    - L252:11@setState:void
    - L257:11@clearRetry:void
    - L264:11@kill:void
    - L284:3@dispose:void
    - L294:17@parseListening:ServerAddress | undefined // exported for the port-parsing edge cases (IPv6, non-default hosts)
# refs
    - start@L77 calls L97:17@spawnAndWait:Promise<ServerAddress>
    - restart@L84 calls L257:11@clearRetry:void
    - restart@L86 calls L264:11@kill:void
    - restart@L87 calls L252:11@setState:void
    - restart@L88 calls L74:9@start:Promise<ServerAddress>
    - stop@L92 calls L257:11@clearRetry:void
    - stop@L93 calls L264:11@kill:void
    - stop@L94 calls L252:11@setState:void
    - spawnAndWait@L98 calls L252:11@setState:void
    - spawnAndWait@L108 calls L252:11@setState:void
    - spawnAndWait@L135 calls L145:11@awaitListening:Promise<ServerAddress>
    - spawnAndWait@L136 calls L213:11@wireExit:void
    - spawnAndWait@L137 calls L252:11@setState:void
    - awaitListening@L158 calls L150:13@finish
    - awaitListening@L177 calls L294:17@parseListening:ServerAddress | undefined
    - awaitListening@L179 calls L150:13@finish
    - awaitListening@L195 calls L150:13@finish
    - awaitListening@L199 calls L150:13@finish
    - wireExit@L233 calls L252:11@setState:void
    - wireExit@L241 calls L252:11@setState:void
    - wireExit@L243 calls L257:11@clearRetry:void
    - wireExit@L247 calls L74:9@start:Promise<ServerAddress>
    - dispose@L286 calls L257:11@clearRetry:void
    - dispose@L287 calls L264:11@kill:void
# note
