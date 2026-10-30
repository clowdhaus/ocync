//! Pretty-printer for `--dry-run`: per-mapping stage attrition, Pareto-sorted
//! drop attribution, include-rescue listing and `min_tags` status, rendered
//! from a `filter::FilterReport`.

use std::collections::HashSet;
use std::io::{self, Write};

use ocync_sync::engine::{ResolvedMapping, TagPair};

use crate::cli::commands::synchronize::{DroppedTarget, UnresolvedMapping};
use ocync_sync::filter::{DropKind, FilterReport};

/// Sample cap per drop reason and per include-rescue list; lifted when verbose.
/// Five is enough to spot a pattern (rc1..rc5) without flooding the terminal.
const SAMPLE_CAP: usize = 5;

/// Print dry-run output for the resolved mappings to stdout.
///
/// `verbose` lifts the sample cap so every rejected tag is printed.
pub(crate) fn print(
    mappings: &[ResolvedMapping],
    unresolved: &[UnresolvedMapping],
    dropped: &[DroppedTarget],
    verbose: bool,
) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    // Write errors here are almost always SIGPIPE (`... | head`): the reader
    // asked us to stop.
    let _ = write_to(&mut handle, mappings, unresolved, dropped, verbose);
}

/// Render the dry-run output for `mappings` to `w`.
pub(crate) fn write_to<W: Write>(
    w: &mut W,
    mappings: &[ResolvedMapping],
    unresolved: &[UnresolvedMapping],
    dropped: &[DroppedTarget],
    verbose: bool,
) -> io::Result<()> {
    // Before the plan: these are the parts of the config it could not
    // describe. Without them a wholly denied dry run prints "no mappings to
    // sync", which reads as a filter problem while the exit code reports a
    // credential failure.
    for u in unresolved {
        writeln!(w, "dry-run: {} -- UNRESOLVED: {}", u.from, u.error)?;
    }
    for d in dropped {
        writeln!(
            w,
            "dry-run: {} -- target {} unreachable: {}",
            d.from, d.registry, d.error
        )?;
    }
    if !unresolved.is_empty() || !dropped.is_empty() {
        writeln!(w)?;
    }

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
        // Exact-tag fast path: source tags were never enumerated.
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

/// A flat tag list for the exact-tag fast path, which has no `FilterReport`.
fn write_simple_tag_list<W: Write>(w: &mut W, tags: &[TagPair]) -> io::Result<()> {
    writeln!(w, "  tags ({}):", tags.len())?;
    write_tag_pairs(w, tags)
}

fn write_tag_pairs<W: Write>(w: &mut W, tags: &[TagPair]) -> io::Result<()> {
    for tag_pair in tags {
        writeln!(w, "    {tag_pair}")?;
    }
    Ok(())
}

/// Include-rescue section: count and sample names, `, ...` when capped.
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

/// Returns `true` when at least one row was emitted, so the caller knows
/// whether the `min_tags` status needs a leading blank line.
fn write_dropped<W: Write>(w: &mut W, report: &FilterReport, verbose: bool) -> io::Result<bool> {
    let total: usize = report.dropped.iter().map(|d| d.count).sum();
    if total == 0 {
        return Ok(false);
    }
    writeln!(w, "  dropped ({total}):")?;
    for reason in &report.dropped {
        let samples_display = render_samples(&reason.samples, verbose);
        // `LatestCap` is already a full clause ("over latest=N limit").
        let display_label = match reason.kind {
            DropKind::LatestCap { .. } => reason.kind.to_string(),
            _ => format!("by {}", reason.kind),
        };
        writeln!(
            w,
            "    {:>4}  {:<28}{}",
            reason.count, display_label, samples_display
        )?;
        if matches!(
            reason.kind,
            DropKind::BuiltinExclude | DropKind::DefaultsExclude { .. }
        ) {
            writeln!(
                w,
                "          hint: to keep these tags, list them under include: (globs supported)"
            )?;
        }
    }
    Ok(true)
}

/// Renders nothing without `min_tags`; warns when `kept < min_tags` that a real
/// sync fails with `BelowMinTags`.
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

/// Joins samples with `, `, truncating to `SAMPLE_CAP` plus `, ...` unless `verbose`.
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
    use crate::cli::test_support::mapping_with_tags_and_report;
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
                    kind: DropKind::BuiltinExclude,
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

    /// Asserts every `present` substring appears in `out` and no `absent` one does.
    fn assert_output(name: &str, out: &str, present: &[&str], absent: &[&str]) {
        for want in present {
            assert!(out.contains(want), "{name}: missing {want:?} in {out}");
        }
        for unwanted in absent {
            assert!(
                !out.contains(unwanted),
                "{name}: unexpected {unwanted:?} in {out}"
            );
        }
    }

    #[test]
    fn dropped_samples_respect_the_cap() {
        let cases: [(&str, bool, &[&str], &[&str]); 2] = [
            (
                "capped: first 5 of 6 then ellipsis",
                false,
                &["3.20.3, 3.20.2, 3.20.1, 3.20.0, 3.19.7, ..."],
                &["3.19.6"],
            ),
            ("verbose: all 6, no ellipsis", true, &["3.19.6"], &[", ..."]),
        ];
        for (name, verbose, present, absent) in cases {
            let report = report_fixture();
            let out = capture(|w| {
                write_dropped(w, &report, verbose)?;
                Ok(())
            });
            assert_output(name, &out, present, absent);
        }
    }

    /// (name, rescued tags, verbose, expected substrings, absent substrings)
    type IncludeCase = (
        &'static str,
        Vec<String>,
        bool,
        &'static [&'static str],
        &'static [&'static str],
    );

    #[test]
    fn include_path_renders_count_and_rescued_names() {
        let ten: Vec<String> = (0..10).map(|i| format!("pin-{i}")).collect();
        let cases: [IncludeCase; 3] = [
            (
                "count and names",
                vec!["latest".into(), "latest-dev".into(), "edge".into()],
                false,
                &["include path:", "rescued (3):", "latest, latest-dev, edge"],
                &[],
            ),
            (
                "long list truncated by default",
                ten.clone(),
                false,
                &["pin-0, pin-1, pin-2, pin-3, pin-4", ", ..."],
                &["pin-5"],
            ),
            (
                "verbose shows all",
                ten,
                true,
                &["pin-0", "pin-9"],
                &[", ..."],
            ),
        ];
        for (name, include_kept, verbose, present, absent) in cases {
            let report = FilterReport {
                candidate_count: 50,
                include_kept,
                pipeline: vec![],
                dropped: vec![],
                min_tags: None,
            };
            let out = capture(|w| write_include_path(w, &report, verbose));
            assert_output(name, &out, present, absent);
        }
    }

    #[test]
    fn exact_tag_fast_path_lists_tags_without_pipeline() {
        let cases: [(&str, Vec<TagPair>, &[&str]); 2] = [
            (
                "same source and target",
                vec![TagPair::same("v1.0.0"), TagPair::same("v1.1.0")],
                &["tags (2):", "    v1.0.0\n", "    v1.1.0\n"],
            ),
            (
                "rename arrow",
                vec![TagPair::retag("v1.0.0", "stable"), TagPair::same("latest")],
                &["    v1.0.0 -> stable\n", "    latest\n"],
            ),
        ];
        for (name, tags, present) in cases {
            let mapping = mapping_with_tags_and_report(tags, None);
            let out = capture(|w| write_mapping(w, &mapping, false));
            assert_output(name, &out, present, &["filter:", "kept (", "dropped"]);
        }
    }

    #[test]
    fn min_tags_status_reports_whether_it_is_satisfied() {
        struct Case {
            name: &'static str,
            kept: usize,
            min_tags: usize,
            present: &'static [&'static str],
            absent: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "satisfied",
                kept: 5,
                min_tags: 3,
                present: &["min_tags: 3", "kept 5", "satisfied"],
                absent: &["FAIL"],
            },
            Case {
                name: "unsatisfied warns real sync will fail",
                kept: 3,
                min_tags: 10,
                present: &["min_tags: 10", "kept 3", "FAIL", "BelowMinTags"],
                absent: &[],
            },
        ];
        for case in cases {
            let out = capture(|w| write_min_tags_status(w, case.kept, Some(case.min_tags), false));
            assert_output(case.name, &out, case.present, case.absent);
        }
    }

    #[test]
    fn system_exclude_emits_override_hint() {
        let report = report_fixture();
        let out = capture(|w| {
            write_dropped(w, &report, false)?;
            Ok(())
        });
        assert!(
            out.contains("hint: to keep these tags, list them under include:"),
            "{out}"
        );
    }

    #[test]
    fn empty_mappings_emits_no_mappings_message() {
        let out = capture(|w| write_to(w, &[], &[], &[], false));
        assert_eq!(out, "dry-run: no mappings to sync\n");
    }

    /// A wholly denied dry run must say so, not just "no mappings to sync".
    #[test]
    fn unresolved_mappings_are_named_before_the_plan() {
        let unresolved = [UnresolvedMapping {
            from: "library/nginx".into(),
            error: "registry error: 403 Forbidden".into(),
            code: crate::cli::ExitCode::AuthError,
        }];
        let dropped = [DroppedTarget {
            from: "library/alpine".into(),
            registry: "backup".into(),
            error: "unavailable".into(),
        }];

        let out = capture(|w| write_to(w, &[], &unresolved, &dropped, false));

        assert!(out.contains("library/nginx"), "{out}");
        assert!(out.contains("403 Forbidden"), "{out}");
        assert!(out.contains("target backup unreachable"), "{out}");
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
    fn min_tags_status_absent_when_not_configured() {
        let out = capture(|w| write_min_tags_status(w, 5, None, false));
        assert!(
            out.is_empty(),
            "expected nothing when min_tags is None: {out:?}"
        );
    }

    /// After dropped rows, the `min_tags` line gets a leading blank separator.
    #[test]
    fn min_tags_status_blank_separator_after_drops() {
        let with_drops = capture(|w| write_min_tags_status(w, 5, Some(3), true));
        assert!(with_drops.starts_with('\n'), "{with_drops:?}");
        let without_drops = capture(|w| write_min_tags_status(w, 5, Some(3), false));
        assert!(!without_drops.starts_with('\n'), "{without_drops:?}");
    }

    /// Dry-run must not silently pass a mapping that a real sync would reject.
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
                    range: ocync_sync::Range::parse(">=2.0").unwrap(),
                },
                count: 8,
                samples: (0..8).map(|i| format!("1.{i}.0")).collect(),
            }],
            min_tags: Some(5),
        };
        let tags = (0..2).map(|i| TagPair::same(format!("v{i}"))).collect();
        let mapping = mapping_with_tags_and_report(tags, Some(report));
        let out = capture(|w| write_mapping(w, &mapping, false));
        assert!(out.contains("min_tags: 5"), "{out}");
        assert!(out.contains("kept 2"), "{out}");
        assert!(out.contains("FAIL"), "{out}");
        assert!(out.contains("BelowMinTags"), "{out}");
    }

    /// Include-rescued kept tags are marked so each include pattern's effect is visible.
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
        assert!(out.contains("    latest  [via include]"), "{out}");
        // Came through the pipeline, so unmarked.
        assert!(out.contains("    1.0.0\n"), "{out}");
        assert!(!out.contains("1.0.0  [via include]"), "{out}");
    }
}
