//! Pure proving and protocol library shared by the OSS server and devtool.
//! No tokio, axum or SQLite.

pub mod evidence;
pub mod protocol;
pub mod unspent;

pub use evidence::build_epoch;
pub use protocol::*;
pub use unspent::prove_unspent;
