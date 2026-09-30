//! This module implements the logic to manage graphic items included in a
//! `Grid` instance.

pub mod sixel;

use std::collections::HashSet;
use std::fmt::Write;
use std::sync::{Arc, Weak};
use std::{cmp, mem};

#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use parking_lot::Mutex;
use smallvec::SmallVec;

use crate::event::{Event, EventListener};
use crate::grid::Dimensions;
use crate::index::{Column, Line};
use crate::term::{Term, TermMode};
use crate::vte::Params;
use crate::vte::ansi::{Handler, Rgb};

/// Max allowed dimensions (width, height) for the graphic, in pixels.
pub const MAX_GRAPHIC_DIMENSIONS: [usize; 2] = [4096, 4096];

/// Max. number of graphics stored in a single cell.
const MAX_GRAPHICS_PER_CELL: usize = 20;

/// Max. total size, in bytes, of the pixel data waiting in the pending queue.
///
/// The queue is drained only when the display draws a frame, so output that
/// never draws (occluded window) or floods Sixel images would otherwise grow
/// it without limit. A single maximum-size graphic is 64 MiB of RGBA pixels,
/// so this allows a handful of them between frames.
const MAX_PENDING_GRAPHICS_BYTES: usize = 256 * 1024 * 1024;

/// Unique identifier for every graphic added to a grid.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Eq, PartialEq, Clone, Debug, Copy, Hash, PartialOrd, Ord)]
pub struct GraphicId(u64);

/// Reference to a texture stored in the display.
///
/// When all references to a single texture are removed, its identifier is
/// added to the remove queue.
#[derive(Clone, Debug)]
pub struct TextureRef {
    /// Graphic identifier.
    pub id: GraphicId,

    /// Width, in pixels, of the graphic.
    pub width: u16,

    /// Height, in pixels, of the graphic.
    pub height: u16,

    /// Height, in pixels, of the cell when the graphic was inserted.
    pub cell_height: usize,

    /// Queue to track removed references.
    pub texture_operations: Weak<Mutex<Vec<TextureOperation>>>,
}

impl PartialEq for TextureRef {
    fn eq(&self, t: &Self) -> bool {
        // Ignore texture_operations.
        self.id == t.id
    }
}

impl Eq for TextureRef {}

impl Drop for TextureRef {
    fn drop(&mut self) {
        if let Some(texture_operations) = self.texture_operations.upgrade() {
            texture_operations.lock().push(TextureOperation::Remove(self.id));
        }
    }
}

/// A list of graphics in a single cell.
pub type GraphicsCell = SmallVec<[GraphicCell; 1]>;

/// Graphic data stored in a single cell.
#[derive(Clone, Debug)]
pub struct GraphicCell {
    /// Texture to draw the graphic in this cell.
    pub texture: Arc<TextureRef>,

    /// Offset in the x direction.
    pub offset_x: u16,

    /// Offset in the y direction.
    pub offset_y: u16,

    /// Queue to track empty subregions.
    pub texture_operations: Weak<Mutex<Vec<TextureOperation>>>,
}

impl PartialEq for GraphicCell {
    fn eq(&self, c: &Self) -> bool {
        // Ignore texture_operations.
        self.texture == c.texture && self.offset_x == c.offset_x && self.offset_y == c.offset_y
    }
}

impl Eq for GraphicCell {}

impl Drop for GraphicCell {
    fn drop(&mut self) {
        if let Some(texture_operations) = self.texture_operations.upgrade() {
            let tex_op = TextureOperation::ClearSubregion(ClearSubregion {
                id: self.texture.id,
                x: self.offset_x,
                y: self.offset_y,
            });

            texture_operations.lock().push(tex_op);
        }
    }
}

#[cfg(feature = "serde")]
#[derive(Serialize, Deserialize)]
struct GraphicCellSerde {
    texture: GraphicId,
    offset_x: u16,
    offset_y: u16,
    tex_width: u16,
    tex_height: u16,
    tex_cell_height: usize,
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for GraphicCell {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let data = GraphicCellSerde::deserialize(deserializer)?;

        let dummy_queue = Arc::new(Mutex::new(Vec::new()));

        let texture = TextureRef {
            id: data.texture,
            width: data.tex_width,
            height: data.tex_height,
            cell_height: data.tex_cell_height,
            texture_operations: Arc::downgrade(&dummy_queue),
        };

        let graphic_cell = GraphicCell {
            texture: Arc::new(texture),
            offset_x: data.offset_x,
            offset_y: data.offset_y,
            texture_operations: Arc::downgrade(&dummy_queue),
        };

        Ok(graphic_cell)
    }
}

#[cfg(feature = "serde")]
impl Serialize for GraphicCell {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let data = GraphicCellSerde {
            texture: self.texture.id,
            offset_x: self.offset_x,
            offset_y: self.offset_y,
            tex_width: self.texture.width,
            tex_height: self.texture.height,
            tex_cell_height: self.texture.cell_height,
        };

        data.serialize(serializer)
    }
}

impl GraphicCell {
    /// Graphic identifier of the texture in this cell.
    #[inline]
    pub fn graphic_id(&self) -> GraphicId {
        self.texture.id
    }
}

/// Specifies the format of the pixel data.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Eq, PartialEq, Clone, Debug, Copy)]
pub enum ColorType {
    /// 3 bytes per pixel (red, green, blue).
    Rgb,

    /// 4 bytes per pixel (red, green, blue, alpha).
    Rgba,
}

/// Defines a single graphic read from the PTY.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct GraphicData {
    /// Graphics identifier.
    pub id: GraphicId,

    /// Width, in pixels, of the graphic.
    pub width: usize,

    /// Height, in pixels, of the graphic.
    pub height: usize,

    /// Color type of the pixels.
    pub color_type: ColorType,

    /// Pixels data.
    pub pixels: Vec<u8>,

    /// Indicate if there are no transparent pixels.
    pub is_opaque: bool,
}

impl GraphicData {
    /// Check if the image may contain transparent pixels. If it returns
    /// `false`, it is guaranteed that there are no transparent pixels.
    #[inline]
    pub fn maybe_transparent(&self) -> bool {
        !self.is_opaque && self.color_type == ColorType::Rgba
    }

    /// Check if all pixels under a region are opaque.
    ///
    /// If the region exceeds the boundaries of the image it is considered as
    /// not filled.
    pub fn is_filled(&self, x: usize, y: usize, width: usize, height: usize) -> bool {
        // If there are pixels outside the picture we assume that the region is
        // not filled.
        if x + width > self.width || y + height > self.height {
            return false;
        }

        // Don't check actual pixels if the image does not contain an alpha
        // channel.
        if !self.maybe_transparent() {
            return true;
        }

        debug_assert!(self.color_type == ColorType::Rgba);

        for offset_y in y..y + height {
            let offset = (offset_y * self.width + x) * 4;
            let row = &self.pixels[offset..offset + width * 4];

            if row.chunks_exact(4).any(|pixel| pixel.last() != Some(&255)) {
                return false;
            }
        }

        true
    }
}

/// Operation to clear a subregion in an existing graphic.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ClearSubregion {
    /// Graphics identifier.
    pub id: GraphicId,

    /// X coordinate.
    pub x: u16,

    /// Y coordinate.
    pub y: u16,
}

/// Queues to add or to remove the textures in the display.
#[derive(Default)]
pub struct UpdateQueues {
    /// Graphics read from the PTY.
    pub pending: Vec<GraphicData>,

    /// Graphics removed from the grid.
    pub remove_queue: Vec<GraphicId>,

    /// Subregions in a graphic to be clear.
    pub clear_subregions: Vec<ClearSubregion>,
}

/// Operations on the existing textures.
#[derive(Debug)]
pub enum TextureOperation {
    /// Remove a texture from the GPU.
    Remove(GraphicId),

    /// Clear a subregion.
    ClearSubregion(ClearSubregion),
}

/// Track changes in the grid to add or to remove graphics.
#[derive(Debug, Default)]
pub struct Graphics {
    /// Last generated identifier.
    pub last_id: u64,

    /// New graphics, received from the PTY.
    pub pending: Vec<GraphicData>,

    /// Graphics removed from the grid.
    pub texture_operations: Arc<Mutex<Vec<TextureOperation>>>,

    /// Shared palette for Sixel graphics.
    pub sixel_shared_palette: Option<Vec<Rgb>>,

    /// Cell height in pixels.
    pub cell_height: f32,

    /// Cell width in pixels.
    pub cell_width: f32,

    /// Current Sixel parser.
    pub sixel_parser: Option<Box<sixel::Parser>>,
}

impl Graphics {
    /// Create a new instance, and initialize it with the dimensions of the
    /// window.
    pub fn new<S: Dimensions>(size: &S) -> Self {
        let mut graphics = Graphics::default();
        graphics.resize(size);
        graphics
    }

    /// Generate a new graphic identifier.
    pub fn next_id(&mut self) -> GraphicId {
        self.last_id += 1;
        GraphicId(self.last_id)
    }

    /// Get queues to update graphics in the grid.
    ///
    /// If all queues are empty, it returns `None`.
    pub fn take_queues(&mut self) -> Option<UpdateQueues> {
        let texture_operations = {
            let mut queue = self.texture_operations.lock();
            if queue.is_empty() { Vec::new() } else { mem::take(&mut *queue) }
        };

        if texture_operations.is_empty() && self.pending.is_empty() {
            return None;
        }

        let mut remove_queue = Vec::new();
        let mut clear_subregions = Vec::new();

        for operation in texture_operations {
            match operation {
                TextureOperation::Remove(id) => remove_queue.push(id),
                TextureOperation::ClearSubregion(cs) => clear_subregions.push(cs),
            }
        }

        Some(UpdateQueues { pending: mem::take(&mut self.pending), remove_queue, clear_subregions })
    }

    /// Update cell dimensions.
    pub fn resize<S: Dimensions>(&mut self, size: &S) {
        self.cell_height = size.cell_height();
        self.cell_width = size.cell_width();
    }

    pub fn graphics_attribute<L: EventListener>(&self, event_proxy: &L, pi: u16, pa: u16) {
        // From Xterm documentation:
        //
        //   CSI ? Pi ; Pa ; Pv S
        //
        //   Pi = 1  -> item is number of color registers.
        //   Pi = 2  -> item is Sixel graphics geometry (in pixels).
        //   Pi = 3  -> item is ReGIS graphics geometry (in pixels).
        //
        //   Pa = 1  -> read attribute.
        //   Pa = 2  -> reset to default.
        //   Pa = 3  -> set to value in Pv.
        //   Pa = 4  -> read the maximum allowed value.
        //
        //   Pv is ignored by xterm except when setting (Pa == 3).
        //   Pv = n <- A single integer is used for color registers.
        //   Pv = width ; height <- Two integers for graphics geometry.
        //
        //   xterm replies with a control sequence of the same form:
        //
        //   CSI ? Pi ; Ps ; Pv S
        //
        //   where Ps is the status:
        //   Ps = 0  <- success.
        //   Ps = 1  <- error in Pi.
        //   Ps = 2  <- error in Pa.
        //   Ps = 3  <- failure.
        //
        //   On success, Pv represents the value read or set.

        fn generate_response(pi: u16, ps: u16, pv: &[usize]) -> String {
            let mut text = format!("\x1b[?{pi};{ps}");
            for item in pv {
                let _ = write!(&mut text, ";{item}");
            }
            text.push('S');
            text
        }

        let (ps, pv) = match pi {
            1 => {
                match pa {
                    1 => (0, &[sixel::MAX_COLOR_REGISTERS][..]), // current value is always the
                    // maximum
                    2 => (3, &[][..]), // Report unsupported
                    3 => (3, &[][..]), // Report unsupported
                    4 => (0, &[sixel::MAX_COLOR_REGISTERS][..]),
                    _ => (2, &[][..]), // Report error in Pa
                }
            },

            2 => {
                match pa {
                    1 => {
                        event_proxy.send_event(Event::TextAreaSizeRequest(Arc::new(
                            move |window_size| {
                                let width = window_size.num_cols * window_size.cell_width;
                                let height = window_size.num_lines * window_size.cell_height;
                                let graphic_dimensions = [
                                    cmp::min(width as usize, MAX_GRAPHIC_DIMENSIONS[0]),
                                    cmp::min(height as usize, MAX_GRAPHIC_DIMENSIONS[1]),
                                ];

                                let (ps, pv) = (0, &graphic_dimensions[..]);
                                generate_response(pi, ps, pv)
                            },
                        )));
                        return;
                    },
                    2 => (3, &[][..]), // Report unsupported
                    3 => (3, &[][..]), // Report unsupported
                    4 => (0, &MAX_GRAPHIC_DIMENSIONS[..]),
                    _ => (2, &[][..]), // Report error in Pa
                }
            },

            3 => {
                (1, &[][..]) // Report error in Pi (ReGIS unknown)
            },

            _ => {
                (1, &[][..]) // Report error in Pi
            },
        };

        event_proxy.send_event(Event::PtyWrite(generate_response(pi, ps, pv)));
    }

    /// Start parsing a Sixel graphic.
    ///
    /// `private_palette` is `true` when `TermMode::SIXEL_PRIV_PALETTE` is set.
    /// In that mode every graphic starts from the default color registers, so
    /// any stale shared palette (kept from before the switch to private mode)
    /// is neither used nor retained.
    pub fn start_sixel_graphic(&mut self, params: &Params, private_palette: bool) {
        let palette = if private_palette {
            self.sixel_shared_palette = None;
            None
        } else {
            self.sixel_shared_palette.take()
        };
        self.sixel_parser = Some(Box::new(sixel::Parser::new(params, palette)));
    }
}

pub fn parse_sixel<L: EventListener>(term: &mut Term<L>, parser: sixel::Parser) {
    match parser.finish() {
        Ok((graphic, palette)) => insert_graphic(term, graphic, Some(palette)),
        Err(err) => log::warn!("Failed to parse Sixel data: {err}"),
    }
}

pub fn insert_graphic<L: EventListener>(
    term: &mut Term<L>,
    graphic: GraphicData,
    palette: Option<Vec<Rgb>>,
) {
    let cell_width = term.graphics.cell_width as usize;
    let cell_height = term.graphics.cell_height as usize;

    // Store last palette if we receive a new one, and it is shared.
    if let Some(palette) = palette {
        if !term.mode().contains(TermMode::SIXEL_PRIV_PALETTE) {
            term.graphics.sixel_shared_palette = Some(palette);
        }
    }

    // Cell dimensions come from the embedder, and `Dimensions` defaults them to
    // zero. Stepping through the graphic by zero-sized cells would panic.
    if cell_width == 0 || cell_height == 0 {
        log::warn!("Ignoring graphic: cell dimensions are {cell_width}x{cell_height} pixels");
        return;
    }

    if graphic.width > MAX_GRAPHIC_DIMENSIONS[0] || graphic.height > MAX_GRAPHIC_DIMENSIONS[1] {
        return;
    }

    // Drop the graphic if the display is not consuming the pending queue.
    let pending_bytes: usize = term.graphics.pending.iter().map(|g| g.pixels.len()).sum();
    if pending_bytes.saturating_add(graphic.pixels.len()) > MAX_PENDING_GRAPHICS_BYTES {
        log::warn!("Ignoring graphic: pending graphics exceed {MAX_PENDING_GRAPHICS_BYTES} bytes");
        return;
    }

    let width = graphic.width as u16;
    let height = graphic.height as u16;

    if width == 0 || height == 0 {
        return;
    }

    let graphic_id = term.graphics.next_id();

    // If SIXEL_DISPLAY is disabled, the start of the graphic is the
    // cursor position, and the grid can be scrolled if the graphic is
    // larger than the screen. The cursor is moved to the next line
    // after the graphic.
    //
    // If it is disabled, the graphic starts at (0, 0), the grid is never
    // scrolled, and the cursor position is unmodified.

    let scrolling = !term.mode().contains(TermMode::SIXEL_DISPLAY);

    let leftmost = if scrolling { term.grid().cursor.point.column.0 } else { 0 };

    // A very simple optimization is to detect is a new graphic is replacing
    // completely a previous one. This happens if the following conditions
    // are met:
    //
    // - Both graphics are attached to the same top-left cell.
    // - Both graphics have the same size.
    // - The new graphic does not contain transparent pixels.
    //
    // In this case, we will ignore cells with a reference to the replaced
    // graphic.

    let skip_textures = {
        if graphic.maybe_transparent() {
            HashSet::new()
        } else {
            let mut set = HashSet::new();

            let line = if scrolling { term.grid().cursor.point.line } else { Line(0) };

            if let Some(old_graphics) = term.grid()[line][Column(leftmost)].graphics() {
                for graphic in old_graphics {
                    let tex = &*graphic.texture;
                    if tex.width == width && tex.height == height && tex.cell_height == cell_height
                    {
                        set.insert(tex.id);
                    }
                }
            }

            set
        }
    };

    // Fill the cells under the graphic.
    //
    // The cell in the first column contains a reference to the
    // graphic, with the offset from the start. The rest of the
    // cells are not overwritten, allowing any text behind
    // transparent portions of the image to be visible.

    // Transparent graphics keep the text under them. Opaque ones hide it, so
    // their cells are reset from the cursor template.
    let keep_cell_content = graphic.maybe_transparent();

    let texture = Arc::new(TextureRef {
        id: graphic_id,
        width,
        height,
        cell_height,
        texture_operations: Arc::downgrade(&term.graphics.texture_operations),
    });

    for (top, offset_y) in (0..).zip((0..height).step_by(cell_height)) {
        let line = if scrolling {
            term.grid().cursor.point.line
        } else {
            // Check if the image is beyond the screen limit.
            if top >= term.screen_lines() as i32 {
                break;
            }

            Line(top)
        };

        // Store a reference to the graphic in the first column.
        let row_len = term.grid()[line].len();
        for (left, offset_x) in (leftmost..).zip((0..width).step_by(cell_width)) {
            if left >= row_len {
                break;
            }

            let texture_operations = Arc::downgrade(&term.graphics.texture_operations);
            let graphic_cell =
                GraphicCell { texture: texture.clone(), offset_x, offset_y, texture_operations };

            let template = (!keep_cell_content).then(|| term.grid().cursor.template.clone());
            let cell_ref = &mut term.grid_mut()[line][Column(left)];

            // If the cell contains any graphics, and the region of the cell
            // is not fully filled by the new graphic, the old graphics are
            // kept in the cell.
            let graphics = match cell_ref.take_graphics() {
                Some(mut old_graphics)
                    if old_graphics
                        .iter()
                        .any(|graphic| !skip_textures.contains(&graphic.texture.id))
                        && !graphic.is_filled(
                            offset_x as usize,
                            offset_y as usize,
                            cell_width,
                            cell_height,
                        ) =>
                {
                    // Ensure that we don't exceed the graphics limit per cell.
                    while old_graphics.len() >= MAX_GRAPHICS_PER_CELL {
                        drop(old_graphics.remove(0));
                    }

                    old_graphics.push(graphic_cell);
                    old_graphics
                },

                _ => smallvec::smallvec![graphic_cell],
            };

            match template {
                Some(mut cell) => {
                    cell.set_graphics(graphics);
                    *cell_ref = cell;
                },
                None => cell_ref.set_graphics(graphics),
            }
        }

        term.mark_line_damaged(line);

        if scrolling && offset_y < height.saturating_sub(cell_height as u16) {
            term.linefeed();
        }
    }

    if term.mode().contains(TermMode::SIXEL_CURSOR_TO_THE_RIGHT) {
        let graphic_columns = graphic.width.div_ceil(cell_width);
        term.move_forward(graphic_columns);
    } else if scrolling {
        term.linefeed();
        term.carriage_return();
    }

    // Add the graphic data to the pending queue.
    term.graphics.pending.push(GraphicData { id: graphic_id, ..graphic });
}

#[test]
fn check_opaque_region() {
    let graphic = GraphicData {
        id: GraphicId(0),
        width: 10,
        height: 10,
        color_type: ColorType::Rgb,
        pixels: vec![255; 10 * 10 * 3],
        is_opaque: true,
    };

    assert!(graphic.is_filled(1, 1, 3, 3));
    assert!(!graphic.is_filled(8, 8, 10, 10));

    let pixels = {
        // Put a transparent 3x3 box inside the picture.
        let mut data = vec![255; 10 * 10 * 4];
        for y in 3..6 {
            let offset = y * 10 * 4;
            data[offset..offset + 3 * 4].fill(0);
        }
        data
    };

    let graphic = GraphicData {
        id: GraphicId(0),
        pixels,
        width: 10,
        height: 10,
        color_type: ColorType::Rgba,
        is_opaque: false,
    };

    assert!(graphic.is_filled(0, 0, 3, 3));
    assert!(!graphic.is_filled(1, 1, 4, 4));
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::VoidListener;
    use crate::term::Config;

    struct Size {
        cell_width: f32,
        cell_height: f32,
    }

    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.screen_lines()
        }

        fn screen_lines(&self) -> usize {
            24
        }

        fn columns(&self) -> usize {
            80
        }

        fn cell_width(&self) -> f32 {
            self.cell_width
        }

        fn cell_height(&self) -> f32 {
            self.cell_height
        }
    }

    fn new_term(cell_width: f32, cell_height: f32) -> Term<VoidListener> {
        Term::new(Config::default(), &Size { cell_width, cell_height }, VoidListener)
    }

    fn rgba_graphic(width: usize, height: usize, alpha: u8) -> GraphicData {
        GraphicData {
            id: GraphicId(0),
            width,
            height,
            color_type: ColorType::Rgba,
            pixels: vec![alpha; width * height * 4],
            is_opaque: alpha == 255,
        }
    }

    #[test]
    fn opaque_check_honors_horizontal_offset_and_exact_edges() {
        // Transparent box in the first three columns of a 10x10 picture.
        let mut pixels = vec![255; 10 * 10 * 4];
        for y in 0..10 {
            let offset = y * 10 * 4;
            pixels[offset..offset + 3 * 4].fill(0);
        }

        let graphic = GraphicData {
            id: GraphicId(0),
            pixels,
            width: 10,
            height: 10,
            color_type: ColorType::Rgba,
            is_opaque: false,
        };

        // Regions right of the transparent columns are filled, including the
        // one ending exactly at the right/bottom edge.
        assert!(graphic.is_filled(3, 0, 3, 3));
        assert!(graphic.is_filled(7, 7, 3, 3));

        // A region overlapping the transparent columns is not filled.
        assert!(!graphic.is_filled(2, 0, 3, 3));
        assert!(!graphic.is_filled(0, 0, 3, 3));

        // Beyond the edge is never filled.
        assert!(!graphic.is_filled(8, 0, 3, 3));
        assert!(!graphic.is_filled(0, 8, 3, 3));
    }

    #[test]
    fn zero_cell_dimensions_do_not_panic() {
        let mut term = new_term(0.0, 0.0);

        insert_graphic(&mut term, rgba_graphic(20, 20, 255), None);

        assert!(term.graphics.pending.is_empty());
    }

    #[test]
    fn pending_queue_is_bounded() {
        let mut term = new_term(10.0, 10.0);

        for _ in 0..8 {
            insert_graphic(&mut term, rgba_graphic(4096, 4096, 255), None);
        }

        let bytes: usize = term.graphics.pending.iter().map(|g| g.pixels.len()).sum();
        assert!(bytes <= MAX_PENDING_GRAPHICS_BYTES);
        assert_eq!(term.graphics.pending.len(), MAX_PENDING_GRAPHICS_BYTES / (4096 * 4096 * 4));
    }

    #[test]
    fn transparent_graphic_keeps_text_and_opaque_graphic_replaces_it() {
        let mut term = new_term(10.0, 10.0);
        term.grid_mut()[Line(0)][Column(0)].c = 'x';
        insert_graphic(&mut term, rgba_graphic(10, 10, 0), None);
        assert_eq!(term.grid()[Line(0)][Column(0)].c, 'x');
        assert!(term.grid()[Line(0)][Column(0)].graphics().is_some());

        let mut term = new_term(10.0, 10.0);
        term.grid_mut()[Line(0)][Column(0)].c = 'x';
        insert_graphic(&mut term, rgba_graphic(10, 10, 255), None);
        assert_eq!(term.grid()[Line(0)][Column(0)].c, ' ');
        assert!(term.grid()[Line(0)][Column(0)].graphics().is_some());
    }

    #[test]
    fn private_palette_mode_ignores_stale_shared_palette() {
        let red = Rgb { r: 255, g: 0, b: 0 };

        for (private, expect_red) in [(true, false), (false, true)] {
            let mut graphics = Graphics::default();
            graphics.sixel_shared_palette = Some(vec![red; 16]);

            graphics.start_sixel_graphic(&Params::default(), private);

            let mut parser = graphics.sixel_parser.take().unwrap();
            for byte in b"#1~" {
                parser.put(*byte).unwrap();
            }
            let (_, palette) = parser.finish().unwrap();

            assert_eq!(palette[1] == red, expect_red, "private = {private}");
            assert!(graphics.sixel_shared_palette.is_none());
        }
    }
}
