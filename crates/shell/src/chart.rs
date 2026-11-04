//! Native range plots cache pixels, labels and geometry before painting.
//!
//! One bounded texture avoids rebuilding paths or allocating vertices on each
//! settled paint. The existing background executor prepares one texture at a
//! time and coalesces replacements. The inspector shares the same samples and
//! time window, with its own resolution and explicit point cap.

use std::borrow::Cow;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    App, AppContext as _, Bounds, Context, IntoElement, MouseMoveEvent, ParentElement, Pixels,
    Render, RenderImage, SharedString, Styled, Subscription, Task, Window, canvas, div, prelude::*,
    px, rgb,
};
use k10s_provider::{FetchTiming, PromSeriesView, QueryWindow};

use crate::notice::{NoticeSnapshot, NoticeSource};

pub const MAX_DRAWN_SERIES: usize = 16;
pub const MAX_TEXTURE_WIDTH: u32 = 2048;
pub const MAX_TEXTURE_HEIGHT: u32 = 512;
pub const MAX_LABEL_BYTES: usize = 512;
pub const MAX_DISPLAY_NOTICES: usize = 8;
pub const MAX_NOTICE_BYTES: usize = 512;
pub const MAX_NOTICE_TEXT_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    pub series: Vec<PromSeriesView>,
    pub window: QueryWindow,
    pub title: String,
    pub unit: String,
    pub source: String,
    pub expr: String,
    pub reducer: Option<String>,
    pub notices: Vec<String>,
    pub timing: FetchTiming,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Preparation {
    pub raw_points: usize,
    pub painted_points: usize,
    pub texture_bytes: usize,
    pub prepare_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextureKey {
    generation: u64,
    width: u32,
    height: u32,
    palette: [u32; 5],
}

#[derive(Default)]
struct PreparationState {
    wanted: Option<TextureKey>,
    active: Option<TextureKey>,
    prepared: Option<TextureKey>,
}

impl PreparationState {
    fn invalidate(&mut self) {
        self.wanted = None;
        self.prepared = None;
    }

    fn ready(&self) -> bool {
        self.prepared.is_some() && self.prepared == self.wanted
    }

    fn begin(&mut self) -> Option<TextureKey> {
        if self.active.is_some() || self.ready() {
            return None;
        }
        self.active = self.wanted;
        self.active
    }

    fn finish(&mut self, key: TextureKey, complete: bool) -> bool {
        if self.active != Some(key) {
            return false;
        }
        self.active = None;
        if !complete || self.wanted != Some(key) {
            return false;
        }
        self.prepared = Some(key);
        true
    }
}

#[derive(Default)]
struct RasterStorage {
    pixels: Vec<u8>,
    plot: k10s_theme::Plot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RasterError {
    series: usize,
    why: &'static str,
}

impl RasterError {
    fn preview(self, data: &ChartData) -> String {
        format!(
            "{}: {}",
            display_label(&data.series[self.series].labels),
            self.why
        )
    }
}

pub struct ChartView {
    data: Option<Arc<ChartData>>,
    compact: bool,
    title: SharedString,
    source: SharedString,
    axis: SharedString,
    notice: SharedString,
    local_notices: Vec<String>,
    raster_errors: Vec<RasterError>,
    paint_error: Option<SharedString>,
    legend: Vec<SharedString>,
    stats: Vec<SharedString>,
    image: Option<Arc<RenderImage>>,
    generation: u64,
    state: PreparationState,
    storage: RasterStorage,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    bounds: Bounds<Pixels>,
    hover_slot: Option<u64>,
    hover: Vec<SharedString>,
    preparation: Preparation,
    release: Option<Subscription>,
}

impl ChartView {
    pub fn new(compact: bool) -> Self {
        Self {
            data: None,
            compact,
            title: SharedString::default(),
            source: SharedString::default(),
            axis: SharedString::default(),
            notice: SharedString::default(),
            local_notices: Vec::new(),
            raster_errors: Vec::new(),
            paint_error: None,
            legend: Vec::new(),
            stats: Vec::new(),
            image: None,
            generation: 0,
            state: PreparationState::default(),
            storage: RasterStorage::default(),
            cancel: None,
            task: None,
            bounds: Bounds::default(),
            hover_slot: None,
            hover: Vec::new(),
            preparation: Preparation::default(),
            release: None,
        }
    }

    pub fn set_data(&mut self, data: Option<Arc<ChartData>>, cx: &mut Context<Self>) {
        if same_data(self.data.as_ref(), data.as_ref()) {
            return;
        }
        self.generation += 1;
        self.state.invalidate();
        self.cancel_preparation();
        self.preparation = Preparation::default();
        self.paint_error = None;
        self.hover_slot = None;
        self.hover.clear();
        self.legend.clear();
        self.stats.clear();
        self.local_notices.clear();
        self.raster_errors.clear();
        if let Some(data) = &data {
            let title = display_label(&data.title);
            self.title = if let Some(reducer) = &data.reducer {
                format!("{title} ({})", display_label(reducer)).into()
            } else {
                title.into()
            };
            self.source = display_label(&data.source).into();
            let unit = display_label(unit_name(&data.unit));
            let values = value_range(&data.series).map_or_else(
                || "No finite values".to_string(),
                |(min, max)| {
                    format!(
                        "{} to {} {}",
                        display_value(min, &data.unit),
                        display_value(max, &data.unit),
                        unit
                    )
                },
            );
            let gaps: usize = data.series.iter().map(|series| series.gaps.len()).sum();
            self.axis = format!(
                "{values}  |  {} to {} UTC  |  {} ms resolution",
                clock(data.window.start_ms),
                clock(data.window.end_ms),
                data.window.step_ms
            )
            .into();
            let notices = &mut self.local_notices;
            if data.series.is_empty() {
                notices.push("No series in this range".to_string());
            } else if data.series.iter().all(|series| series.points.is_empty()) {
                notices.push("No finite samples in this range".to_string());
            }
            if gaps > 0 {
                notices.push(format!("{gaps} non-finite samples remain gaps"));
            }
            if data.series.len() > MAX_DRAWN_SERIES {
                notices.push(format!(
                    "Showing {MAX_DRAWN_SERIES} of {} returned series",
                    data.series.len()
                ));
            }
            if data
                .series
                .iter()
                .take(MAX_DRAWN_SERIES)
                .any(|series| series.labels.len() > MAX_LABEL_BYTES)
            {
                notices.push("Some series labels are abbreviated".into());
            }
            self.notice = display_notice_groups(&[notices, &data.notices]).into();
            for series in data.series.iter().take(MAX_DRAWN_SERIES) {
                self.legend.push(display_label(&series.labels).into());
                if let Some(reducer) = &data.reducer {
                    self.stats.push(
                        match reduce(&series.points, reducer) {
                            Ok(Some(value)) => {
                                format!("{} {}", display_value(value, &data.unit), unit)
                            }
                            Ok(None) => "No finite value".to_string(),
                            Err(why) => why.to_string(),
                        }
                        .into(),
                    );
                }
            }
        } else {
            self.title = SharedString::default();
            self.source = SharedString::default();
            self.axis = SharedString::default();
            self.notice = SharedString::default();
        }
        self.data = data;
        cx.notify();
    }

    pub fn data(&self) -> Option<&Arc<ChartData>> {
        self.data.as_ref()
    }

    pub(crate) fn notice_details(&self) -> Option<NoticeSnapshot> {
        self.data.as_ref().map(|data| {
            notice_details(
                data.clone(),
                &self.raster_errors,
                &self.local_notices,
                self.paint_error.clone(),
            )
        })
    }

    pub fn preparation(&self) -> Preparation {
        self.preparation
    }

    pub(crate) fn pause_preparation(&mut self) {
        self.state.wanted = None;
        self.cancel_preparation();
    }

    fn cancel_preparation(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn start_preparation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.state.begin() else {
            return;
        };
        let data = self.data.clone().expect("a texture request has chart data");
        let mut storage = std::mem::take(&mut self.storage);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let raster = cx.background_spawn(async move {
            let started = std::time::Instant::now();
            let (mut preparation, errors) = render_pixels(
                &data,
                key.width,
                key.height,
                key.palette,
                &mut storage,
                Some(&cancel),
            );
            let complete = !cancel.load(Ordering::Relaxed);
            let previews: Vec<String> = if complete {
                errors.iter().map(|error| error.preview(&data)).collect()
            } else {
                Vec::new()
            };
            preparation.prepare_ns = started.elapsed().as_nanos() as u64;
            (storage, preparation, errors, previews, complete)
        });
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let (mut storage, preparation, errors, previews, complete) = raster.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.task = None;
                this.cancel = None;
                let publish = this.state.finish(key, complete);
                if publish {
                    let pixels = std::mem::take(&mut storage.pixels);
                    let buffer = image::RgbaImage::from_raw(key.width, key.height, pixels)
                        .expect("bounded chart buffer dimensions");
                    if let Some(image) = this.image.take() {
                        let _ = window.drop_image(image);
                    }
                    this.image = Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])));
                    this.preparation = preparation;
                    let data = this
                        .data
                        .as_ref()
                        .expect("a prepared texture has chart data");
                    this.notice =
                        display_notice_groups(&[&previews, &this.local_notices, &data.notices])
                            .into();
                    this.raster_errors = errors;
                    cx.notify();
                }
                this.storage = storage;
                this.start_preparation(window, cx);
            });
        }));
    }

    fn prepare(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        if self.release.is_none() {
            self.release = Some(cx.on_release_in(window, |this, window, _: &mut App| {
                if let Some(image) = this.image.take() {
                    let _ = window.drop_image(image);
                }
            }));
        }
        self.bounds = bounds;
        if self.data.is_none() {
            if let Some(image) = self.image.take() {
                let _ = window.drop_image(image);
            }
            return None;
        }
        let theme = k10s_theme::active(cx);
        let palette = [
            theme.shell.text_accent,
            theme.shell.success,
            theme.shell.warning,
            theme.shell.error,
            theme.shell.border_variant,
        ];
        let scale = window.scale_factor();
        let width = (f32::from(bounds.size.width) * scale)
            .ceil()
            .clamp(1.0, MAX_TEXTURE_WIDTH as f32) as u32;
        let height = (f32::from(bounds.size.height) * scale)
            .ceil()
            .clamp(1.0, MAX_TEXTURE_HEIGHT as f32) as u32;
        let key = TextureKey {
            generation: self.generation,
            width,
            height,
            palette,
        };
        if self.state.wanted != Some(key) {
            self.state.wanted = Some(key);
            if self.state.active != Some(key) {
                self.cancel_preparation();
            }
            cx.notify();
        }
        self.start_preparation(window, cx);
        self.state.ready().then(|| self.image.clone()).flatten()
    }

    fn hover_sample(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.bounds.contains(&event.position) || !self.state.ready() {
            self.clear_hover(cx);
            return;
        }
        let Some(data) = &self.data else {
            return;
        };
        if data.window.validate().is_err() {
            self.clear_hover(cx);
            return;
        }
        let fraction = (f32::from(event.position.x - self.bounds.origin.x)
            / f32::from(self.bounds.size.width).max(1.0))
        .clamp(0.0, 1.0);
        let span = (data.window.end_ms - data.window.start_ms) as u64;
        let elapsed = (span as f64 * fraction as f64).round() as u64;
        let slot = elapsed.saturating_add(data.window.step_ms / 2) / data.window.step_ms;
        if self.hover_slot == Some(slot) {
            return;
        }
        self.hover_slot = Some(slot);
        let time_ms =
            data.window.start_ms + slot.saturating_mul(data.window.step_ms).min(span) as i64;
        self.hover = hover_lines(data, time_ms)
            .into_iter()
            .map(Into::into)
            .collect();
        cx.notify();
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hover_slot.take().is_some() {
            self.hover.clear();
            cx.notify();
        }
    }
}

struct ChartNotices {
    data: Arc<ChartData>,
    raster_errors: Vec<RasterError>,
    local_notices: Vec<String>,
    paint_error: Option<SharedString>,
    range: String,
}

impl NoticeSource for ChartNotices {
    fn notice_count(&self) -> usize {
        5 + self.raster_errors.len()
            + self.local_notices.len()
            + self.data.notices.len()
            + usize::from(self.data.reducer.is_some())
            + usize::from(self.paint_error.is_some())
    }

    fn notice_at(&self, index: usize) -> Option<(&str, &str)> {
        match index {
            0 => return Some(("Chart", &self.data.title)),
            1 => return Some(("Source", &self.data.source)),
            2 => return Some(("PromQL", &self.data.expr)),
            3 => return Some(("Range", &self.range)),
            4 => return Some(("Unit", &self.data.unit)),
            _ => {}
        }
        let index = index - 5;
        let index = match &self.data.reducer {
            Some(reducer) if index == 0 => return Some(("Reducer", reducer)),
            Some(_) => index - 1,
            None => index,
        };
        if let Some(error) = self.raster_errors.get(index) {
            return Some((error.why, &self.data.series[error.series].labels));
        }
        let index = index - self.raster_errors.len();
        if let Some(notice) = self.local_notices.get(index) {
            return Some(("", notice));
        }
        let index = index - self.local_notices.len();
        if let Some(notice) = self.data.notices.get(index) {
            return Some(("", notice));
        }
        let index = index - self.data.notices.len();
        if index == 0 {
            self.paint_error.as_deref().map(|error| ("", error))
        } else {
            None
        }
    }
}

fn notice_details(
    data: Arc<ChartData>,
    raster_errors: &[RasterError],
    local_notices: &[String],
    paint_error: Option<SharedString>,
) -> NoticeSnapshot {
    let range = format!(
        "{} to {} UTC; resolution {} ms",
        clock(data.window.start_ms),
        clock(data.window.end_ms),
        data.window.step_ms,
    );
    NoticeSnapshot::new(ChartNotices {
        data,
        raster_errors: raster_errors.to_vec(),
        local_notices: local_notices.to_vec(),
        paint_error,
        range,
    })
}

impl Drop for ChartView {
    fn drop(&mut self) {
        self.cancel_preparation();
    }
}

fn same_data(current: Option<&Arc<ChartData>>, next: Option<&Arc<ChartData>>) -> bool {
    match (current, next) {
        (Some(current), Some(next)) => Arc::ptr_eq(current, next),
        (None, None) => true,
        _ => false,
    }
}

impl Render for ChartView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let this = cx.entity();
        let painter = this.clone();
        let compact = self.compact;
        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(4.0))
            .text_size(px(fonts.ui_size))
            .text_color(rgb(theme.shell.text))
            .child(div().child(self.title.clone()))
            .child(div().overflow_hidden().child(self.source.clone()))
            .children((!compact).then(|| {
                div().children(
                    self.stats
                        .iter()
                        .cloned()
                        .map(|stat| div().text_size(px(fonts.ui_size * 1.5)).child(stat)),
                )
            }))
            .child(
                div()
                    .id("chart-plot")
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        this.hover_sample(event, cx);
                    }))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !hovered {
                            this.clear_hover(cx);
                        }
                    }))
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                this.update(cx, |this, cx| this.prepare(bounds, window, cx))
                            },
                            move |bounds, image, window, cx| {
                                if let Some(image) = image {
                                    match window.paint_image(
                                        bounds,
                                        Default::default(),
                                        image,
                                        0,
                                        false,
                                    ) {
                                        Ok(()) => {
                                            if painter.read(cx).paint_error.is_some() {
                                                painter.update(cx, |this, cx| {
                                                    this.paint_error = None;
                                                    cx.notify();
                                                });
                                            }
                                        }
                                        Err(error) => {
                                            let message: SharedString =
                                                format!("Chart image could not be drawn: {error}")
                                                    .into();
                                            painter.update(cx, |this, cx| {
                                                if this.paint_error.as_ref() != Some(&message) {
                                                    this.paint_error = Some(message);
                                                    cx.notify();
                                                }
                                            });
                                        }
                                    }
                                }
                            },
                        )
                        .w_full()
                        .h(px(if compact { 64.0 } else { 200.0 })),
                    ),
            )
            .children(
                (self.data.is_some() && !self.state.ready())
                    .then(|| div().child("Preparing chart")),
            )
            .child(div().child(self.axis.clone()))
            .children((!self.notice.is_empty()).then(|| div().child(self.notice.clone())))
            .children(self.paint_error.clone().map(|error| div().child(error)))
            .children(
                self.hover
                    .iter()
                    .cloned()
                    .map(|line| div().overflow_hidden().child(line)),
            )
            .children((!compact).then(|| {
                div().children(
                    self.legend
                        .iter()
                        .cloned()
                        .map(|legend| div().overflow_hidden().child(legend)),
                )
            }))
    }
}

pub fn unit_name(unit: &str) -> &str {
    match unit {
        "s" => "s",
        "reqps" => "requests/s",
        "bytes" => "B",
        "percentunit" => "%",
        "short" | "none" => "",
        "" => "(unit not declared)",
        other => other,
    }
}

pub fn display_value(value: f64, unit: &str) -> String {
    let value = if unit == "percentunit" {
        value * 100.0
    } else {
        value
    };
    if value.abs() >= 1e6 || value != 0.0 && value.abs() < 0.001 {
        format!("{value:.3e}")
    } else {
        format!("{value:.3}")
    }
}

pub fn clock(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds).map_or_else(
        || format!("{milliseconds} ms since Unix epoch"),
        |time| time.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    )
}

pub fn reduce(points: &[(i64, f64)], reducer: &str) -> Result<Option<f64>, &'static str> {
    if !matches!(reducer, "lastNotNull" | "min" | "max" | "mean" | "sum") {
        return Err("Unsupported stat reducer");
    }
    let mut values = points
        .iter()
        .map(|(_, value)| *value)
        .filter(|value| value.is_finite());
    let Some(first) = values.next() else {
        return Ok(None);
    };
    let result = match reducer {
        "lastNotNull" => values.next_back().unwrap_or(first),
        "min" => values.fold(first, f64::min),
        "max" => values.fold(first, f64::max),
        "mean" => {
            let scale = points
                .iter()
                .map(|(_, value)| value.abs())
                .filter(|value| value.is_finite())
                .fold(1.0, f64::max);
            let count = points.iter().filter(|(_, value)| value.is_finite()).count() as f64;
            (first / scale + values.map(|value| value / scale).sum::<f64>()) / count * scale
        }
        "sum" => first + values.sum::<f64>(),
        _ => unreachable!(),
    };
    if result.is_finite() {
        Ok(Some(result))
    } else {
        Err("Stat result is not finite")
    }
}

fn value_range(series: &[PromSeriesView]) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for (_, value) in series
        .iter()
        .take(MAX_DRAWN_SERIES)
        .flat_map(|series| &series.points)
    {
        if value.is_finite() {
            min = min.min(*value);
            max = max.max(*value);
        }
    }
    min.is_finite().then_some((min, max))
}

/// BGRA is GPUI's texture contract; no PNG encoding or browser renderer runs.
pub fn raster(
    data: &ChartData,
    width: u32,
    height: u32,
    palette: [u32; 5],
) -> (Vec<u8>, Preparation, Vec<String>) {
    let started = std::time::Instant::now();
    let mut storage = RasterStorage::default();
    let (mut preparation, errors) = render_pixels(data, width, height, palette, &mut storage, None);
    let errors = errors
        .into_iter()
        .map(|error| error.preview(data))
        .collect();
    preparation.prepare_ns = started.elapsed().as_nanos() as u64;
    (storage.pixels, preparation, errors)
}

fn render_pixels(
    data: &ChartData,
    width: u32,
    height: u32,
    palette: [u32; 5],
    storage: &mut RasterStorage,
    cancel: Option<&AtomicBool>,
) -> (Preparation, Vec<RasterError>) {
    assert!(width > 0 && width <= MAX_TEXTURE_WIDTH && height > 0 && height <= MAX_TEXTURE_HEIGHT);
    let cancelled = || cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed));
    if cancelled() {
        return (Preparation::default(), Vec::new());
    }
    storage
        .pixels
        .resize(width as usize * height as usize * 4, 0);
    let pixels = &mut storage.pixels;
    pixels.fill(0);
    let mut preparation = Preparation {
        texture_bytes: pixels.len(),
        ..Default::default()
    };
    let mut errors = Vec::new();
    let values = value_range(&data.series).unwrap_or((0.0, 1.0));
    for quarter in 1..4 {
        line(
            pixels,
            width,
            height,
            (0, height as i32 * quarter / 4),
            (width as i32 - 1, height as i32 * quarter / 4),
            palette[4],
        );
    }
    for (index, series) in data.series.iter().take(MAX_DRAWN_SERIES).enumerate() {
        if cancelled() {
            break;
        }
        preparation.raw_points += series.points.len();
        if let Err(why) = storage.plot.prepare(
            &series.points,
            &series.gaps,
            data.window.start_ms..data.window.end_ms,
            data.window.step_ms,
            values.0..values.1,
            width as usize * 2,
        ) {
            errors.push(RasterError { series: index, why });
            continue;
        }
        let plot = &storage.plot;
        preparation.painted_points += plot.points.len();
        let xy = |point: k10s_theme::Point| {
            (
                (point.x * (width - 1) as f32).round() as i32,
                ((1.0 - point.y) * (height - 1) as f32).round() as i32,
            )
        };
        for run in &plot.runs {
            let points = &plot.points[run.clone()];
            if points.len() == 1 {
                let (x, y) = xy(points[0]);
                pixel(pixels, width, height, x, y, palette[index % 4]);
            }
            for pair in points.windows(2) {
                if cancelled() {
                    return (preparation, errors);
                }
                line(
                    pixels,
                    width,
                    height,
                    xy(pair[0]),
                    xy(pair[1]),
                    palette[index % 4],
                );
            }
        }
    }
    (preparation, errors)
}

fn nearest_sample(series: &PromSeriesView, time_ms: i64, step_ms: u64) -> Option<(i64, f64)> {
    let index = series.points.partition_point(|(time, _)| *time < time_ms);
    let before = index
        .checked_sub(1)
        .and_then(|index| series.points.get(index));
    let point = before
        .into_iter()
        .chain(series.points.get(index))
        .min_by_key(|(time, _)| time.abs_diff(time_ms))?;
    let distance = point.0.abs_diff(time_ms);
    if distance > step_ms / 2 {
        return None;
    }
    let gap = series.gaps.partition_point(|time| *time < time_ms);
    let before = gap.checked_sub(1).and_then(|index| series.gaps.get(index));
    if before
        .into_iter()
        .chain(series.gaps.get(gap))
        .any(|gap| gap.abs_diff(time_ms) <= distance)
    {
        return None;
    }
    point.1.is_finite().then_some(*point)
}

fn hover_lines(data: &ChartData, time_ms: i64) -> Vec<String> {
    let unit = display_label(unit_name(&data.unit));
    data.series
        .iter()
        .take(MAX_DRAWN_SERIES)
        .map(
            |series| match nearest_sample(series, time_ms, data.window.step_ms) {
                Some((time, value)) => format!(
                    "{} {} at {} UTC; {}",
                    display_value(value, &data.unit),
                    unit,
                    clock(time),
                    display_label(&series.labels),
                ),
                None => format!(
                    "No finite sample near {} UTC; {}",
                    clock(time_ms),
                    display_label(&series.labels),
                ),
            },
        )
        .collect()
}

fn display_label(label: &str) -> Cow<'_, str> {
    if label.len() <= MAX_LABEL_BYTES {
        return Cow::Borrowed(label);
    }
    let mut end = MAX_LABEL_BYTES;
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}...", &label[..end]))
}

pub(crate) fn display_notice_groups(groups: &[&[String]]) -> String {
    use std::fmt::Write as _;
    const SUMMARY_RESERVE: usize = 128;
    let total: usize = groups.iter().map(|group| group.len()).sum();
    let mut text = String::new();
    let mut shown = 0;
    for notice in groups
        .iter()
        .flat_map(|group| group.iter())
        .take(MAX_DISPLAY_NOTICES)
    {
        if shown > 0 {
            text.push_str("; ");
        }
        let available = MAX_NOTICE_TEXT_BYTES - SUMMARY_RESERVE - text.len();
        append_notice(&mut text, notice, available.min(MAX_NOTICE_BYTES));
        shown += 1;
    }
    if total > shown {
        if shown > 0 {
            text.push_str("; ");
        }
        write!(text, "{} notices omitted", total - shown).expect("formatting into a String");
    }
    text
}

pub(crate) fn display_message(message: &str) -> String {
    let mut text = String::new();
    append_notice(&mut text, message, MAX_NOTICE_BYTES);
    text
}

fn append_notice(text: &mut String, message: &str, limit: usize) {
    const ABBREVIATED: &str = " [abbreviated]";
    let message = if message.is_empty() {
        "Empty notice"
    } else {
        message
    };
    if message.len() <= limit {
        text.push_str(message);
        return;
    }
    let mut end = limit - ABBREVIATED.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    text.push_str(&message[..end]);
    text.push_str(ABBREVIATED);
}

fn pixel(bytes: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let offset = (y as usize * width as usize + x as usize) * 4;
    bytes[offset..offset + 4].copy_from_slice(&[
        color as u8,
        (color >> 8) as u8,
        (color >> 16) as u8,
        255,
    ]);
}

fn line(bytes: &mut [u8], width: u32, height: u32, from: (i32, i32), to: (i32, i32), color: u32) {
    let (mut x, mut y) = from;
    let dx = (to.0 - x).abs();
    let sx = if x < to.0 { 1 } else { -1 };
    let dy = -(to.1 - y).abs();
    let sy = if y < to.1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        pixel(bytes, width, height, x, y, color);
        if x == to.0 && y == to.1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += sx;
        }
        if doubled <= dx {
            error += dx;
            y += sy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(series: Vec<PromSeriesView>, end_ms: i64, step_ms: u64) -> ChartData {
        ChartData {
            series,
            window: QueryWindow {
                start_ms: 0,
                end_ms,
                step_ms,
            },
            title: "Test samples".into(),
            unit: "none".into(),
            source: "test source".into(),
            expr: "test_metric".into(),
            reducer: None,
            notices: vec!["Source warning".into()],
            timing: FetchTiming::default(),
        }
    }

    fn series(points: Vec<(i64, f64)>, gaps: Vec<i64>) -> PromSeriesView {
        PromSeriesView {
            labels: "pod=test".into(),
            label_set: vec![("pod".into(), "test".into())],
            points,
            gaps,
        }
    }

    fn key(generation: u64, width: u32) -> TextureKey {
        TextureKey {
            generation,
            width,
            height: 20,
            palette: [1, 2, 3, 4, 5],
        }
    }

    #[test]
    fn preparation_admits_one_job_and_publishes_only_the_latest_identity() {
        let mut state = PreparationState {
            wanted: Some(key(0, 100)),
            ..Default::default()
        };
        assert_eq!(state.begin(), Some(key(0, 100)));
        for width in 101..500 {
            state.wanted = Some(key(0, width));
            assert_eq!(state.begin(), None);
        }
        state.invalidate();
        state.wanted = Some(key(1, 320));
        assert!(!state.finish(key(0, 100), true));
        assert!(!state.ready());
        assert_eq!(state.begin(), Some(key(1, 320)));
        assert!(state.finish(key(1, 320), true));
        assert!(state.ready());
        assert_eq!(state.begin(), None);
        state.invalidate();
        assert!(!state.ready());
        assert_eq!(state.begin(), None);
    }

    #[test]
    fn cancelled_geometry_cannot_settle_even_when_the_original_size_is_restored() {
        let mut state = PreparationState {
            wanted: Some(key(0, 100)),
            ..Default::default()
        };
        assert_eq!(state.begin(), Some(key(0, 100)));
        state.wanted = Some(key(0, 120));
        state.wanted = Some(key(0, 100));
        assert!(!state.finish(key(0, 100), false));
        assert!(!state.ready());
        assert_eq!(state.begin(), Some(key(0, 100)));
    }

    #[test]
    fn shared_samples_keep_their_identity_without_comparing_or_copying_the_matrix() {
        let data = Arc::new(data(vec![series(vec![(0, 1.0)], vec![])], 10, 10));
        let inspector = data.clone();
        assert!(same_data(Some(&data), Some(&inspector)));
        assert!(!same_data(Some(&data), Some(&Arc::new((*data).clone()))));
        assert!(same_data(None, None));
        assert!(!same_data(Some(&data), None));
    }

    #[test]
    fn raster_preserves_the_gap_and_reuses_geometry_and_pixel_storage() {
        let data = data(
            vec![series(
                vec![(0, 1.0), (10, 1.0), (30, 1.0), (40, 1.0)],
                vec![20],
            )],
            40,
            10,
        );
        let mut storage = RasterStorage::default();
        let (preparation, errors) =
            render_pixels(&data, 41, 20, [1, 2, 3, 4, 5], &mut storage, None);
        assert!(errors.is_empty());
        assert_eq!(preparation.raw_points, 4);
        assert_eq!(preparation.painted_points, 4);
        assert_eq!(&storage.pixels[(10 * 41 + 5) * 4..][..4], &[1, 0, 0, 255]);
        assert_eq!(&storage.pixels[(10 * 41 + 20) * 4..][..4], &[5, 0, 0, 255]);
        assert_eq!(&storage.pixels[(10 * 41 + 35) * 4..][..4], &[1, 0, 0, 255]);
        let pixels = storage.pixels.as_ptr();
        let points = storage.plot.points.as_ptr();
        let runs = storage.plot.runs.as_ptr();
        let (_, errors) = render_pixels(&data, 41, 20, [2, 3, 4, 5, 1], &mut storage, None);
        assert!(errors.is_empty());
        assert_eq!(storage.pixels.as_ptr(), pixels);
        assert_eq!(storage.plot.points.as_ptr(), points);
        assert_eq!(storage.plot.runs.as_ptr(), runs);
    }

    #[test]
    fn hover_names_actual_samples_and_keeps_missing_values_distinct_from_zero() {
        let data = data(
            vec![series(vec![(0, 0.0), (10, 2.0), (30, 4.0)], vec![20])],
            40,
            10,
        );
        assert_eq!(nearest_sample(&data.series[0], 0, 10), Some((0, 0.0)));
        assert_eq!(nearest_sample(&data.series[0], 12, 10), Some((10, 2.0)));
        assert_eq!(nearest_sample(&data.series[0], 20, 10), None);
        assert_eq!(nearest_sample(&data.series[0], 40, 10), None);
        assert!(hover_lines(&data, 12)[0].contains("2.000  at 1970-01-01 00:00:00.010 UTC"));
        assert!(hover_lines(&data, 20)[0].contains("No finite sample"));
    }

    #[test]
    fn display_abbreviation_preserves_utf8_and_keeps_full_identity_labels() {
        let label = "界".repeat(1000);
        let shown = display_label(&label);
        assert!(shown.ends_with("..."));
        assert!(shown.len() <= MAX_LABEL_BYTES + 3);
        assert_eq!(display_label("short label"), "short label");
        let mut series = series(vec![(0, 1.0)], Vec::new());
        series.labels = label.clone();
        let data = data(vec![series], 10, 10);
        assert!(hover_lines(&data, 0)[0].starts_with("1.000  at 1970-01-01"));
        assert_eq!(data.series[0].labels, label);
        assert_eq!(data.series[0].label_set[0], ("pod".into(), "test".into()));
    }

    #[test]
    fn chart_details_borrow_full_metadata_after_bounded_previews() {
        let mut chart = data(Vec::new(), 1_000, 500);
        chart.title = format!("Panel {}", "é".repeat(600));
        chart.source = format!("Source {}", "é".repeat(600));
        chart.unit = format!("Unit {}", "é".repeat(600));
        chart.reducer = Some(format!("Reducer {}", "é".repeat(600)));
        let chart = Arc::new(chart);
        let details = notice_details(chart.clone(), &[], &[], None);
        for (index, original) in [
            (0, &chart.title),
            (1, &chart.source),
            (4, &chart.unit),
            (5, chart.reducer.as_ref().unwrap()),
        ] {
            assert!(display_label(original).ends_with("..."));
            assert_eq!(details.get(index).unwrap().1, original.as_str());
            assert_eq!(details.get(index).unwrap().1.as_ptr(), original.as_ptr());
        }
        assert_eq!(details.get(4).unwrap().0, "Unit");
        assert_eq!(details.get(5).unwrap().0, "Reducer");
    }

    #[test]
    fn hover_abbreviates_unknown_units_without_changing_the_data() {
        let mut chart = data(vec![series(vec![(0, 1.0)], Vec::new())], 1_000, 500);
        chart.unit = format!("unit=\"{}\"", "é".repeat(600));
        let full_unit = chart.unit.clone();
        let hover = hover_lines(&chart, 0);
        assert!(hover[0].contains("... at "));
        assert!(hover[0].len() < full_unit.len());
        assert_eq!(chart.unit, full_unit);
    }

    #[test]
    fn notice_display_bounds_utf8_count_and_total_bytes_without_copying_raw_data() {
        let mut data = data(vec![series(vec![(0, 1.0)], Vec::new())], 10, 10);
        data.notices = vec!["界".repeat(100_000); 20];
        let raw = data.notices[0].as_ptr();
        let shown = display_notice_groups(&[&data.notices]);
        assert!(shown.is_char_boundary(shown.len()));
        assert!(shown.len() <= MAX_NOTICE_TEXT_BYTES);
        assert_eq!(shown.matches("[abbreviated]").count(), MAX_DISPLAY_NOTICES);
        assert!(shown.ends_with("12 notices omitted"));
        assert_eq!(data.notices.len(), 20);
        assert_eq!(data.notices[0].len(), 300_000);
        assert_eq!(data.notices[0].as_ptr(), raw);
        let short = vec!["first warning".into(), "second warning".into()];
        assert_eq!(
            display_notice_groups(&[&short]),
            "first warning; second warning"
        );
        assert_eq!(display_message(""), "Empty notice");
        assert!(display_message(&"界".repeat(1000)).len() <= MAX_NOTICE_BYTES);
    }

    #[test]
    fn details_keep_query_identity_and_every_full_notice_in_display_order() {
        let mut labelled = series(vec![(2_000, 1.0)], Vec::new());
        labelled.labels = format!("service=\"{}\"", "é".repeat(1_000));
        let mut chart = data(vec![labelled], 1_000, 500);
        chart.notices = (0..12).map(|index| format!("warning {index}")).collect();
        chart
            .notices
            .push(format!("Scrape warning: {}", "é".repeat(1_000)));
        let mut storage = RasterStorage::default();
        let (_, raster) = render_pixels(&chart, 41, 20, [1, 2, 3, 4, 5], &mut storage, None);
        assert_eq!(raster.len(), 1);
        assert_eq!(raster[0].series, 0);
        assert_eq!(
            raster[0].why,
            "chart samples are outside the window or unordered"
        );
        let local = vec!["Some series labels are abbreviated".into()];

        let preview = raster[0].preview(&chart);
        assert!(preview.contains("...: chart samples are outside the window or unordered"));
        assert!(preview.len() < chart.series[0].labels.len());
        let chart = Arc::new(chart);
        let retained = Arc::downgrade(&chart);
        let label_pointer = chart.series[0].labels.as_ptr();
        let warning_pointer = chart.notices.last().unwrap().as_ptr();
        let details = notice_details(chart.clone(), &raster, &local, Some("paint failed".into()));
        assert_eq!(details.get(0), Some(("Chart", "Test samples")));
        assert_eq!(details.get(1), Some(("Source", "test source")));
        assert_eq!(details.get(2), Some(("PromQL", "test_metric")));
        assert_eq!(
            details.get(3),
            Some((
                "Range",
                "1970-01-01 00:00:00.000 to 1970-01-01 00:00:01.000 UTC; resolution 500 ms"
            ))
        );
        assert_eq!(details.get(4), Some(("Unit", "none")));
        assert_eq!(
            details.get(5),
            Some((raster[0].why, chart.series[0].labels.as_str()))
        );
        assert_eq!(details.get(5).unwrap().1.as_ptr(), label_pointer);
        assert_eq!(details.get(6), Some(("", local[0].as_str())));
        for (index, notice) in chart.notices.iter().enumerate() {
            assert_eq!(details.get(7 + index), Some(("", notice.as_str())));
        }
        assert_eq!(
            details.get(7 + chart.notices.len() - 1).unwrap().1.as_ptr(),
            warning_pointer
        );
        assert_eq!(details.get(details.len() - 1), Some(("", "paint failed")));
        assert_eq!(details.get(details.len()), None);
        drop(chart);
        assert!(retained.upgrade().is_some());
        assert_eq!(details.get(5).unwrap().1.as_ptr(), label_pointer);
        drop(details);
        assert!(retained.upgrade().is_none());
    }

    #[test]
    fn local_empty_and_preparation_failure_states_remain_before_long_source_notices() {
        let failures = vec!["Chart samples are unordered".into()];
        let local = vec!["No finite samples in this range".into()];
        let source = vec!["source warning".repeat(100_000); 20];
        let shown = display_notice_groups(&[&failures, &local, &source]);
        assert!(shown.starts_with(
            "Chart samples are unordered; No finite samples in this range; source warning"
        ));
        assert!(shown.ends_with("14 notices omitted"));
        assert!(shown.len() <= MAX_NOTICE_TEXT_BYTES);
    }

    #[test]
    fn timestamps_preserve_the_date_across_midnight_and_before_the_epoch() {
        assert_eq!(clock(-1), "1969-12-31 23:59:59.999");
        assert_eq!(clock(86_399_999), "1970-01-01 23:59:59.999");
        assert_eq!(clock(86_400_000), "1970-01-02 00:00:00.000");
        assert!(clock(i64::MAX).contains("ms since Unix epoch"));
    }

    #[test]
    fn stat_reducers_refuse_unknown_and_non_finite_results() {
        let points = [(0, 2.0), (1, f64::NAN), (2, 4.0)];
        for (reducer, expected) in [
            ("lastNotNull", 4.0),
            ("min", 2.0),
            ("max", 4.0),
            ("mean", 3.0),
            ("sum", 6.0),
        ] {
            assert_eq!(reduce(&points, reducer), Ok(Some(expected)));
        }
        assert!(reduce(&points, "median").is_err());
        assert_eq!(reduce(&[], "lastNotNull"), Ok(None));
        assert!(reduce(&[(0, f64::MAX), (1, f64::MAX)], "sum").is_err());
    }
}
