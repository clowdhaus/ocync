//! Real `registry:2` containers for tests that need a real registry's wire
//! behavior, such as HTTP/2 flow control, and a corpus to fill them with.

#![allow(dead_code, unused_imports, unreachable_pub)]

use bytes::Bytes;
use futures_util::stream;
use ocync_distribution::RegistryClient;
use ocync_distribution::spec::{MediaType, RepositoryName};

use super::builders::ManifestBuilder;
use super::fixtures::blob_digest;

/// The container starters of `ocync-distribution`'s `registry:2` tests.
#[path = "../../../ocync-distribution/tests/common/mod.rs"]
mod common;

/// `registry:2` over plain HTTP, and over TLS with a self-signed certificate.
pub use common::{start_registry, start_registry_tls};

/// What [`populate_source`] pushes: `images` images, each a config and
/// `layers` distinct layers of `layer_bytes`.
#[derive(Debug, Clone, Copy)]
pub struct Corpus {
    /// Number of images, each in its own repository.
    pub images: usize,
    /// Distinct layers per image.
    pub layers: usize,
    /// Size of each layer in bytes.
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
}

async fn push_blob_stream(client: &RegistryClient, repo: &RepositoryName, data: Vec<u8>) {
    let digest = blob_digest(&data);
    let size = data.len() as u64;
    let body = Bytes::from(data);
    let s = stream::once(async move { Ok::<_, ocync_distribution::Error>(body) });
    let slot = client.reserve_upload_slot(Some(size)).await;
    slot.push_stream(repo, &digest, Some(size), s)
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
        let mut builder = ManifestBuilder::new(&config_data);
        // Streamed the way the engine pushes during a sync, so setup
        // exercises the same upload path.
        push_blob_stream(client, &repo, config_data).await;
        for l in 0..corpus.layers {
            let layer = corpus.layer(i, l);
            builder = builder.layer(&layer);
            push_blob_stream(client, &repo, layer).await;
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
