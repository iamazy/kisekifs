pub mod config;
mod err;
mod handle;
pub mod kiseki;
pub mod storage;

pub use err::VFSError;
pub use kiseki::KisekiVFS;

pub(crate) type FH = u64;
