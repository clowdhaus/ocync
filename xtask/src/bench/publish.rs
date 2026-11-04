//! Published runs: the one benchmark run behind every public number.
//!
//! A run started with `--publish` that passes every check here sets
//! `published: true` on its record, and only such a record writes the
//! surfaces that show a number (the performance page, the README, the
//! homepage and the banners). See `docs/src/content/design/benchmark.md`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use ocync_sync::engine::DEFAULT_MAX_CONCURRENT_TRANSFERS;
use serde::{Deserialize, Serialize};

use crate::bench::report::{
    self, Coverage, Detail, Prewarm, RoundCount, RunRecord, ScenarioKind, ScenarioResult, ToolRun,
};
use crate::bench::runner::Tool;
use crate::bench::{Scenario, ToolSelection, corpus, source, target, value_name};

/// The rounds a published run is asked for and finishes.
pub(crate) const PUBLISHED_ROUNDS: usize = 5;

/// The tools a published run compares, in the order of the page's columns.
/// A tool joins only after it has finished a full benchmark run.
pub(crate) const PUBLISHED_TOOLS: [Tool; 3] = [Tool::Ocync, Tool::Dregsy, Tool::Regsync];

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
    /// The registry the tools read the corpus from.
    pub(crate) source: source::Kind,
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
    /// Whether `--regression` is given: a run that reports a regression
    /// must not have rewritten the surfaces.
    pub(crate) checks_regression: bool,
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
        ("--regression", shape.checks_regression),
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
    if shape.source != source::Kind::Controlled {
        problems.push(format!(
            "--source must be controlled, not {}: public registries rate-limit the repeated \
             rounds",
            value_name(&shape.source)
        ));
    }
    if !matches!(shape.scenario, Scenario::Sync) {
        problems.push(format!(
            "the scenario must be sync, the cold and warm sync the page shows, not {}",
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

/// Whether `--publish` also renders the banner PNGs from the SVGs it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Banners {
    /// Run `npm run render-banners` in `docs` after writing the SVGs.
    Render,
    /// Leave the PNGs as they are.
    #[cfg(test)]
    Skip,
}

/// The performance page, relative to the repository root.
const PERFORMANCE_PAGE: &str = "docs/src/content/performance.md";

/// The README, relative to the repository root.
const README: &str = "README.md";

/// The homepage, relative to the repository root.
const HOMEPAGE: &str = "docs/src/pages/index.astro";

/// The README banner's SVG, relative to the repository root.
const ECR_BANNER: &str = "docs/public/ecr-banner.svg";

/// The social card's SVG, relative to the repository root.
const OG_IMAGE: &str = "docs/public/og-image.svg";

/// The documentation site, relative to the repository root.
const DOCS_DIR: &str = "docs";

/// The `docs` npm script that rasterizes the two SVG banners.
const RENDER_SCRIPT: &str = "render-banners";

// ---------------------------------------------------------------------------
// The record's rounds
// ---------------------------------------------------------------------------

/// The tool a record's label names. The baseline's label, which carries a
/// commit, is no published tool.
fn tool_of(label: &str) -> Result<Tool, String> {
    Tool::from_str(label, true).map_err(|_| format!("the record names no published tool `{label}`"))
}

/// The rounds of a scenario in a record, as the table code takes them.
fn rounds_in(record: &RunRecord, kind: ScenarioKind) -> Result<Vec<ScenarioResult>, String> {
    let name = kind.label();
    record
        .scenarios
        .iter()
        .filter(|scenario| scenario.name == name)
        .map(|scenario| {
            let runs = scenario
                .tools
                .iter()
                .map(|tool| {
                    Ok(ToolRun {
                        tool: tool_of(&tool.tool)?,
                        wall_clock_secs: tool.wall_clock_secs,
                        peak_rss_kb: tool.peak_rss_kb,
                        proxy_metrics: Some(tool.metrics.clone()),
                        phase_timing_ms: tool.phase_timing_ms.clone(),
                        direct_wall_clock_secs: tool.direct_wall_clock_secs,
                    })
                })
                .collect::<Result<_, String>>()?;
            Ok(ScenarioResult {
                round: scenario.round,
                ..ScenarioResult::unshuffled(kind, runs)
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The numbers
// ---------------------------------------------------------------------------

/// The figures every surface quotes, computed from one run's cold sync and
/// nowhere else.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Headline {
    /// The median over the rounds of regsync's wall clock divided by ocync's,
    /// round by round.
    speed: f64,
    /// How many percent fewer requests ocync sent than regsync, from the
    /// medians.
    fewer_requests_percent: f64,
    /// How many decimal gigabytes fewer ocync's responses held than regsync's,
    /// from the medians.
    fewer_gigabytes: f64,
}

impl Headline {
    /// The figures of the cold sync in `record`. An error when regsync is not
    /// in every round with a value, or when ocync is not ahead of it in a
    /// figure after rounding, since every surface words the figures as gains.
    fn of(record: &RunRecord) -> Result<Self, String> {
        let results = rounds_in(record, ScenarioKind::Cold)?;
        let rounds: Vec<&ScenarioResult> = results.iter().collect();
        let by_tool = report::rounds_by_tool(&rounds);
        let row = |label: &str| {
            report::metric_rows()
                .into_iter()
                .find(|row| row.label == label)
                .expect("the table has the row")
        };
        let median = |tool: Tool, row: &report::MetricRow| {
            let runs = by_tool.get(&tool)?;
            let spread = report::spread_of(runs.iter().filter_map(|run| (row.rank)(run)))?;
            (runs.len() == rounds.len()).then_some(spread.median)
        };
        let missing = |what: &str| {
            format!(
                "ocync or regsync has no {what} in every cold round, so there is nothing to \
                 compare them by"
            )
        };

        let wall = row(report::WALL_CLOCK_LABEL);
        let speed = report::round_ratio(&rounds, Tool::Regsync, &wall)
            .ok_or_else(|| missing("wall clock"))?
            .spread
            .median;
        let requests = row(report::REQUESTS_LABEL);
        let (Some(ocync), Some(regsync)) = (
            median(Tool::Ocync, &requests),
            median(Tool::Regsync, &requests),
        ) else {
            return Err(missing("request count"));
        };
        let fewer_requests_percent = (1.0 - ocync / regsync) * 100.0;
        let bytes = row(report::RESPONSE_BYTES_LABEL);
        let (Some(ocync), Some(regsync)) =
            (median(Tool::Ocync, &bytes), median(Tool::Regsync, &bytes))
        else {
            return Err(missing("response byte count"));
        };
        let headline = Self {
            speed,
            fewer_requests_percent,
            fewer_gigabytes: (regsync - ocync) / 1e9,
        };
        for (shown, what) in [
            (
                headline.speed_text() != "1.0" && headline.speed > 1.0,
                "wall clock",
            ),
            (headline.requests_percent() > 0, "requests"),
            (
                headline.gigabytes_text() != "0.0" && headline.fewer_gigabytes > 0.0,
                "response bytes",
            ),
        ] {
            if !shown {
                return Err(format!(
                    "ocync was not ahead of regsync in {what}, and every surface words the \
                     figures as gains"
                ));
            }
        }
        Ok(headline)
    }

    /// The speed to one decimal: `2.9`.
    fn speed_text(&self) -> String {
        format!("{:.1}", self.speed)
    }

    /// The speed as a banner shows it: `2.9x`.
    fn speed_banner(&self) -> String {
        format!("{}x", self.speed_text())
    }

    /// The request saving in whole percent.
    fn requests_percent(&self) -> u32 {
        self.fewer_requests_percent.round().max(0.0) as u32
    }

    /// The byte saving in decimal gigabytes to one decimal: `9.6`.
    fn gigabytes_text(&self) -> String {
        format!("{:.1}", self.fewer_gigabytes)
    }
}

// ---------------------------------------------------------------------------
// The surfaces
// ---------------------------------------------------------------------------

/// How a generated region is fenced in its file.
#[derive(Clone, Copy, Debug)]
enum Fence {
    /// `<!-- NAME:START -->`, in Markdown, HTML and SVG.
    Html,
    /// `// NAME:START`, in the script of an Astro page.
    Script,
    /// `# NAME:START`, in YAML front matter.
    Yaml,
}

impl Fence {
    fn marker(self, name: &str, edge: &str) -> String {
        match self {
            Self::Html => format!("<!-- {name}:{edge} -->"),
            Self::Script => format!("// {name}:{edge}"),
            Self::Yaml => format!("# {name}:{edge}"),
        }
    }
}

/// Where a region's text sits between its markers.
#[derive(Clone, Copy, Debug)]
enum Layout {
    /// On the lines between the lines that hold the markers.
    Lines,
    /// Directly between the markers, on one line.
    Inline,
}

/// The text a surface shows between a pair of markers.
#[derive(Debug)]
struct Region {
    path: &'static str,
    fence: Fence,
    name: &'static str,
    layout: Layout,
    text: String,
}

impl Region {
    /// The byte range of the text between the markers that `content` holds,
    /// exactly one pair of them.
    fn span(&self, content: &str) -> Result<std::ops::Range<usize>, String> {
        let once = |edge: &str| {
            let marker = self.fence.marker(self.name, edge);
            let mut found = content.match_indices(&marker).map(|(at, _)| at);
            match (found.next(), found.next()) {
                (Some(at), None) => Ok((at, marker.len())),
                (None, _) => Err(format!("{} has no `{marker}`", self.path)),
                (Some(_), Some(_)) => Err(format!("{} has `{marker}` more than once", self.path)),
            }
        };
        let (start, start_len) = once("START")?;
        let (end, _) = once("END")?;
        if end < start + start_len {
            return Err(format!(
                "{}: {} END comes before START",
                self.path, self.name
            ));
        }
        Ok(match self.layout {
            Layout::Inline => start + start_len..end,
            Layout::Lines => {
                let first = content[start..]
                    .find('\n')
                    .map(|newline| start + newline + 1)
                    .ok_or_else(|| format!("{}: nothing follows {} START", self.path, self.name))?;
                let last = content[..end].rfind('\n').map_or(0, |newline| newline + 1);
                if last < first {
                    return Err(format!(
                        "{}: {} START and END are not on lines of their own",
                        self.path, self.name
                    ));
                }
                first..last
            }
        })
    }

    /// `content` with this region's text in place of what is there.
    fn applied(&self, content: &str) -> Result<String, String> {
        let span = self.span(content)?;
        Ok(format!(
            "{}{}{}",
            &content[..span.start],
            self.text,
            &content[span.end..]
        ))
    }

    /// Whether `content` holds this region's text.
    #[cfg(test)]
    fn holds(&self, content: &str) -> Result<bool, String> {
        Ok(content[self.span(content)?] == self.text)
    }
}

/// The short form of a commit the page names: its first 7 digits.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// The source kind the page names, with the region of the controlled source.
fn source_phrase(record: &RunRecord) -> String {
    match (record.source, record.source_region.as_deref()) {
        (source::Kind::Controlled, Some(region)) => {
            format!("the controlled source (ECR {region})")
        }
        (source::Kind::Controlled, None) => "the controlled source".to_owned(),
        (source::Kind::Public, _) => "the public registries".to_owned(),
    }
}

/// The page block for one scenario of the run: a line naming the run, the
/// medians of its rounds as a table, and the footnotes.
fn page_block(record: &RunRecord, kind: ScenarioKind, description: &str) -> Result<String, String> {
    let results = rounds_in(record, kind)?;
    if results.is_empty() {
        return Err(format!("the record has no {} rounds", kind.label()));
    }
    let rounds: Vec<&ScenarioResult> = results.iter().collect();
    let machine = &record.machine;
    let mut out =
        format!(
        "Measured {date} (run {run}, commit {commit}) on {instance} ({arch}, {vcpus} vCPUs, \
         {memory:.0} GiB, {network}). Full corpus: {images} images, {tags} tags. {description} \
         from {source} to ECR {region}. Median of {count} rounds. {settings}All traffic \
         routed through bench-proxy for byte-accurate measurement.\n\n",
        date = record.timestamp.get(..10).unwrap_or(&record.timestamp),
        run = record.timestamp,
        commit = short(&record.git_ref),
        instance = machine.instance_type,
        arch = machine.arch,
        vcpus = machine.vcpus,
        memory = machine.memory_gib,
        network = machine.network,
        images = record.corpus.images,
        tags = record.corpus.tags,
        source = source_phrase(record),
        region = machine.region,
        count = rounds.len(),
        settings = record.max_concurrent_transfers.map_or_else(String::new, |cap| {
            format!(
                "ocync ran at its default of {cap} images at once and every other tool at its \
                 own defaults. "
            )
        }),
    );
    report::render_metric_table(
        None,
        &PUBLISHED_TOOLS,
        &report::rounds_by_tool(&rounds),
        Detail::Median,
        &mut out,
    );
    out.push_str(&report::footnotes(None, &rounds));
    Ok(out)
}

/// The tool every published figure is measured against, as a banner's labels
/// name it.
const COMPARED_WITH: &str = "regsync";

/// Which way a label's text is anchored to its `x`.
#[derive(Clone, Copy)]
enum Anchor {
    /// Starts at `x`.
    Start,
    /// Centered on `x`.
    Middle,
}

/// Where and how a banner SVG sets the two lines of the label beside a figure.
struct StatLabel {
    x: u32,
    /// The baseline of each of the two lines.
    ys: [u32; 2],
    size: u32,
    fill: &'static str,
    anchor: Anchor,
}

impl StatLabel {
    /// The `<text>` elements of the two `lines`, one per line, each on a line
    /// of the SVG.
    fn text(&self, lines: [&str; 2]) -> String {
        let anchor = match self.anchor {
            Anchor::Start => "",
            Anchor::Middle => " text-anchor=\"middle\"",
        };
        lines
            .iter()
            .zip(self.ys)
            .map(|(line, y)| {
                format!(
                    "  <text x=\"{x}\" y=\"{y}\"{anchor} font-family=\"system-ui, -apple-system, \
                     'Segoe UI', sans-serif\" font-size=\"{size}\" fill=\"{fill}\">{line}</text>\n",
                    x = self.x,
                    size = self.size,
                    fill = self.fill,
                )
            })
            .collect()
    }
}

/// The labels beside the speed and the request figures of a banner SVG.
struct SvgLabels {
    speed: StatLabel,
    requests: StatLabel,
}

/// The README banner's labels, centered under each figure.
const ECR_BANNER_LABELS: SvgLabels = SvgLabels {
    speed: StatLabel {
        x: 500,
        ys: [175, 192],
        size: 13,
        fill: "#8b92b0",
        anchor: Anchor::Middle,
    },
    requests: StatLabel {
        x: 660,
        ys: [175, 192],
        size: 13,
        fill: "#8b92b0",
        anchor: Anchor::Middle,
    },
};

/// The social card's labels, to the right of each figure.
const OG_IMAGE_LABELS: SvgLabels = SvgLabels {
    speed: StatLabel {
        x: 810,
        ys: [155, 180],
        size: 22,
        fill: "#bcc6ec",
        anchor: Anchor::Start,
    },
    requests: StatLabel {
        x: 845,
        ys: [315, 340],
        size: 22,
        fill: "#bcc6ec",
        anchor: Anchor::Start,
    },
};

/// Every region the published run `record` writes, which is all the text any
/// surface shows that comes from a benchmark.
fn regions(record: &RunRecord) -> Result<Vec<Region>, String> {
    let headline = Headline::of(record)?;
    let run = &record.timestamp;
    let speed = headline.speed_banner();
    let percent = format!("{}%", headline.requests_percent());
    let region = |path, fence, name, layout, text: String| Region {
        path,
        fence,
        name,
        layout,
        text,
    };
    let lines = Layout::Lines;
    let inline = Layout::Inline;
    let rounds = rounds_in(record, ScenarioKind::Cold)?.len();
    let images = record.corpus.images;
    let svg = |path, labels: &SvgLabels| {
        [
            region(
                path,
                Fence::Html,
                "BENCH-RUN",
                inline,
                format!(" Figures from benchmark run {run}."),
            ),
            region(path, Fence::Html, "BENCH-SPEED", inline, speed.clone()),
            region(path, Fence::Html, "BENCH-REQUESTS", inline, percent.clone()),
            region(
                path,
                Fence::Html,
                "BENCH-SPEED-LABEL",
                lines,
                labels.speed.text(["faster than", COMPARED_WITH]),
            ),
            region(
                path,
                Fence::Html,
                "BENCH-REQUESTS-LABEL",
                lines,
                labels
                    .requests
                    .text(["fewer requests", &format!("than {COMPARED_WITH}")]),
            ),
        ]
    };
    let mut regions = vec![
        region(
            PERFORMANCE_PAGE,
            Fence::Html,
            "BENCH",
            lines,
            page_block(record, ScenarioKind::Cold, "Cold sync")?,
        ),
        region(
            PERFORMANCE_PAGE,
            Fence::Html,
            "BENCH-WARM",
            lines,
            page_block(record, ScenarioKind::Warm, "Warm sync (no changes)")?,
        ),
        region(
            PERFORMANCE_PAGE,
            Fence::Yaml,
            "BENCH-DESCRIPTION",
            lines,
            format!(
                "# run {run}\ndescription: How ocync finishes a cold sync to ECR {} times \
                 faster than regsync in {rounds} rounds, with {}% fewer requests and {} GB \
                 fewer bytes, and cross-repo blob mounting.\n",
                headline.speed_text(),
                headline.requests_percent(),
                headline.gigabytes_text(),
            ),
        ),
        region(
            README,
            Fence::Html,
            "BENCH-README",
            inline,
            format!(
                "In a cold sync of {images} images to ECR (the median of {rounds} rounds, run \
                 {run}), it finished {} times faster than regsync, made {}% fewer requests and \
                 moved {} GB less.",
                headline.speed_text(),
                headline.requests_percent(),
                headline.gigabytes_text(),
            ),
        ),
        region(
            README,
            Fence::Html,
            "BENCH-BANNER",
            lines,
            format!(
                "  <!-- run {run} -->\n  <img src=\"docs/public/ecr-banner.png\" alt=\"ocync - \
                 {speed} faster than regsync, {percent} fewer API requests than regsync, adaptive \
                 rate control\" \
                 width=\"900\">\n"
            ),
        ),
        region(
            HOMEPAGE,
            Fence::Script,
            "BENCH-STATS",
            lines,
            format!(
                "  // run {run}\n  {{ value: \"{speed}\", desc: \"faster cold syncs than regsync\", color: \
                 \"stat-pink\" }},\n  {{ value: \"{percent}\", desc: \"fewer API requests than regsync\", \
                 color: \"stat-purple\" }},\n"
            ),
        ),
    ];
    regions.extend(svg(ECR_BANNER, &ECR_BANNER_LABELS));
    regions.extend(svg(OG_IMAGE, &OG_IMAGE_LABELS));
    Ok(regions)
}

/// Every surface file of a published run, rewritten, ready to be written.
#[derive(Debug)]
pub(crate) struct Planned {
    files: Vec<(PathBuf, String)>,
}

/// The surfaces the published run `record` writes under the repository at
/// `root`, all computed and checked before any is written.
pub(crate) fn plan(root: &Path, record: &RunRecord) -> Result<Planned, String> {
    plan_with(record, |path| read(&root.join(path)))
}

/// [`plan`] with the files read by `read`.
fn plan_with(
    record: &RunRecord,
    read: impl Fn(&str) -> Result<String, String>,
) -> Result<Planned, String> {
    // The surfaces show a published run, so a record that is not one, with
    // every tool in every round, is never written.
    check(record)?;
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for region in regions(record)? {
        let at = files
            .iter()
            .position(|(path, _)| path == Path::new(region.path));
        let content = match at {
            Some(at) => files[at].1.clone(),
            None => read(region.path)?,
        };
        let applied = region.applied(&content)?;
        match at {
            Some(at) => files[at].1 = applied,
            None => files.push((PathBuf::from(region.path), applied)),
        }
    }
    Ok(Planned { files })
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

impl Planned {
    /// Writes every surface under `root`, then renders the banner PNGs when
    /// `banners` says so.
    ///
    /// Every file is first written whole beside its target, and only when all
    /// of those are written is each renamed over its target. A file that
    /// cannot be written therefore leaves every surface as it was. The renames
    /// are separate steps, so one that fails after others succeeded leaves
    /// those surfaces updated, and the error says so.
    pub(crate) fn write(self, root: &Path, banners: Banners) -> Result<(), String> {
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (path, content) in &self.files {
            let target = root.join(path);
            let temp = staging_path(&target);
            if let Err(e) = stage(&target, &temp, content) {
                for (temp, _) in &staged {
                    let _ = std::fs::remove_file(temp);
                }
                let _ = std::fs::remove_file(&temp);
                return Err(format!("{e}; no surface was changed"));
            }
            staged.push((temp, target));
        }
        for (done, (temp, target)) in staged.iter().enumerate() {
            if let Err(e) = std::fs::rename(temp, target) {
                for (temp, _) in &staged[done..] {
                    let _ = std::fs::remove_file(temp);
                }
                return Err(format!(
                    "rename onto {}: {e}; {done} of {} surfaces were already updated, so run \
                     `cargo xtask bench-publish-surfaces` to finish",
                    target.display(),
                    staged.len()
                ));
            }
            eprintln!("bench: updated {}", target.display());
        }
        match banners {
            Banners::Render => render_banners(root),
            #[cfg(test)]
            Banners::Skip => Ok(()),
        }
    }
}

/// The file a surface is written to before it replaces `target`.
fn staging_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.bench-new"))
}

/// Writes `content` to `temp` with the permissions `target` has.
fn stage(target: &Path, temp: &Path, content: &str) -> Result<(), String> {
    std::fs::write(temp, content).map_err(|e| format!("write {}: {e}", temp.display()))?;
    let permissions = std::fs::metadata(target)
        .map_err(|e| format!("read {}: {e}", target.display()))?
        .permissions();
    std::fs::set_permissions(temp, permissions)
        .map_err(|e| format!("set permissions of {}: {e}", temp.display()))
}

/// Rasterizes the banner SVGs to PNGs with the docs site's `sharp`.
fn render_banners(root: &Path) -> Result<(), String> {
    let docs = root.join(DOCS_DIR);
    let status = std::process::Command::new("npm")
        .args(["run", "--silent", RENDER_SCRIPT])
        .current_dir(&docs)
        .status()
        .map_err(|e| {
            format!(
                "the surfaces are written, but npm could not run in {} ({e}); put node on PATH \
                 and run `npm run {RENDER_SCRIPT}` there, so the banner PNGs match their SVGs",
                docs.display()
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the surfaces are written, but `npm run {RENDER_SCRIPT}` in {} failed ({status}); \
             run it again, so the banner PNGs match their SVGs",
            docs.display()
        ))
    }
}

/// The newest published record in the archive text `archive`, `None` when it
/// holds none.
#[expect(
    clippy::disallowed_methods,
    reason = "parses the archive the bench wrote"
)]
fn newest_published_record(archive: &str) -> Result<Option<RunRecord>, String> {
    let records: Vec<RunRecord> =
        serde_json::from_str(archive).map_err(|e| format!("parse the archive: {e}"))?;
    Ok(records
        .into_iter()
        .filter(|record| record.publication == Publication::Publish)
        .max_by(|a, b| a.timestamp.cmp(&b.timestamp)))
}

/// The newest published record in the archive `{registry_key}.json` of
/// `archive_dir`; `None` without one or without the archive.
pub(crate) fn newest_published(
    archive_dir: &Path,
    registry_key: &str,
) -> Result<Option<RunRecord>, String> {
    let path = archive_dir.join(format!("{registry_key}.json"));
    if !path.exists() {
        return Ok(None);
    }
    newest_published_record(&read(&path)?).map_err(|e| format!("{}: {e}", path.display()))
}

/// Rewrites every surface under `root` from the newest published run in the
/// ECR archive of `archive_dir`, by the plan and write a publish uses, so the
/// surfaces show that run again after a change to how they are worded. An error
/// when the archive holds no published run, with nothing written.
pub(crate) fn rewrite_surfaces(
    archive_dir: &Path,
    root: &Path,
    banners: Banners,
) -> Result<(), String> {
    let key = super::RegistryKind::Ecr.archive_stem();
    let record = newest_published(archive_dir, key)?.ok_or_else(|| {
        format!(
            "{} holds no published run, so there is nothing to write; publish one with \
             `cargo xtask bench-remote --publish`",
            archive_dir.join(format!("{key}.json")).display()
        )
    })?;
    eprintln!(
        "bench: writing the surfaces of the published run {}",
        record.timestamp
    );
    plan(root, &record)?.write(root, banners)
}

/// The `bench-publish-surfaces` command: rewrites every surface of the
/// repository in the current directory from its newest published run.
pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    Ok(rewrite_surfaces(
        Path::new(report::ARCHIVE_DIR),
        Path::new("."),
        Banners::Render,
    )?)
}

/// The regions of the published run `record` that `read` returns different
/// text for, by file and name: empty when every surface shows the run.
#[cfg(test)]
fn stale_regions(record: &RunRecord, read: impl Fn(&str) -> String) -> Result<Vec<String>, String> {
    let mut stale = Vec::new();
    for region in regions(record)? {
        if !region.holds(&read(region.path))? {
            stale.push(format!("{} {}", region.path, region.name));
        }
    }
    Ok(stale)
}

// ---------------------------------------------------------------------------
// Checking a run before it is published
// ---------------------------------------------------------------------------

/// Whether `record` is the published run in every way the design requires,
/// checked again after the run, whatever the arguments said. The error says
/// which requirement it misses, and the record is then archived as an
/// experiment.
pub(crate) fn verify(record: &RunRecord) -> Result<(), String> {
    check(record)?;
    regions(record).map(|_| ())
}

/// The pre-warm requirement of [`verify`]: done with no blob left unwarmed, or
/// not applicable, which only a run from the controlled source can be, since
/// a public registry has a CDN to warm.
fn check_prewarm(record: &RunRecord) -> Result<(), String> {
    match &record.prewarm {
        Some(Prewarm::Done { unwarmed, .. }) if unwarmed.is_empty() => Ok(()),
        Some(Prewarm::Done { unwarmed, .. }) => Err(format!(
            "the CDN pre-warm left blobs: {}",
            report::not_warmed(unwarmed)
        )),
        Some(Prewarm::NotApplicable) if record.source == source::Kind::Controlled => Ok(()),
        Some(Prewarm::NotApplicable) => {
            Err("the CDN pre-warm is not applicable to a source with a CDN, and was not run".into())
        }
        Some(Prewarm::Skipped) => Err("the CDN pre-warm was skipped".into()),
        None => Err("no CDN pre-warm ran".into()),
    }
}

/// The requirements of [`verify`] that the record's own fields state.
fn check(record: &RunRecord) -> Result<(), String> {
    if record.machine.provider.is_none() {
        return Err("the instance could not be identified".into());
    }
    if !record.corpus.id.is_main() {
        return Err("the corpus is not the main one".into());
    }
    if record.coverage != Coverage::Whole {
        return Err("the run did not sync the whole corpus".into());
    }
    if record.max_concurrent_transfers != Some(DEFAULT_MAX_CONCURRENT_TRANSFERS) {
        return Err("ocync did not run at its default transfer cap".into());
    }
    if record.baseline_max_concurrent_transfers.is_some() {
        return Err("the run carried a baseline build".into());
    }
    if record.source != source::Kind::Controlled {
        return Err("the tools did not read the controlled source".into());
    }
    check_prewarm(record)?;
    let wanted = u32::try_from(PUBLISHED_ROUNDS).expect("a published run has few rounds");
    if record.rounds
        != Some(RoundCount {
            requested: wanted,
            finished: wanted,
        })
    {
        return Err(format!("it did not finish all {PUBLISHED_ROUNDS} rounds"));
    }
    if record.git_ref.len() != 40 || !record.git_ref.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the commit is not recorded by its full hash".into());
    }
    let tools: BTreeSet<Tool> = PUBLISHED_TOOLS.into_iter().collect();
    let published_kinds = [ScenarioKind::Cold, ScenarioKind::Warm];
    if let Some(other) = record.scenarios.iter().find(|scenario| {
        !published_kinds
            .iter()
            .any(|kind| kind.label() == scenario.name)
    }) {
        return Err(format!(
            "the run held the {} scenario, and a published run is the sync scenario alone",
            other.name
        ));
    }
    for kind in published_kinds {
        let rounds = rounds_in(record, kind)?;
        if rounds.len() != PUBLISHED_ROUNDS {
            return Err(format!(
                "{} finished {} of {PUBLISHED_ROUNDS} rounds",
                kind.label(),
                rounds.len()
            ));
        }
        for round in &rounds {
            let ran: BTreeSet<Tool> = round.runs.iter().map(|run| run.tool).collect();
            if ran != tools || round.runs.len() != tools.len() {
                return Err(format!("{} did not run every published tool", kind.label()));
            }
            if round
                .runs
                .iter()
                .any(|run| run.direct_wall_clock_secs.is_some())
            {
                return Err("the run paired proxied runs with direct ones".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::bench::Rounds;
    use crate::bench::proxy::ProxyMetrics;
    use crate::bench::remote::CommitSha;
    use crate::bench::report::{BenchReport, CorpusInfo, tests::sample_report};

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
            source: source::Kind::Controlled,
            corpus: corpus::Id::Main,
            limit: None,
            skips_registries: false,
            skips_prewarm: false,
            disables_proxy: false,
            measures_proxy_overhead: false,
            checks_regression: false,
        }
    }

    #[test]
    fn the_published_shape_is_accepted_in_any_tool_order() {
        assert_eq!(ensure_publishable(&selection(), &shape()), Ok(()));
        let mut reversed = selection();
        reversed.tools.reverse();
        assert_eq!(ensure_publishable(&reversed, &shape()), Ok(()));
    }

    #[test]
    fn every_departure_from_the_published_shape_is_refused_by_name() {
        // Catches: a published run that is not the one the design defines,
        // each departure being a way the page's numbers stop being comparable.
        let mut one_tool = selection();
        one_tool.tools = vec![Tool::Ocync];
        let mut missing_tool = selection();
        missing_tool.tools.pop();
        let mut extra_tool = selection();
        extra_tool.tools.push(Tool::Skopeo);
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
        refused(missing_tool, shape(), "--tools");
        refused(extra_tool, shape(), "--tools");
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
                checks_regression: true,
                ..shape()
            },
            "--regression",
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
        // Catches: the published run synced from the public registries, whose
        // rate limits the repeated rounds would hit.
        refused(
            selection(),
            Shape {
                source: source::Kind::Public,
                ..shape()
            },
            "--source must be controlled, not public",
        );
        // Catches: a run that also runs the partial, scale or prepare scenario
        // (`all` included) being published; prepare reads the public
        // registries whatever the source.
        for scenario in [
            Scenario::Partial,
            Scenario::Scale,
            Scenario::Prepare,
            Scenario::All,
        ] {
            refused(
                selection(),
                Shape {
                    scenario,
                    ..shape()
                },
                "the scenario must be sync",
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

    /// The commit the fixture run was built from.
    const FIXTURE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// What a tool did in one round of the fixture: its wall clock in
    /// seconds, and the proxy's counts.
    fn tool_run(tool: Tool, wall_clock_secs: f64, requests: u64, response_bytes: u64) -> ToolRun {
        ToolRun {
            tool,
            wall_clock_secs,
            peak_rss_kb: Some(60_000),
            proxy_metrics: Some(ProxyMetrics {
                requests,
                response_bytes,
                referrer_calls: u64::from(tool == Tool::Ocync) * 37,
                ..ProxyMetrics::default()
            }),
            phase_timing_ms: BTreeMap::new(),
            direct_wall_clock_secs: None,
        }
    }

    /// A finished run of the published shape: 5 rounds of cold and warm sync
    /// with every published tool. In every round regsync takes 2.9 times as
    /// long as ocync, ocync sends 25% fewer requests and its responses hold
    /// 9.6 GB less. `slowdown` multiplies the other tools' wall clock.
    pub(crate) fn fixture_report(run: &str, ocync_seconds: [f64; 5]) -> BenchReport {
        let round = |kind, n: usize| {
            let ocync = ocync_seconds[n];
            let runs = PUBLISHED_TOOLS
                .iter()
                .map(|&tool| match tool {
                    Tool::Ocync => tool_run(tool, ocync, 6_000, 53_300_000_000),
                    Tool::Regsync => tool_run(tool, ocync * 2.9, 8_000, 62_900_000_000),
                    other => tool_run(other, ocync * 4.0, 9_000, 63_000_000_000),
                })
                .collect();
            ScenarioResult {
                round: Some(n + 1),
                ..ScenarioResult::unshuffled(kind, runs)
            }
        };
        BenchReport {
            timestamp: run.into(),
            rounds: Rounds::DEFAULT,
            repeated: vec![ScenarioKind::Cold],
            corpus: CorpusInfo {
                id: corpus::Id::Main,
                images: 25,
                tags: 39,
            },
            prewarm: Some(Prewarm::NotApplicable),
            tool_versions: PUBLISHED_TOOLS.iter().map(|&t| (t, "1.0".into())).collect(),
            scenarios: (0..5)
                .flat_map(|n| [round(ScenarioKind::Cold, n), round(ScenarioKind::Warm, n)])
                .collect(),
            ..sample_report()
        }
    }

    /// The record of a report, published.
    pub(crate) fn published(report: &BenchReport) -> RunRecord {
        let mut record = RunRecord::of(report, FIXTURE_COMMIT.into(), None);
        record.publication = Publication::Publish;
        record.source = source::Kind::Controlled;
        record.source_region = Some(source::REGION.to_owned());
        record
    }

    /// The fixture run, published, from a run started at `run`.
    fn fixture(run: &str) -> RunRecord {
        published(&fixture_report(run, [200.0, 210.0, 205.0, 208.0, 202.0]))
    }

    const FIXTURE_RUN: &str = "2026-11-02-120000";

    /// The six files the surfaces live in, as the repository holds them.
    fn tracked(path: &str) -> &'static str {
        match path {
            PERFORMANCE_PAGE => include_str!("../../../docs/src/content/performance.md"),
            README => include_str!("../../../README.md"),
            HOMEPAGE => include_str!("../../../docs/src/pages/index.astro"),
            ECR_BANNER => include_str!("../../../docs/public/ecr-banner.svg"),
            OG_IMAGE => include_str!("../../../docs/public/og-image.svg"),
            other => panic!("{other} holds no surface"),
        }
    }

    /// The tracked files with the surfaces of `record` written into them.
    fn written(record: &RunRecord) -> BTreeMap<String, String> {
        plan_with(record, |path| Ok(tracked(path).to_owned()))
            .unwrap()
            .files
            .into_iter()
            .map(|(path, content)| (path.to_string_lossy().into_owned(), content))
            .collect()
    }

    /// The regions of `record` that `files` do not show; the files a plan did
    /// not touch are read as the repository holds them.
    fn stale(record: &RunRecord, files: &BTreeMap<String, String>) -> Vec<String> {
        stale_regions(record, |path| {
            files
                .get(path)
                .map_or_else(|| tracked(path).to_owned(), Clone::clone)
        })
        .unwrap()
    }

    #[test]
    fn the_fixture_is_a_run_that_may_be_published() {
        assert_eq!(verify(&fixture(FIXTURE_RUN)), Ok(()));
    }

    #[test]
    fn a_run_short_of_the_published_shape_is_not_published_and_says_why() {
        // Catches: a run that lost a round, covers part of the corpus, ran a
        // named corpus, a capped ocync or a baseline, skipped the pre-warm,
        // lacks a tool, or is tied to a commit by no full hash, written to
        // every surface because its arguments looked right.
        let base = || fixture_report(FIXTURE_RUN, [200.0, 210.0, 205.0, 208.0, 202.0]);
        let refused = |report: BenchReport, needle: &str| {
            let err = verify(&published(&report)).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
        };

        let mut lost_round = base();
        lost_round.scenarios.truncate(8);
        refused(lost_round, "all 5 rounds");
        let mut subset = base();
        subset.coverage = Coverage::Subset;
        refused(subset, "whole corpus");
        let mut named = base();
        named.corpus.id = corpus::Id::Named(corpus::CorpusName::new("small"));
        refused(named, "main one");
        let mut capped = base();
        capped.max_concurrent_transfers = std::num::NonZeroUsize::new(30);
        refused(capped, "default transfer cap");
        let mut with_baseline = base();
        with_baseline.baseline = Some(CommitSha::fixture());
        refused(with_baseline, "baseline");
        let mut cold_start = base();
        cold_start.prewarm = Some(Prewarm::Skipped);
        refused(cold_start, "pre-warm was skipped");
        let mut no_prewarm = base();
        no_prewarm.prewarm = None;
        refused(no_prewarm, "no CDN pre-warm");
        let mut unwarmed = base();
        unwarmed.prewarm = Some(Prewarm::Done {
            unwarmed: BTreeMap::from([("docker.io".to_owned(), 3)]),
            no_cache: BTreeSet::new(),
        });
        refused(unwarmed, "docker.io (3 blobs)");
        let mut missing_tool = base();
        missing_tool.scenarios[3].runs.pop();
        refused(missing_tool, "every published tool");
        // Catches: a record holding a per-tag tool reaching the page's table.
        let mut extra_tool = base();
        extra_tool.scenarios[3]
            .runs
            .push(tool_run(Tool::Skopeo, 600.0, 8000, 60_000_000_000));
        refused(extra_tool, "every published tool");
        // Catches: a record of an `all` run, whose partial scenario the page
        // does not show, being published.
        let mut with_partial = published(&base());
        with_partial.scenarios.push(report::ScenarioRecord {
            name: ScenarioKind::Partial.label(),
            round: None,
            shuffle_seed: 0,
            tool_order: Vec::new(),
            tools: Vec::new(),
        });
        assert!(
            verify(&with_partial)
                .unwrap_err()
                .contains("sync scenario alone")
        );
        let mut paired = base();
        paired.scenarios[0].runs[0].direct_wall_clock_secs = Some(1.0);
        refused(paired, "direct");
        // Catches: a run from the public registries, the one a rate limit can
        // spoil, being published whatever its arguments said.
        let mut public = published(&base());
        public.source = source::Kind::Public;
        public.source_region = None;
        assert!(
            verify(&public)
                .unwrap_err()
                .contains("did not read the controlled source")
        );
        let mut short_hash = published(&base());
        short_hash.git_ref = "0123456".into();
        assert!(verify(&short_hash).unwrap_err().contains("full hash"));
    }

    #[test]
    fn a_prewarm_that_does_not_apply_is_accepted_only_for_the_controlled_source() {
        // Catches: a public-source run, which has a CDN to warm, passing for
        // having nothing to warm; and a controlled run refused for it.
        let mut record = fixture(FIXTURE_RUN);
        record.prewarm = Some(Prewarm::NotApplicable);
        assert_eq!(check_prewarm(&record), Ok(()));
        record.source = source::Kind::Public;
        assert!(
            check_prewarm(&record)
                .unwrap_err()
                .contains("not applicable to a source with a CDN")
        );
    }

    #[test]
    fn a_run_without_regsync_or_ahead_of_it_cannot_be_published() {
        // Catches: surfaces quoting a ratio against a tool that failed, or a
        // gain that is a loss, after the run had been paid for.
        let mut failed = fixture_report(FIXTURE_RUN, [200.0; 5]);
        for scenario in &mut failed.scenarios {
            scenario.runs.retain(|run| run.tool != Tool::Regsync);
        }
        let record = published(&failed);
        assert!(verify(&record).is_err());
        assert!(plan_with(&record, |_| panic!("no file is read")).is_err());

        let mut slower = fixture_report(FIXTURE_RUN, [200.0; 5]);
        for scenario in &mut slower.scenarios {
            for run in &mut scenario.runs {
                if run.tool == Tool::Regsync {
                    run.wall_clock_secs = 100.0;
                }
            }
        }
        let err = verify(&published(&slower)).unwrap_err();
        assert!(err.contains("not ahead of regsync in wall clock"), "{err}");
    }

    #[test]
    fn the_figures_come_from_the_cold_sync_against_regsync() {
        let headline = Headline::of(&fixture(FIXTURE_RUN)).unwrap();
        assert_eq!(headline.speed_banner(), "2.9x");
        assert_eq!(headline.requests_percent(), 25);
        assert_eq!(headline.gigabytes_text(), "9.6");

        // The speed is the median of the per-round ratios, not the ratio of
        // the medians: round 1 is 4x and the other four are 2x.
        let mut report = fixture_report(FIXTURE_RUN, [100.0; 5]);
        for scenario in report
            .scenarios
            .iter_mut()
            .filter(|s| s.kind == ScenarioKind::Cold)
        {
            let ocync = scenario.runs[0].wall_clock_secs;
            let factor = if scenario.round == Some(1) { 4.0 } else { 2.0 };
            for run in &mut scenario.runs {
                if run.tool == Tool::Regsync {
                    run.wall_clock_secs = ocync * factor;
                }
            }
        }
        assert_eq!(
            Headline::of(&published(&report)).unwrap().speed_text(),
            "2.0"
        );
    }

    #[test]
    fn the_cold_block_names_the_run_and_shows_each_tools_median_with_the_best_bold() {
        let record = fixture(FIXTURE_RUN);
        let block = page_block(&record, ScenarioKind::Cold, "Cold sync").unwrap();

        // Catches: a table with ranges, a ratio table, a missing run name or
        // commit, tools in another order, or no best value bold.
        assert!(
            block.starts_with(
                "Measured 2026-11-02 (run 2026-11-02-120000, commit 0123456) on c7g.xlarge"
            ),
            "{block}"
        );
        assert!(
            block.contains("Cold sync from the controlled source (ECR us-east-1) to ECR us-east-1. Median of 5 rounds."),
            "{block}"
        );
        assert!(
            block.contains("ocync ran at its default of 20 images at once"),
            "{block}"
        );
        assert!(
            block.contains("| Metric | ocync | dregsy | regsync |\n"),
            "{block}"
        );
        assert!(
            block.contains("| Wall clock | **3m 25s** | 13m 40s | 9m 54s |"),
            "{block}"
        );
        assert!(
            block.contains("| Requests | **6000** | 9000 | 8000 |"),
            "{block}"
        );
        assert!(
            !block
                .lines()
                .any(|l| l.starts_with('|') && l.contains("..")),
            "a range on the page:\n{block}"
        );
        assert!(!block.contains("Per-round ratio"), "{block}");
        assert!(block.contains("Referrers calls -- ocync: 37"), "{block}");
        // No per-tag tool is published, so no per-process RSS note.
        assert!(!block.contains("is per process"), "{block}");
    }

    #[test]
    fn the_warm_block_is_the_warm_scenario() {
        let record = fixture(FIXTURE_RUN);
        let block = page_block(&record, ScenarioKind::Warm, "Warm sync (no changes)").unwrap();
        assert!(
            block.contains(
                "Warm sync (no changes) from the controlled source (ECR us-east-1) to ECR"
            ),
            "{block}"
        );
        assert!(!block.contains("Cold sync"), "{block}");
        // Catches: every page naming the controlled source, whatever the
        // record says it read.
        let mut public = fixture(FIXTURE_RUN);
        public.source = source::Kind::Public;
        public.source_region = None;
        let block = page_block(&public, ScenarioKind::Warm, "Warm sync (no changes)").unwrap();
        assert!(
            block.contains("from the public registries to ECR"),
            "{block}"
        );
    }

    #[test]
    fn every_tracked_surface_takes_the_fixture_run_and_then_shows_it() {
        // Catches: a marker deleted or moved in a tracked file, which would
        // make the first published run fail at the end of an expensive run.
        let record = fixture(FIXTURE_RUN);
        let files = written(&record);
        assert_eq!(files.len(), 5, "{:?}", files.keys());
        assert_eq!(stale(&record, &files), Vec::<String>::new());
        // Every file changed, and every region cites the run.
        for (path, content) in &files {
            assert_ne!(content, tracked(path), "{path}");
            assert!(content.contains(FIXTURE_RUN), "{path}");
        }
        // Every label beside a banner figure names what the figure is measured
        // against, in both images, on two lines in the README banner.
        for svg in [ECR_BANNER, OG_IMAGE] {
            assert!(files[svg].contains(">faster than</text>"), "{svg}");
            assert!(files[svg].contains(">regsync</text>"), "{svg}");
            assert!(files[svg].contains(">than regsync</text>"), "{svg}");
            assert!(!files[svg].contains("alternatives"), "{svg}");
        }
        assert!(files[README].contains("2.9 times faster than regsync"));
        assert!(
            files[README].contains("2.9x faster than regsync, 25% fewer API requests than regsync")
        );
        assert!(files[HOMEPAGE].contains(r#"{ value: "2.9x","#));
        assert!(files[PERFORMANCE_PAGE].contains("description: How ocync finishes"));
        assert!(
            files[ECR_BANNER].contains("<!-- BENCH-SPEED:START -->2.9x<!-- BENCH-SPEED:END -->")
        );
        assert!(
            files[OG_IMAGE].contains("<!-- BENCH-REQUESTS:START -->25%<!-- BENCH-REQUESTS:END -->")
        );

        // Written again, nothing moves.
        let again = plan_with(&record, |path| {
            Ok(files
                .get(path)
                .cloned()
                .unwrap_or_else(|| tracked(path).to_owned()))
        })
        .unwrap();
        for (path, content) in again.files {
            assert_eq!(&files[path.to_str().unwrap()], &content);
        }
    }

    #[test]
    fn a_surface_that_differs_or_cites_another_run_is_stale() {
        // Catches: the consistency check passing a hand edit, a publish that
        // updated one surface and not the others, or a surface left citing
        // the run before.
        let record = fixture(FIXTURE_RUN);
        let files = written(&record);

        // A hand edit to one number in one file names that file's region.
        let mut edited = files.clone();
        let readme = edited.get_mut(README).unwrap();
        *readme = readme.replace("2.9 times faster", "3.9 times faster");
        assert_eq!(
            stale(&record, &edited),
            vec![format!("{README} BENCH-README")]
        );

        // One file left out of the publish.
        let mut left_out = files.clone();
        left_out.remove(HOMEPAGE);
        assert_eq!(
            stale(&record, &left_out),
            vec![format!("{HOMEPAGE} BENCH-STATS")]
        );

        // The same figures from a later run: every region that names the run
        // names the old one. The banners' figures are the same, and their
        // `BENCH-RUN` regions carry the citation.
        let later = fixture("2026-11-03-120000");
        let cited = stale(&later, &files);
        assert_eq!(cited.len(), 8, "{cited:?}");
        for svg in [ECR_BANNER, OG_IMAGE] {
            assert!(cited.contains(&format!("{svg} BENCH-RUN")), "{cited:?}");
        }
        assert!(
            cited.contains(&format!("{README} BENCH-BANNER")),
            "{cited:?}"
        );

        // Different figures from the same run time.
        let faster = published(&fixture_report(FIXTURE_RUN, [100.0; 5]));
        assert!(!stale(&faster, &files).is_empty());
    }

    #[test]
    fn a_banner_label_that_does_not_name_regsync_is_stale() {
        // Catches: a figure measured against regsync left beside a label
        // that names something else, as "alternatives" did.
        let record = fixture(FIXTURE_RUN);
        let files = written(&record);
        for (svg, was, now, region) in [
            (
                OG_IMAGE,
                ">regsync</text>",
                ">alternatives</text>",
                "BENCH-SPEED-LABEL",
            ),
            (
                OG_IMAGE,
                ">than regsync</text>",
                ">requests</text>",
                "BENCH-REQUESTS-LABEL",
            ),
            (
                ECR_BANNER,
                ">regsync</text>",
                ">faster</text>",
                "BENCH-SPEED-LABEL",
            ),
            (
                ECR_BANNER,
                ">than regsync</text>",
                ">fewer</text>",
                "BENCH-REQUESTS-LABEL",
            ),
        ] {
            let mut edited = files.clone();
            let file = edited.get_mut(svg).unwrap();
            assert!(file.contains(was), "{svg}");
            *file = file.replace(was, now);
            assert_eq!(stale(&record, &edited), vec![format!("{svg} {region}")]);
        }
    }

    #[test]
    fn a_published_record_that_is_not_the_published_shape_is_never_written() {
        // Catches: surfaces written from a record with a tool missing from a
        // round, which `apply` would otherwise take on trust.
        let mut report = fixture_report(FIXTURE_RUN, [200.0; 5]);
        report.scenarios[0]
            .runs
            .retain(|run| run.tool != Tool::Dregsy);
        let err = plan_with(&published(&report), |_| panic!("no file is read")).unwrap_err();
        assert!(err.contains("every published tool"), "{err}");
    }

    #[test]
    fn a_failed_write_leaves_every_surface_as_it_was() {
        // Catches: the first surfaces rewritten before a later one fails, so
        // the page and the banners show different runs.
        let root = repository();
        let planned = plan(root.path(), &fixture(FIXTURE_RUN)).unwrap();
        std::fs::remove_dir_all(root.path().join("docs/public")).unwrap();

        let err = planned.write(root.path(), Banners::Skip).unwrap_err();

        assert!(err.contains("no surface was changed"), "{err}");
        for path in [PERFORMANCE_PAGE, README, HOMEPAGE] {
            assert_eq!(surface_of(root.path(), path), tracked(path), "{path}");
        }
        // No staging file is left beside a surface.
        let leftovers = std::fs::read_dir(root.path().join("docs/src/pages"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("bench-new")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn the_page_prose_outside_the_generated_blocks_names_no_run() {
        // Catches: hand-written text about the current run (a build such as
        // `ocync@7634416`, a tool left out of it) that a publish would leave
        // standing beside the next run's numbers.
        let page = tracked(PERFORMANCE_PAGE);
        let mut outside = page.to_owned();
        for name in ["BENCH", "BENCH-WARM"] {
            let region = Region {
                path: PERFORMANCE_PAGE,
                fence: Fence::Html,
                name,
                layout: Layout::Lines,
                text: String::new(),
            };
            outside = region.applied(&outside).unwrap();
        }
        for stale in ["ocync@", "is left out", "The two ocync builds"] {
            assert!(!outside.contains(stale), "{stale} outside the blocks");
        }
    }

    #[test]
    fn markers_are_found_once_and_in_order() {
        let region = Region {
            path: "f",
            fence: Fence::Html,
            name: "X",
            layout: Layout::Lines,
            text: "new\n".into(),
        };
        let ok = "a\n  <!-- X:START -->\nold\n  <!-- X:END -->\nb\n";
        // The end marker keeps its indentation, and the text sits between lines.
        assert_eq!(
            region.applied(ok).unwrap(),
            "a\n  <!-- X:START -->\nnew\n  <!-- X:END -->\nb\n"
        );
        for (content, needle) in [
            ("no markers", "no `<!-- X:START -->`"),
            ("<!-- X:START -->\n", "no `<!-- X:END -->`"),
            ("<!-- X:END -->\n<!-- X:START -->\n", "comes before START"),
            (
                "<!-- X:START -->\n<!-- X:START -->\n<!-- X:END -->\n",
                "more than once",
            ),
            ("<!-- X:START --><!-- X:END -->\n", "lines of their own"),
        ] {
            let err = region.applied(content).unwrap_err();
            assert!(err.contains(needle), "{content:?}: {err}");
        }
        let inline = Region {
            layout: Layout::Inline,
            text: "new".into(),
            ..region
        };
        assert_eq!(
            inline
                .applied("a <!-- X:START -->old<!-- X:END --> b")
                .unwrap(),
            "a <!-- X:START -->new<!-- X:END --> b"
        );
    }

    /// A temporary repository root holding the tracked surface files.
    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for path in [PERFORMANCE_PAGE, README, HOMEPAGE, ECR_BANNER, OG_IMAGE] {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, tracked(path)).unwrap();
        }
        root
    }

    fn archive_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn surface_of(root: &Path, path: &str) -> String {
        std::fs::read_to_string(root.join(path)).unwrap()
    }

    #[test]
    fn a_published_run_writes_the_surfaces_and_an_experiment_writes_none() {
        use crate::bench::report::Deferred;
        let root = repository();
        let archive = archive_dir();

        // Catches: a run that qualifies in every way but was not started with
        // `--publish` writing a surface: only a published run may.
        let mut qualifying = fixture(FIXTURE_RUN);
        qualifying.publication = Publication::Archive;
        Deferred::new("ecr", Some(qualifying))
            .apply(archive.path(), root.path(), Banners::Skip)
            .unwrap();
        for path in [PERFORMANCE_PAGE, README, HOMEPAGE, ECR_BANNER, OG_IMAGE] {
            assert_eq!(surface_of(root.path(), path), tracked(path), "{path}");
        }

        let archive = archive_dir();
        for _ in 0..2 {
            Deferred::new("ecr", Some(fixture(FIXTURE_RUN)))
                .apply(archive.path(), root.path(), Banners::Skip)
                .unwrap();
        }
        let files: BTreeMap<String, String> =
            [PERFORMANCE_PAGE, README, HOMEPAGE, ECR_BANNER, OG_IMAGE]
                .into_iter()
                .map(|path| (path.to_owned(), surface_of(root.path(), path)))
                .collect();
        assert_eq!(stale(&fixture(FIXTURE_RUN), &files), Vec::<String>::new());
    }

    #[test]
    fn an_older_published_run_fetched_later_leaves_the_newer_ones_surfaces() {
        use crate::bench::report::Deferred;
        let root = repository();
        let archive = archive_dir();
        Deferred::new("ecr", Some(fixture("2026-11-03-120000")))
            .apply(archive.path(), root.path(), Banners::Skip)
            .unwrap();
        let after_newer = surface_of(root.path(), README);

        Deferred::new("ecr", Some(fixture("2026-11-02-120000")))
            .apply(archive.path(), root.path(), Banners::Skip)
            .unwrap();

        // Catches: fetching an old run turning the surfaces back to it, so
        // that they cite a run that is not the newest published one.
        assert_eq!(surface_of(root.path(), README), after_newer);
        assert!(after_newer.contains("2026-11-03-120000"));
    }

    #[test]
    fn a_surface_that_cannot_take_the_run_keeps_the_record_and_changes_no_file() {
        use crate::bench::report::Deferred;
        let root = repository();
        let archive = archive_dir();
        let homepage = root.path().join(HOMEPAGE);
        std::fs::write(&homepage, "const stats = [];\n").unwrap();

        let err = Deferred::new("ecr", Some(fixture(FIXTURE_RUN)))
            .apply(archive.path(), root.path(), Banners::Skip)
            .unwrap_err();

        // Catches: some surfaces updated and not others, or a finished run's
        // record lost because a marker was missing (a local run has no other
        // copy of it), or an error that does not say how to finish.
        assert!(err.contains("BENCH-STATS"), "{err}");
        assert!(err.contains("cargo xtask bench-publish-surfaces"), "{err}");
        assert_eq!(surface_of(root.path(), README), tracked(README));
        assert!(archive.path().join("ecr.json").exists());
        assert!(newest_published(archive.path(), "ecr").unwrap().is_some());
    }

    #[test]
    fn the_surfaces_show_the_newest_published_run() {
        let archive = include_str!("../../../bench/results/ecr.json");
        // Until the first published run the surfaces keep the text they have.
        let Some(record) = newest_published_record(archive).unwrap() else {
            return;
        };

        assert_eq!(verify(&record), Ok(()), "{}", record.timestamp);
        // Every banner label names what its figure is measured against.
        for region in regions(&record).unwrap() {
            if region.name.ends_with("-LABEL") {
                let shown = &tracked(region.path)[region.span(tracked(region.path)).unwrap()];
                assert!(
                    shown.contains(COMPARED_WITH),
                    "{} {}",
                    region.path,
                    region.name
                );
            }
        }
        assert_eq!(
            stale(&record, &BTreeMap::new()),
            Vec::<String>::new(),
            "a surface differs from, or does not cite, the published run {}; \
             run `cargo xtask bench-publish-surfaces` to rewrite them from it",
            record.timestamp
        );
    }

    #[test]
    fn the_surfaces_are_rewritten_from_the_archive_alone_and_only_from_a_published_run() {
        use crate::bench::report::append_record;
        let root = repository();
        let files_of = |root: &Path| -> BTreeMap<String, String> {
            [PERFORMANCE_PAGE, README, HOMEPAGE, ECR_BANNER, OG_IMAGE]
                .into_iter()
                .map(|path| (path.to_owned(), surface_of(root, path)))
                .collect()
        };

        // Catches: a rewrite from an archive with only experiments in it
        // changing a file, or succeeding.
        let experiments = archive_dir();
        let mut experiment = fixture(FIXTURE_RUN);
        experiment.publication = Publication::Archive;
        append_record(experiments.path(), "ecr", experiment).unwrap();
        let err = rewrite_surfaces(experiments.path(), root.path(), Banners::Skip).unwrap_err();
        assert!(err.contains("no published run"), "{err}");
        let empty = archive_dir();
        assert!(rewrite_surfaces(empty.path(), root.path(), Banners::Skip).is_err());
        for (path, content) in files_of(root.path()) {
            assert_eq!(content, tracked(&path), "{path}");
        }

        // The newest published run wins, and the surfaces show it.
        let archive = archive_dir();
        append_record(archive.path(), "ecr", fixture("2026-11-02-120000")).unwrap();
        append_record(archive.path(), "ecr", fixture("2026-11-03-120000")).unwrap();
        rewrite_surfaces(archive.path(), root.path(), Banners::Skip).unwrap();
        let rewritten = files_of(root.path());
        assert_eq!(
            stale(&fixture("2026-11-03-120000"), &rewritten),
            Vec::<String>::new()
        );
        assert!(!stale(&fixture("2026-11-02-120000"), &rewritten).is_empty());
    }
}
