//! What the runs are shown as: the summary printed after a run, the table of
//! all runs, and the HTML report with its charts (inline SVG, no scripts).

use crate::metrics::{bands, Band, Stage, StageSecs};
use crate::record::{Outcome, Record, Sample, Stages, Synced, Timer};

/// Appends formatted text to a `String` (a macro: the format string must be
/// a literal at the call site).
macro_rules! put {
    ($out:expr, $($format:tt)*) => {
        $out.push_str(&format!($($format)*))
    };
}

/// Runs drawn in a chart of several runs: one per colour of the palette.
const CHART_RUNS: usize = 8;
/// Bands listed in a run's summary.
const SLOWEST_BANDS: usize = 5;
/// Points drawn for one line.
const LINE_POINTS: usize = 800;

/// `secs` to its two largest units: `2h14m`, `33m05s`, `45s`, `2.5s`, `40ms`.
pub(crate) fn duration(secs: f64) -> String {
    if secs < 0.0005 {
        return match secs > 0.0 {
            true => "<1ms".to_owned(),
            false => "0s".to_owned(),
        };
    }
    if secs < 0.9995 {
        return format!("{:.0}ms", secs * 1e3);
    }
    if secs < 10.0 {
        return format!("{secs:.1}s");
    }
    let whole = secs.round() as u64;
    let (hours, minutes, seconds) = (whole / 3_600, whole % 3_600 / 60, whole % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m{seconds:02}s"),
        _ => format!("{hours}h{minutes:02}m"),
    }
}

/// `value` with thousands separators.
pub(crate) fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `bytes` in the largest binary unit that keeps it at or above 1.
fn bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    match unit {
        "B" => format!("{bytes} B"),
        _ => format!("{value:.1} {unit}"),
    }
}

/// Seconds spent on one block, in a unit that keeps it readable.
fn per_block(secs: f64) -> String {
    if secs >= 1.0 {
        format!("{secs:.2} s")
    } else if secs >= 0.001 {
        format!("{:.2} ms", secs * 1e3)
    } else {
        format!("{:.1} µs", secs * 1e6)
    }
}

/// `part` of `whole` as a percentage.
fn share(part: f64, whole: f64) -> String {
    match whole > 0.0 {
        true => format!("{:.1}%", 100.0 * part / whole),
        false => "-".to_owned(),
    }
}

/// Seconds since the Unix epoch as `2026-11-25 22:07:05 UTC`: to the second,
/// so two runs never share a label.
fn date(unix: u64) -> String {
    let (days, secs) = ((unix / 86_400) as i64, unix % 86_400);
    // days to a civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

/// What a run is called in tables and charts.
fn label(record: &Record) -> String {
    format!("{} · {}", record.indexes, date(record.started_unix))
}

fn synced(record: &Record) -> Option<&Synced> {
    match &record.outcome {
        Outcome::Synced(synced) => Some(synced),
        Outcome::Failed { .. } => None,
    }
}

/// The zainod a record ran, as `commit (dirty)` or the binary's hash.
fn zainod(record: &Record) -> String {
    let short = |text: &str| text.chars().take(10).collect::<String>();
    match &record.zainod.build {
        Some(build) if build.dirty => format!("{} (dirty)", short(&build.commit)),
        Some(build) => short(&build.commit),
        None => format!("sha256 {}", short(&record.zainod.sha256)),
    }
}

/// Text rows as a table: columns padded to their widest cell, the first
/// left-aligned and the rest right-aligned.
fn text_table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (column, (cell, width)) in row.iter().zip(&widths).enumerate() {
            let pad = " ".repeat(width.saturating_sub(cell.chars().count()));
            match column {
                0 => line.push_str(&format!("{cell}{pad}")),
                _ => line.push_str(&format!("  {pad}{cell}")),
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// A stage timer as `total  mean per block`.
fn timer_cell(timer: Timer) -> String {
    match timer.mean_secs() {
        Some(mean) => format!("{} ({})", duration(timer.secs), per_block(mean)),
        None => "-".to_owned(),
    }
}

/// The stage table's rows: the header, the whole pipeline's stages, then one
/// row per index. The last column is the row's share of all stage time.
fn stage_rows(stages: &Stages) -> Vec<Vec<String>> {
    let total = StageSecs::of(stages).total();
    let mut rows = vec![std::iter::once("")
        .chain(Stage::ALL.iter().map(|stage| stage.name()))
        .chain(std::iter::once("share"))
        .map(str::to_owned)
        .collect::<Vec<_>>()];
    let none = || "-".to_owned();
    rows.push(vec![
        "(every index)".to_owned(),
        timer_cell(stages.fetch),
        timer_cell(stages.resolve),
        none(),
        none(),
        none(),
        none(),
        share(stages.fetch.secs + stages.resolve.secs, total),
    ]);
    for (index, timers) in &stages.indexes {
        let mut row = vec![index.clone(), none(), none()];
        let mut index_secs = 0.0;
        for stage in Stage::PER_INDEX {
            let timer = timers.timer(stage).unwrap_or_default();
            index_secs += timer.secs;
            row.push(timer_cell(timer));
        }
        row.push(share(index_secs, total));
        rows.push(row);
    }
    rows
}

const STAGE_NOTE: &str = "Stage time: total (mean per block). Stages run in parallel, so they \
                          add up to more than the sync time; share is of all stage time.";

/// The bands that took longest per block, slowest first.
fn slowest_bands(bands: &[Band]) -> Vec<&Band> {
    let mut slowest: Vec<&Band> = bands.iter().filter(|band| band.blocks > 0.0).collect();
    slowest.sort_by(|a, b| {
        let per_block = |band: &Band| band.wall_secs / band.blocks;
        per_block(b).total_cmp(&per_block(a))
    });
    slowest.truncate(SLOWEST_BANDS);
    slowest
}

/// The stage a band spent most stage time in.
fn top_stage(band: &Band) -> Option<Stage> {
    Stage::ALL
        .into_iter()
        .filter(|stage| band.stages.get(*stage) > 0.0)
        .max_by(|a, b| band.stages.get(*a).total_cmp(&band.stages.get(*b)))
}

fn band_rows(bands: &[&Band]) -> Vec<Vec<String>> {
    let mut rows = vec![[
        "from height",
        "blocks",
        "time",
        "blk/s",
        "most time in",
        "slowest index",
    ]
    .map(str::to_owned)
    .to_vec()];
    for band in bands {
        rows.push(vec![
            count(band.first_height),
            count(band.blocks.round() as u64),
            duration(band.wall_secs),
            band.blocks_per_sec()
                .map_or_else(|| "-".to_owned(), |rate| count(rate.round() as u64)),
            top_stage(band).map_or("-", Stage::name).to_owned(),
            band.slowest.clone().unwrap_or_else(|| "-".to_owned()),
        ]);
    }
    rows
}

/// What is printed when a run ends.
pub(crate) fn run_summary(record: &Record, series: &[Sample]) -> String {
    let mut out = format!("\n{} ({})\n", record.run_id, label(record));
    let ready = record
        .ready_secs
        .map_or_else(|| "never".to_owned(), duration);
    match &record.outcome {
        Outcome::Failed { cause } => {
            put!(out, "FAILED: {cause}\ntime to ready: {ready}\n");
        }
        Outcome::Synced(synced) => {
            let height = synced.height.map_or_else(
                || "none (no index built)".to_owned(),
                |h| count(u64::from(h)),
            );
            put!(
                out,
                "sync time: {}   time to ready: {ready}   height reached: {height}\n\
                 disk: {}   peak memory: {}   CPU time: {}\n\n",
                duration(synced.sync_secs),
                bytes(synced.disk_total_bytes),
                bytes(synced.peak_memory_bytes),
                duration(synced.cpu_secs),
            );
            put!(
                out,
                "{STAGE_NOTE}\n{}\n",
                text_table(&stage_rows(&synced.stages))
            );
            let bands = bands(series);
            put!(
                out,
                "Slowest bands of the chain (by time per block):\n{}\n",
                text_table(&band_rows(&slowest_bands(&bands)))
            );
        }
    }
    out
}

/// One row per run: what the table of all runs shows.
fn run_rows(records: &[Record]) -> Vec<Vec<String>> {
    let mut rows = vec![[
        "run",
        "indexes",
        "started",
        "zainod",
        "sync time",
        "to ready",
        "height",
        "disk",
        "peak mem",
    ]
    .map(str::to_owned)
    .to_vec()];
    for record in records {
        let ready = record.ready_secs.map_or_else(|| "-".to_owned(), duration);
        let mut row = vec![
            record.run_id.clone(),
            record.indexes.clone(),
            date(record.started_unix),
            zainod(record),
        ];
        match synced(record) {
            Some(synced) => row.extend([
                duration(synced.sync_secs),
                ready,
                synced
                    .height
                    .map_or_else(|| "-".to_owned(), |h| count(u64::from(h))),
                bytes(synced.disk_total_bytes),
                bytes(synced.peak_memory_bytes),
            ]),
            None => {
                let none = || "-".to_owned();
                row.extend(["FAILED".to_owned(), ready, none(), none(), none()]);
            }
        }
        rows.push(row);
    }
    rows
}

/// The table of all runs, with each failed run's cause under it.
pub(crate) fn runs_table(records: &[Record]) -> String {
    if records.is_empty() {
        return "No runs are recorded yet.\n".to_owned();
    }
    let mut out = text_table(&run_rows(records));
    for record in records {
        if let Outcome::Failed { cause } = &record.outcome {
            put!(out, "\n{} failed: {cause}\n", record.run_id);
        }
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html_table(rows: &[Vec<String>]) -> String {
    let mut out = String::from("<table>");
    for (number, row) in rows.iter().enumerate() {
        out.push_str("<tr>");
        let tag = if number == 0 { "th" } else { "td" };
        for cell in row {
            put!(out, "<{tag}>{}</{tag}>", escape(cell));
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
    out
}

/// The chart's drawing area, inside its margins.
const WIDTH: f64 = 860.0;
const LEFT: f64 = 70.0;
const RIGHT: f64 = 44.0;
const TOP: f64 = 12.0;
const BOTTOM: f64 = 44.0;
const PLOT_HEIGHT: f64 = 260.0;

/// The top of an axis holding `max`, and the step between its ticks.
fn axis(max: f64) -> (f64, f64) {
    if max.is_nan() || max <= 0.0 {
        return (1.0, 1.0);
    }
    let magnitude = 10f64.powf((max / 4.0).log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|factor| factor * magnitude)
        .find(|step| max / step <= 5.0)
        .unwrap_or(10.0 * magnitude);
    ((max / step).ceil() * step, step)
}

/// A tick's value, without a trailing `.0`.
fn tick(value: f64) -> String {
    if value >= 1_000.0 && value.fract() == 0.0 {
        count(value as u64)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// The unit a span of seconds is drawn in: its name and its seconds.
fn time_unit(max_secs: f64) -> (&'static str, f64) {
    if max_secs < 180.0 {
        ("seconds", 1.0)
    } else if max_secs < 3.0 * 3_600.0 {
        ("minutes", 60.0)
    } else {
        ("hours", 3_600.0)
    }
}

/// An SVG element of the chart's width and `height`, labelled `title`.
fn svg(height: f64, title: &str, body: &str) -> String {
    format!(
        "<svg viewBox=\"0 0 {WIDTH} {height}\" role=\"img\" aria-label=\"{}\">{body}</svg>",
        escape(title)
    )
}

/// Gridlines, tick labels and titles of a plot whose axes run to `x_top` and
/// `y_top`.
fn axes(x_top: f64, x_step: f64, x_title: &str, y_top: f64, y_step: f64, y_title: &str) -> String {
    let plot_width = WIDTH - LEFT - RIGHT;
    let mut out = String::new();
    let mut y = 0.0;
    while y <= y_top + y_step / 2.0 {
        let at = TOP + PLOT_HEIGHT * (1.0 - y / y_top);
        put!(
            out,
            "<line class=\"grid\" x1=\"{LEFT}\" x2=\"{}\" y1=\"{at:.1}\" y2=\"{at:.1}\"/>\
             <text class=\"tick\" x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            WIDTH - RIGHT,
            LEFT - 8.0,
            at + 4.0,
            tick(y)
        );
        y += y_step;
    }
    let mut x = 0.0;
    while x <= x_top + x_step / 2.0 {
        let at = LEFT + plot_width * x / x_top;
        put!(
            out,
            "<text class=\"tick\" x=\"{at:.1}\" y=\"{}\" text-anchor=\"middle\">{}</text>",
            TOP + PLOT_HEIGHT + 16.0,
            tick(x)
        );
        x += x_step;
    }
    put!(
        out,
        "<line class=\"axis\" x1=\"{LEFT}\" x2=\"{}\" y1=\"{base}\" y2=\"{base}\"/>\
         <text class=\"tick\" x=\"{}\" y=\"{}\" text-anchor=\"middle\">{}</text>\
         <text class=\"tick\" x=\"14\" y=\"{}\" text-anchor=\"middle\" \
         transform=\"rotate(-90 14 {})\">{}</text>",
        WIDTH - RIGHT,
        LEFT + plot_width / 2.0,
        TOP + PLOT_HEIGHT + 36.0,
        escape(x_title),
        TOP + PLOT_HEIGHT / 2.0,
        TOP + PLOT_HEIGHT / 2.0,
        escape(y_title),
        base = TOP + PLOT_HEIGHT,
    );
    out
}

/// One line of a line chart, in palette slot `slot` (1-based).
struct Line {
    slot: usize,
    name: String,
    points: Vec<(f64, f64)>,
}

/// A legend of palette slots and what each stands for.
fn legend(entries: &[(usize, String)]) -> String {
    let mut out = String::from("<p class=\"legend\">");
    for (slot, name) in entries {
        put!(
            out,
            "<span><svg width=\"12\" height=\"12\"><rect class=\"s{slot}\" width=\"12\" \
             height=\"12\" rx=\"2\"/></svg> {}</span>",
            escape(name)
        );
    }
    out.push_str("</p>");
    out
}

/// Lines over shared axes; `x_unit` divides the points' x before drawing.
fn line_chart(title: &str, x_title: &str, x_unit: f64, y_title: &str, lines: &[Line]) -> String {
    let max = |pick: fn(&(f64, f64)) -> f64| {
        lines
            .iter()
            .flat_map(|line| line.points.iter().map(pick))
            .fold(0.0, f64::max)
    };
    let (x_top, x_step) = axis(max(|point| point.0) / x_unit);
    let (y_top, y_step) = axis(max(|point| point.1));
    let plot_width = WIDTH - LEFT - RIGHT;
    let mut body = axes(x_top, x_step, x_title, y_top, y_step, y_title);
    for line in lines {
        let stride = line.points.len().div_ceil(LINE_POINTS).max(1);
        let last = line.points.len().saturating_sub(1);
        let mut path = String::new();
        for (number, (x, y)) in line.points.iter().enumerate() {
            if number % stride != 0 && number != last {
                continue;
            }
            put!(
                path,
                "{}{:.1},{:.1}",
                if path.is_empty() { "M" } else { " L" },
                LEFT + plot_width * (x / x_unit) / x_top,
                TOP + PLOT_HEIGHT * (1.0 - y / y_top)
            );
        }
        put!(
            body,
            "<path class=\"l{}\" d=\"{path}\"><title>{}</title></path>",
            line.slot,
            escape(&line.name)
        );
    }
    let entries: Vec<(usize, String)> = lines
        .iter()
        .map(|line| (line.slot, line.name.clone()))
        .collect();
    format!(
        "{}{}",
        svg(TOP + PLOT_HEIGHT + BOTTOM, title, &body),
        legend(&entries)
    )
}

/// One row of a horizontal bar chart: its label, its segments (palette slot,
/// seconds, what the segment is) and the text after the bar.
struct BarRow {
    label: String,
    segments: Vec<(usize, f64, String)>,
    value: String,
}

/// Horizontal bars of seconds, one row each, segments stacked.
fn bar_chart(title: &str, rows: &[BarRow]) -> String {
    const ROW: f64 = 24.0;
    const BAR: f64 = 14.0;
    const LABELS: f64 = 250.0;
    const VALUES: f64 = 110.0;
    let row_secs = |row: &BarRow| row.segments.iter().map(|segment| segment.1).sum::<f64>();
    let longest = rows.iter().map(row_secs).fold(0.0, f64::max);
    let scale = match longest > 0.0 {
        true => (WIDTH - LABELS - VALUES) / longest,
        false => 0.0,
    };
    let mut body = String::new();
    for (number, row) in rows.iter().enumerate() {
        let top = TOP + ROW * number as f64;
        put!(
            body,
            "<text class=\"label\" x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            LABELS - 10.0,
            top + BAR - 2.0,
            escape(&row.label)
        );
        let mut x = LABELS;
        for (slot, secs, what) in &row.segments {
            let width = secs * scale;
            // a 2px gap of surface between neighbouring segments
            let drawn = if width > 3.0 { width - 2.0 } else { width };
            put!(
                body,
                "<rect class=\"s{slot}\" x=\"{x:.1}\" y=\"{top:.1}\" width=\"{drawn:.1}\" \
                 height=\"{BAR}\" rx=\"2\"><title>{}: {}</title></rect>",
                escape(what),
                duration(*secs)
            );
            x += width;
        }
        put!(
            body,
            "<text class=\"label\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
            x + 8.0,
            top + BAR - 2.0,
            escape(&row.value)
        );
    }
    svg(TOP * 2.0 + ROW * rows.len() as f64, title, &body)
}

/// The palette slot of each stage, fixed.
fn stage_slot(stage: Stage) -> usize {
    Stage::ALL
        .iter()
        .position(|candidate| *candidate == stage)
        .map_or(1, |position| position + 1)
}

fn stage_legend() -> String {
    let entries: Vec<(usize, String)> = Stage::ALL
        .into_iter()
        .map(|stage| (stage_slot(stage), stage.name().to_owned()))
        .collect();
    legend(&entries)
}

/// Stage time per band of the chain: one stacked column per band.
fn band_chart(bands: &[Band]) -> String {
    let tallest = bands
        .iter()
        .map(|band| band.stages.total())
        .fold(0.0, f64::max);
    let (unit_name, unit) = time_unit(tallest);
    let (y_top, y_step) = axis(tallest / unit);
    let last_height = bands
        .last()
        .map_or(0.0, |band| band.first_height as f64 + band.blocks.max(1.0));
    let (x_top, x_step) = axis(last_height);
    let plot_width = WIDTH - LEFT - RIGHT;
    let mut body = axes(
        x_top,
        x_step,
        "height",
        y_top,
        y_step,
        &format!("stage time ({unit_name})"),
    );
    let band_width = plot_width * crate::metrics::BAND_BLOCKS as f64 / x_top;
    let gap = if band_width >= 6.0 { 2.0 } else { 0.0 };
    for band in bands {
        let x = LEFT + plot_width * band.first_height as f64 / x_top;
        let mut y = TOP + PLOT_HEIGHT;
        for stage in Stage::ALL {
            let secs = band.stages.get(stage);
            let height = PLOT_HEIGHT * (secs / unit) / y_top;
            y -= height;
            put!(
                body,
                "<rect class=\"s{}\" x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" \
                 height=\"{height:.1}\"><title>from height {}: {} {}</title></rect>",
                stage_slot(stage),
                (band_width - gap).max(0.5),
                count(band.first_height),
                stage.name(),
                duration(secs)
            );
        }
    }
    format!(
        "{}{}",
        svg(
            TOP + PLOT_HEIGHT + BOTTOM,
            "stage time per band of the chain",
            &body
        ),
        stage_legend()
    )
}

/// Stage time per index: one stacked bar per index, and one for the stages
/// the whole pipeline shares.
fn index_chart(stages: &Stages) -> String {
    let total = StageSecs::of(stages).total();
    let segment =
        |stage: Stage, timer: Timer| (stage_slot(stage), timer.secs, stage.name().to_owned());
    let pipeline_secs = stages.fetch.secs + stages.resolve.secs;
    let mut rows = vec![BarRow {
        label: "(every index)".to_owned(),
        segments: vec![
            segment(Stage::Fetch, stages.fetch),
            segment(Stage::Resolve, stages.resolve),
        ],
        value: format!(
            "{} · {}",
            duration(pipeline_secs),
            share(pipeline_secs, total)
        ),
    }];
    for (index, timers) in &stages.indexes {
        let segments: Vec<_> = Stage::PER_INDEX
            .into_iter()
            .filter_map(|stage| Some(segment(stage, timers.timer(stage)?)))
            .collect();
        let secs: f64 = segments.iter().map(|segment| segment.1).sum();
        rows.push(BarRow {
            label: index.clone(),
            segments,
            value: format!("{} · {}", duration(secs), share(secs, total)),
        });
    }
    format!(
        "{}{}",
        bar_chart("stage time per index", &rows),
        stage_legend()
    )
}

const STYLE: &str = "
:root{--page:#f9f9f9;--surface:#fff;--ink:#191919;--ink2:#666;--grid:#e7e7e7;--axis:#c2c2c2;
--s1:#257de0;--s2:#f47230;--s3:#00b894;--s4:#f2a600;--s5:#ee7fac;--s6:#008a17;--s7:#6a4ce2;
--s8:#ec4647;--critical:#dc3c3d}
@media(prefers-color-scheme:dark){:root{--page:#0b0b0b;--surface:#191919;--ink:#efefef;
--ink2:#9c9c9c;--grid:#303030;--axis:#474747;--s1:#338ced;--s2:#e76624;--s3:#00a888;
--s4:#d28e05;--s5:#e24d8b;--s6:#0aac23;--s7:#9887ef;--s8:#eb6263}}
body{background:var(--page);color:var(--ink);font:14px/1.45 system-ui,sans-serif;
max-width:920px;margin:24px auto;padding:0 16px}
section{background:var(--surface);border-radius:8px;padding:16px 20px;margin:16px 0}
h1{font-size:22px}h2{font-size:17px;margin:0 0 4px}h3{font-size:14px;margin:18px 0 6px}
p{margin:6px 0}.note,.legend{color:var(--ink2);font-size:12.5px}
.legend span{margin-right:14px;white-space:nowrap}.legend svg{vertical-align:-1px}
.failed{color:var(--critical);font-weight:600}pre{white-space:pre-wrap;font-size:12px}
table{border-collapse:collapse;font-size:12.5px;width:100%}
th,td{padding:3px 8px;text-align:right;border-bottom:1px solid var(--grid)}
th:first-child,td:first-child{text-align:left}th{color:var(--ink2);font-weight:600}
.scroll{overflow-x:auto}svg{max-width:100%}
.grid{stroke:var(--grid);stroke-width:1}.axis{stroke:var(--axis);stroke-width:1}
.tick{fill:var(--ink2);font-size:11px}.label{fill:var(--ink);font-size:12px}
.s1{fill:var(--s1)}.s2{fill:var(--s2)}.s3{fill:var(--s3)}.s4{fill:var(--s4)}
.s5{fill:var(--s5)}.s6{fill:var(--s6)}.s7{fill:var(--s7)}.s8{fill:var(--s8)}
.l1,.l2,.l3,.l4,.l5,.l6,.l7,.l8{fill:none;stroke-width:2;stroke-linejoin:round}
.l1{stroke:var(--s1)}.l2{stroke:var(--s2)}.l3{stroke:var(--s3)}.l4{stroke:var(--s4)}
.l5{stroke:var(--s5)}.l6{stroke:var(--s6)}.l7{stroke:var(--s7)}.l8{stroke:var(--s8)}
";

/// A run and its samples.
pub(crate) type Run = (Record, Vec<Sample>);

/// The HTML report of `runs`, oldest first.
pub(crate) fn html(runs: &[Run]) -> String {
    let records: Vec<Record> = runs.iter().map(|(record, _)| record.clone()).collect();
    let mut out = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>zaino-bench: zainod sync</title><style>{STYLE}</style></head><body>\
         <h1>zaino-bench: zainod sync</h1>"
    );
    put!(
        out,
        "<section><h2>Runs</h2><div class=\"scroll\">{}</div>",
        html_table(&run_rows(&records))
    );
    for record in &records {
        if let Outcome::Failed { cause } = &record.outcome {
            put!(
                out,
                "<p><span class=\"failed\">{} failed</span></p><pre>{}</pre>",
                escape(&record.run_id),
                escape(cause)
            );
        }
    }
    out.push_str("</section>");

    // the charts of several runs: the most recent that synced
    let completed: Vec<(&Record, &Synced, &Vec<Sample>)> = runs
        .iter()
        .filter_map(|(record, series)| Some((record, synced(record)?, series)))
        .collect();
    let skipped = completed.len().saturating_sub(CHART_RUNS);
    let charted: Vec<_> = completed.into_iter().skip(skipped).collect();
    if charted.is_empty() {
        out.push_str("<section><p>No run has completed a sync yet.</p></section>");
    } else {
        let note = match skipped {
            0 => String::new(),
            _ => format!(
                "<p class=\"note\">The {CHART_RUNS} most recent completed runs are drawn; the \
                 table above lists all.</p>"
            ),
        };
        let rows: Vec<BarRow> = charted
            .iter()
            .map(|(record, synced, _)| BarRow {
                label: label(record),
                segments: vec![(1, synced.sync_secs, "sync time".to_owned())],
                value: duration(synced.sync_secs),
            })
            .collect();
        put!(
            out,
            "<section><h2>Sync time</h2>{note}{}</section>",
            bar_chart("sync time per run", &rows)
        );

        let longest = charted
            .iter()
            .map(|(_, synced, _)| synced.sync_secs)
            .fold(0.0, f64::max);
        let (unit_name, unit) = time_unit(longest);
        let mut heights = Vec::new();
        let mut rates = Vec::new();
        for (number, (record, _, series)) in charted.iter().enumerate() {
            let slot = number + 1;
            let mut points = vec![(0.0, 0.0)];
            points.extend(
                series
                    .iter()
                    .filter_map(|sample| Some((sample.elapsed_secs, f64::from(sample.watermark?)))),
            );
            heights.push(Line {
                slot,
                name: label(record),
                points,
            });
            rates.push(Line {
                slot,
                name: label(record),
                points: bands(series)
                    .iter()
                    .filter_map(|band| {
                        Some((
                            band.first_height as f64 + band.blocks / 2.0,
                            band.blocks_per_sec().filter(|_| band.blocks > 0.0)?,
                        ))
                    })
                    .collect(),
            });
        }
        put!(
            out,
            "<section><h2>Height against time</h2>{note}{}</section>\
             <section><h2>Blocks per second against height</h2>{note}\
             <p class=\"note\">One point per band of {} blocks.</p>{}</section>",
            line_chart(
                "height against time",
                &format!("time since start ({unit_name})"),
                unit,
                "height",
                &heights
            ),
            count(crate::metrics::BAND_BLOCKS),
            line_chart(
                "blocks per second against height",
                "height",
                1.0,
                "blocks per second",
                &rates
            ),
        );
    }

    for (record, series) in runs.iter().rev() {
        let Some(synced) = synced(record) else {
            continue;
        };
        let run_bands = bands(series);
        let slowest = slowest_bands(&run_bands);
        put!(
            out,
            "<section><h2>{}</h2><p class=\"note\">{} · zainod {} · {} · {} cores · {} RAM · \
             {} · {}</p>\
             <h3>Stage time per band of the chain</h3>{}\
             <h3>Stage time per index</h3><p class=\"note\">{STAGE_NOTE}</p>{}\
             <div class=\"scroll\">{}</div>\
             <h3>Slowest bands (by time per block)</h3><div class=\"scroll\">{}</div>\
             <h3>Disk per index</h3><div class=\"scroll\">{}</div></section>",
            escape(&label(record)),
            escape(&record.run_id),
            escape(&zainod(record)),
            escape(&record.host.cpu),
            record.host.cores,
            bytes(record.host.ram_bytes),
            escape(&record.host.kernel),
            escape(&record.host.filesystem),
            band_chart(&run_bands),
            index_chart(&synced.stages),
            html_table(&stage_rows(&synced.stages)),
            html_table(&band_rows(&slowest)),
            html_table(&disk_rows(synced)),
        );
    }
    out.push_str("</body></html>\n");
    out
}

fn disk_rows(synced: &Synced) -> Vec<Vec<String>> {
    let mut rows = vec![["index", "disk"].map(str::to_owned).to_vec()];
    for (index, size) in &synced.disk_bytes {
        rows.push(vec![index.clone(), bytes(*size)]);
    }
    rows.push(vec!["total".to_owned(), bytes(synced.disk_total_bytes)]);
    rows
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::record::tests::synced as record;
    use crate::record::IndexStages;

    fn series() -> Vec<Sample> {
        [(10.0, 4_999u32, 4.0), (50.0, 24_999, 24.0)]
            .into_iter()
            .map(|(elapsed_secs, height, fold)| {
                let mut sample = Sample {
                    elapsed_secs,
                    watermark: Some(height),
                    target: Some(24_999),
                    heights: BTreeMap::from([("headers".to_owned(), height)]),
                    ..Sample::default()
                };
                sample.stages.indexes.insert(
                    "headers".to_owned(),
                    IndexStages {
                        fold: Timer {
                            secs: fold,
                            blocks: u64::from(height) + 1,
                        },
                        ..IndexStages::default()
                    },
                );
                sample
            })
            .collect()
    }

    fn failed() -> Record {
        Record {
            run_id: "run-9".to_owned(),
            ready_secs: None,
            outcome: Outcome::Failed {
                cause: "zainod exited <early> & loudly".to_owned(),
            },
            ..record("run-1", "full", 1.0, 1)
        }
    }

    #[test]
    fn figures_are_formatted_for_reading() {
        assert_eq!(duration(0.0), "0s");
        assert_eq!(duration(0.000_02), "<1ms");
        assert_eq!(duration(0.44), "440ms");
        assert_eq!(duration(2.54), "2.5s");
        assert_eq!(duration(45.2), "45s");
        assert_eq!(duration(185.0), "3m05s");
        assert_eq!(duration(8_040.0), "2h14m");
        assert_eq!(count(7), "7");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(100_000), "100,000");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(3 << 30), "3.0 GiB");
        assert_eq!(per_block(0.000_24), "240.0 µs");
        assert_eq!(per_block(0.012), "12.00 ms");
        assert_eq!(share(1.0, 4.0), "25.0%");
        assert_eq!(share(1.0, 0.0), "-");
        assert_eq!(date(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(date(1_795_644_425), "2026-11-25 22:07:05 UTC");
        assert_eq!(date(951_782_400), "2000-02-29 00:00:00 UTC");
    }

    #[test]
    fn axes_end_on_a_round_tick_above_the_data() {
        assert_eq!(axis(0.0), (1.0, 1.0));
        assert_eq!(axis(7.3), (8.0, 2.0));
        assert_eq!(axis(100.0), (100.0, 20.0));
        assert_eq!(axis(2_999_000.0), (3_000_000.0, 1_000_000.0));
        assert_eq!(tick(2_000_000.0), "2,000,000");
        assert_eq!(tick(0.5), "0.5");
        assert_eq!(time_unit(90.0), ("seconds", 1.0));
        assert_eq!(time_unit(4_000.0), ("minutes", 60.0));
        assert_eq!(time_unit(20_000.0), ("hours", 3_600.0));
    }

    #[test]
    fn the_stage_table_gives_each_stage_its_total_mean_and_share() {
        let record = record("run-1", "full", 120.0, 1_199);
        let rows = stage_rows(&synced(&record).expect("synced").stages);
        let table = text_table(&rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{table}");
        // fetch 40s + resolve 10s of 100s; headers' four stages are the other 50s
        assert!(lines.iter().any(|line| line.starts_with("(every index)")
            && line.contains("40s (40.00 ms)")
            && line.contains("10s (10.00 ms)")
            && line.ends_with("50.0%")));
        assert!(lines.iter().any(|line| line.starts_with("headers")
            && line.contains("30s (30.00 ms)")
            && line.contains("17s (17.00 ms)")
            && line.ends_with("50.0%")));
    }

    #[test]
    fn a_run_summary_reports_the_sync_its_stages_and_its_slowest_bands() {
        let summary = run_summary(&record("run-1", "full", 50.0, 24_999), &series());
        for expected in [
            "sync time: 50s",
            "time to ready: 1.5s",
            "height reached: 24,999",
            "peak memory: 2.0 GiB",
            "Stages run in parallel",
            "Slowest bands",
            "headers",
        ] {
            assert!(summary.contains(expected), "no `{expected}` in:\n{summary}");
        }
        let failed = run_summary(&failed(), &[]);
        assert!(failed.contains("FAILED: zainod exited"), "{failed}");
        assert!(failed.contains("time to ready: never"), "{failed}");
    }

    #[test]
    fn the_slowest_bands_are_ordered_by_time_per_block() {
        let band = |first_height, blocks, wall_secs| Band {
            first_height,
            blocks,
            wall_secs,
            stages: StageSecs::default(),
            slowest: None,
        };
        let bands = [
            band(0, 10_000.0, 10.0),
            band(10_000, 10_000.0, 40.0),
            band(20_000, 0.0, 99.0),
            band(30_000, 5_000.0, 15.0),
        ];
        let slowest: Vec<u64> = slowest_bands(&bands)
            .iter()
            .map(|band| band.first_height)
            .collect();
        assert_eq!(slowest, [10_000, 30_000, 0]);
    }

    #[test]
    fn the_table_of_runs_lists_every_run_and_each_failure() {
        assert_eq!(runs_table(&[]), "No runs are recorded yet.\n");
        let table = runs_table(&[record("run-1", "lightclient", 4_320.0, 2_998_999), failed()]);
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines.iter().any(|line| line.starts_with("run-1")
            && line.contains("lightclient")
            && line.contains("7c798f426")
            && line.contains("1h12m")
            && line.contains("2,998,999")));
        assert!(lines
            .iter()
            .any(|line| line.starts_with("run-9") && line.contains("FAILED")));
        assert!(table.contains("run-9 failed: zainod exited <early> & loudly"));
    }

    #[test]
    fn the_html_report_holds_every_chart_and_escapes_what_it_shows() {
        let runs = [
            (record("run-1", "lightclient", 50.0, 24_999), series()),
            (record("run-2", "full", 80.0, 24_999), series()),
            (failed(), Vec::new()),
        ];
        let html = html(&runs);
        for expected in [
            "aria-label=\"sync time per run\"",
            "aria-label=\"height against time\"",
            "aria-label=\"blocks per second against height\"",
            "aria-label=\"stage time per band of the chain\"",
            "aria-label=\"stage time per index\"",
            "run-9 failed",
            "zainod exited &lt;early&gt; &amp; loudly",
            "Disk per index",
        ] {
            assert!(html.contains(expected), "no `{expected}`");
        }
        assert!(!html.contains("<early>"));
        assert!(!html.contains("NaN") && !html.contains("inf"));
        // two completed runs: two lines in each line chart, one section each
        assert_eq!(html.matches("class=\"l1\"").count(), 2);
        assert_eq!(html.matches("class=\"l2\"").count(), 2);
        assert_eq!(html.matches("<h3>Stage time per index</h3>").count(), 2);
        assert!(super::html(&[]).contains("No run has completed a sync yet."));
    }

    #[test]
    fn only_the_most_recent_runs_are_charted() {
        let runs: Vec<Run> = (0..10)
            .map(|number| {
                (
                    record(&format!("run-{number}"), "full", 50.0, 24_999),
                    series(),
                )
            })
            .collect();
        let html = html(&runs);
        assert!(html.contains("most recent completed runs are drawn"));
        assert_eq!(html.matches("class=\"l8\"").count(), 2);
        assert!(!html.contains("class=\"l9\""));
        assert_eq!(html.matches("<td>run-").count(), 10);
    }
}
