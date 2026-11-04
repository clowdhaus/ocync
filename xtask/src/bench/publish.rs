//! Published runs: the one benchmark run behind every public number.
//!
//! A run started with `--publish` that passes every check here sets
//! `published: true` on its record, and only such a record writes the
//! surfaces that show a number (the performance page, the README, the
//! homepage and the banners). See `docs/src/content/design/benchmark.md`.

use serde::{Deserialize, Serialize};

use crate::bench::runner::Tool;
use crate::bench::{Scenario, ToolSelection, corpus, target, value_name};

/// The rounds a published run is asked for and finishes.
pub(crate) const PUBLISHED_ROUNDS: usize = 5;

/// The tools a published run compares, in the order of the page's columns.
pub(crate) const PUBLISHED_TOOLS: [Tool; 7] = [
    Tool::Ocync,
    Tool::Dregsy,
    Tool::Regsync,
    Tool::Skopeo,
    Tool::Crane,
    Tool::Regctl,
    Tool::Oras,
];

/// Whether a run is the published one or an archived experiment.
///
/// Stored in the run record as `published: true`, left out otherwise, so a
/// record written before publication existed reads as an experiment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "bool", into = "bool")]
pub(crate) enum Publication {
    /// Recorded in the archive and written to no surface.
    #[default]
    Archive,
    /// Recorded in the archive and written to every surface that shows a
    /// number.
    Publish,
}

impl From<bool> for Publication {
    fn from(published: bool) -> Self {
        if published {
            Self::Publish
        } else {
            Self::Archive
        }
    }
}

impl From<Publication> for bool {
    fn from(publication: Publication) -> Self {
        publication == Publication::Publish
    }
}

impl Publication {
    /// Whether this is [`Self::Archive`], which a record leaves unwritten.
    pub(crate) fn is_archive(&self) -> bool {
        *self == Self::Archive
    }
}

/// The options of a run that shape what it measures, besides the ones
/// [`ToolSelection`] holds, as `bench` and `bench-remote` each hold them.
pub(crate) struct Shape {
    /// The scenario selection.
    pub(crate) scenario: Scenario,
    /// The registry the run syncs into.
    pub(crate) target: target::Kind,
    /// The corpus the run syncs.
    pub(crate) corpus: corpus::Id,
    /// `--limit`, when given.
    pub(crate) limit: Option<usize>,
    /// Whether `--skip-registries` names any registry.
    pub(crate) skips_registries: bool,
    /// Whether `--skip-prewarm` is given.
    pub(crate) skips_prewarm: bool,
    /// Whether `--no-proxy` is given.
    pub(crate) disables_proxy: bool,
    /// Whether `--measure-proxy-overhead` is given.
    pub(crate) measures_proxy_overhead: bool,
}

/// Refuses a `--publish` run that is not the shape of a published one, naming
/// every option that makes it so. Called by `bench` and `bench-remote` before
/// they do anything.
pub(crate) fn ensure_publishable(selection: &ToolSelection, shape: &Shape) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    let mut wanted = PUBLISHED_TOOLS.to_vec();
    wanted.sort();
    let mut given = selection.tools.clone();
    given.sort();
    given.dedup();
    if given != wanted {
        let all: Vec<String> = PUBLISHED_TOOLS.iter().map(value_name).collect();
        problems.push(format!(
            "--tools must name exactly {} (a tool that fails stays in the table)",
            all.join(",")
        ));
    }
    if selection.rounds.get() != PUBLISHED_ROUNDS {
        problems.push(format!(
            "--rounds must be {PUBLISHED_ROUNDS}, not {}",
            selection.rounds
        ));
    }
    let refused = [
        ("--baseline-ref", selection.baseline_ref.is_some()),
        (
            "--max-concurrent-transfers",
            selection.max_concurrent_transfers.is_some(),
        ),
        ("--limit", shape.limit.is_some()),
        ("--skip-registries", shape.skips_registries),
        ("--skip-prewarm", shape.skips_prewarm),
        ("--no-proxy", shape.disables_proxy),
        ("--measure-proxy-overhead", shape.measures_proxy_overhead),
    ];
    for (option, set) in refused {
        if set {
            problems.push(format!("{option} is not allowed"));
        }
    }
    if !shape.corpus.is_main() {
        problems.push(format!(
            "the corpus must be the main one, not {}",
            shape.corpus.label()
        ));
    }
    if shape.target != target::Kind::Ecr {
        problems.push(format!(
            "the target must be ECR, not {}",
            shape.target.name()
        ));
    }
    if !matches!(shape.scenario, Scenario::Sync | Scenario::All) {
        problems.push(format!(
            "the scenario must include the cold and warm sync (sync or all), not {}",
            value_name(&shape.scenario)
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "--publish takes only the run the design defines: {}",
            problems.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::remote::CommitSha;

    fn selection() -> ToolSelection {
        ToolSelection {
            tools: PUBLISHED_TOOLS.to_vec(),
            baseline_ref: None,
            rounds: PUBLISHED_ROUNDS.to_string().parse().unwrap(),
            max_concurrent_transfers: None,
        }
    }

    fn shape() -> Shape {
        Shape {
            scenario: Scenario::Sync,
            target: target::Kind::Ecr,
            corpus: corpus::Id::Main,
            limit: None,
            skips_registries: false,
            skips_prewarm: false,
            disables_proxy: false,
            measures_proxy_overhead: false,
        }
    }

    #[test]
    fn the_published_shape_is_accepted_in_any_tool_order_and_for_sync_or_all() {
        assert_eq!(ensure_publishable(&selection(), &shape()), Ok(()));
        let mut reversed = selection();
        reversed.tools.reverse();
        assert_eq!(ensure_publishable(&reversed, &shape()), Ok(()));
        let all = Shape {
            scenario: Scenario::All,
            ..shape()
        };
        assert_eq!(ensure_publishable(&selection(), &all), Ok(()));
    }

    #[test]
    fn every_departure_from_the_published_shape_is_refused_by_name() {
        // Catches: a published run that is not the one the design defines,
        // each departure being a way the page's numbers stop being comparable.
        let mut one_tool = selection();
        one_tool.tools = vec![Tool::Ocync];
        let mut six_tools = selection();
        six_tools.tools.pop();
        let mut three_rounds = selection();
        three_rounds.rounds = "3".parse().unwrap();
        let mut baseline = selection();
        baseline.baseline_ref = Some(CommitSha::fixture());
        let mut capped = selection();
        capped.max_concurrent_transfers = std::num::NonZeroUsize::new(20);

        let refused = |selection: ToolSelection, shape: Shape, needle: &str| {
            let err = ensure_publishable(&selection, &shape).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
        };
        refused(one_tool, shape(), "--tools");
        refused(six_tools, shape(), "--tools");
        refused(three_rounds, shape(), "--rounds must be 5, not 3");
        refused(baseline, shape(), "--baseline-ref");
        refused(capped, shape(), "--max-concurrent-transfers");
        refused(
            selection(),
            Shape {
                limit: Some(3),
                ..shape()
            },
            "--limit",
        );
        refused(
            selection(),
            Shape {
                skips_registries: true,
                ..shape()
            },
            "--skip-registries",
        );
        refused(
            selection(),
            Shape {
                skips_prewarm: true,
                ..shape()
            },
            "--skip-prewarm",
        );
        refused(
            selection(),
            Shape {
                disables_proxy: true,
                ..shape()
            },
            "--no-proxy",
        );
        refused(
            selection(),
            Shape {
                measures_proxy_overhead: true,
                ..shape()
            },
            "--measure-proxy-overhead",
        );
        refused(
            selection(),
            Shape {
                corpus: corpus::Id::Named(corpus::CorpusName::new("small")),
                ..shape()
            },
            "the main one, not small",
        );
        refused(
            selection(),
            Shape {
                target: target::Kind::Acr,
                ..shape()
            },
            "must be ECR, not ACR",
        );
        for scenario in [Scenario::Partial, Scenario::Scale, Scenario::Prepare] {
            refused(
                selection(),
                Shape {
                    scenario,
                    ..shape()
                },
                "cold and warm sync",
            );
        }
    }

    #[test]
    fn a_refusal_names_every_departure_at_once() {
        let mut selection = selection();
        selection.rounds = "3".parse().unwrap();
        let err = ensure_publishable(
            &selection,
            &Shape {
                limit: Some(3),
                disables_proxy: true,
                ..shape()
            },
        )
        .unwrap_err();
        for needle in ["--rounds", "--limit", "--no-proxy"] {
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn publication_is_stored_as_a_true_flag_and_an_archive_stores_nothing() {
        assert_eq!(
            serde_json::to_string(&Publication::Publish).unwrap(),
            "true"
        );
        assert_eq!(
            serde_json::from_str::<Publication>("false").unwrap(),
            Publication::Archive
        );
        assert_eq!(
            serde_json::from_str::<Publication>("true").unwrap(),
            Publication::Publish
        );
    }
}
