//! This module implements the functionality to support graphics in the grid.

use std::mem;

use crate::display::SizeInfo;
use crate::renderer::{RenderRect, ShaderVersion};
use alacritty_terminal::graphics::{
    ClearSubregion, ColorType, GraphicData, GraphicId, UpdateQueues,
};

use crossfont::Metrics;
use log::trace;
use serde::{Deserialize, Serialize};

use crate::gl::types::*;
use crate::{gl, renderer};

use std::cmp;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;

mod draw;
mod shader;

/// Max. number of textures stored in the GPU.
const MAX_TEXTURES_COUNT: usize = 1000;

/// Max. total size, in bytes, of the RGBA pixels stored in the GPU across all
/// graphic textures.
///
/// The count cap alone allows 1000 maximum-size (4096x4096, 64 MiB) textures,
/// so a stream of large Sixel images could pin tens of GiB of GPU memory.
/// Matches `MAX_PENDING_GRAPHICS_BYTES` in `alacritty_terminal`, the cap on the
/// CPU-side queue that feeds this store.
///
/// SPK-SHORTCUT: the ceiling reuses the pending-queue cap instead of a measured
/// live-allocation figure; upgrade path is to measure real Sixel workloads and
/// expose the budget in the config if 256 MiB proves too tight or too loose.
const MAX_TEXTURES_BYTES: usize = 256 * 1024 * 1024;

pub use draw::RenderList;

bitflags::bitflags! {
    /// Result of the `run_updates` operation.
    pub struct UpdateResult: u8 {
        const SUCCESS = 1 << 0;
        const NEED_RESET_ACTIVE_TEX = 1 << 1;
    }
}

/// Type for texture names generated in the GPU.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct TextureName(GLuint);

impl Drop for TextureName {
    fn drop(&mut self) {
        if self.0 != 0 {
            trace!("Delete texture {}.", self.0);
            unsafe {
                gl::DeleteTextures(1, &self.0);
            }
        }
    }
}

/// Texture for a graphic in the grid.
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct GraphicTexture {
    /// Texture in the GPU where the graphic pixels are stored.
    texture: TextureName,

    /// Cell height at the moment graphic was created.
    ///
    /// Used to scale it if the user increases or decreases the font size.
    cell_height: f32,

    /// Cell width at the moment graphic was created.
    cell_width: f32,

    /// Width in pixels of the graphic.
    width: u16,

    /// Height in pixels of the graphic.
    height: u16,
}

#[derive(Debug)]
pub struct GraphicsRenderer {
    /// Program in the GPU to render graphics.
    program: shader::GraphicsShaderProgram,

    /// Collection to associate graphic identifiers with their textures.
    graphic_textures: HashMap<GraphicId, GraphicTexture>,

    /// Indicate if the OpenGL driver has the `clear_texture` extension.
    clear_texture_ext: bool,
}

impl GraphicsRenderer {
    pub fn new(shader_version: ShaderVersion) -> Result<GraphicsRenderer, renderer::Error> {
        let program = shader::GraphicsShaderProgram::new(shader_version)?;
        let clear_texture_ext = check_opengl_extensions(&["GL_ARB_clear_texture"]);
        Ok(GraphicsRenderer { program, graphic_textures: HashMap::default(), clear_texture_ext })
    }

    /// Run the required actions to apply changes for the graphics in the grid.
    #[inline]
    pub fn run_updates(
        &mut self,
        update_queues: UpdateQueues,
        size_info: &SizeInfo,
    ) -> UpdateResult {
        let UpdateQueues { pending, remove_queue, clear_subregions } = update_queues;

        // A graphic dropped before its first draw appears in both queues. Removal
        // only acts on textures that already exist, so it would be a no-op for the
        // id and the upload below would create a texture nothing can reach (ids are
        // monotonic and never reused). Skip uploading graphics that are already
        // scheduled for removal.
        let removed: HashSet<GraphicId> = remove_queue.iter().copied().collect();
        let pending: Vec<GraphicData> =
            pending.into_iter().filter(|graphic| !removed.contains(&graphic.id)).collect();

        self.remove_graphics(remove_queue)
            | self.upload_pending_graphics(pending, size_info)
            | self.clear_subregions(clear_subregions)
    }

    /// Release resources used by removed graphics.
    fn remove_graphics(&mut self, removed_ids: Vec<GraphicId>) -> UpdateResult {
        let mut textures = Vec::with_capacity(removed_ids.len());
        for id in removed_ids {
            if let Some(mut graphic_texture) = self.graphic_textures.remove(&id) {
                // Reset the inner value of TextureName, so the Drop implementation
                // (in debug mode) can verify that the texture was deleted.
                textures.push(mem::take(&mut graphic_texture.texture.0));
            }
        }

        trace!("Call glDeleteTextures with {} items", textures.len());

        unsafe {
            gl::DeleteTextures(textures.len() as GLint, textures.as_ptr());
        }

        UpdateResult::SUCCESS
    }

    /// Create new textures in the GPU, and upload the pixels to them.
    fn upload_pending_graphics(
        &mut self,
        graphics: Vec<GraphicData>,
        size_info: &SizeInfo,
    ) -> UpdateResult {
        if graphics.is_empty() {
            return UpdateResult::SUCCESS;
        }

        for graphic in graphics {
            // Make room before allocating, so the GPU never holds the budget plus
            // this graphic at once.
            self.evict_for_budget(texture_bytes(graphic.width, graphic.height));

            let mut texture = 0;

            unsafe {
                gl::GenTextures(1, &mut texture);
                trace!("Texture generated: {texture}");

                gl::BindTexture(gl::TEXTURE_2D, texture);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAX_LEVEL, 0);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as GLint);

                let pixel_format = match graphic.color_type {
                    ColorType::Rgb => gl::RGB,
                    ColorType::Rgba => gl::RGBA,
                };

                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::RGBA as GLint,
                    graphic.width as GLint,
                    graphic.height as GLint,
                    0,
                    pixel_format,
                    gl::UNSIGNED_BYTE,
                    graphic.pixels.as_ptr().cast(),
                );

                gl::BindTexture(gl::TEXTURE_2D, 0);
            }

            let graphic_texture = GraphicTexture {
                texture: TextureName(texture),
                cell_height: size_info.cell_height(),
                cell_width: size_info.cell_width(),
                width: graphic.width as u16,
                height: graphic.height as u16,
            };

            self.graphic_textures.insert(graphic.id, graphic_texture);
        }

        // If we exceed the textures limit, discard the oldest ones.
        while self.graphic_textures.len() > MAX_TEXTURES_COUNT {
            match self.graphic_textures.keys().min().copied() {
                Some(id) => self.graphic_textures.remove(&id),
                None => unreachable!(),
            };
        }

        UpdateResult::NEED_RESET_ACTIVE_TEX
    }

    /// Discard the oldest textures until `incoming` more bytes fit in the budget.
    fn evict_for_budget(&mut self, incoming: usize) {
        let live = self
            .graphic_textures
            .iter()
            .map(|(id, texture)| (*id, texture_bytes(texture.width.into(), texture.height.into())));

        for id in ids_to_evict(live, incoming, MAX_TEXTURES_BYTES) {
            trace!("Evict texture {id:?} to stay inside the byte budget.");
            self.graphic_textures.remove(&id);
        }
    }

    /// Update textures in the GPU to clear specific subregions.
    pub fn clear_subregions(&mut self, clear_subregions: Vec<ClearSubregion>) -> UpdateResult {
        // If the GL_ARB_clear_texture extension is available we can use
        // glClearTexSubImage to clear the region without sending any memory.
        //
        // If the extension is not available, we have to initialize empty memory
        // and upload it with glTexSubImage2D.

        let mut result = UpdateResult::SUCCESS;

        for clear_subregion in clear_subregions {
            let entry = match self.graphic_textures.get(&clear_subregion.id) {
                Some(entry) => entry,
                None => continue,
            };

            let x_offset = clear_subregion.x as GLint;
            let y_offset = clear_subregion.y as GLint;

            let max_width = entry.width as GLint - x_offset;
            let max_height = entry.height as GLint - y_offset;

            let width = cmp::min(entry.cell_width as GLint, max_width);
            let height = cmp::min(entry.cell_height as GLint, max_height);

            if self.clear_texture_ext {
                let empty = [0_u8; 4];

                unsafe {
                    gl::ClearTexSubImage(
                        entry.texture.0,
                        0,
                        x_offset,
                        y_offset,
                        0,
                        width,
                        height,
                        1,
                        gl::RGBA,
                        gl::UNSIGNED_BYTE,
                        empty.as_ptr().cast(),
                    );
                }
            } else {
                let buf_size = width * height * 4;
                let empty = vec![0_u8; buf_size as usize];

                unsafe {
                    gl::BindTexture(gl::TEXTURE_2D, entry.texture.0);
                    gl::TexSubImage2D(
                        gl::TEXTURE_2D,
                        0,
                        x_offset,
                        y_offset,
                        width,
                        height,
                        gl::RGBA,
                        gl::UNSIGNED_BYTE,
                        empty.as_ptr().cast(),
                    )
                }

                result = UpdateResult::NEED_RESET_ACTIVE_TEX;
            }
        }

        result
    }

    /// Draw graphics in the display.
    #[inline]
    pub fn draw(
        &mut self,
        render_list: RenderList,
        size_info: &SizeInfo,
        rects: &mut Vec<RenderRect>,
        metrics: &Metrics,
    ) {
        if !render_list.is_empty() {
            render_list.draw(self, size_info, rects, metrics);
        }
    }
}

/// Bytes a `width` x `height` graphic occupies in the GPU (always uploaded as RGBA).
fn texture_bytes(width: usize, height: usize) -> usize {
    width.saturating_mul(height).saturating_mul(4)
}

/// Pick the oldest ids (lowest first; ids grow monotonically) to drop so that
/// `incoming` more bytes fit in `budget`, given the `(id, bytes)` pairs now live.
fn ids_to_evict<K: Ord + Copy>(
    live: impl Iterator<Item = (K, usize)>,
    incoming: usize,
    budget: usize,
) -> Vec<K> {
    let mut live: Vec<(K, usize)> = live.collect();
    live.sort_unstable_by_key(|(id, _)| *id);

    let mut total = live.iter().fold(0_usize, |sum, (_, bytes)| sum.saturating_add(*bytes));
    let mut victims = Vec::new();
    for (id, bytes) in live {
        if total.saturating_add(incoming) <= budget {
            break;
        }

        total = total.saturating_sub(bytes);
        victims.push(id);
    }

    victims
}

fn check_opengl_extensions(extensions: &[&str]) -> bool {
    // Use a HashSet to track extensions needed to be found. When the set is
    // empty, we know that all extensions are available.
    let mut needed: HashSet<_> = extensions.iter().collect();

    let mut num_exts = 0;
    unsafe {
        gl::GetIntegerv(gl::NUM_EXTENSIONS, &mut num_exts);
    }

    for index in 0..num_exts as GLuint {
        let pointer = unsafe { gl::GetStringi(gl::EXTENSIONS, index) };
        if pointer.is_null() {
            log::warn!("Can't get OpenGL extension name at index {index} of {num_exts}");
            return false;
        }

        let extension = unsafe { CStr::from_ptr(pointer.cast()) };
        if let Ok(ext) = extension.to_str() {
            if needed.remove(&ext) && needed.is_empty() {
                return true;
            }
        }
    }

    log::debug!("Missing OpenGL extensions: {needed:?}");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_bytes_is_rgba() {
        assert_eq!(texture_bytes(4096, 4096), 64 * 1024 * 1024);
        assert_eq!(texture_bytes(0, 100), 0);
        assert_eq!(texture_bytes(usize::MAX, 4), usize::MAX);
    }

    #[test]
    fn nothing_is_evicted_inside_the_budget() {
        let live = [(1_u64, 40), (2, 40)];
        assert!(ids_to_evict(live.into_iter(), 20, 100).is_empty());
    }

    #[test]
    fn oldest_ids_go_first_until_the_incoming_graphic_fits() {
        // Unordered on purpose: the map iterates in arbitrary order.
        let live = [(7_u64, 40), (3, 40), (5, 40)];
        // 120 live + 50 incoming > 100: dropping 3 leaves 80 + 50, dropping 5 leaves 40 + 50.
        assert_eq!(ids_to_evict(live.into_iter(), 50, 100), vec![3, 5]);
    }

    #[test]
    fn everything_goes_when_the_incoming_graphic_needs_the_whole_budget() {
        let live = [(1_u64, 60), (2, 30)];
        assert_eq!(ids_to_evict(live.into_iter(), 100, 100), vec![1, 2]);
    }

    #[test]
    fn an_exact_fit_evicts_nothing() {
        let live = [(1_u64, 60)];
        assert!(ids_to_evict(live.into_iter(), 40, 100).is_empty());
    }
}
