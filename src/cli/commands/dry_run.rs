//! Pretty-printer for `--dry-run`. Renders per-mapping stage attrition and
//! Pareto-sorted drop attribution from a `filter::FilterReport`.

use std::io::{self, Write};

use ocync_sync::engine::{ResolvedMapping, TagPair};
use ocync_sync::filter::{DropKind, FilterReport};

/// Default sample cap per drop reason. Removed when verbose.
const SAMPLE_CAP: usize = 5;

/// Print dry-run output for the resolved mappings to stdout.
///
/// `verbose` is the global `cli.verbose >= 1` toggle; when true, all
/// rejected tags per drop reason are printed (no sample cap).
pub(crate) fn print(mappings: &[ResolvedMapping], verbose: bool) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = write_to(&mut handle, mappings, verbose);
}

fn write_to<W: Write>(w: &mut W, mappings: &[ResolvedMapping], verbose: bool) -> io::Result<()> {
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
        "dry-run: {} -> {}  =>  [{}]",
        m.source_repo,
        m.target_repo,
        target_names.join(", ")
    )?;

    let Some(report) = m.filter_report.as_ref() else {
        // Exact-tag fast path: no report available. Fall back to a simple
        // tag list (matches the pre-Thread-2 print_dry_run behavior).
        return write_simple_tag_list(w, &m.tags);
    };

    writeln!(w, "  source candidates: {}", report.candidates)?;
    writeln!(w)?;

    if report.include_kept > 0 {
        write_include_path(w, report)?;
        writeln!(w)?;
    }

    write_pipeline(w, report)?;
    writeln!(w)?;

    write_kept(w, m, report)?;
    writeln!(w)?;

    write_dropped(w, report, verbose)?;
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

fn write_include_path<W: Write>(w: &mut W, report: &FilterReport) -> io::Result<()> {
    writeln!(w, "  include path:")?;
    writeln!(
        w,
        "    include kept                 -> {}",
        report.include_kept
    )?;
    Ok(())
}

fn write_pipeline<W: Write>(w: &mut W, report: &FilterReport) -> io::Result<()> {
    writeln!(w, "  pipeline:")?;
    for stage in &report.pipeline {
        let delta = stage.count_in as isize - stage.count_out as isize;
        let delta_str = if delta != 0 {
            format!("    (-{})", delta.abs())
        } else {
            String::new()
        };
        writeln!(
            w,
            "    {:<28} {:>4} -> {:<4}{}",
            stage.label, stage.count_in, stage.count_out, delta_str
        )?;
    }
    Ok(())
}

fn write_kept<W: Write>(w: &mut W, m: &ResolvedMapping, report: &FilterReport) -> io::Result<()> {
    let header = if report.include_kept > 0 {
        format!("  kept ({}):  include first, then pipeline", m.tags.len())
    } else {
        format!("  kept ({}):", m.tags.len())
    };
    writeln!(w, "{header}")?;
    write_tag_pairs(w, &m.tags)
}

fn write_dropped<W: Write>(w: &mut W, report: &FilterReport, verbose: bool) -> io::Result<()> {
    let total: usize = report.dropped.iter().map(|d| d.count).sum();
    if total == 0 {
        return Ok(());
    }
    writeln!(w, "  dropped {total}:")?;
    for reason in &report.dropped {
        let samples_display: String = if verbose || reason.samples.len() <= SAMPLE_CAP {
            reason.samples.join(", ")
        } else {
            let head = reason.samples[..SAMPLE_CAP].join(", ");
            format!("{head}, ...")
        };
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
                "          {:<28}to keep prereleases, list patterns under include: (globs supported)",
                ""
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocync_sync::filter::{DropReason, FilterReport, StageDelta};

    fn report_fixture() -> FilterReport {
        FilterReport {
            candidates: 50,
            include_kept: 0,
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
        let out = capture(|w| write_dropped(w, &report, false));
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
        let out = capture(|w| write_dropped(w, &report, true));
        // All 6 samples present, no trailing ellipsis.
        assert!(out.contains("3.19.6"), "{out}");
        assert!(!out.contains(", ..."), "{out}");
    }

    #[test]
    fn system_exclude_emits_override_hint() {
        let report = report_fixture();
        let out = capture(|w| write_dropped(w, &report, false));
        assert!(
            out.contains("to keep prereleases, list patterns under include:"),
            "{out}"
        );
    }

    #[test]
    fn pareto_section_orders_by_count_descending() {
        let report = report_fixture();
        let out = capture(|w| write_dropped(w, &report, false));
        let latest_pos = out
            .find("over latest=3 limit")
            .expect("latest line present");
        let sys_pos = out
            .find("system-exclude")
            .expect("system-exclude line present");
        assert!(latest_pos < sys_pos, "{out}");
    }

    #[test]
    fn drop_labels_prefix_by_except_over() {
        let report = report_fixture();
        let out = capture(|w| write_dropped(w, &report, false));
        // "over latest=N limit" reads naturally without prefix.
        assert!(out.contains("over latest=3 limit"), "{out}");
        // Other labels get "by " prefix.
        assert!(out.contains("by system-exclude"), "{out}");
    }

    #[test]
    fn empty_mappings_emits_no_mappings_message() {
        let out = capture(|w| write_to(w, &[], false));
        assert_eq!(out, "dry-run: no mappings to sync\n");
    }

    #[test]
    fn include_path_renders_count_and_header() {
        let report = FilterReport {
            candidates: 50,
            include_kept: 3,
            pipeline: vec![],
            dropped: vec![],
        };
        let out = capture(|w| write_include_path(w, &report));
        assert!(out.contains("include path:"), "{out}");
        assert!(out.contains("include kept"), "{out}");
        assert!(out.contains("-> 3"), "{out}");
    }

    #[test]
    fn dropped_section_omitted_when_total_is_zero() {
        let report = FilterReport {
            candidates: 5,
            include_kept: 0,
            pipeline: vec![StageDelta {
                label: "glob \"*\"".into(),
                count_in: 5,
                count_out: 5,
            }],
            dropped: vec![],
        };
        let out = capture(|w| write_dropped(w, &report, false));
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
        // No pipeline/kept/dropped sections appear.
        assert!(!out.contains("pipeline:"), "{out}");
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
}
