use tracing::debug;

use crate::meta::types::Ino;

#[derive(Debug, Default)]
pub(crate) struct DataWriter {}

impl DataWriter {
    pub fn get_length(&self, ino: Ino) -> u64 {
        debug!("writer get_length do nothing");
        return 0;
    }
}
