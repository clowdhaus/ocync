//! Statistics over the rounds of a benchmark run.
//!
//! Distribution-free: a median and a range, never a fitted interval. With a
//! handful of rounds a resampled interval claims more confidence than the data
//! holds.

/// Median, minimum and maximum of one metric over the rounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Spread {
    /// Middle value; the mean of the two middle values for an even count.
    pub(crate) median: f64,
    /// Lowest round.
    pub(crate) min: f64,
    /// Highest round.
    pub(crate) max: f64,
}

/// The spread of `values`, or `None` when there are none or any is not finite.
pub(crate) fn spread(values: &[f64]) -> Option<Spread> {
    if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    let median = if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        f64::midpoint(sorted[mid - 1], sorted[mid])
    };
    Some(Spread {
        median,
        min: sorted[0],
        max: sorted[sorted.len() - 1],
    })
}

/// How ocync's metric compares with another tool's across the rounds. The
/// metric is one where lower is better, so a ratio above 1 favors ocync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Lower than the other tool in every round.
    Faster,
    /// Higher than the other tool in every round.
    Slower,
    /// Some rounds on each side of 1, or a round at exactly 1.
    NoMeasuredDifference,
}

/// The per-round ratios between ocync and one other tool.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Ratio {
    /// Median and range of `other / ocync` over the rounds.
    pub(crate) spread: Spread,
    /// Number of rounds the ratios come from.
    pub(crate) rounds: usize,
    /// Chance that the range holds the true median ratio.
    pub(crate) confidence: f64,
    /// What the rounds agree on.
    pub(crate) verdict: Verdict,
}

/// Chance that the lowest to highest of `rounds` samples holds the median of
/// the distribution they are drawn from: each sample falls on either side of
/// it with probability one half, and the range misses only when all fall on
/// the same side. 0.75 at 3 rounds, 0.9375 at 5.
pub(crate) fn confidence(rounds: usize) -> f64 {
    let rounds = i32::try_from(rounds).unwrap_or(i32::MAX);
    1.0 - 2.0 * 0.5_f64.powi(rounds)
}

/// The ratio `other / ocync` of each `(ocync, other)` round, summarized.
/// `None` when there are no rounds, a value is not finite, or ocync's is not
/// above zero (a ratio of nothing says nothing).
pub(crate) fn ratio(rounds: &[(f64, f64)]) -> Option<Ratio> {
    let ratios: Vec<f64> = rounds
        .iter()
        .map(|&(ocync, other)| (ocync.is_finite() && ocync > 0.0).then(|| other / ocync))
        .collect::<Option<_>>()?;
    let spread = spread(&ratios)?;
    let verdict = if spread.min > 1.0 {
        Verdict::Faster
    } else if spread.max < 1.0 {
        Verdict::Slower
    } else {
        Verdict::NoMeasuredDifference
    };
    Some(Ratio {
        spread,
        rounds: rounds.len(),
        confidence: confidence(rounds.len()),
        verdict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_is_the_median_and_the_extremes() {
        // Catches: a median taken before sorting, and an even count taking one
        // middle value instead of the mean of both.
        let cases: [(&[f64], f64, f64, f64); 4] = [
            (&[3.0, 1.0, 2.0], 2.0, 1.0, 3.0),
            (&[4.0, 1.0, 3.0, 2.0], 2.5, 1.0, 4.0),
            (&[7.0], 7.0, 7.0, 7.0),
            (&[5.0, 5.0, 9.0, 1.0, 5.0], 5.0, 1.0, 9.0),
        ];
        for (values, median, min, max) in cases {
            assert_eq!(spread(values), Some(Spread { median, min, max }));
        }
    }

    #[test]
    fn no_values_or_a_value_that_is_not_finite_has_no_spread() {
        assert_eq!(spread(&[]), None);
        assert_eq!(spread(&[1.0, f64::NAN, 3.0]), None);
        assert_eq!(spread(&[1.0, f64::INFINITY, 3.0]), None);
    }

    #[test]
    fn confidence_is_one_minus_twice_the_chance_all_rounds_share_a_side() {
        // Catches: a hard-coded 93.75% quoted for a run of another length.
        for (rounds, expected) in [(1, 0.0), (2, 0.5), (3, 0.75), (4, 0.875), (5, 0.9375)] {
            assert_eq!(confidence(rounds), expected, "{rounds} rounds");
        }
    }

    #[test]
    fn the_verdict_needs_every_round_on_one_side_of_one() {
        // (name, (ocync, other) per round, verdict, median, min, max)
        type Case = (&'static str, &'static [(f64, f64)], Verdict, f64, f64, f64);
        let cases: [Case; 5] = [
            (
                "ocync lower every round",
                &[(10.0, 20.0), (10.0, 30.0), (10.0, 15.0)],
                Verdict::Faster,
                2.0,
                1.5,
                3.0,
            ),
            (
                "ocync higher every round",
                &[(10.0, 5.0), (10.0, 8.0), (10.0, 9.0)],
                Verdict::Slower,
                0.8,
                0.5,
                0.9,
            ),
            (
                "one round on the other side",
                &[(10.0, 20.0), (10.0, 8.0), (10.0, 15.0)],
                Verdict::NoMeasuredDifference,
                1.5,
                0.8,
                2.0,
            ),
            (
                "a round at exactly 1 is not a win",
                &[(10.0, 10.0), (10.0, 20.0), (10.0, 30.0)],
                Verdict::NoMeasuredDifference,
                2.0,
                1.0,
                3.0,
            ),
            (
                "an even count averages the middle ratios",
                &[(10.0, 20.0), (10.0, 40.0), (10.0, 30.0), (10.0, 50.0)],
                Verdict::Faster,
                3.5,
                2.0,
                5.0,
            ),
        ];
        for (name, rounds, verdict, median, min, max) in cases {
            let got = ratio(rounds).unwrap_or_else(|| panic!("{name}: no ratio"));
            assert_eq!(got.verdict, verdict, "{name}");
            assert_eq!(got.spread, Spread { median, min, max }, "{name}");
            assert_eq!(got.rounds, rounds.len(), "{name}");
            assert_eq!(got.confidence, confidence(rounds.len()), "{name}");
        }
    }

    #[test]
    fn a_ratio_over_nothing_is_not_reported() {
        // Catches: a warm pass with zero bytes for ocync reporting an infinite
        // ratio as a win.
        assert_eq!(ratio(&[]), None);
        assert_eq!(ratio(&[(10.0, 20.0), (0.0, 20.0), (10.0, 20.0)]), None);
        assert_eq!(ratio(&[(f64::NAN, 20.0)]), None);
        assert_eq!(ratio(&[(10.0, f64::INFINITY)]), None);
    }
}
