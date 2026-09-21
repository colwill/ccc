# binary.ts.md (20260921-12-11-30) UTC
# source: extensions/vscode/src/binary.ts [typescript]
# modules
# imports
    - L1@node:child_process (execFile)
    - L2@node:fs (fs)
    - L3@node:path (path)
    - L4@vscode (vscode)
    - L5@./config (Cfg)
    - L6@./log (Log)
# const
    - L26@EXE
    - L29@REPO
    - L32@DOWNLOAD_TIMEOUT_MS
# funcs
    - L17:3@constructor
    - L39:23@resolveCccBinary:Promise<BinaryResolution> // find a usable `ccc` binary - a broken `ccc.binaryPath` errors rather than silently falling through
    - L63:23@bootstrapCccBinary:Promise<BinaryResolution> // The install-time path, run once when the extension activates: make sure a
    - L104:16@findCcc:Promise<BinaryResolution | undefined> // search the places a ccc may already be, in order of how much the user meant it.
    - L169:10@missingBinary:CccBinaryError
    - L181:10@note:void // the same place can be reached from several folders - say it once
    - L186:10@versionMatches:boolean // `ccc --version` prints "ccc <version>", so compare the version token alone
    - L193:10@managedPath:string // where a downloaded binary is kept: the extension's own storage, so installing
    - L198:10@assetName:string | undefined // the release asset for this machine, named as install.sh names it
    - L212:17@installCccBinary:Promise<BinaryResolution | undefined> // Download the matching release into extension storage. Resolves undefined on
    - L220:16@download:Promise<BinaryResolution | undefined>
    - L264:16@fetchAsset:Promise<Buffer> // fetch a release asset, following the redirect GitHub serves for `latest`
    - L273:10@probe:Promise<string | undefined> // run `<bin> --version`; undefined means "not a usable ccc binary"
# refs
    - resolveCccBinary@L47 calls L104:16@findCcc:Promise<BinaryResolution | undefined>
    - resolveCccBinary@L51 calls L212:17@installCccBinary:Promise<BinaryResolution | undefined>
    - resolveCccBinary@L53 calls L181:10@note:void
    - resolveCccBinary@L56 calls L169:10@missingBinary:CccBinaryError
    - bootstrapCccBinary@L74 calls L104:16@findCcc:Promise<BinaryResolution | undefined>
    - bootstrapCccBinary@L82 calls L186:10@versionMatches:boolean
    - bootstrapCccBinary@L87 calls L169:10@missingBinary:CccBinaryError
    - bootstrapCccBinary@L91 calls L212:17@installCccBinary:Promise<BinaryResolution | undefined>
    - bootstrapCccBinary@L98 calls L181:10@note:void
    - bootstrapCccBinary@L99 calls L169:10@missingBinary:CccBinaryError
    - findCcc@L120 calls L181:10@note:void
    - findCcc@L121 calls L273:10@probe:Promise<string | undefined>
    - findCcc@L133 calls L181:10@note:void
    - findCcc@L134 calls L273:10@probe:Promise<string | undefined>
    - findCcc@L146 calls L181:10@note:void
    - findCcc@L148 calls L273:10@probe:Promise<string | undefined>
    - findCcc@L157 calls L193:10@managedPath:string
    - findCcc@L158 calls L181:10@note:void
    - findCcc@L159 calls L273:10@probe:Promise<string | undefined>
    - installCccBinary@L214 calls L220:16@download:Promise<BinaryResolution | undefined>
    - download@L221 calls L198:10@assetName:string | undefined
    - download@L227 calls L193:10@managedPath:string
    - download@L239 calls L264:16@fetchAsset:Promise<Buffer>
    - download@L250 calls L273:10@probe:Promise<string | undefined>
# note
