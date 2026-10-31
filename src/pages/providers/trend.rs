//! The cumulative trend chart drawn under a card meter: the recorded samples
//! solid over the used time span, a dashed join from 0% at the window start to
//! where the series begins, and a dashed projection from the last sample to
//! the reset — the straight-line estimate of the current rate. The y-axis
//! always tops at the 100% allowance: a projection past it pins there and runs
//! flat to the reset.
//!
//! This module owns only the chart geometry and painting; callers derive the
//! [`Trend`] and render their own header above [`chart`].

use std::rc::Rc;

use chrono::{DateTime, Datelike, Utc};
use gpui::{
    App, BorderStyle, Bounds, Corners, Div, Edges, Hsla, ParentElement, Path, PathBuilder, Pixels,
    Point, ShapedLine, SharedString, Styled, TextAlign, TextRun, Window, canvas, point, px, quad,
    size,
};
use gpui_kit::component::ActiveTheme;

/// Chart element height: plot + one label row.
const TREND_HEIGHT: f32 = 88.;
/// The chart's y-axis top: the window allowance is the story, so the scale
/// never stretches past it — a projection past 100% pins there and runs flat.
const TREND_TOP_PCT: f64 = 100.;
const TREND_TOP: f32 = 4.;
const TREND_LABEL_ROW: f32 = 13.;
const TREND_RIGHT: f32 = 4.;
const AXIS_FONT: f32 = 9.5;
const AXIS_LINE_HEIGHT: f32 = 12.;

/// One plotted sample: the window's cumulative used share at an instant.
#[derive(Clone, Debug)]
pub(super) struct TrendPoint {
    pub(super) at: DateTime<Utc>,
    pub(super) pct: f64,
}

/// Everything the chart paints, derived once per render from the reading and
/// its recorded samples. The projection is pure arithmetic:
/// `used_pct / elapsed_fraction`, extended to the window's end.
pub(super) struct Trend {
    pub(super) start: DateTime<Utc>,
    pub(super) end: DateTime<Utc>,
    /// The reading instant, clamped into the window.
    pub(super) now: DateTime<Utc>,
    /// Current used share (the projection's anchor).
    pub(super) used_pct: f64,
    /// Samples within the current window, oldest first; the current reading is
    /// always the last point.
    pub(super) points: Vec<TrendPoint>,
    /// Projected share at the window's end; `None` before an hour of elapsed
    /// window (extrapolating from minutes produces noise, not a forecast).
    pub(super) forecast: Option<f64>,
    /// When the projection pins at the 100% cap (already over the limit counts
    /// as now).
    pub(super) crossing: Option<DateTime<Utc>>,
}

/// The chart canvas, `TREND_HEIGHT` tall and full width; the caller supplies
/// the header row above it.
pub(super) fn chart(trend: Rc<Trend>) -> Div {
    let chart = canvas(
        move |bounds, window, cx| prepaint_trend(bounds, &trend, window, cx),
        |_bounds, pre, window, cx| paint_trend(pre, window, cx),
    );
    gpui::div()
        .h(px(TREND_HEIGHT))
        .w_full()
        .child(chart.size_full())
}

/// Chart geometry and paths, absolute in window space (prepaint folds the
/// bounds origin in), so painting is a straight walk.
struct TrendPaint {
    plot_left: f32,
    plot_right: f32,
    baseline: f32,
    grid: Vec<f32>,
    y_labels: Vec<(ShapedLine, Point<Pixels>)>,
    x_labels: Vec<(ShapedLine, Point<Pixels>)>,
    area: Option<Path<Pixels>>,
    usage: Option<Path<Pixels>>,
    /// The dashed uniform-rate reference across the whole window.
    plan: Option<Path<Pixels>>,
}

fn prepaint_trend(
    bounds: Bounds<Pixels>,
    trend: &Trend,
    window: &mut Window,
    cx: &mut App,
) -> TrendPaint {
    let theme = cx.theme();
    let axis_color = theme.muted_foreground;
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    let origin_x: f32 = bounds.origin.x.into();
    let origin_y: f32 = bounds.origin.y.into();
    let plot_top = origin_y + TREND_TOP;
    let baseline = origin_y + height - TREND_LABEL_ROW;
    let plot_h = (baseline - plot_top).max(1.);

    let font = window.text_style().font();
    let shape = |window: &mut Window, text: &str| {
        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color: axis_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window.text_system().shape_line(
            SharedString::from(text.to_string()),
            px(AXIS_FONT),
            &[run],
            None,
        )
    };

    let ticks = [0., 50., TREND_TOP_PCT];
    let y_lines: Vec<(ShapedLine, f64)> = ticks
        .iter()
        .map(|pct| (shape(window, &format!("{pct:.0}%")), *pct))
        .collect();
    let widest = y_lines
        .iter()
        .map(|(line, _)| f32::from(line.width()))
        .fold(0., f32::max);
    let plot_left = origin_x + widest + 4.;
    let plot_right = (origin_x + width - TREND_RIGHT).max(plot_left + 1.);

    let span = (trend.end - trend.start).num_seconds().max(1) as f32;
    let x_of = |at: DateTime<Utc>| {
        let rel = ((at - trend.start).num_seconds() as f32 / span).clamp(0., 1.);
        plot_left + (plot_right - plot_left) * rel
    };
    let y_of = |pct: f64| {
        let rel = (pct / TREND_TOP_PCT).clamp(0., 1.) as f32;
        plot_top + plot_h * (1. - rel)
    };

    let y_labels: Vec<(ShapedLine, Point<Pixels>)> = y_lines
        .into_iter()
        .map(|(line, pct)| {
            let w: f32 = line.width().into();
            (
                line,
                point(
                    px(plot_left - 4. - w),
                    px(y_of(pct) - AXIS_LINE_HEIGHT / 2.),
                ),
            )
        })
        .collect();

    let points: Vec<Point<Pixels>> = trend
        .points
        .iter()
        .map(|sample| point(px(x_of(sample.at)), px(y_of(sample.pct))))
        .collect();

    let (area, usage) = match (points.first(), points.last()) {
        (Some(first), Some(last)) => {
            let area = build_path(PathBuilder::fill(), |builder| {
                builder.move_to(point(first.x, px(y_of(0.))));
                for at in &points {
                    builder.line_to(*at);
                }
                builder.line_to(point(last.x, px(y_of(0.))));
                builder.close();
            });
            let usage = build_path(PathBuilder::stroke(px(1.5)), |builder| {
                builder.move_to(*first);
                for at in points.iter().skip(1) {
                    builder.line_to(*at);
                }
            });
            (area, usage)
        }
        _ => (None, None),
    };

    // Dashed connectors around the observed span: from 0% at the window start
    // to the first sample (no readings exist before the series began), and
    // from the last sample to the reset. A projection past the limit pins at
    // 100% the moment it gets there and runs flat to the reset; under the
    // limit it lands wherever it lands. With no samples at all the reference
    // is the full uniform-rate diagonal.
    let plan = trend.forecast.and_then(|projected| {
        if projected <= 0. {
            return None;
        }
        build_path(
            PathBuilder::stroke(px(1.2)).dash_array(&[px(4.), px(3.)]),
            |builder| {
                // The run from `from` to the reset: to the projected share
                // under the limit, or up to the pin and flat at 100%.
                let emit = |builder: &mut PathBuilder, from_at: DateTime<Utc>, from_pct: f64| {
                    builder.move_to(point(px(x_of(from_at)), px(y_of(from_pct))));
                    if projected > TREND_TOP_PCT {
                        let crossing = trend.crossing.unwrap_or(from_at);
                        builder.line_to(point(px(x_of(crossing)), px(y_of(TREND_TOP_PCT))));
                        builder.line_to(point(px(x_of(trend.end)), px(y_of(TREND_TOP_PCT))));
                    } else {
                        builder.line_to(point(px(x_of(trend.end)), px(y_of(projected))));
                    }
                };
                match trend.points.first() {
                    None => emit(builder, trend.start, 0.),
                    Some(first) => {
                        // 0% at the window start, joined to where the series begins.
                        builder.move_to(point(px(x_of(trend.start)), px(y_of(0.))));
                        builder.line_to(point(px(x_of(first.at)), px(y_of(first.pct))));
                        // From the end of the observed usage, straight to the
                        // projection at the reset.
                        if projected > trend.used_pct && trend.used_pct < TREND_TOP_PCT {
                            emit(builder, trend.now, trend.used_pct);
                        }
                    }
                }
            },
        )
    });

    let now_x = x_of(trend.now);
    let mut x_labels = vec![(
        shape(
            window,
            &short_date(trend.start.with_timezone(&crate::ui_tz::display_tz())),
        ),
        point(px(plot_left), px(baseline + 2.)),
    )];
    // "今天" only when it clears both edge labels.
    if now_x - plot_left > 34. && plot_right - now_x > 34. {
        let line = shape(window, "今天");
        let w: f32 = line.width().into();
        x_labels.push((line, point(px(now_x - w / 2.), px(baseline + 2.))));
    }
    {
        let line = shape(
            window,
            &short_date(trend.end.with_timezone(&crate::ui_tz::display_tz())),
        );
        let w: f32 = line.width().into();
        x_labels.push((line, point(px(plot_right - w), px(baseline + 2.))));
    }

    TrendPaint {
        plot_left,
        plot_right,
        baseline,
        grid: ticks.iter().map(|pct| y_of(*pct)).collect(),
        y_labels,
        x_labels,
        area,
        usage,
        plan,
    }
}

/// Build a path from a builder callback; a build error (degenerate geometry)
/// paints nothing instead of failing the frame.
fn build_path(
    mut builder: PathBuilder,
    draw: impl FnOnce(&mut PathBuilder),
) -> Option<Path<Pixels>> {
    draw(&mut builder);
    builder.build().ok()
}

fn paint_trend(pre: TrendPaint, window: &mut Window, cx: &mut App) {
    let theme = cx.theme();

    // Dashed grid at the 50% steps; the 100% limit line is dashed but stronger.
    for (index, y) in pre.grid.iter().enumerate() {
        let color = if index == 2 {
            theme.border
        } else {
            theme.muted
        };
        let mut x = pre.plot_left;
        while x < pre.plot_right {
            window.paint_quad(quad(
                Bounds {
                    origin: point(px(x), px(*y)),
                    size: size(px(2f32.min(pre.plot_right - x)), px(1.)),
                },
                Corners::default(),
                color,
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
            x += 5.;
        }
    }

    // Baseline.
    window.paint_quad(quad(
        Bounds {
            origin: point(px(pre.plot_left), px(pre.baseline)),
            size: size(px(pre.plot_right - pre.plot_left), px(1.)),
        },
        Corners::default(),
        theme.border,
        Edges::default(),
        Hsla::transparent_black(),
        BorderStyle::default(),
    ));

    // The dashed reference goes under everything data-shaped, so the solid
    // usage line reads on top of it.
    if let Some(plan) = pre.plan {
        window.paint_path(plan, theme.danger.opacity(0.75));
    }
    if let Some(area) = pre.area {
        window.paint_path(area, theme.success.opacity(0.10));
    }
    if let Some(usage) = pre.usage {
        window.paint_path(usage, theme.success);
    }

    for (line, origin) in pre.y_labels.iter().chain(&pre.x_labels) {
        line.paint(
            *origin,
            px(AXIS_LINE_HEIGHT),
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
    }
}

/// `MM-DD` in the display timezone (the chart's axis labels).
pub(super) fn short_date(at: DateTime<chrono::FixedOffset>) -> String {
    format!("{:02}-{:02}", at.month(), at.day())
}
