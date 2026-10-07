//! Latency percentile plots (M12, D60).
//!
//! `hgrm` writes a histogram in HdrHistogram's standard percentile-distribution text
//! format, `parse_hgrm` reads it back, and `svg` draws one or more of them on log-log
//! axes: x is `1/(1-percentile)`, so each decade adds a nine (90%, 99%, 99.9%...),
//! and y is nanoseconds. Like the rest of the harness, this never touches the engine.

use std::fmt::Write;

use hdrhistogram::Histogram;

/// One line of a percentile distribution: the value reached at this percentile, and how
/// many samples were at or below it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub value: u64,
    pub quantile: f64,
    pub total: u64,
}

/// HdrHistogram's text format, as its Java `outputPercentileDistribution` prints it
/// (5 steps per halving of the remaining percentiles), so the files also load in its
/// online plotter. The last line has percentile 1.0 and no `1/(1-Percentile)` column.
pub fn hgrm(h: &Histogram<u64>) -> String {
    let mut out = String::from("       Value     Percentile TotalCount 1/(1-Percentile)\n\n");
    let mut total = 0;
    for v in h.iter_quantiles(5) {
        total += v.count_since_last_iteration();
        let (value, q) = (v.value_iterated_to() as f64, v.quantile_iterated_to());
        if q < 1.0 {
            let _ = writeln!(
                out,
                "{value:12.3} {q:2.12} {total:10} {:14.2}",
                1.0 / (1.0 - q)
            );
        } else {
            let _ = writeln!(out, "{value:12.3} {q:2.12} {total:10}");
        }
    }
    let _ = writeln!(
        out,
        "#[Mean    = {:12.3}, StdDeviation   = {:12.3}]",
        h.mean(),
        h.stdev()
    );
    let _ = writeln!(
        out,
        "#[Max     = {:12.3}, Total count    = {:12}]",
        h.max() as f64,
        h.len()
    );
    out
}

/// Read the lines `hgrm` writes. The header, blank lines and `#` lines are skipped.
pub fn parse_hgrm(text: &str) -> Result<Vec<Point>, String> {
    let mut points = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("Value") {
            continue;
        }
        let bad = || format!("line {}: expected `value percentile total`", i + 1);
        let mut cols = line.split_whitespace();
        let mut next = || cols.next().ok_or_else(bad);
        let value: f64 = next()?.parse().map_err(|_| bad())?;
        let quantile: f64 = next()?.parse().map_err(|_| bad())?;
        let total: u64 = next()?.parse().map_err(|_| bad())?;
        if !(0.0..=1.0).contains(&quantile) || value < 0.0 {
            return Err(bad());
        }
        points.push(Point {
            value: value.round() as u64,
            quantile,
            total,
        });
    }
    if points.is_empty() {
        return Err("no percentile lines".to_string());
    }
    Ok(points)
}

/// The x of a point, in decades of `1/(1-q)`: 0 at p0, 2 at p99, 3 at p99.9. A sample
/// set of n can't resolve past `1/(1-q) = n`, and the last line (q = 1, the max) has an
/// infinite `1/(1-q)`, so both are placed at `log10(n)`.
pub fn decades(p: &Point, samples: u64) -> f64 {
    let limit = (samples.max(1) as f64).log10();
    if p.quantile >= 1.0 {
        return limit;
    }
    (-(1.0 - p.quantile).log10()).min(limit)
}

const W: f64 = 760.0;
const H: f64 = 440.0;
const LEFT: f64 = 70.0;
const RIGHT: f64 = 40.0;
const TOP: f64 = 40.0;
const BOTTOM: f64 = 50.0;
const COLORS: [&str; 4] = ["#2f6fb2", "#d1495b", "#7a7a7a", "#2a9d5c"];

/// Linear interpolation of `v` from `[lo, hi]` onto `[a, b]`.
fn scale(v: f64, lo: f64, hi: f64, a: f64, b: f64) -> f64 {
    a + (v - lo) / (hi - lo) * (b - a)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn ns_label(ns: u64) -> String {
    match ns {
        n if n >= 1_000_000_000 => format!("{} s", n / 1_000_000_000),
        n if n >= 1_000_000 => format!("{} ms", n / 1_000_000),
        n if n >= 1_000 => format!("{} µs", n / 1_000),
        n => format!("{n} ns"),
    }
}

/// "90%", "99%", "99.9%"... for decade `d` (0 is "0%").
fn percentile_label(d: u32) -> String {
    match d {
        0 => "0%".to_string(),
        1 => "90%".to_string(),
        d => format!("99.{}%", "9".repeat(d as usize - 2)).replace(".%", "%"),
    }
}

/// An SVG plot of the given series, each `(label, points)`, on shared log-log axes.
/// The output depends only on the input, so the same files always give the same picture.
pub fn svg(title: &str, series: &[(&str, &[Point])]) -> String {
    assert!(
        !series.is_empty() && series.len() <= COLORS.len(),
        "1 to 4 series"
    );
    let samples = |pts: &[Point]| pts.last().map_or(1, |p| p.total);
    // X: whole decades, enough for the largest sample set.
    let x_max = series
        .iter()
        .map(|(_, pts)| (samples(pts).max(10) as f64).log10().ceil() as u32)
        .max()
        .unwrap_or(1);
    // Y: whole decades of nanoseconds around every value.
    let values = series
        .iter()
        .flat_map(|(_, pts)| pts.iter().map(|p| p.value.max(1)));
    let (lo, hi) = values.fold((u64::MAX, 1), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let y_lo = (lo as f64).log10().floor() as u32;
    let y_hi = ((hi as f64).log10().ceil() as u32).max(y_lo + 1);

    let px = |d: f64| scale(d, 0.0, x_max as f64, LEFT, W - RIGHT);
    let py = |v: u64| {
        let l = (v.max(1) as f64).log10();
        scale(l, y_lo as f64, y_hi as f64, H - BOTTOM, TOP)
    };

    let mut s = String::new();
    let _ = writeln!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" font-family="sans-serif" font-size="12">"#
    );
    let _ = writeln!(s, r#"<rect width="{W}" height="{H}" fill="white"/>"#);
    let _ = writeln!(
        s,
        r#"<text x="{}" y="22" text-anchor="middle" font-size="15">{}</text>"#,
        W / 2.0,
        escape(title)
    );
    // Grid and labels.
    for d in 0..=x_max {
        let x = px(d as f64);
        let _ = writeln!(
            s,
            r##"<line x1="{x:.1}" y1="{TOP}" x2="{x:.1}" y2="{}" stroke="#ddd"/><text x="{x:.1}" y="{}" text-anchor="middle">{}</text>"##,
            H - BOTTOM,
            H - BOTTOM + 18.0,
            percentile_label(d)
        );
    }
    for e in y_lo..=y_hi {
        let v = 10u64.pow(e);
        let y = py(v);
        let _ = writeln!(
            s,
            r##"<line x1="{LEFT}" y1="{y:.1}" x2="{}" y2="{y:.1}" stroke="#ddd"/><text x="{}" y="{:.1}" text-anchor="end">{}</text>"##,
            W - RIGHT,
            LEFT - 6.0,
            y + 4.0,
            ns_label(v)
        );
    }
    let _ = writeln!(
        s,
        r#"<text x="{}" y="{}" text-anchor="middle">percentile</text>"#,
        (LEFT + W - RIGHT) / 2.0,
        H - 12.0
    );
    // One line per series, then its legend entry.
    for (i, ((label, pts), color)) in series.iter().zip(COLORS).enumerate() {
        let n = samples(pts);
        let coords: Vec<String> = pts
            .iter()
            .map(|p| format!("{:.1},{:.1}", px(decades(p, n)), py(p.value)))
            .collect();
        let _ = writeln!(
            s,
            r#"<polyline fill="none" stroke="{color}" stroke-width="2" points="{}"/>"#,
            coords.join(" ")
        );
        let y = TOP + 14.0 + 18.0 * i as f64;
        let _ = writeln!(
            s,
            r#"<line x1="{}" y1="{y}" x2="{}" y2="{y}" stroke="{color}" stroke-width="2"/><text x="{}" y="{}">{}</text>"#,
            LEFT + 12.0,
            LEFT + 36.0,
            LEFT + 42.0,
            y + 4.0,
            escape(label)
        );
    }
    s.push_str("</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latency::histogram;

    fn point(value: u64, quantile: f64, total: u64) -> Point {
        Point {
            value,
            quantile,
            total,
        }
    }

    /// 1000 samples: 1..=1000 ns, one each.
    fn uniform() -> Histogram<u64> {
        let mut h = histogram();
        for v in 1..=1000 {
            h.record(v).unwrap();
        }
        h
    }

    #[test]
    fn hgrm_round_trips_through_parse() {
        let h = uniform();
        let points = parse_hgrm(&hgrm(&h)).unwrap();
        let last = points.last().unwrap();
        assert_eq!((last.value, last.quantile, last.total), (1000, 1.0, 1000));
        assert_eq!(points[0].value, 1);
        // Percentiles, values and running totals only go up.
        for w in points.windows(2) {
            assert!(w[0].quantile <= w[1].quantile);
            assert!(w[0].value <= w[1].value);
            assert!(w[0].total <= w[1].total);
        }
        // Each line's total is the number of samples at or below its value.
        for p in &points {
            assert_eq!(p.total, h.count_between(0, p.value), "{p:?}");
        }
        // And the median line holds the median.
        let median = points.iter().find(|p| p.quantile >= 0.5).unwrap();
        assert_eq!(median.value, h.value_at_quantile(0.5));
    }

    #[test]
    fn parses_the_standard_format() {
        let text = "       Value     Percentile TotalCount 1/(1-Percentile)

      14.000 0.000000000000          3           1.00
      48.000 0.500000000000        700           2.00
     381.000 0.990000000000       1386         100.00
  381695.000 1.000000000000       1400
#[Mean    =       51.000, StdDeviation   =       10.000]
#[Max     =   381695.000, Total count    =         1400]
";
        assert_eq!(
            parse_hgrm(text).unwrap(),
            [
                point(14, 0.0, 3),
                point(48, 0.5, 700),
                point(381, 0.99, 1386),
                point(381695, 1.0, 1400)
            ]
        );
    }

    #[test]
    fn parse_rejects_bad_lines() {
        for bad in [
            "",
            "# only a comment",
            "12.0 0.5",
            "x 0.5 3",
            "12.0 1.5 3",
            "-1.0 0.5 3",
            "12.0 0.5 -3",
        ] {
            assert!(parse_hgrm(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_hgrm("1.0 0.0 1\n2.0 oops 2").unwrap_err(),
            "line 2: expected `value percentile total`"
        );
    }

    #[test]
    fn decades_add_a_nine_each() {
        let at = |q| decades(&point(0, q, 0), 1_000_000);
        assert_eq!(at(0.0), 0.0);
        assert!((at(0.9) - 1.0).abs() < 1e-9);
        assert!((at(0.99) - 2.0).abs() < 1e-9);
        assert!((at(0.999) - 3.0).abs() < 1e-9);
        // The max sits at log10(samples), and nothing goes past it.
        assert_eq!(at(1.0), 6.0);
        assert_eq!(decades(&point(0, 0.99999, 0), 1000), 3.0);
    }

    #[test]
    fn labels() {
        let p: Vec<String> = (0..5).map(percentile_label).collect();
        assert_eq!(p, ["0%", "90%", "99%", "99.9%", "99.99%"]);
        let n: Vec<String> = [10, 100, 1_000, 100_000, 1_000_000, 1_000_000_000]
            .map(ns_label)
            .to_vec();
        assert_eq!(n, ["10 ns", "100 ns", "1 µs", "100 µs", "1 ms", "1 s"]);
    }

    /// The numbers in a `points="..."` attribute, as (x, y) pairs.
    fn polylines(svg: &str) -> Vec<Vec<(f64, f64)>> {
        svg.lines()
            .filter_map(|l| l.split("points=\"").nth(1))
            .map(|rest| {
                rest.split('"')
                    .next()
                    .unwrap()
                    .split(' ')
                    .map(|xy| {
                        let (x, y) = xy.split_once(',').unwrap();
                        (x.parse().unwrap(), y.parse().unwrap())
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn svg_places_points_on_log_log_axes() {
        // 1000 samples: 3 decades of x. Values 10..10_000: y decades 1 to 4.
        let a = [
            point(10, 0.0, 1),
            point(100, 0.99, 990),
            point(10_000, 1.0, 1000),
        ];
        let b = [
            point(20, 0.0, 1),
            point(1000, 0.9, 900),
            point(2000, 1.0, 1000),
        ];
        let out = svg("A & B <test>", &[("ref", &a), ("fast", &b)]);
        assert!(out.starts_with("<svg") && out.ends_with("</svg>\n"));
        assert!(out.contains("A &amp; B &lt;test&gt;"));
        assert!(!out.contains("<test>"));

        let lines = polylines(&out);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 3);
        let (left, right) = (LEFT, W - RIGHT);
        let (bottom, top) = (H - BOTTOM, TOP);
        let third = (right - left) / 3.0;
        let close = |a: f64, b: f64| (a - b).abs() < 0.11;
        // p0 at the left edge, p99 two decades in, the max at the right edge.
        assert!(close(lines[0][0].0, left));
        assert!(close(lines[0][1].0, left + 2.0 * third));
        assert!(close(lines[0][2].0, right));
        // 10 ns at the bottom, 10 µs at the top, 100 ns a third of the way up.
        assert!(close(lines[0][0].1, bottom));
        assert!(close(lines[0][2].1, top));
        assert!(close(lines[0][1].1, bottom - (bottom - top) / 3.0));
        // p90 is one decade in.
        assert!(close(lines[1][1].0, left + third));
        // Gridlines and labels for every decade on both axes.
        for label in [
            "0%", "90%", "99%", "99.9%", "10 ns", "100 ns", "1 µs", "10 µs",
        ] {
            assert!(out.contains(&format!(">{label}</text>")), "{label}");
        }
        assert!(!out.contains(">99.99%<") && !out.contains(">100 µs<"));
        // Legend entries.
        assert!(out.contains(">ref</text>") && out.contains(">fast</text>"));
    }

    #[test]
    fn svg_is_deterministic() {
        let points = parse_hgrm(&hgrm(&uniform())).unwrap();
        let one = svg("t", &[("a", &points)]);
        assert_eq!(one, svg("t", &[("a", &points)]));
    }
}
