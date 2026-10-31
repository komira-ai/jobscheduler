//! The t and chi-square quantiles against published table values.

use js_analysis::{chi_square_quantile, t_critical, t_quantile};

fn close(got: f64, want: f64, tolerance: f64) {
    assert!((got - want).abs() <= tolerance, "got {got}, want {want}");
}

/// Asserts the two-sided 95% critical values of t at the degrees of freedom of a standard
/// table (three decimals), including the four the analysis rule is checked against.
#[test]
fn t_critical_matches_the_table() {
    let table = [
        (1.0, 12.706),
        (2.0, 4.303),
        (4.0, 2.776),
        (5.0, 2.571),
        (10.0, 2.228),
        (20.0, 2.086),
        (30.0, 2.042),
        (120.0, 1.980),
    ];
    for (df, want) in table {
        close(t_critical(df), want, 1e-3);
    }
}

/// Asserts the 0.8 quantiles of t (the power term) against the table.
#[test]
fn t_quantile_at_80_percent_matches_the_table() {
    for (df, want) in [(2.0, 1.061), (4.0, 0.941), (10.0, 0.879)] {
        close(t_quantile(0.8, df), want, 1e-3);
    }
}

/// Asserts that a fractional df lies between its neighbours, as Welch's df does.
#[test]
fn t_critical_falls_between_neighbouring_degrees_of_freedom() {
    let between = t_critical(7.5);
    assert!(between < t_critical(7.0) && between > t_critical(8.0));
}

/// Asserts the chi-square quantiles used by the std/mean interval against the table.
#[test]
fn chi_square_quantiles_match_the_table() {
    let table = [
        (1.0, 0.975, 5.024),
        (2.0, 0.975, 7.378),
        (2.0, 0.025, 0.0506),
        (9.0, 0.975, 19.023),
        (9.0, 0.025, 2.700),
        (19.0, 0.975, 32.852),
        (19.0, 0.025, 8.907),
        (29.0, 0.975, 45.722),
        (29.0, 0.025, 16.047),
    ];
    for (df, p, want) in table {
        close(chi_square_quantile(p, df), want, 2e-3);
    }
}

/// Asserts ten-digit reference values. df 1 is tan(pi * 0.475), df 2 is 0.95 / sqrt(2 * 0.975
/// * 0.025), chi-square with 2 df is -2 ln(1 - p), and the others are the published values.
#[test]
fn quantiles_agree_with_reference_values_to_eight_digits() {
    let t = [
        (1.0, 12.706_204_736_2),
        (2.0, 4.302_652_729_7),
        (4.0, 2.776_445_105_2),
        (10.0, 2.228_138_852_0),
        (30.0, 2.042_272_456_3),
    ];
    for (df, want) in t {
        close(t_critical(df), want, 1e-8);
    }
    let chi = [
        (1.0, 0.975, 5.023_886_187_3),
        (2.0, 0.975, 7.377_758_908_2),
        (2.0, 0.025, 0.050_635_616_0),
        (9.0, 0.975, 19.022_767_798_6),
    ];
    for (df, p, want) in chi {
        close(chi_square_quantile(p, df), want, 1e-8);
    }
}
