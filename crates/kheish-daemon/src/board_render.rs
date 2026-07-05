//! Server-side rasterization of board drawing elements.
//!
//! Boards store vector elements in their state asset; this module turns
//! them into the PNG render asset every revision must carry, so agent
//! drawings become visible to models (via `board_reference`) and humans
//! alike. Agent-authored batches are stamped with a small name tag in the
//! author's color, which is how viewers tell who drew what.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use image::{Rgba, RgbaImage};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_line_segment_mut, draw_text_mut, text_size,
};
use imageproc::rect::Rect;
use serde::{Deserialize, Serialize};

/// The embedded label font (DejaVu Sans, Bitstream Vera license).
const FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

/// Hard bounds keeping a render cheap even with hostile input.
const MIN_CANVAS_DIM: u32 = 64;
const MAX_CANVAS_DIM: u32 = 2048;
pub(crate) const MAX_ELEMENTS_PER_DRAW: usize = 200;
pub(crate) const MAX_PATH_POINTS: usize = 2000;
pub(crate) const MAX_TEXT_CHARS: usize = 300;

/// The stable palette agents draw with; the session id picks one entry so
/// each agent keeps its color across revisions.
const AGENT_PALETTE: [&str; 8] = [
    "#7C3AED", "#0EA5E9", "#059669", "#D97706", "#DB2777", "#4F46E5", "#0D9488", "#DC2626",
];

/// Returns the stable drawing color for one agent session.
pub(crate) fn agent_color_for_session(session_id: &str) -> &'static str {
    let hash = session_id
        .bytes()
        .fold(0usize, |acc, byte| acc.wrapping_mul(31).wrapping_add(byte as usize));
    AGENT_PALETTE[hash % AGENT_PALETTE.len()]
}

/// One vector drawing element as stored in the board state envelope.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct BoardElement {
    /// The element shape.
    pub kind: BoardElementKind,
    /// Freehand path points, for `path`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<[f32; 2]>,
    /// Segment start, for `line` and `arrow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<[f32; 2]>,
    /// Segment end, for `line` and `arrow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<[f32; 2]>,
    /// Top-left X, for `rect`/`ellipse`; text anchor X for `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f32>,
    /// Top-left Y, for `rect`/`ellipse`; text baseline-top Y for `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f32>,
    /// Width, for `rect`/`ellipse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<f32>,
    /// Height, for `rect`/`ellipse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<f32>,
    /// The text content, for `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The text size in pixels, for `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// The stroke or text color as `#RRGGBB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The stroke width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f32>,
    /// Who drew the element; agents are stamped automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<BoardElementAuthor>,
}

/// The supported element shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BoardElementKind {
    Path,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Text,
}

/// The recorded author of one element.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct BoardElementAuthor {
    /// `agent` or `human`.
    pub kind: String,
    /// The display name shown on the canvas tag.
    pub name: String,
    /// The author's stable drawing color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The backing session for agent authors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Validates one batch of elements before rendering or persisting.
pub(crate) fn validate_elements(elements: &[BoardElement]) -> Result<()> {
    anyhow::ensure!(!elements.is_empty(), "at least one element is required");
    anyhow::ensure!(
        elements.len() <= MAX_ELEMENTS_PER_DRAW,
        "at most {MAX_ELEMENTS_PER_DRAW} elements per draw call"
    );
    for element in elements {
        match element.kind {
            BoardElementKind::Path => {
                anyhow::ensure!(
                    element.points.len() >= 2,
                    "path elements need at least two points"
                );
                anyhow::ensure!(
                    element.points.len() <= MAX_PATH_POINTS,
                    "path elements are capped at {MAX_PATH_POINTS} points"
                );
            }
            BoardElementKind::Line | BoardElementKind::Arrow => {
                anyhow::ensure!(
                    element.from.is_some() && element.to.is_some(),
                    "line and arrow elements need from and to"
                );
            }
            BoardElementKind::Rect | BoardElementKind::Ellipse => {
                anyhow::ensure!(
                    element.x.is_some()
                        && element.y.is_some()
                        && element.w.is_some()
                        && element.h.is_some(),
                    "rect and ellipse elements need x, y, w, and h"
                );
            }
            BoardElementKind::Text => {
                let text = element.text.as_deref().unwrap_or_default();
                anyhow::ensure!(!text.trim().is_empty(), "text elements need text");
                anyhow::ensure!(
                    text.chars().count() <= MAX_TEXT_CHARS,
                    "text elements are capped at {MAX_TEXT_CHARS} characters"
                );
                anyhow::ensure!(
                    element.x.is_some() && element.y.is_some(),
                    "text elements need x and y"
                );
            }
        }
    }
    Ok(())
}

/// Clamps requested canvas dimensions to the supported raster range.
pub(crate) fn clamp_canvas(width: u32, height: u32) -> (u32, u32) {
    (
        width.clamp(MIN_CANVAS_DIM, MAX_CANVAS_DIM),
        height.clamp(MIN_CANVAS_DIM, MAX_CANVAS_DIM),
    )
}

/// Renders one revision image: the previous render (when present) with the
/// new elements drawn on top, or a fresh white canvas carrying them.
/// `label` stamps one author name tag near the new batch.
pub(crate) fn render_board_png(
    canvas: (u32, u32),
    previous_render_png: Option<&[u8]>,
    new_elements: &[BoardElement],
    label: Option<(&str, &str)>,
) -> Result<Vec<u8>> {
    let mut image = match previous_render_png {
        Some(bytes) => image::load_from_memory(bytes)
            .context("previous board render is not a decodable image")?
            .into_rgba8(),
        None => {
            let (width, height) = clamp_canvas(canvas.0, canvas.1);
            RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]))
        }
    };

    for element in new_elements {
        draw_element(&mut image, element);
    }
    if let Some((name, color)) = label {
        draw_author_tag(&mut image, new_elements, name, parse_color(Some(color)));
    }

    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .context("failed to encode board render")?;
    Ok(bytes)
}

fn parse_color(value: Option<&str>) -> Rgba<u8> {
    let fallback = Rgba([30, 33, 43, 255]);
    let Some(value) = value else {
        return fallback;
    };
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return fallback;
    }
    match (
        u8::from_str_radix(&hex[0..2], 16),
        u8::from_str_radix(&hex[2..4], 16),
        u8::from_str_radix(&hex[4..6], 16),
    ) {
        (Ok(red), Ok(green), Ok(blue)) => Rgba([red, green, blue, 255]),
        _ => fallback,
    }
}

fn element_color(element: &BoardElement) -> Rgba<u8> {
    parse_color(
        element
            .color
            .as_deref()
            .or(element
                .author
                .as_ref()
                .and_then(|author| author.color.as_deref())),
    )
}

fn draw_element(image: &mut RgbaImage, element: &BoardElement) {
    let color = element_color(element);
    let stroke = element.stroke_width.unwrap_or(3.0).clamp(1.0, 24.0);
    match element.kind {
        BoardElementKind::Path => {
            for pair in element.points.windows(2) {
                draw_thick_segment(image, pair[0], pair[1], stroke, color);
            }
        }
        BoardElementKind::Line => {
            if let (Some(from), Some(to)) = (element.from, element.to) {
                draw_thick_segment(image, from, to, stroke, color);
            }
        }
        BoardElementKind::Arrow => {
            if let (Some(from), Some(to)) = (element.from, element.to) {
                draw_thick_segment(image, from, to, stroke, color);
                let angle = (to[1] - from[1]).atan2(to[0] - from[0]);
                let head = (stroke * 4.0).max(10.0);
                for side in [-1.0f32, 1.0] {
                    let theta = angle + std::f32::consts::PI - side * 0.45;
                    let tip = [to[0] + head * theta.cos(), to[1] + head * theta.sin()];
                    draw_thick_segment(image, to, tip, stroke, color);
                }
            }
        }
        BoardElementKind::Rect => {
            if let (Some(x), Some(y), Some(w), Some(h)) =
                (element.x, element.y, element.w, element.h)
            {
                let corners = [
                    [x, y],
                    [x + w, y],
                    [x + w, y + h],
                    [x, y + h],
                ];
                for index in 0..4 {
                    draw_thick_segment(image, corners[index], corners[(index + 1) % 4], stroke, color);
                }
            }
        }
        BoardElementKind::Ellipse => {
            if let (Some(x), Some(y), Some(w), Some(h)) =
                (element.x, element.y, element.w, element.h)
            {
                let (cx, cy) = (x + w / 2.0, y + h / 2.0);
                let (rx, ry) = ((w / 2.0).abs().max(1.0), (h / 2.0).abs().max(1.0));
                let steps = 180usize;
                let mut previous = [cx + rx, cy];
                for step in 1..=steps {
                    let theta = step as f32 / steps as f32 * std::f32::consts::TAU;
                    let point = [cx + rx * theta.cos(), cy + ry * theta.sin()];
                    draw_thick_segment(image, previous, point, stroke, color);
                    previous = point;
                }
            }
        }
        BoardElementKind::Text => {
            if let (Some(x), Some(y), Some(text)) = (element.x, element.y, element.text.as_deref())
            {
                let scale = ab_glyph::PxScale::from(element.font_size.unwrap_or(18.0).clamp(8.0, 96.0));
                draw_text_mut(image, color, x as i32, y as i32, scale, font(), text);
            }
        }
    }
}

fn draw_thick_segment(
    image: &mut RgbaImage,
    from: [f32; 2],
    to: [f32; 2],
    stroke: f32,
    color: Rgba<u8>,
) {
    if stroke <= 1.5 {
        draw_line_segment_mut(image, (from[0], from[1]), (to[0], to[1]), color);
        return;
    }
    let radius = (stroke / 2.0).round().max(1.0) as i32;
    let distance = ((to[0] - from[0]).powi(2) + (to[1] - from[1]).powi(2)).sqrt();
    let steps = (distance / (stroke / 2.0).max(1.0)).ceil().max(1.0) as usize;
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let x = from[0] + (to[0] - from[0]) * t;
        let y = from[1] + (to[1] - from[1]) * t;
        draw_filled_circle_mut(image, (x as i32, y as i32), radius, color);
    }
}

/// Draws the author name tag just above the bounding box of the new batch.
fn draw_author_tag(
    image: &mut RgbaImage,
    elements: &[BoardElement],
    name: &str,
    color: Rgba<u8>,
) {
    let Some((min_x, min_y)) = batch_anchor(elements) else {
        return;
    };
    let scale = ab_glyph::PxScale::from(13.0);
    let (text_width, text_height) = text_size(scale, font(), name);
    let pad = 5i32;
    let tag_width = text_width as i32 + pad * 2;
    let tag_height = text_height as i32 + pad * 2 - 2;
    let x = (min_x as i32).clamp(0, (image.width() as i32 - tag_width).max(0));
    let y = (min_y as i32 - tag_height - 4).clamp(0, (image.height() as i32 - tag_height).max(0));
    draw_filled_rect_mut(
        image,
        Rect::at(x, y).of_size(tag_width.max(1) as u32, tag_height.max(1) as u32),
        color,
    );
    draw_text_mut(
        image,
        Rgba([255, 255, 255, 255]),
        x + pad,
        y + pad - 1,
        scale,
        font(),
        name,
    );
}

fn batch_anchor(elements: &[BoardElement]) -> Option<(f32, f32)> {
    let mut anchor: Option<(f32, f32)> = None;
    let mut consider = |x: f32, y: f32| {
        anchor = Some(match anchor {
            Some((ax, ay)) => (ax.min(x), ay.min(y)),
            None => (x, y),
        });
    };
    for element in elements {
        for point in &element.points {
            consider(point[0], point[1]);
        }
        for point in element.from.iter().chain(element.to.iter()) {
            consider(point[0], point[1]);
        }
        if let (Some(x), Some(y)) = (element.x, element.y) {
            consider(x, y);
        }
    }
    anchor
}

fn font() -> &'static ab_glyph::FontRef<'static> {
    static FONT: std::sync::OnceLock<ab_glyph::FontRef<'static>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        ab_glyph::FontRef::try_from_slice(FONT_BYTES).expect("embedded board font is valid")
    })
}

/// Parses the `elements` array out of one board state envelope.
pub(crate) fn elements_from_state(state: &serde_json::Value) -> Vec<BoardElement> {
    state
        .get("elements")
        .and_then(|value| value.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the canvas dimensions from one board state envelope.
pub(crate) fn canvas_from_state(state: &serde_json::Value) -> Option<(u32, u32)> {
    let canvas = state.get("canvas")?;
    let width = canvas.get("width")?.as_u64()? as u32;
    let height = canvas.get("height")?.as_u64()? as u32;
    Some((width, height))
}

/// The occupancy map column count exposed by [`scene_summary`].
const OCCUPANCY_COLS: usize = 16;
/// The occupancy map row count exposed by [`scene_summary`].
const OCCUPANCY_ROWS: usize = 10;
/// The maximum number of elements listed before [`scene_summary`] truncates.
const SCENE_ELEMENT_CAP: usize = 120;
/// The maximum character count kept for a text element in a scene summary.
const SCENE_TEXT_CHARS: usize = 48;
/// The render fallback color used when an element carries no explicit color.
const SCENE_FALLBACK_COLOR: &str = "#1E212B";

/// Builds a compact, model-facing summary of one board scene.
///
/// Tool outputs are JSON only, so a model can never see the rasterized PNG.
/// This produces the structured substitute an agent reads before drawing: a
/// capped list of placed elements, an ASCII occupancy map showing which author
/// owns each region of the canvas, and a one-line hint pointing at the largest
/// open area. It is pure so it can back both `board_view` and the post-draw
/// payload of `board_draw`. The returned object carries `canvas`, `elements`,
/// `occupancy`, and `free_hint`, plus a `note` when the board is empty.
pub(crate) fn scene_summary(canvas: (u32, u32), elements: &[BoardElement]) -> serde_json::Value {
    let (width, height) = clamp_canvas(canvas.0, canvas.1);
    let (occupancy, free_hint) = occupancy_map(elements, (width, height));
    let mut summary = serde_json::json!({
        "canvas": {"width": width, "height": height},
        "elements": compact_elements(elements),
        "occupancy": occupancy,
        "free_hint": free_hint,
    });
    if elements.is_empty()
        && let serde_json::Value::Object(map) = &mut summary
    {
        map.insert(
            "note".to_string(),
            serde_json::Value::String("the board is empty".to_string()),
        );
    }
    summary
}

/// Renders the capped, per-element summary rows used by [`scene_summary`].
fn compact_elements(elements: &[BoardElement]) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    for (index, element) in elements.iter().take(SCENE_ELEMENT_CAP).enumerate() {
        let mut row = serde_json::Map::new();
        row.insert("n".to_string(), serde_json::json!(index + 1));
        row.insert(
            "kind".to_string(),
            serde_json::json!(element_kind_name(element.kind)),
        );
        row.insert("at".to_string(), serde_json::json!(element_anchor_text(element)));
        if let Some(size) = element_size_text(element) {
            row.insert("size".to_string(), serde_json::json!(size));
        }
        if matches!(element.kind, BoardElementKind::Text)
            && let Some(text) = element.text.as_deref()
        {
            row.insert("text".to_string(), serde_json::json!(truncate_scene_text(text)));
        }
        row.insert("color".to_string(), serde_json::json!(element_color_hex(element)));
        row.insert(
            "author".to_string(),
            serde_json::json!(element_author_label(element)),
        );
        rows.push(serde_json::Value::Object(row));
    }
    if elements.len() > SCENE_ELEMENT_CAP {
        rows.push(serde_json::json!({"truncated": elements.len() - SCENE_ELEMENT_CAP}));
    }
    rows
}

/// Returns the wire name of one element kind.
fn element_kind_name(kind: BoardElementKind) -> &'static str {
    match kind {
        BoardElementKind::Path => "path",
        BoardElementKind::Line => "line",
        BoardElementKind::Arrow => "arrow",
        BoardElementKind::Rect => "rect",
        BoardElementKind::Ellipse => "ellipse",
        BoardElementKind::Text => "text",
    }
}

/// Formats the anchor coordinate string for one element.
fn element_anchor_text(element: &BoardElement) -> String {
    match element.kind {
        BoardElementKind::Path => element
            .points
            .first()
            .map(|point| format!("{},{}", round_coord(point[0]), round_coord(point[1])))
            .unwrap_or_else(|| "0,0".to_string()),
        BoardElementKind::Line | BoardElementKind::Arrow => {
            let from = element.from.unwrap_or([0.0, 0.0]);
            let to = element.to.unwrap_or([0.0, 0.0]);
            format!(
                "{},{}\u{2192}{},{}",
                round_coord(from[0]),
                round_coord(from[1]),
                round_coord(to[0]),
                round_coord(to[1])
            )
        }
        BoardElementKind::Rect | BoardElementKind::Ellipse | BoardElementKind::Text => format!(
            "{},{}",
            round_coord(element.x.unwrap_or(0.0)),
            round_coord(element.y.unwrap_or(0.0))
        ),
    }
}

/// Formats the `w×h` size string for the sized element kinds.
fn element_size_text(element: &BoardElement) -> Option<String> {
    match element.kind {
        BoardElementKind::Rect | BoardElementKind::Ellipse => match (element.w, element.h) {
            (Some(w), Some(h)) => Some(format!("{}\u{00D7}{}", round_coord(w), round_coord(h))),
            _ => None,
        },
        _ => None,
    }
}

/// Returns the effective `#RRGGBB` color reported for one element.
fn element_color_hex(element: &BoardElement) -> String {
    element
        .color
        .clone()
        .or_else(|| {
            element
                .author
                .as_ref()
                .and_then(|author| author.color.clone())
        })
        .unwrap_or_else(|| SCENE_FALLBACK_COLOR.to_string())
}

/// Returns the display author label for one element: the author name when
/// known, otherwise `human` for human authors or `unknown`.
fn element_author_label(element: &BoardElement) -> String {
    match element.author.as_ref() {
        Some(author) if !author.name.trim().is_empty() => author.name.trim().to_string(),
        Some(author) if author.kind == "human" => "human".to_string(),
        _ => "unknown".to_string(),
    }
}

/// Rounds one canvas coordinate to the nearest integer for display.
fn round_coord(value: f32) -> i64 {
    if value.is_finite() {
        value.round() as i64
    } else {
        0
    }
}

/// Shortens a text element down to a bounded preview for the summary.
fn truncate_scene_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= SCENE_TEXT_CHARS {
        return trimmed.to_string();
    }
    let mut preview = trimmed
        .chars()
        .take(SCENE_TEXT_CHARS - 1)
        .collect::<String>();
    preview.push('\u{2026}');
    preview
}

/// Builds the ASCII occupancy grid string (with a trailing legend line) and
/// the free-space hint for one board scene.
fn occupancy_map(elements: &[BoardElement], canvas: (u32, u32)) -> (String, String) {
    let mut grid = vec![None::<char>; OCCUPANCY_COLS * OCCUPANCY_ROWS];
    let mut letter_labels: BTreeMap<char, String> = BTreeMap::new();
    for element in elements {
        let label = element_author_label(element);
        let Some(letter) = label.chars().next().map(|value| value.to_ascii_uppercase()) else {
            continue;
        };
        let cells = element_cells(element, canvas);
        if cells.is_empty() {
            continue;
        }
        letter_labels.entry(letter).or_insert(label);
        for (col, row) in cells {
            grid[row * OCCUPANCY_COLS + col] = Some(letter);
        }
    }

    let mut lines = Vec::with_capacity(OCCUPANCY_ROWS + 1);
    for row in 0..OCCUPANCY_ROWS {
        let mut line = String::with_capacity(OCCUPANCY_COLS);
        for col in 0..OCCUPANCY_COLS {
            line.push(grid[row * OCCUPANCY_COLS + col].unwrap_or('.'));
        }
        lines.push(line);
    }
    let present = grid.iter().flatten().copied().collect::<BTreeSet<char>>();
    let legend = if present.is_empty() {
        "legend: (empty)".to_string()
    } else {
        let entries = present
            .iter()
            .filter_map(|letter| {
                letter_labels
                    .get(letter)
                    .map(|name| format!("{letter}={name}"))
            })
            .collect::<Vec<_>>();
        format!("legend: {}", entries.join(", "))
    };
    let mut occupancy = lines.join("\n");
    occupancy.push('\n');
    occupancy.push_str(&legend);
    (occupancy, free_hint(&grid, canvas))
}

/// Returns the occupancy cells covered by one element, in draw order.
fn element_cells(element: &BoardElement, canvas: (u32, u32)) -> Vec<(usize, usize)> {
    let mut cells = Vec::new();
    match element.kind {
        BoardElementKind::Rect | BoardElementKind::Ellipse => {
            if let (Some(x), Some(y), Some(w), Some(h)) =
                (element.x, element.y, element.w, element.h)
            {
                let (col0, row0) = cell_of(x.min(x + w), y.min(y + h), canvas);
                let (col1, row1) = cell_of(x.max(x + w), y.max(y + h), canvas);
                for row in row0..=row1 {
                    for col in col0..=col1 {
                        cells.push((col, row));
                    }
                }
            }
        }
        BoardElementKind::Line | BoardElementKind::Arrow => {
            if let (Some(from), Some(to)) = (element.from, element.to) {
                push_segment_cells(from, to, canvas, &mut cells);
            }
        }
        BoardElementKind::Path => {
            for pair in element.points.windows(2) {
                push_segment_cells(pair[0], pair[1], canvas, &mut cells);
            }
            if let Some(first) = element.points.first() {
                cells.push(cell_of(first[0], first[1], canvas));
            }
        }
        BoardElementKind::Text => {
            if let (Some(x), Some(y)) = (element.x, element.y) {
                cells.push(cell_of(x, y, canvas));
            }
        }
    }
    cells
}

/// Samples the occupancy cells a straight segment passes through.
fn push_segment_cells(
    from: [f32; 2],
    to: [f32; 2],
    canvas: (u32, u32),
    cells: &mut Vec<(usize, usize)>,
) {
    let cell_w = canvas.0 as f32 / OCCUPANCY_COLS as f32;
    let cell_h = canvas.1 as f32 / OCCUPANCY_ROWS as f32;
    let distance = ((to[0] - from[0]).powi(2) + (to[1] - from[1]).powi(2)).sqrt();
    let step = (cell_w.min(cell_h) / 2.0).max(1.0);
    let steps = (distance / step).ceil().max(1.0) as usize;
    for step_index in 0..=steps {
        let t = step_index as f32 / steps as f32;
        let px = from[0] + (to[0] - from[0]) * t;
        let py = from[1] + (to[1] - from[1]) * t;
        cells.push(cell_of(px, py, canvas));
    }
}

/// Maps one canvas pixel to its clamped occupancy cell.
fn cell_of(px: f32, py: f32, canvas: (u32, u32)) -> (usize, usize) {
    let width = canvas.0.max(1) as f32;
    let height = canvas.1.max(1) as f32;
    let cx = if px.is_finite() { px } else { 0.0 }.clamp(0.0, width - 1.0);
    let cy = if py.is_finite() { py } else { 0.0 }.clamp(0.0, height - 1.0);
    let col = ((cx / width) * OCCUPANCY_COLS as f32).floor() as usize;
    let row = ((cy / height) * OCCUPANCY_ROWS as f32).floor() as usize;
    (col.min(OCCUPANCY_COLS - 1), row.min(OCCUPANCY_ROWS - 1))
}

/// Names the largest broadly-empty region of the occupancy grid.
fn free_hint(grid: &[Option<char>], canvas: (u32, u32)) -> String {
    let total = OCCUPANCY_COLS * OCCUPANCY_ROWS;
    let empty = grid.iter().filter(|cell| cell.is_none()).count();
    if empty == total {
        return "the whole board is free".to_string();
    }
    if empty == 0 {
        return "the board is full; extend or reuse existing elements instead of adding new ones"
            .to_string();
    }
    let (width, height) = canvas;
    let region_fraction = |cols: std::ops::Range<usize>, rows: std::ops::Range<usize>| -> f32 {
        let mut empty = 0usize;
        let mut count = 0usize;
        for row in rows.clone() {
            for col in cols.clone() {
                count += 1;
                if grid[row * OCCUPANCY_COLS + col].is_none() {
                    empty += 1;
                }
            }
        }
        if count == 0 {
            0.0
        } else {
            empty as f32 / count as f32
        }
    };
    let candidates = [
        (
            region_fraction(0..OCCUPANCY_COLS, OCCUPANCY_ROWS / 2..OCCUPANCY_ROWS),
            format!("the bottom half below y\u{2248}{} is mostly free", height / 2),
        ),
        (
            region_fraction(0..OCCUPANCY_COLS, 0..OCCUPANCY_ROWS / 2),
            format!("the top half above y\u{2248}{} is mostly free", height / 2),
        ),
        (
            region_fraction(OCCUPANCY_COLS / 2..OCCUPANCY_COLS, 0..OCCUPANCY_ROWS),
            format!("the right half right of x\u{2248}{} is mostly free", width / 2),
        ),
        (
            region_fraction(0..OCCUPANCY_COLS / 2, 0..OCCUPANCY_ROWS),
            format!("the left half left of x\u{2248}{} is mostly free", width / 2),
        ),
    ];
    let best = candidates
        .iter()
        .max_by(|left, right| {
            left.0
                .partial_cmp(&right.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("candidate regions are non-empty");
    if best.0 >= 0.6 {
        best.1.clone()
    } else {
        "free space is scattered; place new elements in the '.' cells of the occupancy map"
            .to_string()
    }
}

/// Rejects clearly malformed draw payloads before touching any state.
pub(crate) fn ensure_reasonable_text(elements: &[BoardElement]) -> Result<()> {
    for element in elements {
        if let Some(text) = element.text.as_deref()
            && text.chars().any(|character| character.is_control() && character != '\n')
        {
            bail!("text elements must not contain control characters");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(points: Vec<[f32; 2]>) -> BoardElement {
        BoardElement {
            kind: BoardElementKind::Path,
            points,
            from: None,
            to: None,
            x: None,
            y: None,
            w: None,
            h: None,
            text: None,
            font_size: None,
            color: Some("#7C3AED".to_string()),
            stroke_width: Some(4.0),
            author: None,
        }
    }

    #[test]
    fn renders_fresh_canvas_and_composites_over_previous() -> anyhow::Result<()> {
        let first = render_board_png(
            (320, 200),
            None,
            &[stroke(vec![[10.0, 10.0], [100.0, 80.0]])],
            Some(("Atlas", "#7C3AED")),
        )?;
        let decoded = image::load_from_memory(&first)?;
        assert_eq!((decoded.width(), decoded.height()), (320, 200));

        let second = render_board_png(
            (320, 200),
            Some(&first),
            &[stroke(vec![[50.0, 50.0], [200.0, 150.0]])],
            Some(("Nova", "#0EA5E9")),
        )?;
        assert!(image::load_from_memory(&second).is_ok());
        assert_ne!(first, second);
        Ok(())
    }

    #[test]
    fn validation_rejects_bad_batches() {
        assert!(validate_elements(&[]).is_err());
        assert!(validate_elements(&[stroke(vec![[0.0, 0.0]])]).is_err());
        let mut text = stroke(Vec::new());
        text.kind = BoardElementKind::Text;
        text.x = Some(4.0);
        text.y = Some(4.0);
        text.text = Some("hello".to_string());
        assert!(validate_elements(std::slice::from_ref(&text)).is_ok());
        text.text = Some("x".repeat(MAX_TEXT_CHARS + 1));
        assert!(validate_elements(std::slice::from_ref(&text)).is_err());
    }

    #[test]
    fn agent_colors_are_stable_per_session() {
        assert_eq!(
            agent_color_for_session("session-a"),
            agent_color_for_session("session-a")
        );
    }

    #[test]
    fn canvas_dimensions_are_clamped() {
        assert_eq!(clamp_canvas(10, 90000), (MIN_CANVAS_DIM, MAX_CANVAS_DIM));
    }

    fn authored(name: &str, mut element: BoardElement) -> BoardElement {
        element.color = Some("#7C3AED".to_string());
        element.author = Some(BoardElementAuthor {
            kind: "agent".to_string(),
            name: name.to_string(),
            color: Some("#7C3AED".to_string()),
            session_id: Some("session-1".to_string()),
        });
        element
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> BoardElement {
        BoardElement {
            kind: BoardElementKind::Rect,
            points: Vec::new(),
            from: None,
            to: None,
            x: Some(x),
            y: Some(y),
            w: Some(w),
            h: Some(h),
            text: None,
            font_size: None,
            color: None,
            stroke_width: None,
            author: None,
        }
    }

    fn occupancy_rows(summary: &serde_json::Value) -> Vec<String> {
        summary["occupancy"]
            .as_str()
            .expect("occupancy string")
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn scene_summary_marks_rect_area_and_legend() {
        // On a 1600x1000 canvas each cell is 100px wide (1600/16) and 100px
        // tall (1000/10). A rect at x 0..310, y 0..190 stays inside columns
        // 0..=3 and rows 0..=1 without touching the next cell boundary.
        let summary = scene_summary((1600, 1000), &[authored("Atlas", rect(0.0, 0.0, 310.0, 190.0))]);
        let rows = occupancy_rows(&summary);
        assert_eq!(rows[0], "AAAA............");
        assert_eq!(rows[1], "AAAA............");
        assert_eq!(rows[2], "................");
        assert!(
            rows.last().expect("legend line").contains("A=Atlas"),
            "legend should map A to Atlas: {:?}",
            rows.last()
        );
        assert_eq!(summary["elements"][0]["kind"], "rect");
        assert_eq!(summary["elements"][0]["at"], "0,0");
        assert_eq!(summary["elements"][0]["size"], "310\u{00D7}190");
        assert_eq!(summary["elements"][0]["author"], "Atlas");
        assert!(summary.get("note").is_none(), "populated board has no note");
        assert!(
            summary["free_hint"]
                .as_str()
                .expect("free hint")
                .contains("free"),
            "free hint should describe open space: {}",
            summary["free_hint"]
        );
    }

    #[test]
    fn scene_summary_empty_board_is_all_dots_with_note() {
        let summary = scene_summary((1600, 1000), &[]);
        let rows = occupancy_rows(&summary);
        assert_eq!(rows.len(), 11, "ten grid rows plus one legend line");
        for row in &rows[..10] {
            assert_eq!(row, "................");
        }
        assert_eq!(rows[10], "legend: (empty)");
        assert_eq!(summary["note"], "the board is empty");
        assert_eq!(summary["free_hint"], "the whole board is free");
        assert_eq!(
            summary["elements"].as_array().expect("elements array").len(),
            0
        );
        assert_eq!(summary["canvas"]["width"], 1600);
        assert_eq!(summary["canvas"]["height"], 1000);
    }

    #[test]
    fn scene_summary_truncates_beyond_element_cap() {
        let elements = (0..SCENE_ELEMENT_CAP + 5)
            .map(|index| authored("Nova", rect(index as f32, 0.0, 4.0, 4.0)))
            .collect::<Vec<_>>();
        let summary = scene_summary((1600, 1000), &elements);
        let listed = summary["elements"].as_array().expect("elements array");
        assert_eq!(listed.len(), SCENE_ELEMENT_CAP + 1, "cap plus truncation row");
        assert_eq!(listed[SCENE_ELEMENT_CAP]["truncated"], 5);
        assert_eq!(listed[SCENE_ELEMENT_CAP - 1]["n"], SCENE_ELEMENT_CAP);
    }

    #[test]
    fn scene_summary_covers_line_text_and_topmost_author() {
        let mut line = BoardElement {
            kind: BoardElementKind::Line,
            points: Vec::new(),
            from: Some([0.0, 0.0]),
            to: Some([1599.0, 999.0]),
            x: None,
            y: None,
            w: None,
            h: None,
            text: None,
            font_size: None,
            color: None,
            stroke_width: None,
            author: None,
        };
        line = authored("Nova", line);
        let mut text = BoardElement {
            kind: BoardElementKind::Text,
            points: Vec::new(),
            from: None,
            to: None,
            x: Some(800.0),
            y: Some(500.0),
            w: None,
            h: None,
            text: Some("hello world".to_string()),
            font_size: Some(18.0),
            color: None,
            stroke_width: None,
            author: None,
        };
        text = authored("Atlas", text);
        // The diagonal line passes through the center cell; the text drawn
        // afterwards is topmost there, so that cell shows Atlas' letter.
        let summary = scene_summary((1600, 1000), &[line, text]);
        let rows = occupancy_rows(&summary);
        assert_eq!(rows[0].chars().next(), Some('N'), "line starts top-left");
        assert_eq!(
            rows[5].chars().nth(8),
            Some('A'),
            "text cell is topmost at the center: {:?}",
            rows[5]
        );
        assert_eq!(summary["elements"][1]["text"], "hello world");
        let legend = rows.last().expect("legend");
        assert!(legend.contains("A=Atlas"), "legend has Atlas: {legend}");
        assert!(legend.contains("N=Nova"), "legend has Nova: {legend}");
    }
}
