//! The `analyze` subcommand — reports blob sharing and cross-repo mount potential
//! without performing any sync.
//!
//! Pulls source manifests only (never blobs), walks index manifests to collect
//! all platform-specific blob descriptors, and aggregates by digest to show:
//! - Total unique blobs and total bytes
//! - Shared blobs (same digest across 2+ images) and deduplicated bytes saved
//! - Per-target-registry mount opportunities (how many pushes cross-repo mount
//!   would replace)

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

use ocync_distribution::ecr::BatchBlobChecker;
use ocync_distribution::spec::ManifestKind;
use ocync_distribution::{Digest, RepositoryName};

use crate::cli::commands::synchronize::{build_clients, resolve_mapping};
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode};

/// Arguments for the `analyze` subcommand.
#[derive(Debug, clap::Args)]
pub(crate) struct AnalyzeArgs {
    /// Path to the sync config file.
    #[arg(short, long)]
    pub(crate) config: PathBuf,
    /// Emit a JSON report instead of the text summary.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Per-blob aggregate across all mappings.
#[derive(Debug)]
struct BlobAggregate {
    size: u64,
    /// Image references (`source/repo:tag`) that include this blob.
    images: BTreeSet<String>,
    /// Target registry aliases this blob would be pushed to.
    targets: BTreeSet<String>,
    /// Target repositories this blob would be pushed to, per target registry.
    /// `target_alias → {target_repo}`.
    target_repos: BTreeMap<String, BTreeSet<RepositoryName>>,
}

/// Run the analyze command.
pub(crate) async fn run(args: &AnalyzeArgs) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;
    let clients = build_clients(&config).await?;
    // Analyze doesn't push anything, so no batch checkers needed.
    let no_checkers: HashMap<String, Rc<dyn BatchBlobChecker>> = HashMap::new();

    let mut blobs: HashMap<Digest, BlobAggregate> = HashMap::new();
    let mut image_count = 0usize;

    for mapping in &config.mappings {
        let resolved = match resolve_mapping(mapping, &config, &clients, &no_checkers).await? {
            Some(r) => r,
            None => continue,
        };

        for tag_pair in &resolved.tags {
            image_count += 1;
            let image_ref = format!("{}:{}", resolved.source_repo, tag_pair.source);
            collect_blobs(
                &resolved.source_client,
                &resolved.source_repo,
                &tag_pair.source,
                &image_ref,
                &resolved.targets,
                &resolved.target_repo,
                &mut blobs,
            )
            .await?;
        }
    }

    if args.json {
        print_json(&blobs, image_count)?;
    } else {
        print_text(&blobs, image_count);
    }

    Ok(ExitCode::Success)
}

/// Pull a manifest (recursively for indexes) and record every blob's
/// descriptor against the source image reference and target set.
async fn collect_blobs(
    source_client: &ocync_distribution::RegistryClient,
    source_repo: &RepositoryName,
    tag: &str,
    image_ref: &str,
    targets: &[ocync_sync::engine::TargetEntry],
    target_repo: &RepositoryName,
    blobs: &mut HashMap<Digest, BlobAggregate>,
) -> Result<(), CliError> {
    let pulled = source_client
        .manifest_pull(source_repo, tag)
        .await
        .map_err(|e| CliError::Input(format!("manifest_pull {image_ref}: {e}")))?;

    let descriptors = descriptors_of(&pulled.manifest);
    for descriptor in descriptors {
        record_blob(
            descriptor.digest,
            descriptor.size,
            image_ref,
            targets,
            target_repo,
            blobs,
        );
    }

    // Recurse into index children to collect per-platform manifest blobs.
    if let ManifestKind::Index(index) = &pulled.manifest {
        for child in &index.manifests {
            let child_pulled = source_client
                .manifest_pull(source_repo, &child.digest.to_string())
                .await
                .map_err(|e| {
                    CliError::Input(format!(
                        "manifest_pull {image_ref} child {}: {e}",
                        child.digest
                    ))
                })?;
            for descriptor in descriptors_of(&child_pulled.manifest) {
                record_blob(
                    descriptor.digest,
                    descriptor.size,
                    image_ref,
                    targets,
                    target_repo,
                    blobs,
                );
            }
        }
    }

    Ok(())
}

/// Descriptor data extracted from a manifest.
struct BlobDescriptor {
    digest: Digest,
    size: u64,
}

/// Return the (digest, size) of every blob referenced by a manifest.
fn descriptors_of(manifest: &ManifestKind) -> Vec<BlobDescriptor> {
    match manifest {
        ManifestKind::Image(image) => {
            let mut out = Vec::with_capacity(1 + image.layers.len());
            out.push(BlobDescriptor {
                digest: image.config.digest.clone(),
                size: image.config.size,
            });
            for layer in &image.layers {
                out.push(BlobDescriptor {
                    digest: layer.digest.clone(),
                    size: layer.size,
                });
            }
            out
        }
        // Index descriptors themselves aren't blobs we push; children handle that.
        ManifestKind::Index(_) => Vec::new(),
    }
}

fn record_blob(
    digest: Digest,
    size: u64,
    image_ref: &str,
    targets: &[ocync_sync::engine::TargetEntry],
    target_repo: &RepositoryName,
    blobs: &mut HashMap<Digest, BlobAggregate>,
) {
    let entry = blobs.entry(digest).or_insert_with(|| BlobAggregate {
        size,
        images: BTreeSet::new(),
        targets: BTreeSet::new(),
        target_repos: BTreeMap::new(),
    });
    entry.images.insert(image_ref.to_owned());
    for target in targets {
        let alias = target.name.to_string();
        entry.targets.insert(alias.clone());
        entry
            .target_repos
            .entry(alias)
            .or_default()
            .insert(target_repo.clone());
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_text(blobs: &HashMap<Digest, BlobAggregate>, image_count: usize) {
    let total_blobs = blobs.len();
    let total_bytes: u64 = blobs.values().map(|b| b.size).sum();

    let shared: Vec<&BlobAggregate> = blobs.values().filter(|b| b.images.len() > 1).collect();
    let shared_bytes: u64 = shared.iter().map(|b| b.size).sum();

    // Cross-repo mount opportunity: per target registry, count blobs whose
    // target_repos set has more than one repo — those are the mount candidates
    // (a blob pushed once can be mounted into every other repo in the set).
    let mut mount_savings_by_target: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for blob in blobs.values() {
        for (target, repos) in &blob.target_repos {
            if repos.len() > 1 {
                // One blob, pushed once, then mounted into (repos.len() - 1) other repos.
                let savings_count = repos.len() - 1;
                let savings_bytes = blob.size * savings_count as u64;
                let entry = mount_savings_by_target.entry(target.clone()).or_default();
                entry.0 += savings_count;
                entry.1 += savings_bytes;
            }
        }
    }

    println!("Analyzed {image_count} image mappings");
    println!();
    println!(
        "Unique blobs: {total_blobs} ({})",
        format_bytes(total_bytes)
    );
    println!(
        "Shared blobs: {} ({}) across 2+ images",
        shared.len(),
        format_bytes(shared_bytes)
    );
    if !mount_savings_by_target.is_empty() {
        println!();
        println!("Cross-repo mount opportunities (per target registry):");
        for (target, (count, bytes)) in &mount_savings_by_target {
            println!(
                "  {target}: {count} redundant pushes avoidable, {} savings",
                format_bytes(*bytes)
            );
        }
    }
}

fn print_json(blobs: &HashMap<Digest, BlobAggregate>, image_count: usize) -> Result<(), CliError> {
    let mut mount_savings_by_target: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for blob in blobs.values() {
        for (target, repos) in &blob.target_repos {
            if repos.len() > 1 {
                let savings_count = repos.len() - 1;
                let savings_bytes = blob.size * savings_count as u64;
                let entry = mount_savings_by_target.entry(target.clone()).or_default();
                entry.0 += savings_count;
                entry.1 += savings_bytes;
            }
        }
    }

    let report = serde_json::json!({
        "images_analyzed": image_count,
        "total_blobs": blobs.len(),
        "total_bytes": blobs.values().map(|b| b.size).sum::<u64>(),
        "shared_blobs": blobs.values().filter(|b| b.images.len() > 1).count(),
        "shared_bytes": blobs.values().filter(|b| b.images.len() > 1).map(|b| b.size).sum::<u64>(),
        "mount_savings_by_target": mount_savings_by_target
            .iter()
            .map(|(k, (c, b))| (k.clone(), serde_json::json!({"redundant_pushes": c, "bytes": b})))
            .collect::<BTreeMap<_, _>>(),
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::Input(format!("serialize report: {e}")))?
    );
    Ok(())
}

/// Format bytes with SI decimal prefixes (1 KB = 1000 B).
fn format_bytes(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    const KB: u64 = 1_000;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
