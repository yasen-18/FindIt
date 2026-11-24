//! FindIt core: everything about searching that does not touch the operating system.
//!
//! * [`index`]: compact in-memory index of names, parents, sizes and dates
//! * [`query`]: query language parser and matcher
//! * [`search`]: parallel, cancellable search with ranking and column sorts
//! * [`snapshot`]: crash-safe on-disk snapshot for fast startup
//! * [`fold`]: Unicode case and accent folding
//! * [`time`], [`types`]: date filters and file type groups

pub mod fold;
pub mod index;
pub mod query;
pub mod search;
pub mod snapshot;
pub mod time;
pub mod types;

pub use index::{flags, shared, Entry, Index, Meta, SharedIndex, Volume, VolumeKind, NONE};
pub use query::{Query, QueryError, QueryOptions};
pub use search::{search, Chip, SearchOptions, SearchOutcome, SortColumn};
