//! Small charts as inline SVG, rendered server-side so a page needs no
//! script to show them. Geometry lives in the SVG (a fixed viewBox
//! stretched to the container, strokes kept at their pixel width); the
//! text — axis labels, tick labels — is HTML beside it, so nothing
//! stretches. Colours come from `currentColor` under a `data-hue`
//! attribute the stylesheet maps to its ink tokens, so every theme
//! applies. Each day gets a full-height hover column whose `<title>`
//! carries the exact values.

use std::fmt::Write as _;

/// The plot's coordinate space.
const W: f64 = 1000.0;
const H: f64 = 200.0;
/// Room above the tallest point so it isn't clipped by the frame.
const TOP_PAD: f64 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Red,
    Amber,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Accent,
    Muted,
}

impl Hue {
    /// The `data-hue` value the stylesheet maps to a colour.
    pub fn attr(self) -> &'static str {
        match self {
            Hue::Red => "red",
            Hue::Amber => "amber",
            Hue::Green => "green",
            Hue::Teal => "teal",
            Hue::Blue => "blue",
            Hue::Purple => "purple",
            Hue::Pink => "pink",
            Hue::Accent => "accent",
            Hue::Muted => "muted",
        }
    }

    /// Distinct hues for series that have none assigned, in order.
    pub const CYCLE: [Hue; 7] = [
        Hue::Blue,
        Hue::Green,
        Hue::Amber,
        Hue::Purple,
        Hue::Teal,
        Hue::Pink,
        Hue::Red,
    ];
}

/// One line or one stack layer.
#[derive(Debug, Clone, Copy)]
pub struct Series<'a> {
    pub label: &'a str,
    pub values: &'a [f64],
    pub hue: Hue,
}

/// How a value reads in labels and tooltips.
pub type Fmt = fn(f64) -> String;

/// "1,204".
pub fn fmt_int(v: f64) -> String {
    let n = v.round() as i64;
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// "1.2k" for tick labels.
pub fn fmt_compact(v: f64) -> String {
    let a = v.abs();
    let s = if a >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if a >= 10_000.0 {
        format!("{:.0}k", v / 1000.0)
    } else if a >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else {
        fmt_int(v)
    };
    s.replace(".0k", "k").replace(".0M", "M")
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// A coordinate as short text, never NaN or infinite.
fn num(v: f64) -> String {
    let v = if v.is_finite() { v } else { 0.0 };
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// The axis top: 1, 2, 2.5 or 5 times a power of ten, at or above `max`
/// and never below 1.
pub fn nice_max(max: f64) -> f64 {
    if !(max.is_finite() && max > 1.0) {
        return 1.0;
    }
    let exp = max.log10().floor();
    let base = 10f64.powf(exp);
    let m = max / base;
    let step = if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 2.5 {
        2.5
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * base
}

/// Which x positions get a label: the first, the last, and a few evenly
/// between.
pub fn tick_indices(n: usize, want: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 || want < 2 {
        return vec![0];
    }
    let want = want.min(n);
    let mut out: Vec<usize> = (0..want)
        .map(|k| (k as f64 * (n - 1) as f64 / (want - 1) as f64).round() as usize)
        .collect();
    out.dedup();
    out
}

/// Points sit on the centre of equal columns, like bars do, so every
/// hover column is the same width.
fn x_at(i: usize, n: usize) -> f64 {
    (i as f64 + 0.5) * W / n.max(1) as f64
}

fn y_at(v: f64, max: f64) -> f64 {
    let v = if v.is_finite() { v.max(0.0) } else { 0.0 };
    H - (v / max) * (H - TOP_PAD)
}

fn max_of(series: &[Series]) -> f64 {
    series
        .iter()
        .flat_map(|s| s.values.iter().copied())
        .filter(|v| v.is_finite())
        .fold(0.0, f64::max)
}

fn stacked_max(series: &[Series], n: usize) -> f64 {
    (0..n)
        .map(|i| {
            series
                .iter()
                .map(|s| s.values.get(i).copied().unwrap_or(0.0).max(0.0))
                .sum::<f64>()
        })
        .fold(0.0, f64::max)
}

fn shortest(series: &[Series]) -> usize {
    series.iter().map(|s| s.values.len()).min().unwrap_or(0)
}

fn gridlines(out: &mut String) {
    out.push_str("<g class=\"fl-chart-grid\">");
    for k in 0..4 {
        let y = TOP_PAD + (H - TOP_PAD) * k as f64 / 3.0;
        let _ = write!(
            out,
            "<line x1=\"0\" x2=\"{W}\" y1=\"{}\" y2=\"{}\"/>",
            num(y),
            num(y)
        );
    }
    out.push_str("</g>");
}

/// The four gridline values, top first. On a small axis two gridlines
/// can round to the same label; the repeat is left blank.
fn y_labels(out: &mut String, max: f64, fmt: Fmt) {
    out.push_str("<div class=\"fl-chart-y\">");
    let mut previous = String::new();
    for k in 0..4 {
        let v = max * (3 - k) as f64 / 3.0;
        let label = fmt(v);
        let shown = if label == previous {
            ""
        } else {
            label.as_str()
        };
        let _ = write!(out, "<span>{}</span>", esc(shown));
        previous = label;
    }
    out.push_str("</div>");
}

fn x_labels(out: &mut String, x: &[String], n: usize) {
    out.push_str("<div class=\"fl-chart-x\">");
    for i in tick_indices(n, 4) {
        let left = (i as f64 + 0.5) * 100.0 / n.max(1) as f64;
        let label = x.get(i).map(String::as_str).unwrap_or("");
        let _ = write!(
            out,
            "<span style=\"left:{}%\">{}</span>",
            num(left),
            esc(label)
        );
    }
    out.push_str("</div>");
}

/// One transparent column per index carrying the values as a tooltip.
fn hit_columns(out: &mut String, x: &[String], series: &[Series], n: usize, fmt: Fmt) {
    out.push_str("<g class=\"fl-chart-hit\">");
    let width = if n <= 1 { W } else { W / n as f64 };
    for i in 0..n {
        let (left, right) = (i as f64 * width, (i as f64 + 1.0) * width);
        let mut title = x.get(i).cloned().unwrap_or_default();
        for s in series {
            let v = s.values.get(i).copied().unwrap_or(0.0);
            let _ = write!(title, " · {} {}", s.label, fmt(v));
        }
        let _ = write!(
            out,
            "<rect x=\"{}\" y=\"0\" width=\"{}\" height=\"{H}\"><title>{}</title></rect>",
            num(left),
            num(right - left),
            esc(&title)
        );
    }
    out.push_str("</g>");
}

fn open(out: &mut String, height_px: u32) {
    let _ = write!(
        out,
        "<div class=\"fl-chart\" style=\"--fl-chart-h:{height_px}px\">"
    );
}

fn open_svg(out: &mut String) {
    let _ = write!(
        out,
        "<svg class=\"fl-chart-plot\" viewBox=\"0 0 {W} {H}\" preserveAspectRatio=\"none\" aria-hidden=\"true\">"
    );
}

/// Lines over a shared axis; the first series is also filled to the
/// baseline. Values beyond the shortest series are ignored.
pub fn line_chart(x: &[String], series: &[Series], fmt: Fmt, height_px: u32) -> String {
    let n = shortest(series).min(x.len().max(if series.is_empty() { 0 } else { usize::MAX }));
    let n = if series.is_empty() { x.len() } else { n };
    let max = nice_max(max_of(series));
    let mut out = String::new();
    open(&mut out, height_px);
    y_labels(&mut out, max, fmt);
    open_svg(&mut out);
    gridlines(&mut out);
    for (k, s) in series.iter().enumerate() {
        let _ = write!(out, "<g data-hue=\"{}\">", s.hue.attr());
        if n == 1 {
            let _ = write!(
                out,
                "<circle class=\"fl-chart-dot\" cx=\"{}\" cy=\"{}\" r=\"4\"/>",
                num(x_at(0, 1)),
                num(y_at(s.values[0], max))
            );
        } else if n > 1 {
            let mut path = String::new();
            for i in 0..n {
                let _ = write!(
                    path,
                    "{}{},{}",
                    if i == 0 { "M" } else { " L" },
                    num(x_at(i, n)),
                    num(y_at(s.values[i], max))
                );
            }
            if k == 0 {
                let _ = write!(
                    out,
                    "<path class=\"fl-chart-area\" d=\"M{},{H} {} L{},{H} Z\"/>",
                    num(x_at(0, n)),
                    path.trim_start_matches('M').replacen("", "L", 1),
                    num(x_at(n - 1, n))
                );
            }
            let _ = write!(out, "<path class=\"fl-chart-line\" d=\"{path}\"/>");
            if n <= 31 {
                for i in 0..n {
                    let _ = write!(
                        out,
                        "<circle class=\"fl-chart-dot\" cx=\"{}\" cy=\"{}\" r=\"2.5\"/>",
                        num(x_at(i, n)),
                        num(y_at(s.values[i], max))
                    );
                }
            }
        }
        out.push_str("</g>");
    }
    hit_columns(&mut out, x, series, n, fmt);
    out.push_str("</svg>");
    x_labels(&mut out, x, n);
    out.push_str("</div>");
    out
}

/// Bars stacked per index, one layer per series.
pub fn stacked_bars(x: &[String], series: &[Series], fmt: Fmt, height_px: u32) -> String {
    let n = if series.is_empty() {
        x.len()
    } else {
        shortest(series).min(x.len())
    };
    let max = nice_max(stacked_max(series, n));
    let mut out = String::new();
    open(&mut out, height_px);
    y_labels(&mut out, max, fmt);
    open_svg(&mut out);
    gridlines(&mut out);
    if n > 0 {
        let slot = W / n as f64;
        let bar = (slot * 0.7).max(1.0);
        // Where the next layer of each column starts: the top of the one
        // below it.
        let mut floor = vec![H; n];
        for s in series {
            let _ = write!(out, "<g data-hue=\"{}\">", s.hue.attr());
            for (i, column_floor) in floor.iter_mut().enumerate() {
                let v = s.values.get(i).copied().unwrap_or(0.0);
                if !(v.is_finite() && v > 0.0) {
                    continue;
                }
                let height = (v / max) * (H - TOP_PAD);
                let top = *column_floor - height;
                let _ = write!(
                    out,
                    "<rect class=\"fl-chart-bar\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>",
                    num(i as f64 * slot + (slot - bar) / 2.0),
                    num(top),
                    num(bar),
                    num(height)
                );
                *column_floor = top;
            }
            out.push_str("</g>");
        }
    }
    hit_columns(&mut out, x, series, n, fmt);
    out.push_str("</svg>");
    x_labels(&mut out, x, n);
    out.push_str("</div>");
    out
}

/// A tiny trend line with no axes: the shape of a number over time.
pub fn sparkline(values: &[f64], hue: Hue, title: &str) -> String {
    const SW: f64 = 100.0;
    const SH: f64 = 28.0;
    let n = values.len();
    let max = nice_max(
        values
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .fold(0.0, f64::max),
    );
    let mut out = String::new();
    let _ = write!(
        out,
        "<svg class=\"fl-spark\" viewBox=\"0 0 {SW} {SH}\" preserveAspectRatio=\"none\" data-hue=\"{}\" role=\"img\"><title>{}</title>",
        hue.attr(),
        esc(title)
    );
    let y = |v: f64| SH - 2.0 - (v.max(0.0) / max) * (SH - 4.0);
    match n {
        0 => {
            let _ = write!(
                out,
                "<path class=\"fl-chart-line\" d=\"M0,{} L{SW},{}\"/>",
                num(SH - 2.0),
                num(SH - 2.0)
            );
        }
        1 => {
            let _ = write!(
                out,
                "<path class=\"fl-chart-line\" d=\"M0,{} L{SW},{}\"/><circle class=\"fl-chart-dot\" cx=\"{}\" cy=\"{}\" r=\"2\"/>",
                num(y(values[0])),
                num(y(values[0])),
                num(SW / 2.0),
                num(y(values[0]))
            );
        }
        _ => {
            let mut path = String::new();
            for (i, v) in values.iter().enumerate() {
                let _ = write!(
                    path,
                    "{}{},{}",
                    if i == 0 { "M" } else { " L" },
                    num(i as f64 * SW / (n - 1) as f64),
                    num(y(*v))
                );
            }
            let _ = write!(
                out,
                "<path class=\"fl-chart-area\" d=\"{} L{SW},{SH} L0,{SH} Z\"/><path class=\"fl-chart-line\" d=\"{path}\"/><circle class=\"fl-chart-dot\" cx=\"{SW}\" cy=\"{}\" r=\"2\"/>",
                path,
                num(y(values[n - 1]))
            );
        }
    }
    out.push_str("</svg>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("d{i}")).collect()
    }

    #[test]
    fn nice_max_rounds_up_to_a_clean_top() {
        assert_eq!(nice_max(0.0), 1.0);
        assert_eq!(nice_max(1.0), 1.0);
        assert_eq!(nice_max(7.0), 10.0);
        assert_eq!(nice_max(12.0), 20.0);
        assert_eq!(nice_max(24.0), 25.0);
        assert_eq!(nice_max(260.0), 500.0);
        assert_eq!(nice_max(f64::NAN), 1.0);
    }

    #[test]
    fn ticks_keep_the_ends() {
        assert_eq!(tick_indices(0, 4), Vec::<usize>::new());
        assert_eq!(tick_indices(1, 4), vec![0]);
        assert_eq!(tick_indices(2, 4), vec![0, 1]);
        assert_eq!(tick_indices(30, 4), vec![0, 10, 19, 29]);
    }

    #[test]
    fn a_line_chart_has_one_hover_column_per_point() {
        let values: Vec<f64> = (0..7).map(|i| i as f64).collect();
        let svg = line_chart(
            &days(7),
            &[Series {
                label: "signups",
                values: &values,
                hue: Hue::Blue,
            }],
            fmt_int,
            160,
        );
        assert_eq!(svg.matches("<title>").count(), 7);
        assert_eq!(svg.matches("<rect ").count(), 7);
        // Every column is the same width and they tile the frame.
        assert!(
            svg.contains("<rect x=\"0\" y=\"0\" width=\"142.9\""),
            "{svg}"
        );
        assert!(
            svg.contains("<rect x=\"857.1\" y=\"0\" width=\"142.9\""),
            "{svg}"
        );
        assert!(svg.contains("fl-chart-area"));
        assert!(svg.contains("data-hue=\"blue\""));
        assert!(svg.contains("d6 · signups 6"));
        assert!(!svg.contains("NaN"));
    }

    #[test]
    fn empty_and_single_point_series_still_render() {
        let empty = line_chart(&[], &[], fmt_int, 160);
        assert!(empty.contains("fl-chart-grid"));
        assert!(!empty.contains("<path"));
        let one = line_chart(
            &days(1),
            &[Series {
                label: "x",
                values: &[3.0],
                hue: Hue::Green,
            }],
            fmt_int,
            160,
        );
        assert_eq!(one.matches("<circle").count(), 1);
        assert!(!one.contains("fl-chart-line"));
        assert!(!one.contains("NaN") && !one.contains("inf"));
    }

    #[test]
    fn small_axes_do_not_repeat_a_label() {
        let one = [1.0, 0.0];
        let svg = line_chart(
            &days(2),
            &[Series {
                label: "x",
                values: &one,
                hue: Hue::Blue,
            }],
            fmt_compact,
            160,
        );
        // Top 1, then 0.67 and 0.33 round to 1 and 0: the repeats go blank.
        assert!(
            svg.contains("<span>1</span><span></span><span>0</span><span></span>"),
            "{svg}"
        );
    }

    #[test]
    fn all_zero_series_sit_on_the_baseline() {
        let zeros = [0.0; 5];
        let svg = line_chart(
            &days(5),
            &[Series {
                label: "z",
                values: &zeros,
                hue: Hue::Blue,
            }],
            fmt_int,
            160,
        );
        assert!(svg.contains("M100,200"), "{svg}");
        assert!(svg.contains("<span>0</span>"));
        assert!(!svg.contains("NaN"));
    }

    #[test]
    fn stacked_bars_pile_up_and_skip_zeros() {
        let a = [1.0, 0.0, 2.0];
        let b = [1.0, 1.0, 0.0];
        let svg = stacked_bars(
            &days(3),
            &[
                Series {
                    label: "a",
                    values: &a,
                    hue: Hue::Blue,
                },
                Series {
                    label: "b",
                    values: &b,
                    hue: Hue::Green,
                },
            ],
            fmt_int,
            160,
        );
        assert_eq!(svg.matches("fl-chart-bar").count(), 4);
        assert!(svg.contains("d1 · a 0 · b 1"));
    }

    #[test]
    fn labels_are_escaped_and_sparklines_never_vanish() {
        let svg = line_chart(
            &["<b>".to_string()],
            &[Series {
                label: "a&b",
                values: &[1.0],
                hue: Hue::Blue,
            }],
            fmt_int,
            160,
        );
        assert!(svg.contains("&lt;b&gt;") && svg.contains("a&amp;b"));
        assert!(!svg.contains("<b>"));
        for values in [&[][..], &[2.0][..], &[1.0, 4.0, 2.0][..]] {
            let s = sparkline(values, Hue::Accent, "trend");
            assert!(s.starts_with("<svg") && s.contains("fl-chart-line"));
            assert!(!s.contains("NaN"));
        }
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(fmt_int(1204.0), "1,204");
        assert_eq!(fmt_int(-42.0), "-42");
        assert_eq!(fmt_compact(950.0), "950");
        assert_eq!(fmt_compact(1200.0), "1.2k");
        assert_eq!(fmt_compact(2000.0), "2k");
        assert_eq!(fmt_compact(15000.0), "15k");
        assert_eq!(fmt_compact(2_500_000.0), "2.5M");
    }
}
