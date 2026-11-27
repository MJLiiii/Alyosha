//! Where images are kept: the `media` table, by the SHA-256 of their bytes.
//! See `docs/Media.md#the-media-table`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use sha2::{Digest, Sha256};

use crate::BoxFuture;

/// A stored image: its MIME type and bytes.
pub type MediaRow = (String, Vec<u8>);

/// The images of every session. Rows are never deleted.
pub trait MediaStore: Send + Sync {
    /// Stores the bytes, once per hash, and returns the hash.
    fn put_media<'a>(&'a self, mime: &'a str, bytes: &'a [u8])
    -> BoxFuture<'a, Result<String, String>>;
    /// The MIME type and bytes stored under a hash.
    fn get_media<'a>(
        &'a self,
        sha256: &'a str,
    ) -> BoxFuture<'a, Result<Option<MediaRow>, String>>;
}

/// The hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Images in memory, for tests and sessions without a database.
#[derive(Default)]
pub struct MemoryMedia {
    rows: Mutex<HashMap<String, MediaRow>>,
}

impl MemoryMedia {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// How many images are stored.
    pub fn len(&self) -> usize {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets an image, to test a missing row.
    pub fn remove(&self, sha256: &str) {
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(sha256);
    }
}

impl MediaStore for MemoryMedia {
    fn put_media<'a>(
        &'a self,
        mime: &'a str,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let hash = sha256_hex(bytes);
            self.rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(hash.clone())
                .or_insert_with(|| (mime.to_owned(), bytes.to_vec()));
            Ok(hash)
        })
    }

    fn get_media<'a>(
        &'a self,
        sha256: &'a str,
    ) -> BoxFuture<'a, Result<Option<MediaRow>, String>> {
        Box::pin(async move {
            Ok(self
                .rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(sha256)
                .cloned())
        })
    }
}
