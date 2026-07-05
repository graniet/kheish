//! Server-side rasterization of board drawing elements.
//!
//! Boards store vector elements in their state asset; this module turns
//! them into the PNG render asset every revision must carry, so agent
//! drawings become visible to models (via `board_reference`) and humans
//! alike. Agent-authored batches are stamped with a small name tag in the
//! author's color, which is how viewers tell who drew what.

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
}
