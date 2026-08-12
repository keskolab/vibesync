//! VibeSync engine: scans AI-tool session storage, tokenizes machine-specific
//! paths, tracks sync state, and pushes/pulls through pluggable stores.

pub mod adapters;
pub mod atlas;
pub mod claude;
pub mod codec;
pub mod codex;
pub(crate) mod dbsync;
pub mod opencode;
pub mod config;
pub mod copilot;
pub mod dlog;
pub mod gitmap;
pub mod registry;
pub mod scanner;
pub mod state;
pub mod store;
pub mod sync;
pub mod tokenizer;
pub mod zed;
pub mod vscode;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use codec::{AgeCodec, Codec, GzipCodec};
pub use config::{machine_name, open_store, open_store_cached, StoreConfig};
pub use scanner::FileEntry;
pub use state::{FileState, SyncState};
pub use store::{AzureSasStore, FolderStore, RemoteMeta, S3Store, SyncStore};
pub use sync::Report;
pub use tokenizer::Tokenizer;
