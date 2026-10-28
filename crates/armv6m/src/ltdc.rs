//! The H7's LCD-TFT display controller, the LTDC (RM0433 34), as far as
//! what a panel on it shows: the active area its timing registers make,
//! the background colour, and its two layers — each a window onto a frame
//! in memory, in one of its pixel formats, through its colour look-up
//! table (CLUT) if it has one.
//!
//! The panel is what the controller makes of memory, so it is read out
//! whole when someone looks ([`Ltdc::scan`]) rather than a line at a time
//! as the pixel clock runs, and the pixel clock is not needed. A layer's
//! registers are shadowed, as on the part: what software writes takes
//! effect when SRCR reloads it, at once (IMR) or at the next vertical blank
//! (VBR) — which, nothing being scanned in between, is at once too. So a
//! frame drawn into one buffer while another is shown, and then shown by
//! moving the layer to it, is never seen half drawn. Each reload counts a
//! frame, for a watcher to tell a new one from the last.
//!
//! Not modelled: blending beyond the layers' order (layer 2 is drawn over
//! layer 1, both opaque), colour keying, the default colour outside a
//! frame, dithering, the line and error interrupts, and the timing
//! registers' sync and porch widths beyond where the active area is.

// The global registers, by offset.
const BPCR: u32 = 0x0C;
const AWCR: u32 = 0x10;
const GCR: u32 = 0x18;
const SRCR: u32 = 0x24;
const BCCR: u32 = 0x2C;
const CDSR: u32 = 0x48;
/// The layers' registers, from each one's base.
const LAYERS: [u32; 2] = [0x84, 0x104];
const LAYER_SIZE: u32 = 0x44;
const CLUTWR: u32 = 0x40;
// In a layer.
const CR: usize = 0x00 / 4;
const WHPCR: usize = 0x04 / 4;
const WVPCR: usize = 0x08 / 4;
const PFCR: usize = 0x10 / 4;
const CFBAR: usize = 0x28 / 4;
const CFBLR: usize = 0x2C / 4;
const CFBLNR: usize = 0x30 / 4;

/// GCR's reset value: its polarity bits, and the dither width fields.
const GCR_RESET: u32 = 0x0000_2220;

#[derive(Clone, Debug)]
pub struct Ltdc {
    global: [u32; 0x14],
    layers: [Layer; 2],
    /// Reloads of the shadow registers since reset: frames shown.
    pub frames: u64,
}

#[derive(Clone, Debug)]
struct Layer {
    /// The registers as software wrote them, and as the controller uses
    /// them.
    shadow: [u32; 13],
    active: [u32; 13],
    /// The CLUT's 256 entries, RGB.
    clut: Vec<u32>,
}

impl Default for Layer {
    fn default() -> Self {
        Layer { shadow: [0; 13], active: [0; 13], clut: vec![0; 256] }
    }
}

/// A pixel format, as a layer's PFCR has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Argb8888,
    Rgb888,
    Rgb565,
    Argb1555,
    Argb4444,
    /// 8-bit palette indices: Doom's, a game's, a terminal's.
    L8,
    Al44,
    Al88,
}

impl Format {
    fn of(pfcr: u32) -> Format {
        [Format::Argb8888, Format::Rgb888, Format::Rgb565, Format::Argb1555, Format::Argb4444, Format::L8, Format::Al44, Format::Al88]
            [(pfcr & 7) as usize]
    }

    fn bytes(self) -> usize {
        match self {
            Format::Argb8888 => 4,
            Format::Rgb888 => 3,
            Format::Rgb565 | Format::Argb1555 | Format::Argb4444 | Format::Al88 => 2,
            Format::L8 | Format::Al44 => 1,
        }
    }

    /// A pixel's colour, RGB, from its bytes.
    fn rgb(self, bytes: &[u8], clut: &[u32]) -> u32 {
        let five = |v: u32| (v << 3 | v >> 2) & 0xFF;
        let six = |v: u32| (v << 2 | v >> 4) & 0xFF;
        let four = |v: u32| v << 4 | v;
        let half = || u32::from(u16::from_le_bytes([bytes[0], bytes[1]]));
        match self {
            Format::Argb8888 | Format::Rgb888 => u32::from(bytes[2]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[0]),
            Format::Rgb565 => {
                let v = half();
                five(v >> 11) << 16 | six(v >> 5 & 0x3F) << 8 | five(v & 0x1F)
            }
            Format::Argb1555 => {
                let v = half();
                five(v >> 10 & 0x1F) << 16 | five(v >> 5 & 0x1F) << 8 | five(v & 0x1F)
            }
            Format::Argb4444 => {
                let v = half();
                four(v >> 8 & 0xF) << 16 | four(v >> 4 & 0xF) << 8 | four(v & 0xF)
            }
            Format::L8 => clut[bytes[0] as usize],
            Format::Al44 => clut[(bytes[0] & 0xF) as usize],
            Format::Al88 => clut[bytes[0] as usize],
        }
    }
}

/// What the panel shows: its size, and its pixels — as indices into a
/// palette, when one L8 layer covers the whole of it, or as colours.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    pub width: usize,
    pub height: usize,
    pub pixels: Pixels,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pixels {
    /// A byte a pixel, and the 256 colours, RGB, they index.
    Indexed { indices: Vec<u8>, palette: Vec<u32> },
    /// RGB, three bytes a pixel.
    Rgb(Vec<u8>),
}

impl Default for Ltdc {
    fn default() -> Self {
        let mut ltdc = Ltdc { global: [0; 0x14], layers: Default::default(), frames: 0 };
        ltdc.global[(GCR / 4) as usize] = GCR_RESET;
        ltdc
    }
}

impl Ltdc {
    pub const BASE: u32 = 0x5000_1000;
    pub const SIZE: u32 = 0x1000;

    pub fn contains(addr: u32) -> bool {
        (Self::BASE..Self::BASE + Self::SIZE).contains(&addr)
    }

    /// The layer whose registers `offset` is in, and the register's
    /// offset in it.
    fn layer(offset: u32) -> Option<(usize, u32)> {
        LAYERS.iter().position(|&base| (base..base + LAYER_SIZE).contains(&offset)).map(|n| (n, offset - LAYERS[n]))
    }

    pub fn read(&self, offset: u32) -> u32 {
        if let Some((n, at)) = Self::layer(offset) {
            // The CLUT's port is write-only.
            return if at == CLUTWR { 0 } else { self.layers[n].shadow[(at / 4) as usize] };
        }
        match offset {
            // SRCR's bits clear once the reload is done, which it is.
            SRCR => 0,
            // CDSR: in the vertical blank, ready for a reload.
            CDSR => 1 << 1,
            offset => self.global.get((offset / 4) as usize).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, offset: u32, value: u32) {
        if let Some((n, at)) = Self::layer(offset) {
            let layer = &mut self.layers[n];
            if at == CLUTWR {
                layer.clut[(value >> 24) as usize] = value & 0xFF_FFFF;
            } else {
                layer.shadow[(at / 4) as usize] = value;
            }
            return;
        }
        match offset {
            // IMR or VBR: the shadow registers take effect.
            SRCR if value & 3 != 0 => {
                for layer in &mut self.layers {
                    layer.active = layer.shadow;
                }
                self.frames += 1;
            }
            SRCR => {}
            offset => {
                if let Some(register) = self.global.get_mut((offset / 4) as usize) {
                    *register = value;
                }
            }
        }
    }

    pub fn reset(&mut self) {
        *self = Ltdc { frames: self.frames, ..Ltdc::default() };
    }

    fn global(&self, offset: u32) -> u32 {
        self.global[(offset / 4) as usize]
    }

    /// The active area's size: from the back porch's end to the active
    /// width's, each way. `None` while the controller is off.
    pub fn size(&self) -> Option<(usize, usize)> {
        if self.global(GCR) & 1 == 0 {
            return None;
        }
        let (bp, aw) = (self.global(BPCR), self.global(AWCR));
        let width = (aw >> 16 & 0xFFF).saturating_sub(bp >> 16 & 0xFFF) as usize;
        let height = (aw & 0x7FF).saturating_sub(bp & 0x7FF) as usize;
        (width > 0 && height > 0).then_some((width, height))
    }

    /// What the panel shows, its frames read from memory by `read`
    /// (`address`, `length`), which answers `None` where nothing is.
    /// `None` while the controller is off.
    pub fn scan(&self, mut read: impl FnMut(u32, usize) -> Option<Vec<u8>>) -> Option<Screen> {
        let (width, height) = self.size()?;
        let (h0, v0) = (self.global(BPCR) >> 16 & 0xFFF, self.global(BPCR) & 0x7FF);
        let background = self.global(BCCR) & 0xFF_FFFF;

        // The layers enabled, each with its window in the active area.
        let shown: Vec<(&Layer, Format, [usize; 4])> = self
            .layers
            .iter()
            .filter(|layer| layer.active[CR] & 1 != 0)
            .map(|layer| {
                let (wh, wv) = (layer.active[WHPCR], layer.active[WVPCR]);
                let x0 = ((wh & 0xFFF) as usize).saturating_sub(h0 as usize + 1);
                let x1 = ((wh >> 16 & 0xFFF) as usize).saturating_sub(h0 as usize).min(width);
                let y0 = ((wv & 0x7FF) as usize).saturating_sub(v0 as usize + 1);
                let y1 = ((wv >> 16 & 0x7FF) as usize).saturating_sub(v0 as usize).min(height);
                (layer, Format::of(layer.active[PFCR]), [x0, x1, y0, y1])
            })
            .collect();

        // One L8 layer over the whole panel, its CLUT on: Doom's case, and
        // sent as it is, a byte a pixel.
        if let [(layer, Format::L8, [0, x1, 0, y1])] = shown[..] {
            if x1 == width && y1 == height && layer.active[CR] & 1 << 4 != 0 {
                let mut indices = Vec::with_capacity(width * height);
                for y in 0..height {
                    indices.extend(Self::line(layer, y, width, 1, &mut read));
                }
                return Some(Screen { width, height, pixels: Pixels::Indexed { indices, palette: layer.clut.clone() } });
            }
        }

        let mut rgb = vec![0u32; width * height];
        rgb.fill(background);
        for (layer, format, [x0, x1, y0, y1]) in shown {
            let bytes = format.bytes();
            let identity: Vec<u32>;
            let clut = if layer.active[CR] & 1 << 4 != 0 {
                &layer.clut[..]
            } else {
                // A CLUT turned off passes the index through as grey.
                identity = (0..256u32).map(|i| i << 16 | i << 8 | i).collect();
                &identity[..]
            };
            for y in y0..y1 {
                let line = Self::line(layer, y - y0, x1.saturating_sub(x0), bytes, &mut read);
                for (x, pixel) in line.chunks_exact(bytes).enumerate() {
                    rgb[y * width + x0 + x] = format.rgb(pixel, clut);
                }
            }
        }
        let pixels = rgb.iter().flat_map(|c| [(c >> 16) as u8, (c >> 8) as u8, *c as u8]).collect();
        Some(Screen { width, height, pixels: Pixels::Rgb(pixels) })
    }

    /// The `n`th line of a layer's frame, `pixels` of `bytes` each: from
    /// CFBAR, a pitch apart, as many lines as CFBLNR says; zeros where it
    /// runs out or memory does not answer.
    fn line(layer: &Layer, n: usize, pixels: usize, bytes: usize, read: &mut impl FnMut(u32, usize) -> Option<Vec<u8>>) -> Vec<u8> {
        let length = pixels * bytes;
        let pitch = (layer.active[CFBLR] >> 16 & 0x1FFF) as usize;
        if n >= (layer.active[CFBLNR] & 0x7FF) as usize {
            return vec![0; length];
        }
        let address = layer.active[CFBAR].wrapping_add((n * pitch) as u32);
        let mut line = read(address, length).unwrap_or_default();
        line.resize(length, 0);
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A controller set up as Doom's port sets it: 320x200 behind a back
    /// porch of one each way, one L8 layer over all of it at `frame`.
    fn doom(frame: u32) -> Ltdc {
        let mut ltdc = Ltdc::default();
        ltdc.write(BPCR, 1 << 16 | 1);
        ltdc.write(AWCR, 321 << 16 | 201);
        let layer = LAYERS[0];
        ltdc.write(layer + 0x04, 321 << 16 | 2);
        ltdc.write(layer + 0x08, 201 << 16 | 2);
        ltdc.write(layer + 0x10, 5);
        ltdc.write(layer + 0x28, frame);
        ltdc.write(layer + 0x2C, 320 << 16 | 327);
        ltdc.write(layer + 0x30, 200);
        ltdc.write(layer, 1 << 4 | 1);
        ltdc.write(layer + CLUTWR, 7 << 24 | 0x12_3456);
        ltdc.write(SRCR, 1);
        ltdc.write(GCR, GCR_RESET | 1);
        ltdc
    }

    fn memory(base: u32, bytes: &[u8]) -> impl FnMut(u32, usize) -> Option<Vec<u8>> + '_ {
        move |address, length| {
            let at = address.checked_sub(base)? as usize;
            bytes.get(at..at + length).map(<[u8]>::to_vec)
        }
    }

    #[test]
    fn an_l8_layer_over_the_panel_is_its_indices_and_its_clut() {
        let ltdc = doom(0x3000_0000);
        let frame: Vec<u8> = (0..64000u32).map(|i| (i % 251) as u8).collect();
        let screen = ltdc.scan(memory(0x3000_0000, &frame)).unwrap();
        assert_eq!((screen.width, screen.height), (320, 200));
        let Pixels::Indexed { indices, palette } = screen.pixels else { panic!("not indexed") };
        assert_eq!(indices, frame);
        assert_eq!(palette[7], 0x12_3456);
    }

    #[test]
    fn a_layer_moves_to_a_new_frame_only_when_reloaded() {
        let mut ltdc = doom(0x3000_0000);
        let frames = ltdc.frames;
        ltdc.write(LAYERS[0] + 0x28, 0x3001_0000);
        let both: Vec<u8> = [vec![1u8; 0x1_0000], vec![2u8; 64000]].concat();
        let index = |ltdc: &Ltdc| match ltdc.scan(memory(0x3000_0000, &both)).unwrap().pixels {
            Pixels::Indexed { indices, .. } => indices[0],
            Pixels::Rgb(_) => panic!("not indexed"),
        };
        assert_eq!(index(&ltdc), 1, "still showing the first frame");
        ltdc.write(SRCR, 2);
        assert_eq!(index(&ltdc), 2);
        assert_eq!(ltdc.frames, frames + 1);
    }

    #[test]
    fn rgb565_is_widened_to_eight_bits_a_channel() {
        assert_eq!(Format::Rgb565.rgb(&0xF800u16.to_le_bytes(), &[]), 0xFF_0000);
        assert_eq!(Format::Rgb565.rgb(&0x07E0u16.to_le_bytes(), &[]), 0x00_FF00);
    }
}
