//! Conversions into the types admission reads (`js_types::sched::usage`).

use js_types::sched::usage::{Curve, CurveTooLong};

use crate::fit::Fit;

impl TryFrom<&Fit> for Curve {
    type Error = CurveTooLong;

    /// The curve of a fit: its points, from its runs. A fit holds at most 64 points by
    /// construction; the limit is checked anyway and is never cut to fit.
    fn try_from(fit: &Fit) -> Result<Self, CurveTooLong> {
        Curve::new(fit.points.clone(), fit.runs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_types::sched::usage::MinutePoint;

    /// Catches: points reordered or cut, the run count lost, and a fit over the limit
    /// silently truncated to 64 instead of refused.
    #[test]
    fn a_fit_becomes_the_curve_with_its_points_and_runs() {
        let p = |c, w| MinutePoint {
            cores_milli: c,
            working_set_bytes: w,
        };
        let fit = Fit {
            points: vec![p(1, 2), p(3, 4)],
            runs: 7,
            wall_minutes_p90: 2,
        };
        let curve = Curve::try_from(&fit).unwrap();
        assert_eq!(curve.points(), &[p(1, 2), p(3, 4)]);
        assert_eq!(curve.runs(), 7);

        let long = Fit {
            points: vec![p(0, 0); 65],
            runs: 3,
            wall_minutes_p90: 1,
        };
        assert_eq!(Curve::try_from(&long), Err(CurveTooLong { len: 65 }));
    }
}
