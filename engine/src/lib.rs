//! Code Sync engine: scans AI-tool session storage, tokenizes machine-specific
//! paths, tracks sync state, and pushes/pulls through pluggable stores.

pub mod adapters;
pub mod scanner;
pub mod state;
pub mod store;
pub mod sync;
pub mod tokenizer;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use scanner::FileEntry;
pub use state::{FileState, SyncState};
pub use store::{FolderStore, RemoteMeta, SyncStore};
pub use sync::Report;
pub use tokenizer::Tokenizer;
