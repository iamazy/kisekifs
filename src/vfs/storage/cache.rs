mod file_cache;
mod juice_cache;
mod write_cache;

use std::{fmt::Debug, sync::Arc};

use async_trait::async_trait;
use opendal::Reader;

use crate::meta::types::SliceID;

pub fn new_juice_builder() -> juice_cache::JuiceFileCacheBuilder {
    juice_cache::JuiceFileCacheBuilder::default()
}

/// The exposed cache trait.
#[async_trait]
pub trait Cache: Send + Sync + Debug + Unpin + 'static {
    async fn cache(&self, slice_id: u64, block: Arc<Vec<u8>>) -> bool;
    async fn get(&self, slice_id: SliceID) -> Option<Reader>;
    async fn wait_on_all_flush_finish(&self);
    /// close the cache and wait on all background task exit.
    async fn close(&self);
}

/// The cache manager.
pub(crate) struct CacheManager {}
