//! Embeds the app icon (`src/icon.rs`) in the exe: writes a compiled resource file (.res) and
//! hands it to the linker, which takes .res files directly (no resource compiler needed).

#[path = "src/icon.rs"]
mod icon;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const SIZES: [u32; 6] = [16, 24, 32, 48, 64, 128];

/// One resource with a numeric type and name, padded to 4 bytes.
fn resource(out: &mut Vec<u8>, ty: u16, name: u16, flags: u16, data: &[u8]) {
    for v in [data.len() as u32, 32] {
        out.extend(v.to_le_bytes());
    }
    for v in [0xFFFF, ty, 0xFFFF, name] {
        out.extend(v.to_le_bytes());
    }
    out.extend(0u32.to_le_bytes()); // data version
    out.extend(flags.to_le_bytes());
    out.extend(0x0409u16.to_le_bytes()); // US English
    out.extend([0u8; 8]); // version, characteristics
    out.extend(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// An icon image: a 32-bit DIB (rows bottom-up) followed by an all-zero AND mask.
fn dib(n: u32) -> Vec<u8> {
    let px = icon::bgra(n);
    let mask_row = n.div_ceil(32) * 4;
    let mut d = Vec::new();
    for v in [40u32, n, 2 * n] {
        d.extend(v.to_le_bytes());
    }
    d.extend(1u16.to_le_bytes());
    d.extend(32u16.to_le_bytes());
    for v in [0u32, n * n * 4 + mask_row * n, 0, 0, 0, 0] {
        d.extend(v.to_le_bytes());
    }
    for row in px.chunks((n * 4) as usize).rev() {
        d.extend(row);
    }
    d.extend(vec![0u8; (mask_row * n) as usize]);
    d
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// A node of a version resource: length, value length, type (1 = text), key, value, children.
fn node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut v = vec![0, 0];
    v.extend(value_len.to_le_bytes());
    v.extend((text as u16).to_le_bytes());
    for c in key.encode_utf16().chain([0]) {
        v.extend(c.to_le_bytes());
    }
    pad4(&mut v);
    v.extend(value);
    for c in children {
        pad4(&mut v);
        v.extend(c);
    }
    let len = v.len() as u16;
    v[..2].copy_from_slice(&len.to_le_bytes());
    v
}

fn text(key: &str, value: &str) -> Vec<u8> {
    let w: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let bytes: Vec<u8> = w.iter().flat_map(|c| c.to_le_bytes()).collect();
    node(key, &bytes, w.len() as u16, true, &[])
}

/// The version resource shown on the exe's Properties > Details tab.
fn version() -> Vec<u8> {
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap();
    let n: Vec<u32> = ver.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let (ms, ls) = ((n[0] << 16) | n[1], n[2] << 16);
    let mut fixed = Vec::new();
    // Signature, struct version, file and product versions, flags mask, flags, NT Win32,
    // application, subtype, date.
    for v in [0xFEEF04BD, 0x10000, ms, ls, ms, ls, 0x3F, 0, 0x40004, 1, 0, 0, 0u32] {
        fixed.extend(v.to_le_bytes());
    }
    let strings = node(
        "040904B0",
        &[],
        0,
        true,
        &[
            text("FileDescription", "Window3D"),
            text("FileVersion", &ver),
            text("ProductName", "Window3D"),
            text("ProductVersion", &ver),
            text("InternalName", "window3d"),
            text("OriginalFilename", "window3d.exe"),
        ],
    );
    let mut lang = Vec::new();
    lang.extend(0x0409u16.to_le_bytes());
    lang.extend(1200u16.to_le_bytes());
    let vars = node("VarFileInfo", &[], 0, true, &[node("Translation", &lang, 4, false, &[])]);
    node(
        "VS_VERSION_INFO",
        &fixed,
        fixed.len() as u16,
        false,
        &[node("StringFileInfo", &[], 0, true, &[strings]), vars],
    )
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let mut res = Vec::new();
    resource(&mut res, 0, 0, 0, &[]);
    let mut group = Vec::new();
    for v in [0u16, 1, SIZES.len() as u16] {
        group.extend(v.to_le_bytes());
    }
    for (i, &n) in SIZES.iter().enumerate() {
        let d = dib(n);
        resource(&mut res, RT_ICON, i as u16 + 1, 0x1010, &d);
        group.extend([n as u8, n as u8, 0, 0]);
        group.extend(1u16.to_le_bytes());
        group.extend(32u16.to_le_bytes());
        group.extend((d.len() as u32).to_le_bytes());
        group.extend((i as u16 + 1).to_le_bytes());
    }
    resource(&mut res, RT_GROUP_ICON, 1, 0x1030, &group);
    resource(&mut res, RT_VERSION, 1, 0x0030, &version());
    let path = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("icon.res");
    std::fs::write(&path, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", path.display());
}
