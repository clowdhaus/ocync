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

/// What the rounds agree on, for a metric where lower is better. A ratio above
/// 1 favors ocync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// ocync's value was lower than the other tool's in every round.
    OcyncLower,
    /// ocync's value was higher than the other tool's in every round.
    OcyncHigher,
    /// Some rounds on each side of 1, or a round at exactly 1.
    NoMeasuredDifference,
}

/// One round's value of a metric for ocync and for another tool.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pair {
    /// ocync's value.
    pub(crate) ocync: f64,
    /// The other tool's value.
    pub(crate) other: f64,
}

/// The per-round ratios between ocync and one other tool.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Ratio {
    /// Median and range of `other / ocync` over the rounds.
    pub(crate) spread: Spread,
    /// What the rounds agree on; `None` below [`VERDICT_ROUNDS`], where a
    /// one-sided run is too likely between identical builds to be claimed.
    pub(crate) verdict: Option<Verdict>,
}

/// The fewest rounds a verdict is given for. At 5, identical builds would put
/// every round on one side of 1 about one time in sixteen.
pub(crate) const VERDICT_ROUNDS: usize = 5;

/// Chance that the lowest to highest of `rounds` samples holds the median of
/// the distribution they are drawn from: each sample falls on either side of
/// it with probability one half, and the range misses only when all fall on
/// the same side. 0.75 at 3 rounds, 0.9375 at 5. Its complement is the chance
/// that identical builds show one-sided ratios in every round.
pub(crate) fn confidence(rounds: u32) -> f64 {
    1.0 - 2.0 * 0.5_f64.powi(i32::try_from(rounds).expect("a run has fewer than 2^31 rounds"))
}

/// The ratio `other / ocync` of each round, summarized. `None` when there are
/// no rounds, a value is not finite, or ocync's is not above zero (a ratio of
/// nothing says nothing).
pub(crate) fn ratio(rounds: &[Pair]) -> Option<Ratio> {
    let ratios: Vec<f64> = rounds
        .iter()
        .map(|pair| (pair.ocync.is_finite() && pair.ocync > 0.0).then(|| pair.other / pair.ocync))
        .collect::<Option<_>>()?;
    let spread = spread(&ratios)?;
    let verdict = (rounds.len() >= VERDICT_ROUNDS).then_some({
        if spread.min > 1.0 {
            Verdict::OcyncLower
        } else if spread.max < 1.0 {
            Verdict::OcyncHigher
        } else {
            Verdict::NoMeasuredDifference
        }
    });
    Some(Ratio { spread, verdict })
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

    fn pairs(rounds: &[(f64, f64)]) -> Vec<Pair> {
        rounds
            .iter()
            .map(|&(ocync, other)| Pair { ocync, other })
            .collect()
    }

    #[test]
    fn the_verdict_needs_every_round_on_one_side_of_one() {
        // (name, (ocync, other) per round, verdict, median, min, max)
        type Case = (&'static str, &'static [(f64, f64)], Verdict, f64, f64, f64);
        let cases: [Case; 4] = [
            (
                "ocync lower every round",
                &[
                    (10.0, 20.0),
                    (10.0, 30.0),
                    (10.0, 15.0),
                    (10.0, 25.0),
                    (10.0, 20.0),
                ],
                Verdict::OcyncLower,
                2.0,
                1.5,
                3.0,
            ),
            (
                "ocync higher every round",
                &[
                    (10.0, 5.0),
                    (10.0, 8.0),
                    (10.0, 9.0),
                    (10.0, 6.0),
                    (10.0, 8.0),
                ],
                Verdict::OcyncHigher,
                0.8,
                0.5,
                0.9,
            ),
            (
                "one round on the other side",
                &[
                    (10.0, 20.0),
                    (10.0, 8.0),
                    (10.0, 15.0),
                    (10.0, 25.0),
                    (10.0, 20.0),
                ],
                Verdict::NoMeasuredDifference,
                2.0,
                0.8,
                2.5,
            ),
            (
                "a round at exactly 1 is not a win",
                &[
                    (10.0, 10.0),
                    (10.0, 20.0),
                    (10.0, 30.0),
                    (10.0, 25.0),
                    (10.0, 20.0),
                ],
                Verdict::NoMeasuredDifference,
                2.0,
                1.0,
                3.0,
            ),
        ];
        for (name, rounds, verdict, median, min, max) in cases {
            let got = ratio(&pairs(rounds)).unwrap_or_else(|| panic!("{name}: no ratio"));
            assert_eq!(got.verdict, Some(verdict), "{name}");
            assert_eq!(got.spread, Spread { median, min, max }, "{name}");
        }
    }

    #[test]
    fn fewer_than_five_rounds_give_a_range_and_no_verdict() {
        // Catches: a verdict claimed at 3 or 4 rounds, where identical builds
        // would produce it one time in four or eight.
        for rounds in 3..VERDICT_ROUNDS {
            let one_sided = vec![(10.0, 20.0); rounds];
            let got = ratio(&pairs(&one_sided)).unwrap();
            assert_eq!(got.verdict, None, "{rounds} rounds");
            assert_eq!(got.spread.median, 2.0, "{rounds} rounds");
        }
        let five = ratio(&pairs(&[(10.0, 20.0); 5])).unwrap();
        assert_eq!(five.verdict, Some(Verdict::OcyncLower));
    }

    #[test]
    fn a_ratio_over_nothing_is_not_reported() {
        // Catches: a warm pass with zero bytes for ocync reporting an infinite
        // ratio as a win.
        assert_eq!(ratio(&[]), None);
        assert_eq!(
            ratio(&pairs(&[(10.0, 20.0), (0.0, 20.0), (10.0, 20.0)])),
            None
        );
        assert_eq!(ratio(&pairs(&[(f64::NAN, 20.0)])), None);
        assert_eq!(ratio(&pairs(&[(10.0, f64::INFINITY)])), None);
    }
}
