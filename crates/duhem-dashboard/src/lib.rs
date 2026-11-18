//! `duhem-dashboard` — read-only web dashboard over the Duhem
//! evidence store (specs #53 / #85 / #86 / #87, live streaming #84;
//! store posture #189).
//!
//! Read and export use a read-only handle. Serve mode can explicitly
//! enable bundle ingest, which validates and replays recorded evidence
//! before writing it. The dashboard never invokes the runtime.
//!
//! Three surfaces:
//! - [`server::router`] — the JSON API + embedded SPA (serve mode);
//! - [`live::live_stream`] — replay-then-follow SSE over a run's
//!   event stream (#84);
//! - [`export::export`] — a self-contained static rendering for
//!   upload from CI (#87).

pub mod export;
mod ingest;
pub mod live;
pub mod model;
pub mod reader;
pub mod server;

pub use export::{ExportStats, export};
pub use ingest::ingest_router;
pub use reader::{EvidenceReader, ReaderError, RunEvidence, events_to_jsonl};
pub use server::router;

/// Default listen port (#53 alignment: bikeshed-accepted 7878).
pub const DEFAULT_PORT: u16 = 7878;
