//! The app icon, drawn in code: three stacked cards in perspective. Shared with `build.rs`,
//! which embeds it in the exe, so it uses nothing but `std`.

/// The icon at `n` x `n` pixels, BGRA with alpha, top row first.
pub fn bgra(n: u32) -> Vec<u8> {
    let mut px = vec![0u8; (n * n * 4) as usize];
    let s = n as f32 / 32.0;
    let cards = [(3.0, 5.0, 0.55), (8.0, 9.0, 0.8), (13.0, 13.0, 1.0)];
    for (ox, oy, k) in cards {
        let (x0, y0, x1, y1) = (ox * s, oy * s, (ox + 16.0) * s, (oy + 13.0) * s);
        for y in 0..n {
            for x in 0..n {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                if fx < x0 || fx > x1 || fy < y0 || fy > y1 {
                    continue;
                }
                let border = fx - x0 < 1.5 * s
                    || x1 - fx < 1.5 * s
                    || fy - y0 < 1.5 * s
                    || y1 - fy < 1.5 * s;
                let o = ((y * n + x) * 4) as usize;
                let (b, g, r) = if border {
                    (255.0, 190.0, 90.0)
                } else if fy - y0 < 4.0 * s {
                    (120.0, 80.0, 50.0)
                } else {
                    (70.0, 45.0, 30.0)
                };
                px[o..o + 4].copy_from_slice(&[(b * k) as u8, (g * k) as u8, (r * k) as u8, 255]);
            }
        }
    }
    px
}
