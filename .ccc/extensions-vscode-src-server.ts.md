# server.ts.md (20260820-07-57-23) UTC
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
    - L59:7@state:ServerState
    - L63:7@address:ServerAddress | undefined
    - L67:3@updateConfig:void
    - L72:9@start:Promise<ServerAddress> // idempotent - returns the existing address when already running
    - L81:9@restart:Promise<ServerAddress>
    - L89:3@stop:void
    - L95:17@spawnAndWait:Promise<ServerAddress>
    - L143:11@awaitListening:Promise<ServerAddress>
    - L148:13@finish
    - L211:11@wireExit:void // restart with backoff when a healthy process dies unexpectedly
    - L250:11@setState:void
    - L255:11@clearRetry:void
    - L262:11@kill:void
    - L282:3@dispose:void
    - L292:17@parseListening:ServerAddress | undefined // exported for the port-parsing edge cases (IPv6, non-default hosts)
# refs
    - start@L75 calls L95:17@spawnAndWait:Promise<ServerAddress>
    - restart@L82 calls L255:11@clearRetry:void
    - restart@L84 calls L262:11@kill:void
    - restart@L85 calls L250:11@setState:void
    - restart@L86 calls L72:9@start:Promise<ServerAddress>
    - stop@L90 calls L255:11@clearRetry:void
    - stop@L91 calls L262:11@kill:void
    - stop@L92 calls L250:11@setState:void
    - spawnAndWait@L96 calls L250:11@setState:void
    - spawnAndWait@L106 calls L250:11@setState:void
    - spawnAndWait@L133 calls L143:11@awaitListening:Promise<ServerAddress>
    - spawnAndWait@L134 calls L211:11@wireExit:void
    - spawnAndWait@L135 calls L250:11@setState:void
    - awaitListening@L156 calls L148:13@finish
    - awaitListening@L175 calls L292:17@parseListening:ServerAddress | undefined
    - awaitListening@L177 calls L148:13@finish
    - awaitListening@L193 calls L148:13@finish
    - awaitListening@L197 calls L148:13@finish
    - wireExit@L231 calls L250:11@setState:void
    - wireExit@L239 calls L250:11@setState:void
    - wireExit@L241 calls L255:11@clearRetry:void
    - wireExit@L245 calls L72:9@start:Promise<ServerAddress>
    - dispose@L284 calls L255:11@clearRetry:void
    - dispose@L285 calls L262:11@kill:void
# note
