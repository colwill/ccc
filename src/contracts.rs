//! gRPC contracts: the rpcs a `.proto` schema declares, and the code in every
//! other language that implements or calls them.

use crate::changes::{module_segments, path_str};
use crate::externals::Endpoint;
use crate::languages::Language;
use crate::model::FileCache;
use globset::{GlobBuilder, GlobMatcher};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

pub const TRANSPORT: &str = "grpc";
// how an endpoint derived here is told apart from a `ccc:serves` comment
pub const VIA: &str = "rpc";

// what a generated stub type is called after its service's name: go
// `BillingClient`, python `BillingStub`, grpc-web `BillingPromiseClient`, java
// `BillingBlockingStub`
const STUB_SUFFIXES: &[&str] = &[
    "client",
    "stub",
    "asyncstub",
    "blockingstub",
    "futurestub",
    "promiseclient",
    "asyncclient",
    "grpcclient",
    "clientimpl",
];
// what a generated module adds to its schema's file stem: `billing_pb2_grpc`,
// `billing_grpc_pb`, `billing_connect`, tonic's `billing_client`, java
// `BillingGrpc`
const GENERATED_MARKERS: &[&str] = &[
    "pb", "grpc", "connect", "twirp", "proto", "client", "server", "servicer", "stub",
];

// one rpc of one service
#[derive(Debug, Clone)]
pub struct Contract {
    // the wire name: `acme.billing.v1.Billing/CreateInvoice`
    pub key: String,
    pub package: String,
    pub service: String,
    pub method: String,
    pub request: Option<String>,
    pub response: Option<String>,
    // the schema it is declared in, relative to the project root
    pub file: String,
    pub line: usize,
    // (file, func) in the project's caches; `None` for a schema from `contracts`
    pub def: Option<(usize, usize)>,
    stem: String,
}

// How code was tied to an rpc. Strongest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LinkEvidence {
    // a handler that takes the rpc's request message
    RequestType,
    // a call through the service's generated stub
    StubType,
    // a handler on a type named after the service, beside an import of the
    // generated code
    ServiceOwner,
    // a call in a file that imports the generated code
    GeneratedImport,
}

impl LinkEvidence {
    pub fn label(self) -> &'static str {
        match self {
            LinkEvidence::RequestType => "request-type",
            LinkEvidence::StubType => "stub-type",
            LinkEvidence::ServiceOwner => "service-owner",
            LinkEvidence::GeneratedImport => "generated-import",
        }
    }
}

// a function that implements an rpc
#[derive(Debug, Clone)]
pub struct Handler {
    pub contract: usize,
    // (file, func) in the project's caches
    pub def: (usize, usize),
    pub evidence: LinkEvidence,
}

// a call site that invokes an rpc
#[derive(Debug, Clone)]
pub struct Caller {
    pub contract: usize,
    pub file: usize,
    // index into that file's `calls`
    pub call: usize,
    pub evidence: LinkEvidence,
}

#[derive(Debug, Default)]
pub struct ContractIndex {
    pub contracts: Vec<Contract>,
    // the schemas from outside the project it was built with, kept so a peer
    // repository is linked through the same ones
    pub schemas: Vec<FileCache>,
    pub handlers: Vec<Handler>,
    pub callers: Vec<Caller>,
    by_call: HashMap<(usize, usize), usize>,
    by_def: HashMap<(usize, usize), usize>,
}

impl ContractIndex {
    // the index for a project: its own schemas plus the ones `.ccc/map.json`
    // names under `contracts`
    pub fn for_root(root: &Path, caches: &[FileCache]) -> ContractIndex {
        let patterns = crate::changes::ChangesConfig::load(root)
            .map(|c| c.contracts)
            .unwrap_or_default();
        ContractIndex::build(caches, &load_schemas(root, &patterns))
    }

    pub fn build(caches: &[FileCache], schemas: &[FileCache]) -> ContractIndex {
        let mut contracts = Vec::new();
        for (fi, c) in caches.iter().enumerate() {
            declare(c, Some(fi), &mut contracts);
        }
        for c in schemas {
            if !caches.iter().any(|p| p.rel_path == c.rel_path) {
                declare(c, None, &mut contracts);
            }
        }
        // one schema vendored into two places is still one contract; the
        // project's own copy was declared first and wins
        let mut seen = std::collections::BTreeSet::new();
        contracts.retain(|c: &Contract| seen.insert(c.key.clone()));

        let mut by_fold: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, c) in contracts.iter().enumerate() {
            by_fold.entry(fold(&c.method)).or_default().push(i);
        }
        // (owning type, method) the project defines - a call through one of
        // those is a local method call, whatever it is named
        let mut project_methods = std::collections::BTreeSet::new();
        for c in caches {
            for f in &c.funcs {
                if let Some(o) = &f.owner {
                    project_methods.insert((o.as_str(), f.name.as_str()));
                }
            }
        }

        let mut idx = ContractIndex { contracts, schemas: schemas.to_vec(), ..Default::default() };
        if by_fold.is_empty() {
            return idx;
        }
        for (fi, cache) in caches.iter().enumerate() {
            if cache.language == Language::Proto || is_generated(&cache.rel_path) {
                continue;
            }
            let generated: Vec<bool> =
                idx.contracts.iter().map(|c| imports_generated(cache, c)).collect();

            for (ki, f) in cache.funcs.iter().enumerate() {
                let Some(cands) = by_fold.get(&fold(&f.name)) else { continue };
                let best = pick(cands.iter().filter_map(|&ci| {
                    let c = &idx.contracts[ci];
                    // a client wrapper shaped like the rpc is not a handler of it
                    if f.owner.as_deref().is_some_and(|o| is_stub(o, &c.service)) {
                        return None;
                    }
                    if c.request.as_ref().is_some_and(|r| f.param_types.contains(r)) {
                        return Some((ci, LinkEvidence::RequestType));
                    }
                    let owned = f.owner.as_deref().is_some_and(|o| {
                        o.to_ascii_lowercase().contains(&c.service.to_ascii_lowercase())
                    });
                    (owned && generated[ci]).then_some((ci, LinkEvidence::ServiceOwner))
                }));
                if let Some((contract, evidence)) = best {
                    idx.by_def.insert((fi, ki), idx.handlers.len());
                    idx.handlers.push(Handler { contract, def: (fi, ki), evidence });
                }
            }

            for (ci_call, call) in cache.calls.iter().enumerate() {
                let Some(cands) = by_fold.get(&fold(&call.name)) else { continue };
                let best = pick(cands.iter().filter_map(|&ci| {
                    let c = &idx.contracts[ci];
                    if let Some(t) = call.recv_type.as_deref() {
                        if is_stub(t, &c.service) {
                            return Some((ci, LinkEvidence::StubType));
                        }
                        // the project's own method on a type it defines
                        if project_methods.contains(&(t, call.name.as_str())) {
                            return None;
                        }
                    }
                    generated[ci].then_some((ci, LinkEvidence::GeneratedImport))
                }));
                if let Some((contract, evidence)) = best {
                    idx.by_call.insert((fi, ci_call), idx.callers.len());
                    idx.callers.push(Caller { contract, file: fi, call: ci_call, evidence });
                }
            }
        }
        idx
    }

    // the rpc a call site invokes, if it invokes one
    pub fn caller(&self, file: usize, call: usize) -> Option<&Caller> {
        self.by_call.get(&(file, call)).map(|&i| &self.callers[i])
    }

    // the rpc a definition implements, if it implements one
    pub fn handler(&self, def: (usize, usize)) -> Option<&Handler> {
        self.by_def.get(&def).map(|&i| &self.handlers[i])
    }

    pub fn handlers_of(&self, contract: usize) -> impl Iterator<Item = &Handler> {
        self.handlers.iter().filter(move |h| h.contract == contract)
    }

    pub fn callers_of(&self, contract: usize) -> impl Iterator<Item = &Caller> {
        self.callers.iter().filter(move |c| c.contract == contract)
    }

    // Every definition a call to `contract` reaches: the rpc in the schema
    // when the project holds it, and each handler.
    pub fn targets(&self, contract: usize) -> Vec<(usize, usize)> {
        self.contracts[contract]
            .def
            .into_iter()
            .chain(self.handlers_of(contract).map(|h| h.def))
            .collect()
    }

    // The rpcs a lookup names: `CreateInvoice`, `create_invoice`,
    // `Billing.CreateInvoice`, `acme.billing.v1.Billing/CreateInvoice`. A
    // qualifier has to name the service or a segment of its package.
    pub fn matching(&self, name: &str, qualifier: Option<&str>) -> Vec<usize> {
        let folded = fold(name);
        self.contracts
            .iter()
            .enumerate()
            .filter(|(_, c)| fold(&c.method) == folded)
            .filter(|(_, c)| {
                qualifier.map_or(true, |q| {
                    module_segments(q).all(|seg| {
                        seg.eq_ignore_ascii_case(&c.service)
                            || c.package.split('.').any(|p| p.eq_ignore_ascii_case(seg))
                    })
                })
            })
            .map(|(i, _)| i)
            .collect()
    }

    // The boundary endpoints this project serves and calls, for its surface:
    // what lets a peer in another repository link to it without a comment
    // written at either end.
    pub fn endpoints(&self, caches: &[FileCache]) -> (Vec<Endpoint>, Vec<Endpoint>) {
        let endpoint = |contract: usize, file: usize, function: &str, line: usize| Endpoint {
            key: self.contracts[contract].key.clone(),
            transport: TRANSPORT.to_string(),
            function: function.to_string(),
            file: path_str(&caches[file].rel_path),
            line,
            service: None,
            via: Some(VIA.to_string()),
        };
        let provides = self
            .handlers
            .iter()
            .map(|h| {
                let f = &caches[h.def.0].funcs[h.def.1];
                endpoint(h.contract, h.def.0, &f.name, f.line)
            })
            .collect();
        let consumes = self
            .callers
            .iter()
            .map(|c| {
                let site = &caches[c.file].calls[c.call];
                endpoint(c.contract, c.file, &site.caller, site.line)
            })
            .collect();
        (provides, consumes)
    }
}

// the strongest link, or none when two rpcs tie for it
fn pick(links: impl Iterator<Item = (usize, LinkEvidence)>) -> Option<(usize, LinkEvidence)> {
    let mut links: Vec<(usize, LinkEvidence)> = links.collect();
    links.sort_by_key(|&(_, e)| e);
    match links[..] {
        [] => None,
        [only] => Some(only),
        [(a, ea), (b, eb), ..] => (a == b || ea < eb).then_some((a, ea)),
    }
}

// every rpc a schema file declares
fn declare(cache: &FileCache, fi: Option<usize>, out: &mut Vec<Contract>) {
    if cache.language != Language::Proto {
        return;
    }
    let package = cache.modules.first().cloned().unwrap_or_default();
    let stem = cache
        .rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    for (ki, f) in cache.funcs.iter().enumerate() {
        let Some(service) = f.owner.clone() else { continue };
        let key = if package.is_empty() {
            format!("{service}/{}", f.name)
        } else {
            format!("{package}.{service}/{}", f.name)
        };
        out.push(Contract {
            key,
            package: package.clone(),
            service,
            method: f.name.clone(),
            request: f.param_types.first().cloned(),
            response: f.ret.as_deref().map(crate::extract::normalize_type),
            file: path_str(&cache.rel_path),
            line: f.line,
            def: fi.map(|fi| (fi, ki)),
            stem: stem.clone(),
        });
    }
}

// an rpc's name as every language's generator spells it: `CreateInvoice`,
// `createInvoice` and `create_invoice` are one method
fn fold(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

// is `ty` the generated stub for `service`
fn is_stub(ty: &str, service: &str) -> bool {
    let ty = ty.to_ascii_lowercase();
    ty.strip_prefix(&service.to_ascii_lowercase())
        .is_some_and(|rest| STUB_SUFFIXES.contains(&rest))
}

// Does this file import code generated from the contract's schema? Either a
// module named after the schema file (`billing_pb2_grpc`, `billing_grpc_pb`),
// or one whose path ends in the schema's package (`.../billing/v1`,
// `acme.billing.v1`, `Acme.Billing.V1`).
fn imports_generated(cache: &FileCache, c: &Contract) -> bool {
    let stem = c.stem.to_ascii_lowercase();
    let pkg: Vec<String> = c.package.split('.').map(|s| s.to_ascii_lowercase()).collect();
    // one segment names too little to tell a generated package from any other
    let tail = (pkg.len() >= 2).then(|| &pkg[pkg.len() - 2..]);
    cache.imports.iter().any(|imp| {
        let segs: Vec<String> = module_segments(&imp.module).map(|s| s.to_ascii_lowercase()).collect();
        let by_package = tail.is_some_and(|t| segs.windows(t.len()).any(|w| w == t));
        let by_stem = segs
            .iter()
            .map(String::as_str)
            .chain(imp.names.iter().map(String::as_str))
            .any(|seg| {
                seg.to_ascii_lowercase()
                    .strip_prefix(&stem)
                    .is_some_and(|rest| GENERATED_MARKERS.iter().any(|m| rest.contains(m)))
            });
        by_package || by_stem
    })
}

// Code protoc wrote. It defines every rpc and calls none, so reading it as
// handlers would link every contract to its own stubs.
pub fn is_generated(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let stem = name.split('.').next().unwrap_or(&name);
    name.contains(".pb.")
        || name.contains(".connect.")
        || name.contains(".twirp.")
        || name.contains(".pb2")
        || [
            "_pb2", "_pb2_grpc", "_pb", "_grpc_pb", "_grpc_web_pb", "_connect", "_connectweb",
        ]
        .iter()
        .any(|s| stem.ends_with(s))
        // c# puts the stubs in `BillingGrpc.cs`
        || (name.ends_with(".cs") && stem.ends_with("grpc"))
}

// Parse the schemas `contracts` names. A pattern is a file, a directory (every
// `.proto` under it), or a glob; relative ones resolve against `root`, and may
// leave it (`../protos/**/*.proto`). Ignore files are not consulted: vendored
// schemas are routinely ignored, and naming them here is the opt-in.
pub fn load_schemas(root: &Path, patterns: &[String]) -> Vec<FileCache> {
    let mut files: Vec<PathBuf> = Vec::new();
    for pattern in patterns {
        let (base, glob) = split_glob(pattern);
        let base = if base.is_absolute() { base } else { root.join(base) };
        let matcher: Option<GlobMatcher> = glob.and_then(|g| {
            GlobBuilder::new(&g)
                .literal_separator(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        });
        if base.is_file() {
            files.push(base);
            continue;
        }
        for dent in ignore::WalkBuilder::new(&base).standard_filters(false).build().flatten() {
            let path = dent.path();
            if Language::from_path(path) != Some(Language::Proto) || !path.is_file() {
                continue;
            }
            let rel = path.strip_prefix(&base).unwrap_or(path);
            if matcher.as_ref().map_or(true, |m| m.is_match(rel)) {
                files.push(path.to_path_buf());
            }
        }
    }
    files.sort();
    files.dedup();
    crate::scan::build_caches(root, &files)
}

// `../protos/acme/**/*.proto` -> (`../protos/acme`, `**/*.proto`)
fn split_glob(pattern: &str) -> (PathBuf, Option<String>) {
    let mut base = PathBuf::new();
    let mut rest: Vec<String> = Vec::new();
    for comp in Path::new(pattern).components() {
        let s = comp.as_os_str().to_string_lossy().to_string();
        let globby = s.contains(['*', '?', '[', '{']);
        if rest.is_empty() && !globby {
            match comp {
                Component::CurDir => {}
                _ => base.push(comp),
            }
        } else {
            rest.push(s);
        }
    }
    let glob = (!rest.is_empty()).then(|| rest.join("/"));
    (base, glob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract;
    use crate::model::FileCache;

    fn cache(rel: &str, src: &str) -> FileCache {
        let lang = Language::from_path(Path::new(rel)).unwrap();
        let ex = extract(lang, src).unwrap();
        FileCache {
            cache_name: rel.to_string(),
            display_name: rel.to_string(),
            rel_path: PathBuf::from(rel),
            language: lang,
            lines: src.lines().count(),
            consts: ex.consts,
            funcs: ex.funcs,
            refs: ex.refs,
            notes: ex.notes,
            calls: ex.calls,
            uses: ex.uses,
            imports: ex.imports,
            types: ex.types,
            modules: ex.modules,
            annotations: ex.annotations,
        }
    }

    const SCHEMA: &str = "syntax = \"proto3\";\n\
        package acme.billing.v1;\n\
        message CreateInvoiceRequest { string customer = 1; }\n\
        message Invoice { string id = 1; }\n\
        service Billing {\n\
        \x20 rpc CreateInvoice(CreateInvoiceRequest) returns (Invoice);\n\
        }\n";

    fn polyglot() -> Vec<FileCache> {
        vec![
            cache(
                "gosvc/server.go",
                "package gosvc\n\
                 import billingv1 \"github.com/acme/protos/gen/go/acme/billing/v1\"\n\
                 type server struct{}\n\
                 func (s *server) CreateInvoice(ctx context.Context, req *billingv1.CreateInvoiceRequest) (*billingv1.Invoice, error) {\n\
                 \treturn nil, nil\n\
                 }\n",
            ),
            cache(
                "gosvc/client.go",
                "package gosvc\n\
                 import billingv1 \"github.com/acme/protos/gen/go/acme/billing/v1\"\n\
                 func Charge(conn *grpc.ClientConn) {\n\
                 \tclient := billingv1.NewBillingClient(conn)\n\
                 \tclient.CreateInvoice(ctx, &billingv1.CreateInvoiceRequest{})\n\
                 }\n",
            ),
            cache(
                "gosvc/billing_grpc.pb.go",
                "package billingv1\n\
                 type billingClient struct{}\n\
                 func (c *billingClient) CreateInvoice(ctx context.Context, in *CreateInvoiceRequest) (*Invoice, error) {\n\
                 \treturn nil, nil\n\
                 }\n",
            ),
            cache(
                "py/service.py",
                "from acme.billing.v1 import billing_pb2, billing_pb2_grpc\n\
                 class BillingService(billing_pb2_grpc.BillingServicer):\n\
                 \x20   def CreateInvoice(self, request, context):\n\
                 \x20       return billing_pb2.Invoice()\n",
            ),
            cache(
                "py/client.py",
                "from acme.billing.v1 import billing_pb2_grpc\n\
                 def charge(channel):\n\
                 \x20   stub = billing_pb2_grpc.BillingStub(channel)\n\
                 \x20   return stub.CreateInvoice(None)\n",
            ),
            cache(
                "ts/charge.ts",
                "import { createClient } from \"@connectrpc/connect\";\n\
                 import { Billing } from \"./gen/acme/billing/v1/billing_connect\";\n\
                 export async function charge(t: any) {\n\
                 \x20 const client = createClient(Billing, t);\n\
                 \x20 await client.createInvoice({});\n\
                 }\n",
            ),
            cache(
                "rs/src/lib.rs",
                "use billing::v1::billing_client::BillingClient;\n\
                 pub async fn charge() {\n\
                 \x20   let mut client = BillingClient::connect(\"http://x\").await.unwrap();\n\
                 \x20   client.create_invoice(req).await;\n\
                 }\n",
            ),
            // a same-named method with no tie to the schema
            cache(
                "ts/ledger.ts",
                "export function record(l: Ledger) { l.createInvoice(); }\n",
            ),
            cache("proto/acme/billing/v1/billing.proto", SCHEMA),
        ]
    }

    fn at(caches: &[FileCache], file: usize) -> &str {
        caches[file].rel_path.to_str().unwrap()
    }

    #[test]
    fn every_language_is_tied_to_the_rpc_by_evidence() {
        let caches = polyglot();
        let idx = ContractIndex::build(&caches, &[]);
        assert_eq!(idx.contracts.len(), 1);
        assert_eq!(idx.contracts[0].key, "acme.billing.v1.Billing/CreateInvoice");
        assert_eq!(idx.contracts[0].request.as_deref(), Some("CreateInvoiceRequest"));

        let handlers: Vec<(&str, &str)> = idx
            .handlers
            .iter()
            .map(|h| (at(&caches, h.def.0), h.evidence.label()))
            .collect();
        // the generated client is neither a handler nor a caller
        assert_eq!(
            handlers,
            vec![("gosvc/server.go", "request-type"), ("py/service.py", "service-owner")]
        );

        let callers: Vec<(&str, &str)> = idx
            .callers
            .iter()
            .map(|c| (at(&caches, c.file), c.evidence.label()))
            .collect();
        // `ts/ledger.ts` shares the name and nothing else
        assert_eq!(
            callers,
            vec![
                ("gosvc/client.go", "stub-type"),
                ("py/client.py", "generated-import"),
                ("ts/charge.ts", "generated-import"),
                ("rs/src/lib.rs", "stub-type"),
            ]
        );
        // a call reaches the schema and every handler
        assert_eq!(idx.targets(0).len(), 3);
    }

    #[test]
    fn a_schema_outside_the_project_still_links_its_callers() {
        let mut caches = polyglot();
        let schema = caches.pop().unwrap();
        let idx = ContractIndex::build(&caches, &[schema]);
        assert_eq!(idx.contracts[0].def, None);
        assert_eq!(idx.callers.len(), 4);
        // with no definition in the project, a call reaches the handlers only
        assert_eq!(idx.targets(0).len(), 2);
    }

    #[test]
    fn two_services_with_one_method_name_link_by_stub_and_otherwise_not_at_all() {
        let two = "syntax = \"proto3\";\npackage acme.v1;\n\
                   service Billing { rpc Get(A) returns (B); }\n\
                   service Ledger { rpc Get(C) returns (D); }\n";
        let caches = vec![
            cache("acme.proto", two),
            cache(
                "a.go",
                "package a\nimport v1 \"x/acme/v1\"\n\
                 func F(c v1.LedgerClient) { c.Get(ctx, nil) }\n\
                 func G(c other) { c.Get(ctx, nil) }\n",
            ),
        ];
        let idx = ContractIndex::build(&caches, &[]);
        let linked: Vec<&str> = idx
            .callers
            .iter()
            .map(|c| idx.contracts[c.contract].service.as_str())
            .collect();
        // `G` imports the generated package, but so would a call to either
        // service - that is a tie, and a tie links nothing
        assert_eq!(linked, vec!["Ledger"]);
    }

    #[test]
    fn a_lookup_matches_any_spelling_and_a_qualifier_must_agree() {
        let idx = ContractIndex::build(&polyglot(), &[]);
        assert_eq!(idx.matching("create_invoice", None), vec![0]);
        assert_eq!(idx.matching("createInvoice", Some("Billing")), vec![0]);
        assert_eq!(idx.matching("CreateInvoice", Some("acme.billing.v1.Billing")), vec![0]);
        assert!(idx.matching("CreateInvoice", Some("Ledger")).is_empty());
    }

    #[test]
    fn generated_files_are_recognised_by_name() {
        for f in [
            "x/billing.pb.go",
            "x/billing_grpc.pb.go",
            "x/billing_pb2.py",
            "x/billing_pb2_grpc.py",
            "x/billing_grpc_pb.js",
            "x/billing_pb.d.ts",
            "x/billing_connect.ts",
            "x/billingv1connect/billing.connect.go",
            "x/BillingGrpc.cs",
        ] {
            assert!(is_generated(Path::new(f)), "{f}");
        }
        for f in ["x/server_grpc.go", "x/billing.go", "x/grpc.rs", "x/client.ts"] {
            assert!(!is_generated(Path::new(f)), "{f}");
        }
    }

    #[test]
    fn a_glob_splits_into_the_directory_to_walk_and_the_pattern_under_it() {
        assert_eq!(
            split_glob("../protos/acme/**/*.proto"),
            (PathBuf::from("../protos/acme"), Some("**/*.proto".to_string()))
        );
        assert_eq!(split_glob("./third_party/proto"), (PathBuf::from("third_party/proto"), None));
    }
}
