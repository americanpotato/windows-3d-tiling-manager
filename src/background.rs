//! What is drawn behind the cards (`background` in the config): the grid room, a plain colour,
//! or a picture. Pictures twice as wide as high are 360° panoramas (equirectangular) drawn
//! all around; others are a fixed backdrop filling the screen. Pictures are decoded with WIC
//! on a worker thread, with their mip chain, so opening the 3D view never waits for them.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::{SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW};
use windows::core::PCWSTR;

use crate::image::Image;

/// File types offered from the backgrounds folder (WIC decodes them all on Windows 10/11).
const EXTS: [&str; 6] = ["jpg", "jpeg", "png", "bmp", "webp", "jxr"];

pub enum Kind {
    Room,
    /// Linear rgb.
    Plain([f32; 3]),
    Picture(PathBuf),
    /// An animated scene drawn by a shader (`SCENES`).
    Scene(&'static Scene),
}

/// An animated background drawn by `fs_scene` in `shader.wgsl` (its `index` there), set as
/// `"scene:<id>"`.
pub struct Scene {
    pub id: &'static str,
    pub name: &'static str,
    pub index: u32,
    pub thumb: &'static [u8],
}

macro_rules! scene {
    ($id:literal, $name:literal, $i:literal) => {
        Scene { id: $id, name: $name, index: $i, thumb: include_bytes!(concat!("../assets/scenes/", $id, ".jpg")) }
    };
}

pub const SCENES: &[Scene] = &[
    scene!("twin_moons", "Twin moons", 0),
    scene!("aurora_meadow", "Aurora meadow", 1),
    scene!("amber_sea", "Amber sea", 2),
    scene!("rain_veil", "Rain veil", 3),
    scene!("deep_blue", "Deep blue", 4),
    scene!("salt_mirror", "Salt mirror", 5),
    scene!("galaxy_rise", "Galaxy rise", 6),
    scene!("snowfall", "Snowfall", 7),
];

/// The setting value of a scene.
pub fn scene_value(s: &Scene) -> String {
    format!("scene:{}", s.id)
}

/// The thumbnail path key of a scene (see `make_thumbs`).
pub fn scene_thumb(s: &Scene) -> PathBuf {
    PathBuf::from(format!("scene:{}", s.id))
}

/// Reads a `background` setting: "room", "#rrggbb", "wallpaper" (the Windows desktop picture)
/// "scene:<id>" or a picture's path (relative paths are in the backgrounds folder).
pub fn parse(s: &str) -> Kind {
    let s = s.trim();
    if let Some(id) = s.strip_prefix("scene:") {
        return match SCENES.iter().find(|x| x.id.eq_ignore_ascii_case(id.trim())) {
            Some(x) => Kind::Scene(x),
            None => Kind::Room,
        };
    }
    if let Some(c) = crate::render::background_color(s) {
        return Kind::Plain(c);
    }
    if s.eq_ignore_ascii_case("wallpaper") {
        return match wallpaper() {
            Some(p) => Kind::Picture(p),
            // The desktop is a solid colour.
            None => match crate::render::background_color(&desktop_color()) {
                Some(c) => Kind::Plain(c),
                None => Kind::Room,
            },
        };
    }
    if s.is_empty() || s.eq_ignore_ascii_case("room") {
        return Kind::Room;
    }
    let p = Path::new(s);
    Kind::Picture(if p.is_absolute() { p.to_path_buf() } else { folder().join(p) })
}

/// `%APPDATA%\Window3D\backgrounds`: pictures put there are offered in the options menu.
pub fn folder() -> PathBuf {
    crate::store::app_dir().join("backgrounds")
}

/// File names of the pictures in the backgrounds folder, sorted.
pub fn pictures() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(folder())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            let ext = Path::new(n).extension().and_then(|x| x.to_str()).unwrap_or("");
            EXTS.iter().any(|x| x.eq_ignore_ascii_case(ext))
        })
        .collect();
    v.sort_by_key(|n| n.to_lowercase());
    v
}

/// The Windows desktop picture, if there is one.
pub fn wallpaper() -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let p = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
    (n > 0 && p.is_file()).then_some(p)
}

/// The desktop's solid colour as "#rrggbb" (shown when there is no wallpaper picture).
pub fn desktop_color() -> String {
    use windows::Win32::Graphics::Gdi::{COLOR_DESKTOP, GetSysColor};
    let c = unsafe { GetSysColor(COLOR_DESKTOP) };
    format!("#{:02x}{:02x}{:02x}", c & 0xff, (c >> 8) & 0xff, (c >> 16) & 0xff)
}

/// Whether picture `path` is a 360° panorama (about 2:1); only its header is read.
pub fn is_panorama(path: &Path) -> bool {
    picture_size(path).is_ok_and(|(w, h)| is_pano(w, h))
}

/// A 360° panorama offered in the options menu: a Poly Haven picture (CC0), downloaded into
/// the backgrounds folder when first picked. Only its thumbnail is in the program.
pub struct Pano {
    pub id: &'static str,
    pub name: &'static str,
    pub thumb: &'static [u8],
}

pub const PANORAMAS: &[Pano] = &[
    Pano { id: "kloofendal_48d_partly_cloudy_puresky", name: "Partly cloudy sky", thumb: include_bytes!("../assets/panoramas/kloofendal_48d_partly_cloudy_puresky.jpg") },
    Pano { id: "sunflowers_puresky", name: "Clear blue sky", thumb: include_bytes!("../assets/panoramas/sunflowers_puresky.jpg") },
    Pano { id: "kloofendal_43d_clear_puresky", name: "Clear midday sky", thumb: include_bytes!("../assets/panoramas/kloofendal_43d_clear_puresky.jpg") },
    Pano { id: "qwantani_noon_puresky", name: "Noon sky", thumb: include_bytes!("../assets/panoramas/qwantani_noon_puresky.jpg") },
    Pano { id: "qwantani_afternoon_puresky", name: "Afternoon sky", thumb: include_bytes!("../assets/panoramas/qwantani_afternoon_puresky.jpg") },
    Pano { id: "rustig_koppie_puresky", name: "Deep blue sky", thumb: include_bytes!("../assets/panoramas/rustig_koppie_puresky.jpg") },
    Pano { id: "kloppenheim_05_puresky", name: "Little clouds", thumb: include_bytes!("../assets/panoramas/kloppenheim_05_puresky.jpg") },
    Pano { id: "kloofendal_38d_partly_cloudy_puresky", name: "Fleecy clouds", thumb: include_bytes!("../assets/panoramas/kloofendal_38d_partly_cloudy_puresky.jpg") },
    Pano { id: "table_mountain_1_puresky", name: "Big white clouds", thumb: include_bytes!("../assets/panoramas/table_mountain_1_puresky.jpg") },
    Pano { id: "aristea_wreck_puresky", name: "Cloud bank", thumb: include_bytes!("../assets/panoramas/aristea_wreck_puresky.jpg") },
    Pano { id: "farm_field_puresky", name: "Stormy clouds", thumb: include_bytes!("../assets/panoramas/farm_field_puresky.jpg") },
    Pano { id: "qwantani_sunset_puresky", name: "Golden sunset", thumb: include_bytes!("../assets/panoramas/qwantani_sunset_puresky.jpg") },
    Pano { id: "syferfontein_0d_clear_puresky", name: "Clear sunrise", thumb: include_bytes!("../assets/panoramas/syferfontein_0d_clear_puresky.jpg") },
    Pano { id: "qwantani_dawn_puresky", name: "Pale dawn", thumb: include_bytes!("../assets/panoramas/qwantani_dawn_puresky.jpg") },
    Pano { id: "kloofendal_overcast_puresky", name: "Overcast sky", thumb: include_bytes!("../assets/panoramas/kloofendal_overcast_puresky.jpg") },
    Pano { id: "kloofendal_misty_morning_puresky", name: "Misty morning", thumb: include_bytes!("../assets/panoramas/kloofendal_misty_morning_puresky.jpg") },
    Pano { id: "qwantani_moonrise_puresky", name: "Moonlit sky", thumb: include_bytes!("../assets/panoramas/qwantani_moonrise_puresky.jpg") },
    Pano { id: "belfast_sunset_puresky", name: "Sunset sky", thumb: include_bytes!("../assets/panoramas/belfast_sunset_puresky.jpg") },
    Pano { id: "the_sky_is_on_fire", name: "Sky on fire", thumb: include_bytes!("../assets/panoramas/the_sky_is_on_fire.jpg") },
    Pano { id: "venice_sunset", name: "Venice sunset", thumb: include_bytes!("../assets/panoramas/venice_sunset.jpg") },
    Pano { id: "spiaggia_di_mondello", name: "Mondello beach", thumb: include_bytes!("../assets/panoramas/spiaggia_di_mondello.jpg") },
    Pano { id: "kiara_1_dawn", name: "Dawn hills", thumb: include_bytes!("../assets/panoramas/kiara_1_dawn.jpg") },
    Pano { id: "shanghai_bund", name: "Shanghai Bund", thumb: include_bytes!("../assets/panoramas/shanghai_bund.jpg") },
    Pano { id: "canary_wharf", name: "Canary Wharf", thumb: include_bytes!("../assets/panoramas/canary_wharf.jpg") },
    Pano { id: "satara_night", name: "Savanna night", thumb: include_bytes!("../assets/panoramas/satara_night.jpg") },
    Pano { id: "moonless_golf", name: "Starry night", thumb: include_bytes!("../assets/panoramas/moonless_golf.jpg") },
    Pano { id: "fireplace", name: "Fireplace room", thumb: include_bytes!("../assets/panoramas/fireplace.jpg") },
    Pano { id: "cayley_interior", name: "Old hall", thumb: include_bytes!("../assets/panoramas/cayley_interior.jpg") },
];

/// Where a panorama is downloaded from (8192x4096, 3-22 MB).
fn pano_url(p: &Pano) -> String {
    format!("https://dl.polyhaven.org/file/ph-assets/HDRIs/extra/Tonemapped%20JPG/{}.jpg", p.id)
}

/// A panorama's file name in the backgrounds folder (its `background` setting).
pub fn pano_file(p: &Pano) -> String {
    format!("{} (360).jpg", p.name)
}

/// The panorama a background picture is the download of.
pub fn pano_of(path: &Path) -> Option<&'static Pano> {
    let dir = folder();
    PANORAMAS.iter().find(|p| dir.join(pano_file(p)) == path)
}

/// The thumbnail path key of a panorama (see `make_thumbs`).
pub fn pano_thumb(p: &Pano) -> PathBuf {
    PathBuf::from(format!("pano:{}", p.id))
}

fn embedded(path: &Path) -> Option<&'static [u8]> {
    let p = path.to_str()?;
    if let Some(id) = p.strip_prefix("scene:") {
        return SCENES.iter().find(|s| s.id == id).map(|s| s.thumb);
    }
    let id = p.strip_prefix("pano:")?;
    PANORAMAS.iter().find(|p| p.id == id).map(|p| p.thumb)
}

#[derive(Clone, PartialEq, Debug)]
pub enum PanoState {
    Ready,
    Missing,
    Downloading,
    Failed(String),
}

/// Downloads running or failed, by panorama id.
static DOWNLOADS: std::sync::Mutex<Vec<(&'static str, PanoState)>> = std::sync::Mutex::new(Vec::new());
/// Counts finished downloads (the 3D view then shows the picture).
static DOWNLOADED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn pano_state(p: &Pano) -> PanoState {
    if let Some(s) = DOWNLOADS.lock().ok().and_then(|d| d.iter().find(|e| e.0 == p.id).map(|e| e.1.clone())) {
        return s;
    }
    if folder().join(pano_file(p)).is_file() { PanoState::Ready } else { PanoState::Missing }
}

/// How many downloads have ended (either way).
pub fn downloads_ended() -> u32 {
    DOWNLOADED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Starts downloading `p` into the backgrounds folder, unless it already is.
pub fn fetch(p: &'static Pano) {
    {
        let Ok(mut d) = DOWNLOADS.lock() else { return };
        if d.iter().any(|e| e.0 == p.id && e.1 == PanoState::Downloading) {
            return;
        }
        d.retain(|e| e.0 != p.id);
        d.push((p.id, PanoState::Downloading));
    }
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let file = folder().join(pano_file(p));
        let r = download(p, &file);
        crate::store::log(&match &r {
            Ok(()) => format!("panorama {}: downloaded in {} ms", p.id, t0.elapsed().as_millis()),
            Err(e) => format!("panorama {}: download failed: {e}", p.id),
        });
        if let Ok(mut d) = DOWNLOADS.lock() {
            d.retain(|e| e.0 != p.id);
            if let Err(e) = r {
                d.push((p.id, PanoState::Failed(e)));
            }
        }
        DOWNLOADED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Downloads to a `.part` file, checks it is a 2:1 picture, then gives it its name.
fn download(p: &Pano, file: &Path) -> Result<(), String> {
    let _ = std::fs::create_dir_all(folder());
    let part = file.with_extension("part");
    let url: Vec<u16> = std::ffi::OsStr::new(&pano_url(p)).encode_wide_z();
    let to: Vec<u16> = part.as_os_str().encode_wide_z();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        windows::Win32::System::Com::Urlmon::URLDownloadToFileW(None, PCWSTR(url.as_ptr()), PCWSTR(to.as_ptr()), 0, None)
            .map_err(|e| format!("{e}"))?;
    }
    let size = picture_size(&part);
    if !size.as_ref().is_ok_and(|&(w, h)| is_pano(w, h)) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("not a panorama ({size:?})"));
    }
    std::fs::rename(&part, file).map_err(|e| e.to_string())
}

fn picture_size(path: &Path) -> Result<(u32, u32), String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide_z();
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("WIC: {e}"))?;
        let dec = f
            .CreateDecoderFromFilename(PCWSTR(wide.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .map_err(|e| e.to_string())?;
        let frame = dec.GetFrame(0).map_err(|e| e.to_string())?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).map_err(|e| e.to_string())?;
        Ok((w, h))
    }
}

/// Small copies of pictures for the options menu, by path and size; kept while the app runs.
static THUMBS: std::sync::Mutex<Option<std::collections::HashMap<(PathBuf, u32, u32), std::sync::Arc<Image>>>> =
    std::sync::Mutex::new(None);

/// A thumbnail already made.
pub fn thumb_cached(path: &Path, w: u32, h: u32) -> Option<std::sync::Arc<Image>> {
    let g = THUMBS.lock().ok()?;
    g.as_ref()?.get(&(path.to_path_buf(), w, h)).cloned()
}

/// Makes `w`x`h` thumbnails (cropped to fill) of `paths` on worker threads; each path is sent
/// on when its thumbnail is in the cache (or failed).
pub fn make_thumbs(paths: Vec<PathBuf>, w: u32, h: u32) -> Receiver<PathBuf> {
    let (tx, rx) = channel();
    let todo: Vec<PathBuf> = paths.into_iter().filter(|p| thumb_cached(p, w, h).is_none()).collect();
    let todo = std::sync::Arc::new(std::sync::Mutex::new(todo));
    for _ in 0..3 {
        let (todo, tx) = (todo.clone(), tx.clone());
        std::thread::spawn(move || loop {
            let Some(p) = todo.lock().ok().and_then(|mut t| t.pop()) else { break };
            if let Ok(img) = decode_cover(&p, w, h) {
                if let Ok(mut g) = THUMBS.lock() {
                    g.get_or_insert_with(Default::default).insert((p.clone(), w, h), std::sync::Arc::new(img));
                }
            }
            if tx.send(p).is_err() {
                break;
            }
        });
    }
    rx
}

/// Identifies a picture's content: its path, size and last change.
pub fn key(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    if let Ok(m) = std::fs::metadata(path) {
        m.len().hash(&mut h);
        m.modified().ok().hash(&mut h);
    }
    h.finish()
}

/// A decoded picture with its mip chain (level 0 first).
pub struct Loaded {
    pub key: u64,
    /// Drawn all around as a 360° panorama (else a fixed backdrop).
    pub pano: bool,
    pub levels: Vec<Image>,
    /// Size of the file's picture before any scaling down.
    pub source: (u32, u32),
    pub ms: u128,
}

/// Decodes `path` on a worker thread. `max_dim` is the GPU's largest texture side.
pub fn load(path: PathBuf, max_dim: u32) -> Receiver<Result<Loaded, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let r = decode(&path, max_dim).map(|(img, source, pano)| {
            let mut levels = vec![img];
            while levels.last().is_some_and(|l| l.w > 1 || l.h > 1) {
                let next = levels.last().unwrap().half();
                levels.push(next);
            }
            Loaded { key: key(&path), pano, levels, source, ms: t0.elapsed().as_millis() }
        });
        let _ = tx.send(r.map_err(|e| format!("{}: {e}", path.display())));
    });
    rx
}

/// Panoramas are about 2:1.
fn is_pano(w: u32, h: u32) -> bool {
    let r = w as f32 / h.max(1) as f32;
    (1.9..=2.1).contains(&r)
}

/// Decodes a picture to opaque BGRA, scaled down to at most 8192 wide (panoramas: there they
/// are about as sharp as the screen) or 4096 on the longest side (backdrops), and `max_dim`.
fn decode(path: &Path, max_dim: u32) -> Result<(Image, (u32, u32), bool), String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide_z();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("WIC: {e}"))?;
        let dec = f
            .CreateDecoderFromFilename(PCWSTR(wide.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .map_err(|e| format!("can't open ({e})"))?;
        let frame = dec.GetFrame(0).map_err(|e| format!("no picture ({e})"))?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).map_err(|e| e.to_string())?;
        if w == 0 || h == 0 {
            return Err("empty picture".into());
        }
        let pano = is_pano(w, h);
        let cap = if pano { 8192 } else { 4096 }.min(max_dim.max(1));
        let s = (cap as f32 / w.max(h) as f32).min(1.0);
        let (tw, th) = (((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1));
        let src: IWICBitmapSource = if (tw, th) != (w, h) {
            let sc = f.CreateBitmapScaler().map_err(|e| e.to_string())?;
            sc.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant)
                .map_err(|e| format!("scale: {e}"))?;
            sc.into()
        } else {
            frame.into()
        };
        let conv = f.CreateFormatConverter().map_err(|e| e.to_string())?;
        conv.Initialize(&src, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .map_err(|e| format!("convert: {e}"))?;
        let mut img = Image::new(tw, th);
        conv.CopyPixels(std::ptr::null(), tw * 4, &mut img.px)
            .map_err(|e| format!("read: {e}"))?;
        img.set_opaque();
        Ok((img, (w, h), pano))
    }
}

/// Decodes a picture (or a panorama's thumbnail, `pano:{id}`) scaled to cover `w`x`h`, the
/// middle cropped out.
fn decode_cover(path: &Path, w: u32, h: u32) -> Result<Image, String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide_z();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("WIC: {e}"))?;
        let dec = match embedded(path) {
            Some(bytes) => {
                let st = f.CreateStream().map_err(|e| e.to_string())?;
                st.InitializeFromMemory(bytes).map_err(|e| e.to_string())?;
                f.CreateDecoderFromStream(&st, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            }
            None => f.CreateDecoderFromFilename(PCWSTR(wide.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand),
        }
        .map_err(|e| e.to_string())?;
        let frame = dec.GetFrame(0).map_err(|e| e.to_string())?;
        let (mut sw, mut sh) = (0u32, 0u32);
        frame.GetSize(&mut sw, &mut sh).map_err(|e| e.to_string())?;
        if sw == 0 || sh == 0 {
            return Err("empty picture".into());
        }
        let s = (w as f32 / sw as f32).max(h as f32 / sh as f32);
        let (tw, th) = (((sw as f32 * s).ceil() as u32).max(w), ((sh as f32 * s).ceil() as u32).max(h));
        let sc = f.CreateBitmapScaler().map_err(|e| e.to_string())?;
        sc.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant).map_err(|e| e.to_string())?;
        let conv = f.CreateFormatConverter().map_err(|e| e.to_string())?;
        conv.Initialize(&sc, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .map_err(|e| e.to_string())?;
        let mut img = Image::new(w, h);
        let r = WICRect { X: ((tw - w) / 2) as i32, Y: ((th - h) / 2) as i32, Width: w as i32, Height: h as i32 };
        conv.CopyPixels(&r, w * 4, &mut img.px).map_err(|e| e.to_string())?;
        img.set_opaque();
        Ok(img)
    }
}

trait WideZ {
    fn encode_wide_z(&self) -> Vec<u16>;
}

impl WideZ for std::ffi::OsStr {
    fn encode_wide_z(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(std::iter::once(0)).collect()
    }
}
