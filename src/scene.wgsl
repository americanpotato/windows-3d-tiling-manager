
// ---------------------------------------------------------------- animated scenes
// Appended to shader.wgsl (render.rs). Drawn behind the cards like a panorama: a sky at
// infinity, and below the horizon a ground (water, sand, meadow, clouds) at a real height, so
// it moves under you as you walk. Colours are written as seen on screen (sRGB) and made
// linear at the end. Scene numbers are `background::SCENES`' `index`.

fn h21(p: vec2<f32>) -> f32 {
    var q = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    q += dot(q, q.yzx + 33.33);
    return fract((q.x + q.y) * q.z);
}

fn h31(p: vec3<f32>) -> f32 {
    var q = fract(p * 0.1031);
    q += dot(q, q.zyx + 31.32);
    return fract((q.x + q.y) * q.z);
}

fn n2(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    return mix(mix(h21(i), h21(i + vec2<f32>(1.0, 0.0)), u.x),
               mix(h21(i + vec2<f32>(0.0, 1.0)), h21(i + vec2<f32>(1.0, 1.0)), u.x), u.y);
}

fn n3(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = mix(mix(h31(i), h31(i + vec3<f32>(1.0, 0.0, 0.0)), u.x),
                mix(h31(i + vec3<f32>(0.0, 1.0, 0.0)), h31(i + vec3<f32>(1.0, 1.0, 0.0)), u.x), u.y);
    let b = mix(mix(h31(i + vec3<f32>(0.0, 0.0, 1.0)), h31(i + vec3<f32>(1.0, 0.0, 1.0)), u.x),
                mix(h31(i + vec3<f32>(0.0, 1.0, 1.0)), h31(i + vec3<f32>(1.0, 1.0, 1.0)), u.x), u.y);
    return mix(a, b, u.z);
}

fn fbm2(p: vec2<f32>, octaves: i32) -> f32 {
    var s = 0.0;
    var a = 0.5;
    var q = p;
    for (var i = 0; i < octaves; i++) {
        s += a * n2(q);
        q = q * 2.03 + vec2<f32>(1.7, 9.2);
        a *= 0.5;
    }
    return s;
}

fn fbm3(p: vec3<f32>, octaves: i32) -> f32 {
    var s = 0.0;
    var a = 0.5;
    var q = p;
    for (var i = 0; i < octaves; i++) {
        s += a * n3(q);
        q = q * 2.03 + vec3<f32>(1.7, 9.2, 4.1);
        a *= 0.5;
    }
    return s;
}

// Twinkling stars, `density` the share of cells that have one.
fn stars(d: vec3<f32>, t: f32, density: f32) -> f32 {
    let q = d * 90.0;
    let id = floor(q);
    let h = h31(id);
    let off = vec3<f32>(h31(id + 1.7), h31(id + 3.1), h31(id + 5.3)) - 0.5;
    let r = length(fract(q) - 0.5 - off * 0.6);
    let tw = 0.65 + 0.35 * sin(t * (0.7 + h * 2.0) + h * 60.0);
    return step(1.0 - density, h) * tw * smoothstep(0.13, 0.0, r) * (0.4 + 0.6 * fract(h * 13.0));
}

// Angle between two unit directions.
fn ang(a: vec3<f32>, b: vec3<f32>) -> f32 {
    return acos(clamp(dot(a, b), -1.0, 1.0));
}

// Where `d` is on a disc of the sky around `c`, in units of `r` (x right, y up).
fn local(d: vec3<f32>, c: vec3<f32>, r: f32) -> vec2<f32> {
    let right = normalize(cross(c, vec3<f32>(0.0, 1.0, 0.0)));
    let up = cross(right, c);
    return vec2<f32>(dot(d, right), dot(d, up)) / r;
}

// Gentle waves: the slope of the water at `p`.
fn ripple(p: vec2<f32>, t: f32) -> vec2<f32> {
    return vec2<f32>(
        sin(p.x * 1.7 + t * 0.9) + 0.6 * sin(p.x * 0.9 - p.y * 1.3 + t * 0.7) + 0.4 * sin(p.x * 3.1 + p.y * 2.3 - t * 1.3),
        cos(p.y * 1.5 + t * 0.8) + 0.6 * sin(p.y * 1.1 + p.x * 0.8 - t * 0.6) + 0.4 * cos(p.y * 2.7 - p.x * 3.3 + t * 1.1));
}

// The ground hit by `d` (going down) on the plane at height `h`: its xz and distance.
fn ground_at(d: vec3<f32>, h: f32) -> vec3<f32> {
    let dist = (g.cam_pos.y - h) / max(-d.y, 1e-4);
    let p = g.cam_pos.xz + d.xz * dist;
    return vec3<f32>(p, dist);
}

// The direction reflected off water seen along `d`, the waves fading with distance.
fn water_dir(d: vec3<f32>, t: f32, amp: f32) -> vec3<f32> {
    let gp = ground_at(d, 0.0);
    let s = ripple(gp.xy * 0.8, t) * amp / (1.0 + gp.z * 0.12);
    let n = normalize(vec3<f32>(s.x, 1.0, s.y));
    var r = reflect(d, n);
    r.y = max(r.y, 0.002);
    return normalize(r);
}

fn fresnel(d: vec3<f32>) -> f32 {
    return 0.04 + 0.96 * pow(1.0 - clamp(-d.y, 0.0, 1.0), 5.0);
}

// 0: Twin moons. Mauve dusk over a calm sea, a peach moon and a mint crescent.
fn sky0(d: vec3<f32>, t: f32) -> vec3<f32> {
    let y = max(d.y, 0.0);
    var c = mix(vec3<f32>(0.66, 0.44, 0.62), vec3<f32>(0.16, 0.16, 0.36), pow(y, 0.45));
    c = mix(c, vec3<f32>(0.05, 0.06, 0.16), smoothstep(0.35, 1.0, y));
    c += vec3<f32>(0.9, 0.92, 1.0) * stars(d, t, 0.10) * smoothstep(0.06, 0.45, y);
    // Thin clouds low in the sky.
    let w = fbm3(vec3<f32>(d.x * 2.5 + t * 0.012, d.y * 9.0, d.z * 2.5), 4);
    let cl = smoothstep(0.52, 0.8, w) * smoothstep(0.0, 0.08, y) * smoothstep(0.45, 0.12, y);
    c = mix(c, vec3<f32>(0.95, 0.62, 0.66), cl * 0.55);
    let ca = normalize(vec3<f32>(0.45, 0.30, -1.0));
    let a = ang(d, ca);
    let ra = 0.095;
    c += vec3<f32>(1.0, 0.75, 0.6) * exp(-max(a - ra, 0.0) * 10.0) * 0.22;
    let la = local(d, ca, ra);
    let surf = vec3<f32>(1.0, 0.88, 0.74) * (0.82 + 0.18 * fbm3(d * 45.0, 3)) * (0.75 + 0.25 * sqrt(max(1.0 - dot(la, la), 0.0)));
    c = mix(c, surf, smoothstep(ra, ra - 0.003, a));
    let cb = normalize(vec3<f32>(-0.9, 0.5, -0.45));
    let b = ang(d, cb);
    let rb = 0.06;
    c += vec3<f32>(0.5, 1.0, 0.85) * exp(-max(b - rb, 0.0) * 14.0) * 0.25;
    let lb = local(d, cb, rb);
    // Lit where it is outside the shadow disc offset to the left.
    let lit = smoothstep(0.92, 1.02, length(lb - vec2<f32>(-0.42, 0.1)));
    let mint = vec3<f32>(0.75, 1.0, 0.9) * (0.88 + 0.12 * fbm3(d * 60.0, 3));
    c = mix(c, mix(c * 0.75 + vec3<f32>(0.02, 0.05, 0.05), mint, lit), smoothstep(rb, rb - 0.002, b));
    return c;
}

// 1: Aurora meadow. Green and violet curtains over a dark meadow of glowing specks.
fn grad1(y: f32) -> vec3<f32> {
    let c = mix(vec3<f32>(0.2, 0.3, 0.38), vec3<f32>(0.08, 0.1, 0.22), pow(y, 0.5));
    return mix(c, vec3<f32>(0.03, 0.04, 0.12), smoothstep(0.4, 1.0, y));
}

fn sky1(d: vec3<f32>, t: f32) -> vec3<f32> {
    let y = max(d.y, 0.0);
    var c = grad1(y);
    c += vec3<f32>(0.85, 0.95, 1.0) * stars(d, t, 0.12) * smoothstep(0.05, 0.4, y);
    for (var i = 0; i < 3; i++) {
        let fi = f32(i);
        let uv = d.xz * ((1.0 + fi * 0.5) / max(y, 0.08));
        let band = uv.x * 0.9 + uv.y * 0.35 + fbm2(uv * 0.35 + vec2<f32>(t * 0.015, fi * 7.0), 3) * 2.2 + sin(t * 0.05 + fi * 2.0) * 0.8;
        let curtain = smoothstep(0.1, 0.95, sin(band)) * (0.6 + 0.4 * smoothstep(0.95, 0.6, sin(band)));
        let rays = 0.7 + 0.3 * n2(vec2<f32>(band * 14.0, t * 0.2 + fi));
        let fade = smoothstep(0.04, 0.22, y) * smoothstep(0.9, 0.4, y);
        let col = mix(vec3<f32>(0.35, 1.0, 0.68), vec3<f32>(0.7, 0.45, 1.0), smoothstep(0.2, 0.7, y + fi * 0.12));
        c += col * curtain * rays * fade * (0.42 - fi * 0.1);
    }
    return c;
}

fn ground1(d: vec3<f32>, t: f32) -> vec3<f32> {
    let gp = ground_at(d, 0.0);
    let p = gp.xy;
    let fade = 1.0 / (1.0 + gp.z * 0.15);
    var c = vec3<f32>(0.07, 0.13, 0.11) * (0.7 + 0.6 * fbm2(p * 0.4, 3));
    // A little of the aurora on the wet ground.
    c += sky1(normalize(vec3<f32>(d.x, -d.y, d.z)), t) * 0.2;
    let q = p * 1.3;
    let id = floor(q);
    let h = h21(id);
    let off = vec2<f32>(h21(id + 4.1), h21(id + 7.7)) - 0.5;
    let r = length(fract(q) - 0.5 - off * 0.7);
    let pulse = 0.5 + 0.5 * sin(t * (0.6 + h) + h * 30.0);
    let spec = step(0.72, h) * smoothstep(0.09, 0.0, r) * pulse * fade;
    c += mix(vec3<f32>(0.3, 1.0, 0.75), vec3<f32>(1.0, 0.85, 0.4), step(0.92, h)) * spec * 1.4;
    return mix(c, grad1(0.0), 1.0 - exp(-gp.z * 0.04));
}

// The sky right at the horizon in the direction of `d`: grounds fade into it with distance so
// there is no seam where they meet the sky.
fn flat(d: vec3<f32>) -> vec3<f32> {
    return normalize(vec3<f32>(d.x, 0.0005, d.z));
}

// Soft dots drifting through the air in all directions (snow, petals, motes): `cells` around
// the horizon, `fall` how fast they sink (negative rises), `sway` side to side, `size` in cell
// units, `keep` the share of empty cells. Returns the dot's strength.
fn drift(d: vec3<f32>, t: f32, cells: f32, fall: f32, sway: f32, size: f32, keep: f32, seed: f32) -> f32 {
    let s = cells / (2.0 * PI);
    let az = atan2(d.x, -d.z);
    let el = asin(clamp(d.y, -1.0, 1.0));
    let uv = vec2<f32>(az * s, el * s + t * fall);
    let id = floor(uv);
    let h = h21(id + seed);
    let off = vec2<f32>(h21(id + 2.3 + seed), h21(id + 8.1 + seed)) - 0.5;
    let f = fract(uv) - 0.5 - off * 0.5 - vec2<f32>(sin(t * (0.5 + h) + h * 40.0) * sway, 0.0);
    return step(keep, h) * smoothstep(size, size * 0.25, length(f)) * smoothstep(0.92, 0.7, abs(d.y));
}

// 2: Amber sea. A big soft sun low over a warm sea: long evenings, nostalgia.
fn sky2(d: vec3<f32>, t: f32) -> vec3<f32> {
    let y = max(d.y, 0.0);
    var c = mix(vec3<f32>(1.0, 0.74, 0.46), vec3<f32>(0.93, 0.5, 0.48), smoothstep(0.0, 0.22, y));
    c = mix(c, vec3<f32>(0.36, 0.28, 0.52), smoothstep(0.18, 0.85, y));
    c += vec3<f32>(1.0, 0.95, 0.9) * stars(d, t, 0.04) * smoothstep(0.55, 0.95, y) * 0.5;
    let cs = normalize(vec3<f32>(0.2, 0.06, -1.0));
    let s = ang(d, cs);
    c += vec3<f32>(1.0, 0.7, 0.4) * (exp(-s * 3.0) * 0.35 + exp(-s * 12.0) * 0.3);
    c = mix(c, vec3<f32>(1.0, 0.9, 0.7), smoothstep(0.15, 0.14, s));
    // Long thin clouds drifting across, darker against the sun.
    let w = fbm3(vec3<f32>(d.x * 1.6 + t * 0.006, d.y * 22.0, d.z * 1.6), 4);
    let cl = smoothstep(0.5, 0.75, w) * smoothstep(0.015, 0.06, y) * smoothstep(0.4, 0.1, y);
    let lit = exp(-s * 4.0);
    c = mix(c, mix(vec3<f32>(0.85, 0.48, 0.5), vec3<f32>(0.62, 0.32, 0.42), lit), cl * 0.7);
    return c;
}

// 3: Rain veil. Soft rain over a misty lake under a grey-blue sky, a warm glow far off: cozy,
// the feeling of being indoors while it rains.
fn sky3(d: vec3<f32>, t: f32) -> vec3<f32> {
    let y = max(d.y, 0.0);
    var c = mix(vec3<f32>(0.6, 0.64, 0.7), vec3<f32>(0.34, 0.38, 0.48), smoothstep(0.0, 0.6, y));
    let q = vec3<f32>(d.x / (y + 0.25), 0.0, d.z / (y + 0.25)) * 0.8 + vec3<f32>(t * 0.01, t * 0.002, 0.0);
    let n = fbm3(q, 5);
    c = mix(c, vec3<f32>(0.28, 0.31, 0.4), smoothstep(0.45, 0.75, n) * 0.6 * smoothstep(0.0, 0.25, y));
    c = mix(c, vec3<f32>(0.7, 0.72, 0.78), smoothstep(0.45, 0.2, n) * 0.25);
    let cg = normalize(vec3<f32>(-0.8, 0.0, -0.6));
    c += vec3<f32>(1.0, 0.62, 0.32) * exp(-ang(d, cg) * 1.6) * exp(-y * 6.0) * 0.45;
    return c;
}

fn ground3(d: vec3<f32>, t: f32) -> vec3<f32> {
    let gp = ground_at(d, 0.0);
    let p = gp.xy;
    // Rings from drops, each in its own cell.
    let cs = 1.5;
    let id = floor(p / cs);
    let h = h21(id);
    let ph = fract(t * 0.7 + h);
    let ctr = (id + 0.5 + (vec2<f32>(h, h21(id + 5.0)) - 0.5) * 0.4) * cs;
    let v = p - ctr;
    let r = length(v);
    let rr = ph * 0.42;
    let ring = sin((r - rr) * 40.0) * smoothstep(0.07, 0.0, abs(r - rr)) * (1.0 - ph);
    var sl = ripple(p * 0.6, t) * 0.012 + v / max(r, 1e-3) * ring * 0.25;
    sl /= 1.0 + gp.z * 0.15;
    let n = normalize(vec3<f32>(sl.x, 1.0, sl.y));
    var rd = reflect(d, n);
    rd.y = max(rd.y, 0.002);
    let c = sky3(normalize(rd), t) * (0.35 + 0.55 * fresnel(d)) + vec3<f32>(0.04, 0.05, 0.06);
    return mix(c, sky3(flat(d), t), 1.0 - exp(-gp.z * 0.03));
}

// Falling rain: thin streaks in the air, over sky and lake alike.
fn rain(d: vec3<f32>, t: f32) -> f32 {
    var a = 0.0;
    for (var i = 0; i < 2; i++) {
        let fi = f32(i);
        let s = (70.0 + fi * 50.0) / (2.0 * PI);
        let uv = vec2<f32>(atan2(d.x, -d.z) * s, asin(clamp(d.y, -1.0, 1.0)) * s * 0.12 + t * (1.6 - fi * 0.5));
        let id = floor(uv);
        let h = h21(id + fi * 9.0);
        let f = fract(uv) - vec2<f32>(0.5 + (h21(id + 3.3) - 0.5) * 0.6, 0.5);
        a += step(0.55, h) * smoothstep(0.04, 0.0, abs(f.x)) * smoothstep(0.5, 0.1, abs(f.y)) * (0.22 - fi * 0.08);
    }
    return a * smoothstep(0.65, 0.25, abs(d.y));
}

// 4: Deep blue. Under the sea: light from the surface, slow rays, drifting motes, a sandy
// floor fading into blue: calm, inward.
fn water4(d: vec3<f32>, t: f32) -> vec3<f32> {
    var c = mix(vec3<f32>(0.02, 0.1, 0.17), vec3<f32>(0.05, 0.3, 0.4), smoothstep(-0.6, 0.5, d.y));
    // The bright window of the surface overhead, rippling.
    let sw = smoothstep(0.6, 0.8, d.y);
    let cau = fbm2(d.xz / max(d.y, 0.3) * 3.0 + vec2<f32>(t * 0.05, t * 0.03), 3);
    c = mix(c, vec3<f32>(0.5, 0.85, 0.9) * (0.75 + 0.5 * cau), sw * 0.6);
    // Rays slanting down from the surface, slowly swaying.
    let az = atan2(d.x, -d.z);
    let ray = fbm2(vec2<f32>(az * 6.0 + d.y * 1.5 + sin(t * 0.1) * 0.3, t * 0.04), 3);
    c += vec3<f32>(0.4, 0.8, 0.85) * smoothstep(0.5, 0.8, ray) * smoothstep(-0.2, 0.5, d.y) * smoothstep(0.85, 0.5, d.y) * 0.22;
    return c;
}

fn ground4(d: vec3<f32>, t: f32) -> vec3<f32> {
    let w = water4(d, t);
    if (d.y > -0.02) {
        return w;
    }
    let gp = ground_at(d, -5.0);
    let p = gp.xy;
    let dunes = 0.8 + 0.2 * sin(p.x * 0.5 + fbm2(p * 0.1, 3) * 6.0);
    // Caustics: bright wavy lines of light on the sand.
    let q = p * 0.7 + vec2<f32>(t * 0.1, t * 0.07);
    let k = abs(fbm2(q, 3) - 0.5) + abs(fbm2(q * 1.3 + 7.0 - vec2<f32>(t * 0.08, 0.0), 3) - 0.5);
    let cau = smoothstep(0.12, 0.0, k) / (1.0 + gp.z * 0.1);
    let c = vec3<f32>(0.5, 0.62, 0.55) * dunes + vec3<f32>(0.6, 0.9, 0.85) * cau * 0.5;
    return mix(c, w, 1.0 - exp(-gp.z * 0.07));
}

// 5: Salt mirror. A bright day on a flooded salt flat that mirrors the sky, puffy clouds and
// pink petals on the breeze: light, open, joyful.
fn sky5(d: vec3<f32>, t: f32) -> vec3<f32> {
    let y = max(d.y, 0.0);
    var c = mix(vec3<f32>(0.98, 0.9, 0.9), vec3<f32>(0.6, 0.8, 0.96), smoothstep(0.0, 0.3, y));
    c = mix(c, vec3<f32>(0.36, 0.6, 0.92), smoothstep(0.25, 1.0, y));
    let cs = normalize(vec3<f32>(0.5, 0.75, -0.4));
    c += vec3<f32>(1.0, 0.95, 0.85) * (exp(-ang(d, cs) * 4.0) * 0.25 + smoothstep(0.035, 0.03, ang(d, cs)));
    // Puffy clouds on a layer overhead, lit from above.
    let uv = d.xz / (y + 0.12) * 0.5 + vec2<f32>(t * 0.012, t * 0.005);
    let n = fbm2(uv, 5);
    let lo = fbm2(uv + vec2<f32>(0.08, 0.06), 4);
    let cov = smoothstep(0.5, 0.68, n) * smoothstep(0.0, 0.12, y);
    let cc = mix(vec3<f32>(0.86, 0.8, 0.92), vec3<f32>(1.0, 0.99, 0.97), smoothstep(-0.05, 0.08, n - lo));
    return mix(c, cc, cov * 0.92);
}

fn ground5(d: vec3<f32>, t: f32) -> vec3<f32> {
    let gp = ground_at(d, 0.0);
    let p = gp.xy;
    let near = 1.0 / (1.0 + gp.z * 0.12);
    let sl = ripple(p * 0.4, t * 0.6) * 0.006 * near;
    var rd = reflect(d, normalize(vec3<f32>(sl.x, 1.0, sl.y)));
    rd.y = max(rd.y, 0.002);
    var c = sky5(normalize(rd), t) * 0.9;
    // Faint salt crust ridges under the thin water near you.
    let cell = fract(p * 0.6 + vec2<f32>(fbm2(p * 0.5, 2), fbm2(p * 0.5 + 3.0, 2)) * 0.6) - 0.5;
    let edge = smoothstep(0.43, 0.5, max(abs(cell.x), abs(cell.y)));
    c = mix(c, vec3<f32>(1.0, 0.97, 0.95), edge * 0.08 * near * smoothstep(0.35, 0.6, fbm2(p * 0.3, 2)));
    return mix(c, sky5(flat(d), t), 1.0 - exp(-gp.z * 0.01));
}

// 6: Galaxy rise. A great spiral galaxy over a black, still sea: awe.
fn sky6(d: vec3<f32>, t: f32) -> vec3<f32> {
    let y = max(d.y, 0.0);
    var c = mix(vec3<f32>(0.2, 0.12, 0.3), vec3<f32>(0.03, 0.02, 0.08), smoothstep(0.0, 0.45, y));
    c += vec3<f32>(0.9, 0.92, 1.0) * stars(d, t, 0.22) * smoothstep(0.0, 0.2, y);
    // The band of our own galaxy across the sky.
    let bn = normalize(vec3<f32>(0.3, 0.5, 0.8));
    let bd = abs(dot(d, bn));
    let dust = fbm3(d * 6.0, 4);
    c += vec3<f32>(0.45, 0.4, 0.7) * exp(-bd * 4.0) * (0.15 + 0.45 * dust) * smoothstep(0.0, 0.15, y);
    // The galaxy, tilted, turning very slowly.
    let cg = normalize(vec3<f32>(-0.25, 0.32, -1.0));
    if (dot(d, cg) > 0.6) {
        var l = local(d, cg, 0.5);
        let ct = cos(0.5);
        let st = sin(0.5);
        l = vec2<f32>(ct * l.x - st * l.y, st * l.x + ct * l.y);
        l.y /= 0.42;
        let r = length(l);
        let th = atan2(l.y, l.x) + t * 0.004;
        let arm = 0.5 + 0.5 * cos(2.0 * (th - log(max(r, 0.02)) * 2.6));
        let knots = fbm2(l * 9.0, 3);
        let disc = exp(-r * 1.8) * (0.25 + 0.75 * pow(arm, 3.0) * (0.6 + 0.6 * knots));
        let core = exp(-r * r * 25.0);
        let gc = mix(vec3<f32>(0.55, 0.65, 1.0), vec3<f32>(1.0, 0.6, 0.8), smoothstep(0.62, 0.8, knots) * arm);
        c += gc * disc * 1.1 + vec3<f32>(1.0, 0.86, 0.6) * core * 1.2;
    }
    return c;
}

// 7: Snowfall. A snowy plain at night, snow falling softly, a warm light far off: quiet, safe.
fn sky7(d: vec3<f32>, t: f32) -> vec3<f32> {
    let y = max(d.y, 0.0);
    var c = mix(vec3<f32>(0.32, 0.36, 0.5), vec3<f32>(0.08, 0.1, 0.2), smoothstep(0.0, 0.55, y));
    let cm = normalize(vec3<f32>(0.6, 0.45, -0.7));
    let m = ang(d, cm);
    // The moon behind thin cloud.
    let n = fbm3(vec3<f32>(d.x / (y + 0.3), t * 0.01, d.z / (y + 0.3)) * 1.5, 4);
    c += vec3<f32>(0.7, 0.78, 0.95) * (exp(-m * 5.0) * 0.3 + smoothstep(0.05, 0.045, m) * (0.85 - 0.4 * smoothstep(0.4, 0.7, n)));
    c = mix(c, vec3<f32>(0.38, 0.42, 0.55), smoothstep(0.42, 0.75, n) * 0.5 * smoothstep(0.02, 0.2, y));
    c += vec3<f32>(0.9, 0.95, 1.0) * stars(d, t, 0.08) * smoothstep(0.15, 0.5, y) * smoothstep(0.55, 0.35, n);
    let cw = normalize(vec3<f32>(-0.5, 0.0, -0.85));
    c += vec3<f32>(1.0, 0.68, 0.38) * exp(-ang(d, cw) * 6.0) * exp(-y * 14.0) * 0.4;
    return c;
}

fn ground7(d: vec3<f32>, t: f32) -> vec3<f32> {
    let gp = ground_at(d, 0.0);
    let p = gp.xy;
    let near = 1.0 / (1.0 + gp.z * 0.1);
    let hill = fbm2(p * 0.06, 4);
    let slope = fbm2(p * 0.06 + vec2<f32>(0.05, 0.0), 4) - hill;
    var c = vec3<f32>(0.62, 0.68, 0.82) * (0.85 + slope * 6.0 * near);
    let cell = floor(p * 14.0);
    c += vec3<f32>(0.8, 0.9, 1.0) * step(0.992, h21(cell)) * smoothstep(0.3, 0.05, length(fract(p * 14.0) - 0.5)) * near * (0.5 + 0.5 * sin(t * 1.5 + h21(cell + 1.0) * 40.0));
    return mix(c, sky7(flat(d), t), 1.0 - exp(-gp.z * 0.035));
}

fn sky_of(k: i32, d: vec3<f32>, t: f32) -> vec3<f32> {
    switch k {
        case 1: { return sky1(d, t); }
        case 2: { return sky2(d, t); }
        case 3: { return sky3(d, t); }
        case 4: { return water4(d, t); }
        case 5: { return sky5(d, t); }
        case 6: { return sky6(d, t); }
        case 7: { return sky7(d, t); }
        default: { return sky0(d, t); }
    }
}

@fragment
fn fs_scene(i: SkyOut) -> @location(0) vec4<f32> {
    let n = g.inv_view_proj * vec4<f32>(i.ndc, 0.0, 1.0);
    let f = g.inv_view_proj * vec4<f32>(i.ndc, 1.0, 1.0);
    let d = normalize(f.xyz / f.w - n.xyz / n.w);
    let t = g.misc.x;
    let k = i32(g.bg.y + 0.5);
    var c: vec3<f32>;
    if (d.y >= 0.0 && k != 4) {
        c = sky_of(k, d, t);
    } else {
        switch k {
            case 1: { c = ground1(d, t); }
            case 3: { c = ground3(d, t); }
            case 4: { c = ground4(d, t); }
            case 5: { c = ground5(d, t); }
            case 7: { c = ground7(d, t); }
            default: {
                // Water reflecting the sky: lively under the amber sun, glassy under the galaxy.
                var amp = 0.05;
                var deep = vec3<f32>(0.03, 0.04, 0.09);
                if (k == 2) {
                    amp = 0.03;
                    deep = vec3<f32>(0.08, 0.04, 0.06);
                } else if (k == 6) {
                    amp = 0.012;
                    deep = vec3<f32>(0.0);
                }
                c = sky_of(k, water_dir(d, t, amp), t) * (0.3 + 0.6 * fresnel(d)) + deep;
            }
        }
    }
    // Things in the air, in front of sky and ground alike.
    switch k {
        case 3: { c = mix(c, vec3<f32>(0.85, 0.88, 0.95), rain(d, t)); }
        case 4: {
            c += vec3<f32>(0.6, 0.85, 0.85) * (drift(d, t, 60.0, -0.25, 0.05, 0.1, 0.8, 0.0) * 0.35 + drift(d, t, 110.0, -0.15, 0.04, 0.08, 0.75, 3.0) * 0.2);
        }
        case 5: {
            c = mix(c, vec3<f32>(1.0, 0.72, 0.8), drift(d, t, 50.0, 0.6, 0.12, 0.12, 0.88, 0.0) * 0.85);
            c = mix(c, vec3<f32>(1.0, 0.85, 0.9), drift(d, t, 90.0, 0.4, 0.1, 0.1, 0.85, 4.0) * 0.6);
        }
        case 7: {
            c = mix(c, vec3<f32>(0.95, 0.97, 1.0), drift(d, t, 40.0, 1.0, 0.12, 0.12, 0.8, 0.0) * 0.9);
            c = mix(c, vec3<f32>(0.9, 0.93, 1.0), drift(d, t, 80.0, 0.6, 0.1, 0.1, 0.7, 4.0) * 0.6);
            c = mix(c, vec3<f32>(0.85, 0.9, 1.0), drift(d, t, 150.0, 0.35, 0.08, 0.1, 0.6, 7.0) * 0.4);
        }
        default: {}
    }
    // Dither against banding in the smooth gradients.
    c += (h21(i.clip.xy) - 0.5) / 255.0;
    return vec4<f32>(pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2)) * g.bg.x, 1.0);
}
