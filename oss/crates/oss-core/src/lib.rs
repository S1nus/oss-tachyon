//! Pure proving and protocol library shared by the OSS server and devtool.
//! No tokio, axum or SQLite.

pub mod chain;
pub mod evidence;
pub mod par;
pub mod protocol;
pub mod spans;
pub mod unspent;

pub use evidence::build_epoch;
pub use protocol::*;
pub use spans::{SpanBuilder, SpanChunk, Spans};
pub use unspent::{UnspentJob, prove_unspent, prove_unspent_batch};
