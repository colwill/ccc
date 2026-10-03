//! Collateral Code Check (ccc)

pub mod audit;
pub mod coverage;
pub mod deps;
pub mod edit;
pub mod extract;
pub mod externals;
pub mod html;
pub mod insights;
pub mod languages;
pub mod model;
pub mod naming;
pub mod pr_summary;
pub mod prompts;
pub mod render;
pub mod sast;
pub mod scan;
pub mod serve;
pub mod changes;
pub mod contracts;
pub mod telemetry;
pub mod tokenize;
pub mod vis;

// `check` is deprecated but still re-exported: deprecating it is what warns
// callers, and pulling the re-export would break them instead
#[allow(deprecated)]
pub use scan::{check, scan, Change, ChangeKind, CheckReport, ScanReport};
pub use serve::{serve, ServeOptions};
pub use changes::{init_config, changes, ChangesOptions, ChangesReport};
pub use externals::{ExternalRepo, ExternalService, Surface};
pub use prompts::{prompts, PromptRef, PromptsOptions, PromptsReport, Turn};
pub use tokenize::{tokenize, Encoding, TokenCache, TokenizeReport};
