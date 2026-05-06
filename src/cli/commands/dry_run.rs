//! Pretty-printer for `--dry-run`. Renders per-mapping stage attrition,
//! Pareto-sorted drop attribution, include-rescue listing, and `min_tags`
//! status from a `filter::FilterReport`.

use std::collections::HashSet;
use std::io::{self, Write};

use ocync_sync::engine::{ResolvedMapping, TagPair};
use ocync_sync::filter::{DropKind, FilterReport};

/// Default sample cap per drop reason and per include-rescue list. Removed
/// when verbose. Five lines is enough to spot the pattern (rc1..rc5,
/// 1.20.0..1.20.4) without flooding the terminal.
const SAMPLE_CAP: usize = 5;

/// Print dry-run output for the resolved mappings to stdout.
///
/// `verbose` is the global `cli.verbose >= 1` toggle; when true, all
/// rejected tags per drop reason are printed (no sample cap).
pub(crate) fn print(mappings: &[ResolvedMapping], verbose: bool) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    // Stdout write errors during dry-run are almost always SIGPIPE (e.g.
    // `ocync sync --dry-run | head`). Swallow them: the user explicitly
    // asked us to stop writing.
    let _ = write_to(&mut handle, mappings, verbose);
}

pub(crate) fn write_to<W: Write>(
    w: &mut W,
    mappings: &[ResolvedMapping],
    verbose: bool,
) -> io::Result<()> {
    if mappings.is_empty() {
        writeln!(w, "dry-run: no mappings to sync")?;
        return Ok(());
    }

    for (i, m) in mappings.iter().enumerate() {
        if i > 0 {
            writeln!(w)?;
        }
        write_mapping(w, m, verbose)?;
    }
    Ok(())
}

fn write_mapping<W: Write>(w: &mut W, m: &ResolvedMapping, verbose: bool) -> io::Result<()> {
    let target_names: Vec<&str> = m.targets.iter().map(|t| &*t.name).collect();
    writeln!(
        w,
        "dry-run: {} -> {} [{}]",
        m.source_repo,
        m.target_repo,
        target_names.join(", ")
    )?;

    let Some(report) = m.filter_report.as_ref() else {
        // Exact-tag fast path: no report available. Fall back to a simple
        // tag list (matches the pre-Thread-2 print_dry_run behavior).
        return write_simple_tag_list(w, &m.tags);
    };

    writeln!(w, "  source tags: {}", report.candidate_count)?;
    writeln!(w)?;

    if !report.include_kept.is_empty() {
        write_include_path(w, report, verbose)?;
        writeln!(w)?;
    }

    write_pipeline(w, report)?;
    writeln!(w)?;

    write_kept(w, &m.tags, &report.include_kept)?;
    writeln!(w)?;

    let any_drops = write_dropped(w, report, verbose)?;
    write_min_tags_status(w, m.tags.len(), report.min_tags, any_drops)?;
    Ok(())
}

/// Render a flat tag list (used on the exact-tag fast path where no
/// `FilterReport` is available because source tags were never enumerated).
fn write_simple_tag_list<W: Write>(w: &mut W, tags: &[TagPair]) -> io::Result<()> {
    writeln!(w, "  tags ({}):", tags.len())?;
    write_tag_pairs(w, tags)
}

/// Render one indented line per [`TagPair`]. Shared by the exact-tag fast
/// path and the `kept (N):` section.
fn write_tag_pairs<W: Write>(w: &mut W, tags: &[TagPair]) -> io::Result<()> {
    for tag_pair in tags {
        writeln!(w, "    {tag_pair}")?;
    }
    Ok(())
}

/// Render the include-rescue section: count + sample tag names, with a
/// `, ...` ellipsis when truncated under `SAMPLE_CAP`.
fn write_include_path<W: Write>(w: &mut W, report: &FilterReport, verbose: bool) -> io::Result<()> {
    writeln!(w, "  include path:")?;
    let count = report.include_kept.len();
    let display = render_samples(&report.include_kept, verbose);
    writeln!(w, "    rescued ({count}):  {display}")?;
    Ok(())
}

fn write_pipeline<W: Write>(w: &mut W, report: &FilterReport) -> io::Result<()> {
    writeln!(w, "  filter:")?;
    for stage in &report.pipeline {
        let delta = stage.count_in as isize - stage.count_out as isize;
        let delta_str = if delta != 0 {
            format!("    (-{})", delta.abs())
        } else {
            String::new()
        };
        let line = format!(
            "    {:<28} {:>4} -> {:<4}{}",
            stage.label, stage.count_in, stage.count_out, delta_str
        );
        writeln!(w, "{}", line.trim_end())?;
    }
    Ok(())
}

fn write_kept<W: Write>(w: &mut W, tags: &[TagPair], include_kept: &[String]) -> io::Result<()> {
    let header = if include_kept.is_empty() {
        format!("  kept ({}):", tags.len())
    } else {
        format!("  kept ({}):  include first, then pipeline", tags.len())
    };
    writeln!(w, "{header}")?;
    let include_set: HashSet<&str> = include_kept.iter().map(String::as_str).collect();
    for tag_pair in tags {
        if include_set.contains(tag_pair.source.as_str()) {
            writeln!(w, "    {tag_pair}  [via include]")?;
        } else {
            writeln!(w, "    {tag_pair}")?;
        }
    }
    Ok(())
}

/// Render the `dropped` section. Returns `true` when at least one row was
/// emitted, so the caller can decide whether `min_tags` status needs an
/// extra leading blank line.
fn write_dropped<W: Write>(w: &mut W, report: &FilterReport, verbose: bool) -> io::Result<bool> {
    let total: usize = report.dropped.iter().map(|d| d.count).sum();
    if total == 0 {
        return Ok(false);
    }
    writeln!(w, "  dropped ({total}):")?;
    for reason in &report.dropped {
        let samples_display = render_samples(&reason.samples, verbose);
        // `LatestCap` reads as a complete clause ("over latest=N limit"); every
        // other reason gets a "by " preposition so the line reads as English.
        let display_label = match reason.kind {
            DropKind::LatestCap { .. } => reason.kind.to_string(),
            _ => format!("by {}", reason.kind),
        };
        writeln!(
            w,
            "    {:>4}  {:<28}{}",
            reason.count, display_label, samples_display
        )?;
        if matches!(reason.kind, DropKind::SystemExclude) {
            writeln!(
                w,
                "          hint: to keep prereleases, list patterns under include: (globs supported)"
            )?;
        }
    }
    Ok(true)
}

/// Render `min_tags` status when configured. When `kept < min_tags`, surface
/// a clear warning that real-sync will fail with `BelowMinTags`. When the
/// config is absent, render nothing.
fn write_min_tags_status<W: Write>(
    w: &mut W,
    kept: usize,
    min_tags: Option<usize>,
    any_drops: bool,
) -> io::Result<()> {
    let Some(min) = min_tags else {
        return Ok(());
    };
    if any_drops {
        writeln!(w)?;
    }
    if kept < min {
        writeln!(
            w,
            "  min_tags: {min}  (kept {kept}, real sync will FAIL with BelowMinTags)"
        )?;
    } else {
        writeln!(w, "  min_tags: {min}  (kept {kept}, satisfied)")?;
    }
    Ok(())
}

/// Format a sample list with the default cap of `SAMPLE_CAP`, joining with
/// `, ` and appending `, ...` when truncated. With `verbose`, all samples
/// are printed without truncation.
fn render_samples(samples: &[String], verbose: bool) -> String {
    if verbose || samples.len() <= SAMPLE_CAP {
        samples.join(", ")
    } else {
        let head = samples[..SAMPLE_CAP].join(", ");
        format!("{head}, ...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocync_sync::filter::{DropReason, FilterReport, StageDelta};

    fn report_fixture() -> FilterReport {
        FilterReport {
            candidate_count: 50,
            include_kept: Vec::new(),
            pipeline: vec![
                StageDelta {
                    label: "glob \"3.*\"".into(),
                    count_in: 50,
                    count_out: 48,
                },
                StageDelta {
                    label: "semver \">=3.18\"".into(),
                    count_in: 48,
                    count_out: 40,
                },
            ],
            dropped: vec![
                DropReason {
                    kind: DropKind::LatestCap { limit: 3 },
                    count: 32,
                    samples: vec![
                        "3.20.3".into(),
                        "3.20.2".into(),
                        "3.20.1".into(),
                        "3.20.0".into(),
                        "3.19.7".into(),
                        "3.19.6".into(),
                    ],
                },
                DropReason {
                    kind: DropKind::SystemExclude,
                    count: 2,
                    samples: vec!["3.18.0-rc.1".into(), "3.19.0-beta.1".into()],
                },
            ],
            min_tags: None,
        }
    }

    fn capture<F>(f: F) -> String
    where
        F: FnOnce(&mut Vec<u8>) -> io::Result<()>,
    {
        let mut buf = Vec::new();
        f(&mut buf).expect("write into Vec never fails");
        String::from_utf8(buf).expect("formatter output is utf8")
    }

    #[test]
    fn sample_cap_default_truncates_with_ellipsis() {
        let report = report_fixture();
        let out = capture(|w| {
            write_dropped(w, &report, false)?;
            Ok(())
        });
        // 6 samples -> first 5 listed + ", ..."
        assert!(
            out.contains("3.20.3, 3.20.2, 3.20.1, 3.20.0, 3.19.7, ..."),
            "{out}"
        );
        // The 6th sample must NOT appear.
        assert!(!out.contains("3.19.6"), "{out}");
    }

    #[test]
    fn sample_cap_verbose_prints_all_samples() {
        let report = report_fixture();
        let out = capture(|w| {
            write_dropped(w, &report, true)?;
            Ok(())
        });
        // All 6 samples present, no trailing ellipsis.
        assert!(out.contains("3.19.6"), "{out}");
        assert!(!out.contains(", ..."), "{out}");
    }

    #[test]
    fn system_exclude_emits_override_hint() {
        let report = report_fixture();
        let out = capture(|w| {
            write_dropped(w, &report, false)?;
            Ok(())
        });
        assert!(
            out.contains("to keep prereleases, list patterns under include:"),
            "{out}"
        );
    }

    #[test]
    fn empty_mappings_emits_no_mappings_message() {
        let out = capture(|w| write_to(w, &[], false));
        assert_eq!(out, "dry-run: no mappings to sync\n");
    }

    #[test]
    fn include_path_renders_count_and_rescued_names() {
        let report = FilterReport {
            candidate_count: 50,
            include_kept: vec!["latest".into(), "latest-dev".into(), "edge".into()],
            pipeline: vec![],
            dropped: vec![],
            min_tags: None,
        };
        let out = capture(|w| write_include_path(w, &report, false));
        assert!(out.contains("include path:"), "{out}");
        // Count and the actual rescued tag names appear together.
        assert!(out.contains("rescued (3):"), "{out}");
        assert!(out.contains("latest, latest-dev, edge"), "{out}");
    }

    #[test]
    fn include_path_truncates_long_rescue_list_under_default() {
        let names: Vec<String> = (0..10).map(|i| format!("pin-{i}")).collect();
        let report = FilterReport {
            candidate_count: 50,
            include_kept: names,
            pipeline: vec![],
            dropped: vec![],
            min_tags: None,
        };
        let out = capture(|w| write_include_path(w, &report, false));
        // First 5 names appear, ellipsis present.
        assert!(out.contains("pin-0, pin-1, pin-2, pin-3, pin-4"), "{out}");
        assert!(out.contains(", ..."), "{out}");
        // Sixth name is truncated.
        assert!(!out.contains("pin-5"), "{out}");
    }

    #[test]
    fn include_path_verbose_shows_all_rescued() {
        let names: Vec<String> = (0..10).map(|i| format!("pin-{i}")).collect();
        let report = FilterReport {
            candidate_count: 50,
            include_kept: names,
            pipeline: vec![],
            dropped: vec![],
            min_tags: None,
        };
        let out = capture(|w| write_include_path(w, &report, true));
        assert!(out.contains("pin-0"), "{out}");
        assert!(out.contains("pin-9"), "{out}");
        assert!(!out.contains(", ..."), "{out}");
    }

    #[test]
    fn dropped_section_omitted_when_total_is_zero() {
        let report = FilterReport {
            candidate_count: 5,
            include_kept: Vec::new(),
            pipeline: vec![StageDelta {
                label: "glob \"*\"".into(),
                count_in: 5,
                count_out: 5,
            }],
            dropped: vec![],
            min_tags: None,
        };
        let out = capture(|w| {
            write_dropped(w, &report, false)?;
            Ok(())
        });
        assert!(
            out.is_empty(),
            "expected no output when no drops; got: {out:?}"
        );
    }

    #[test]
    fn exact_tag_fast_path_lists_tags_without_pipeline() {
        // Same source and target tags: print just the tag.
        let same = vec![TagPair::same("v1.0.0"), TagPair::same("v1.1.0")];
        let out = capture(|w| write_simple_tag_list(w, &same));
        assert!(out.contains("tags (2):"), "{out}");
        assert!(out.contains("    v1.0.0\n"), "{out}");
        assert!(out.contains("    v1.1.0\n"), "{out}");
        // No filter/kept/dropped sections appear.
        assert!(!out.contains("filter:"), "{out}");
        assert!(!out.contains("kept ("), "{out}");
        assert!(!out.contains("dropped"), "{out}");
    }

    #[test]
    fn exact_tag_fast_path_renders_rename_arrow() {
        // source != target: render with `source -> target`.
        let renamed = vec![TagPair::retag("v1.0.0", "stable"), TagPair::same("latest")];
        let out = capture(|w| write_simple_tag_list(w, &renamed));
        assert!(out.contains("    v1.0.0 -> stable\n"), "{out}");
        assert!(out.contains("    latest\n"), "{out}");
    }

    /// `min_tags: N` configured with kept >= N renders a "satisfied" line.
    #[test]
    fn min_tags_status_satisfied_renders_satisfied() {
        let out = capture(|w| write_min_tags_status(w, 5, Some(3), false));
        assert!(out.contains("min_tags: 3"), "{out}");
        assert!(out.contains("kept 5"), "{out}");
        assert!(out.contains("satisfied"), "{out}");
        assert!(!out.contains("FAIL"), "{out}");
    }

    /// `min_tags: N` configured with kept < N renders a hard FAIL warning so
    /// the user knows real-sync will reject this configuration.
    #[test]
    fn min_tags_status_unsatisfied_warns_real_sync_will_fail() {
        let out = capture(|w| write_min_tags_status(w, 3, Some(10), false));
        assert!(out.contains("min_tags: 10"), "{out}");
        assert!(out.contains("kept 3"), "{out}");
        assert!(out.contains("FAIL"), "{out}");
        assert!(out.contains("BelowMinTags"), "{out}");
    }

    /// `min_tags` not configured: no line emitted.
    #[test]
    fn min_tags_status_absent_when_not_configured() {
        let out = capture(|w| write_min_tags_status(w, 5, None, false));
        assert!(
            out.is_empty(),
            "expected nothing when min_tags is None: {out:?}"
        );
    }

    /// When the dropped section emitted rows, the `min_tags` line gets a
    /// leading blank to separate it visually.
    #[test]
    fn min_tags_status_blank_separator_after_drops() {
        let with_drops = capture(|w| write_min_tags_status(w, 5, Some(3), true));
        assert!(with_drops.starts_with('\n'), "{with_drops:?}");
        let without_drops = capture(|w| write_min_tags_status(w, 5, Some(3), false));
        assert!(!without_drops.starts_with('\n'), "{without_drops:?}");
    }

    /// End-to-end: a mapping with `min_tags` configured but unsatisfied
    /// renders the FAIL warning in the full mapping output. This is the
    /// scenario that motivated the fix -- dry-run silently passing what
    /// real-sync would reject.
    #[test]
    fn full_mapping_render_surfaces_min_tags_failure() {
        let report = FilterReport {
            candidate_count: 10,
            include_kept: Vec::new(),
            pipeline: vec![StageDelta {
                label: "semver \">=2.0\"".into(),
                count_in: 10,
                count_out: 2,
            }],
            dropped: vec![DropReason {
                kind: DropKind::Semver {
                    range: ">=2.0".into(),
                },
                count: 8,
                samples: (0..8).map(|i| format!("1.{i}.0")).collect(),
            }],
            min_tags: Some(5),
        };
        let mapping = mapping_with_kept_and_report(2, Some(report));
        let out = capture(|w| write_mapping(w, &mapping, false));
        assert!(out.contains("min_tags: 5"), "{out}");
        assert!(out.contains("kept 2"), "{out}");
        assert!(out.contains("FAIL"), "{out}");
        assert!(out.contains("BelowMinTags"), "{out}");
    }

    /// End-to-end: kept tags rescued via include get a `[via include]`
    /// marker so the operator can visually verify what each include pattern
    /// is rescuing.
    #[test]
    fn full_mapping_render_marks_include_rescued_tags() {
        let report = FilterReport {
            candidate_count: 5,
            include_kept: vec!["latest".into()],
            pipeline: vec![StageDelta {
                label: "semver \">=1.0\"".into(),
                count_in: 5,
                count_out: 1,
            }],
            dropped: vec![],
            min_tags: None,
        };
        let mapping = mapping_with_tags_and_report(
            vec![TagPair::same("latest"), TagPair::same("1.0.0")],
            Some(report),
        );
        let out = capture(|w| write_mapping(w, &mapping, false));
        // latest came through include path, so it is marked.
        assert!(out.contains("    latest  [via include]"), "{out}");
        // 1.0.0 came through pipeline, no marker.
        assert!(out.contains("    1.0.0\n"), "{out}");
        assert!(!out.contains("1.0.0  [via include]"), "{out}");
    }

    // -- Test fixture builders -----------------------------------------------

    fn mapping_with_kept_and_report(n: usize, report: Option<FilterReport>) -> ResolvedMapping {
        let tags: Vec<TagPair> = (0..n).map(|i| TagPair::same(format!("v{i}"))).collect();
        mapping_with_tags_and_report(tags, report)
    }

    fn mapping_with_tags_and_report(
        tags: Vec<TagPair>,
        filter_report: Option<FilterReport>,
    ) -> ResolvedMapping {
        use ocync_distribution::spec::{RegistryAuthority, RepositoryName};
        use ocync_sync::engine::{RegistryAlias, ResolvedArtifacts, TargetEntry};
        use std::collections::HashSet;
        use std::rc::Rc;
        use std::sync::Arc;

        // Build a minimal mock client. We never issue requests in formatter
        // tests; the client is a placeholder for the type system.
        let client = Arc::new(
            ocync_distribution::RegistryClientBuilder::new(
                url::Url::parse("http://127.0.0.1").unwrap(),
            )
            .build()
            .unwrap(),
        );

        ResolvedMapping {
            source_authority: RegistryAuthority::new("source.test:443"),
            source_client: client.clone(),
            source_repo: RepositoryName::new("repo").unwrap(),
            target_repo: RepositoryName::new("repo").unwrap(),
            targets: vec![TargetEntry {
                name: RegistryAlias::new("target"),
                client,
                batch_checker: None,
                existing_tags: HashSet::new(),
            }],
            tags,
            platforms: None,
            head_first: false,
            immutable_glob: None,
            artifacts_config: Rc::new(ResolvedArtifacts::default()),
            candidate_count: filter_report.as_ref().map(|r| r.candidate_count),
            filter_report,
        }
    }
}
