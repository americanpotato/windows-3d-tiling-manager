struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    // x: time in seconds, y: 1 to anti-alias card edges in the shader (no MSAA)
    misc: vec4<f32>,
    // w: 0 the grid room; 1 plain colour rgb (linear); 2 panorama, 3 fixed backdrop, with
    // x the brightness and yz the backdrop's scale to cover the screen
    bg: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var t_img: texture_2d<f32>;
@group(1) @binding(1) var s_img: sampler;

// ---------------------------------------------------------------- room

struct RoomOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

@vertex
fn vs_room(@location(0) p: vec3<f32>, @location(1) n: vec3<f32>) -> RoomOut {
    var o: RoomOut;
    o.clip = g.view_proj * vec4<f32>(p, 1.0);
    o.world = p;
    o.normal = n;
    return o;
}

fn grid_line(uv: vec2<f32>, width: f32) -> f32 {
    let d = abs(fract(uv - 0.5) - 0.5) / max(fwidth(uv), vec2<f32>(1e-4));
    return 1.0 - clamp(min(d.x, d.y) / width, 0.0, 1.0);
}

@fragment
fn fs_room(i: RoomOut) -> @location(0) vec4<f32> {
    let an = abs(i.normal);
    let uv = select(select(i.world.xy, i.world.zy, an.x > 0.5), i.world.xz, an.y > 0.5);
    let minor = grid_line(uv, 1.0);
    let major = grid_line(uv / 4.0, 1.5);
    let is_floor = select(0.0, 1.0, i.normal.y > 0.5);
    let is_ceiling = select(0.0, 1.0, i.normal.y < -0.5);
    if (g.bg.w > 0.5) {
        // Plain background: no walls or ceiling, only a faint floor grid that fades out well
        // before the room's edges, so the box never shows.
        if (is_floor < 0.5) {
            discard;
        }
        let edge = max(abs(i.world.x), abs(i.world.z));
        let fade = (1.0 - smoothstep(6.0, 14.0, edge))
            * (1.0 - smoothstep(8.0, 20.0, distance(i.world, g.cam_pos.xyz)));
        let lum = dot(g.bg.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
        // Lighter lines on dark colours, darker lines on light ones.
        let line = select(vec3<f32>(0.0), vec3<f32>(1.0), lum < 0.25);
        let a = (minor * 0.05 + major * 0.12) * fade;
        return vec4<f32>(mix(g.bg.rgb, line, a), 1.0);
    }
    var base = vec3<f32>(0.030, 0.036, 0.056);
    base = mix(base, vec3<f32>(0.022, 0.026, 0.042), is_floor);
    base = mix(base, vec3<f32>(0.016, 0.018, 0.028), is_ceiling);
    // Soft light pooling toward the middle of the floor.
    let center_glow = exp(-dot(i.world.xz, i.world.xz) * 0.006) * 0.05 * is_floor;
    let accent = vec3<f32>(0.32, 0.52, 1.0);
    var col = base + accent * (minor * 0.045 + major * 0.11) + vec3<f32>(center_glow * 0.6, center_glow * 0.8, center_glow);
    // Fade grid with distance to avoid moire, and darken far surfaces.
    let d = distance(i.world, g.cam_pos.xyz);
    col = mix(col, base * 0.6, clamp(d / 40.0, 0.0, 0.7));
    return vec4<f32>(col, 1.0);
}

// ---------------------------------------------------------------- background picture

struct SkyOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

// One triangle covering the screen.
@vertex
fn vs_sky(@builtin(vertex_index) i: u32) -> SkyOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u)) * 2.0 - 1.0;
    var o: SkyOut;
    o.clip = vec4<f32>(p, 0.5, 1.0);
    o.ndc = p;
    return o;
}

const PI: f32 = 3.14159265;

@fragment
fn fs_sky(i: SkyOut) -> @location(0) vec4<f32> {
    var col: vec3<f32>;
    if (g.bg.w < 2.5) {
        // Panorama (equirectangular): the view direction picks the pixel.
        let n = g.inv_view_proj * vec4<f32>(i.ndc, 0.0, 1.0);
        let f = g.inv_view_proj * vec4<f32>(i.ndc, 1.0, 1.0);
        let d = normalize(f.xyz / f.w - n.xyz / n.w);
        let u = atan2(d.x, -d.z) / (2.0 * PI) + 0.5;
        let v = acos(clamp(d.y, -1.0, 1.0)) / PI;
        // u jumps from 1 to 0 behind the viewer; the gradients of a copy shifted by half a
        // turn are smooth there, so take the smaller ones to keep the right mip level.
        let u2 = fract(u + 0.5);
        let dx = select(dpdx(u), dpdx(u2), abs(dpdx(u2)) < abs(dpdx(u)));
        let dy = select(dpdy(u), dpdy(u2), abs(dpdy(u2)) < abs(dpdy(u)));
        col = textureSampleGrad(t_img, s_img, vec2<f32>(u, v), vec2<f32>(dx, dpdx(v)), vec2<f32>(dy, dpdy(v))).rgb;
    } else {
        // Fixed backdrop covering the screen.
        let uv = (vec2<f32>(i.ndc.x, -i.ndc.y) * 0.5) * g.bg.yz + 0.5;
        col = textureSample(t_img, s_img, uv).rgb;
    }
    return vec4<f32>(col * g.bg.x, 1.0);
}

// ---------------------------------------------------------------- cards & hud

struct Inst {
    // card: center.xyz, alpha      | hud: rect x0 y0 x1 y1 (NDC)
    @location(2) a: vec4<f32>,
    // card: half-right.xyz, ghost  | hud: alpha
    @location(3) b: vec4<f32>,
    // card: half-up.xyz, highlight
    @location(4) c: vec4<f32>,
};

struct CardOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) params: vec4<f32>,
};

fn corner(vi: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    return c[vi];
}

@vertex
fn vs_card(@builtin(vertex_index) vi: u32, inst: Inst) -> CardOut {
    let k = corner(vi);
    let world = inst.a.xyz + inst.b.xyz * k.x + inst.c.xyz * k.y;
    var o: CardOut;
    o.clip = g.view_proj * vec4<f32>(world, 1.0);
    o.uv = vec2<f32>(k.x * 0.5 + 0.5, 0.5 - k.y * 0.5);
    o.params = vec4<f32>(inst.a.w, inst.b.w, inst.c.w, 0.0);
    return o;
}

@fragment
fn fs_card(i: CardOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    var c = textureSample(t_img, s_img, i.uv).rgb;
    let alpha = i.params.x;
    let ghost = i.params.y;
    // highlight: -1 no border at all (a live frame filling the view), 0 none, 1 hovered,
    // 2 hovered at its size limit (orange outline).
    let bare = step(i.params.z, -0.5);
    let maxed = step(1.5, i.params.z);
    let hl = clamp(i.params.z, 0.0, 1.0);
    // Ghost cards: desaturated and cool-tinted.
    let lum = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    c = mix(c, lum * vec3<f32>(0.62, 0.72, 0.95), ghost * 0.85) * (1.0 - ghost * 0.3);
    // Border, measured in screen pixels so it stays crisp at any distance.
    let fw = max(fwidth(i.uv), vec2<f32>(1e-5));
    let edge = min(min(i.uv.x, 1.0 - i.uv.x) / fw.x, min(i.uv.y, 1.0 - i.uv.y) / fw.y);
    let border_px = mix(1.5, 4.0, hl);
    let border = (1.0 - smoothstep(border_px - 0.75, border_px + 0.75, edge)) * (1.0 - bare);
    let pulse = 0.85 + 0.15 * sin(g.misc.x * 6.0);
    var border_col = mix(vec3<f32>(0.25, 0.30, 0.42), vec3<f32>(0.35, 0.75, 1.0) * pulse, hl);
    border_col = mix(border_col, vec3<f32>(1.0, 0.55, 0.1) * pulse, maxed);
    c = mix(c, border_col, border);
    // Subtle lift of the hovered card.
    c = c * (1.0 + hl * 0.08);
    if (!front) {
        c = c * 0.25;
    }
    // Without MSAA, fade the outermost pixel of the card (cards are drawn back to front).
    let cover = mix(1.0, clamp(edge, 0.0, 1.0), g.misc.y * (1.0 - bare));
    return vec4<f32>(c, alpha * cover);
}

@vertex
fn vs_hud(@builtin(vertex_index) vi: u32, inst: Inst) -> CardOut {
    let k = corner(vi);
    let t = k * 0.5 + 0.5;
    var o: CardOut;
    o.clip = vec4<f32>(mix(inst.a.x, inst.a.z, t.x), mix(inst.a.y, inst.a.w, t.y), 0.0, 1.0);
    o.uv = vec2<f32>(t.x, 1.0 - t.y);
    o.params = vec4<f32>(inst.b.x, 0.0, 0.0, 0.0);
    return o;
}

@fragment
fn fs_hud(i: CardOut) -> @location(0) vec4<f32> {
    let c = textureSample(t_img, s_img, i.uv);
    return vec4<f32>(c.rgb, c.a * i.params.x);
}
