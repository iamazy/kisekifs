use object_store::aws::AmazonS3Builder;
use object_store::ObjectStore;
use std::sync::Arc;

pub type ObjectStorage = Arc<dyn ObjectStore>;

pub type LocalStorage = Arc<dyn ObjectStore>;

pub type ObjectStorageError = object_store::Error;

pub type ObjectStoragePath = object_store::path::Path;

pub fn is_not_found_error(e: &ObjectStorageError) -> bool {
    matches!(e, ObjectStorageError::NotFound { .. })
}

pub fn new_memory_object_store() -> ObjectStorage {
    Arc::new(object_store::memory::InMemory::new())
}

pub fn new_local_object_store(path: &str) -> Result<ObjectStorage, object_store::Error> {
    std::fs::create_dir_all(path).unwrap();
    let object_sto: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(path)?);
    Ok(object_sto)
}

pub fn new_minio_store() -> Result<ObjectStorage, ObjectStorageError> {
    let object_sto: Arc<dyn ObjectStore> = Arc::new(
        AmazonS3Builder::new()
            .with_region("auto")
            .with_endpoint("http://localhost:9000")
            .with_allow_http(true)
            .with_bucket_name("test")
            .with_access_key_id("minioadmin")
            .with_secret_access_key("minioadmin")
            .build()?,
    );
    Ok(object_sto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use object_store::path::Path;
    use object_store::ObjectStore;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn basic() {
        // let object_sto = debug_minio_store().unwrap();
        // let object_sto =
        //     new_local_object_store(kiseki_common::KISEKI_DEBUG_OBJECT_STORAGE).unwrap();
        let object_sto = new_memory_object_store();

        let bytes = Bytes::from_static(b"hello");
        let path = Path::parse("data/large_file").unwrap();
        object_sto.put(&path, bytes).await.unwrap();

        let bytes = object_sto.get(&path).await.unwrap();
        let result = bytes.bytes().await.unwrap();
        assert_eq!(result.as_ref(), b"hello".as_slice());

        let path = Path::parse("data/large_file_multipart").unwrap();
        let (_id, mut writer) = object_sto.put_multipart(&path).await.unwrap();
        let bytes = Bytes::from_static(b"hello");
        writer.write_all(&bytes).await.unwrap();
        writer.flush().await.unwrap();
        writer.shutdown().await.unwrap();
    }
}
