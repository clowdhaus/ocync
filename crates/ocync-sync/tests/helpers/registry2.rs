//! Real `registry:2` containers for tests that need a real registry's wire
//! behavior, such as HTTP/2 flow control, and a corpus to fill them with.

#![allow(dead_code, unused_imports, unreachable_pub)]

use bytes::Bytes;
use futures_util::stream;
use ocync_distribution::sha256::Sha256;
use ocync_distribution::spec::{MediaType, RepositoryName};
use ocync_distribution::{Digest, RegistryClient};

use super::builders::ManifestBuilder;

/// The container starters `ocync-distribution`'s own `registry:2` tests use.
#[path = "../../../ocync-distribution/tests/common/mod.rs"]
mod common;

pub use common::{start_registry, start_registry_tls};

/// What [`populate_source`] pushes: `images` images, each a config and
/// `layers` distinct layers of `layer_bytes`.
#[derive(Debug, Clone, Copy)]
pub struct Corpus {
    pub images: usize,
    pub layers: usize,
    pub layer_bytes: usize,
}

impl Corpus {
    /// The config blob of image `image`.
    pub fn config(&self, image: usize) -> Vec<u8> {
        format!("{{\"image\":{image}}}").into_bytes()
    }

    /// Layer `layer` of image `image`: zeros, varied per (image, layer) so
    /// digests are unique.
    pub fn layer(&self, image: usize, layer: usize) -> Vec<u8> {
        let mut data = vec![0u8; self.layer_bytes];
        data[0] = (image & 0xff) as u8;
        data[1] = (layer & 0xff) as u8;
        data
    }

    /// The bytes of all of an image's blobs.
    pub fn image_bytes(&self, image: usize) -> u64 {
        (self.config(image).len() + self.layers * self.layer_bytes) as u64
    }
}

async fn push_blob_stream(client: &RegistryClient, repo: &RepositoryName, data: Vec<u8>) {
    let digest = Digest::from_sha256(Sha256::digest(&data));
    let size = data.len() as u64;
    let body = Bytes::from(data);
    let s = stream::once(async move { Ok::<_, ocync_distribution::Error>(body) });
    client
        .blob_push_stream(repo, &digest, Some(size), s)
        .await
        .expect("blob_push_stream");
}

/// Pushes `corpus` to `client`, each image tagged `v1` in repository
/// `test/img-NNNN`, and returns the repositories.
pub async fn populate_source(client: &RegistryClient, corpus: &Corpus) -> Vec<RepositoryName> {
    let mut repos = Vec::with_capacity(corpus.images);
    for i in 0..corpus.images {
        let repo = RepositoryName::new(format!("test/img-{i:04}")).unwrap();
        let config_data = corpus.config(i);
        // Streamed the way the engine pushes during a sync, so setup
        // exercises the same upload path.
        push_blob_stream(client, &repo, config_data.clone()).await;

        let mut builder = ManifestBuilder::new(&config_data);
        for l in 0..corpus.layers {
            let layer = corpus.layer(i, l);
            push_blob_stream(client, &repo, layer.clone()).await;
            builder = builder.layer(&layer);
        }
        let parts = builder.build();
        client
            .manifest_push(&repo, "v1", &MediaType::OciManifest, &parts.bytes)
            .await
            .expect("manifest push");
        repos.push(repo);
    }
    repos
}
