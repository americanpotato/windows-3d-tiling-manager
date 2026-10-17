//! CPU-side BGRA images, a tiny QOI-style codec for screenshot files and mip generation.

/// Tightly packed 8-bit BGRA pixels, top row first.
#[derive(Clone)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

impl Image {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![0; (w * h * 4) as usize],
        }
    }

    pub fn set_opaque(&mut self) {
        for p in self.px.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }

    /// Next mip level (2x2 box filter). Odd edges are clamped.
    pub fn half(&self) -> Image {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let mut out = Image::new(w, h);
        let sw = self.w as usize;
        for y in 0..h as usize {
            let y0 = (y * 2).min(self.h as usize - 1);
            let y1 = (y * 2 + 1).min(self.h as usize - 1);
            for x in 0..w as usize {
                let x0 = (x * 2).min(sw - 1);
                let x1 = (x * 2 + 1).min(sw - 1);
                let o = (y * w as usize + x) * 4;
                for c in 0..4 {
                    let s = self.px[(y0 * sw + x0) * 4 + c] as u32
                        + self.px[(y0 * sw + x1) * 4 + c] as u32
                        + self.px[(y1 * sw + x0) * 4 + c] as u32
                        + self.px[(y1 * sw + x1) * 4 + c] as u32;
                    out.px[o + c] = ((s + 2) / 4) as u8;
                }
            }
        }
        out
    }
}

// --- QOI-style codec -------------------------------------------------------
// Same op set as the QOI spec, applied to BGRA byte order. Screenshots shrink
// 4-10x and decode at several hundred MB/s, keeping overlay startup quick.

const MAGIC: &[u8; 4] = b"W3DQ";
const OP_INDEX: u8 = 0x00;
const OP_DIFF: u8 = 0x40;
const OP_LUMA: u8 = 0x80;
const OP_RUN: u8 = 0xc0;
const OP_RGB: u8 = 0xfe;
const OP_RGBA: u8 = 0xff;

#[inline]
fn hash(p: [u8; 4]) -> usize {
    (p[0] as usize * 3 + p[1] as usize * 5 + p[2] as usize * 7 + p[3] as usize * 11) % 64
}

pub fn qoi_encode(img: &Image) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.px.len() / 4 + 16);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&img.w.to_le_bytes());
    out.extend_from_slice(&img.h.to_le_bytes());
    let mut index = [[0u8; 4]; 64];
    let mut prev = [0u8, 0, 0, 255];
    let mut run = 0u8;
    let n = img.px.len() / 4;
    for i in 0..n {
        let s = &img.px[i * 4..i * 4 + 4];
        let px = [s[0], s[1], s[2], s[3]];
        if px == prev {
            run += 1;
            if run == 62 || i == n - 1 {
                out.push(OP_RUN | (run - 1));
                run = 0;
            }
            continue;
        }
        if run > 0 {
            out.push(OP_RUN | (run - 1));
            run = 0;
        }
        let h = hash(px);
        if index[h] == px {
            out.push(OP_INDEX | h as u8);
        } else {
            index[h] = px;
            if px[3] == prev[3] {
                let vr = px[0].wrapping_sub(prev[0]) as i8;
                let vg = px[1].wrapping_sub(prev[1]) as i8;
                let vb = px[2].wrapping_sub(prev[2]) as i8;
                let vg_r = vr.wrapping_sub(vg);
                let vg_b = vb.wrapping_sub(vg);
                if (-2..=1).contains(&vr) && (-2..=1).contains(&vg) && (-2..=1).contains(&vb) {
                    out.push(
                        OP_DIFF
                            | (((vr + 2) as u8) << 4)
                            | (((vg + 2) as u8) << 2)
                            | (vb + 2) as u8,
                    );
                } else if (-8..=7).contains(&vg_r)
                    && (-32..=31).contains(&vg)
                    && (-8..=7).contains(&vg_b)
                {
                    out.push(OP_LUMA | (vg + 32) as u8);
                    out.push((((vg_r + 8) as u8) << 4) | (vg_b + 8) as u8);
                } else {
                    out.extend_from_slice(&[OP_RGB, px[0], px[1], px[2]]);
                }
            } else {
                out.extend_from_slice(&[OP_RGBA, px[0], px[1], px[2], px[3]]);
            }
        }
        prev = px;
    }
    out
}

pub fn qoi_decode(data: &[u8]) -> Option<Image> {
    if data.len() < 12 || &data[0..4] != MAGIC {
        return None;
    }
    let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let n = (w * h) as usize;
    let mut px_out = Vec::with_capacity(n * 4);
    let mut index = [[0u8; 4]; 64];
    let mut px = [0u8, 0, 0, 255];
    let mut run = 0u32;
    let mut p = 12usize;
    for _ in 0..n {
        if run > 0 {
            run -= 1;
        } else {
            let b1 = *data.get(p)?;
            p += 1;
            if b1 == OP_RGB {
                px[0..3].copy_from_slice(data.get(p..p + 3)?);
                p += 3;
            } else if b1 == OP_RGBA {
                px.copy_from_slice(data.get(p..p + 4)?);
                p += 4;
            } else {
                match b1 & 0xc0 {
                    OP_INDEX => px = index[b1 as usize],
                    OP_DIFF => {
                        px[0] = px[0].wrapping_add(((b1 >> 4) & 3).wrapping_sub(2));
                        px[1] = px[1].wrapping_add(((b1 >> 2) & 3).wrapping_sub(2));
                        px[2] = px[2].wrapping_add((b1 & 3).wrapping_sub(2));
                    }
                    OP_LUMA => {
                        let b2 = *data.get(p)?;
                        p += 1;
                        let vg = (b1 & 0x3f).wrapping_sub(32);
                        px[0] =
                            px[0].wrapping_add(vg.wrapping_sub(8).wrapping_add((b2 >> 4) & 0x0f));
                        px[1] = px[1].wrapping_add(vg);
                        px[2] = px[2].wrapping_add(vg.wrapping_sub(8).wrapping_add(b2 & 0x0f));
                    }
                    _ => run = (b1 & 0x3f) as u32,
                }
            }
            index[hash(px)] = px;
        }
        px_out.extend_from_slice(&px);
    }
    Some(Image { w, h, px: px_out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qoi_roundtrip() {
        let mut img = Image::new(97, 53);
        let mut seed = 12345u32;
        for (i, b) in img.px.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            // Mix of runs, small deltas and random noise.
            *b = match (i / 4) % 7 {
                0 | 1 => 40,
                2 => (i % 5) as u8,
                3 => (seed >> 24) as u8,
                _ => ((i / 4) % 256) as u8,
            };
        }
        let enc = qoi_encode(&img);
        let dec = qoi_decode(&enc).expect("decode");
        assert_eq!((dec.w, dec.h), (img.w, img.h));
        assert!(dec.px == img.px);
    }

    #[test]
    fn qoi_roundtrip_flat() {
        let mut img = Image::new(300, 2);
        img.set_opaque();
        let dec = qoi_decode(&qoi_encode(&img)).unwrap();
        assert!(dec.px == img.px);
    }
}
