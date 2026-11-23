//! Text rendering with fontdue on top of a tiny-skia pixmap.
//! Uses the Windows system fonts (Segoe UI) and lazily loads fallback
//! fonts (symbols, CJK, Korean, Indic) only when a glyph is missing.

use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;
use std::path::PathBuf;
use tiny_skia::{Mask, Pixmap};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weight {
    Regular = 0,
    Semibold = 1,
    Bold = 2,
    /// Bahnschrift: titles and numerals.
    Display = 3,
}

struct Glyph {
    m: Metrics,
    bmp: Vec<u8>,
}

struct Fallback {
    path: PathBuf,
    index: u32,
    font: Option<Option<Font>>, // None = not tried yet, Some(None) = failed
}

pub struct Fonts {
    primary: Vec<Font>,
    fallbacks: Vec<Fallback>,
    cache: HashMap<(u16, u16, u32), Glyph>,
    gamma: [u8; 256],
}

fn fonts_dir() -> PathBuf {
    let win = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    PathBuf::from(win).join("Fonts")
}

fn load(path: &PathBuf, index: u32) -> Option<Font> {
    let bytes = std::fs::read(path).ok()?;
    let settings = FontSettings {
        collection_index: index,
        scale: 40.0,
        ..Default::default()
    };
    Font::from_bytes(bytes, settings).ok()
}

impl Fonts {
    pub fn new() -> Fonts {
        let dir = fonts_dir();
        let pick = |names: &[&str]| -> Option<Font> {
            names.iter().find_map(|n| load(&dir.join(n), 0))
        };
        let regular = pick(&["segoeui.ttf", "arial.ttf", "tahoma.ttf"])
            .expect("no usable system font found in Windows\\Fonts");
        let semibold = pick(&["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"]);
        let bold = pick(&["segoeuib.ttf", "arialbd.ttf"]);
        let semibold = semibold.unwrap_or_else(|| regular.clone());
        let bold = bold.unwrap_or_else(|| semibold.clone());
        let display = pick(&["bahnschrift.ttf"]).unwrap_or_else(|| semibold.clone());

        let fallbacks = [
            ("seguisym.ttf", 0),
            ("msyh.ttc", 0),
            ("YuGothM.ttc", 0),
            ("malgun.ttf", 0),
            ("Nirmala.ttf", 0),
            ("seguiemj.ttf", 0),
        ]
        .iter()
        .map(|(n, i)| Fallback {
            path: dir.join(n),
            index: *i,
            font: None,
        })
        .collect();

        let mut gamma = [0u8; 256];
        for (i, g) in gamma.iter_mut().enumerate() {
            *g = ((i as f32 / 255.0).powf(0.78) * 255.0).round() as u8;
        }

        Fonts {
            primary: vec![regular, semibold, bold, display],
            fallbacks,
            cache: HashMap::new(),
            gamma,
        }
    }

    fn font(&self, id: u16) -> &Font {
        if id < 10 {
            &self.primary[id as usize]
        } else {
            self.fallbacks[(id - 10) as usize]
                .font
                .as_ref()
                .and_then(|f| f.as_ref())
                .unwrap_or(&self.primary[0])
        }
    }

    fn resolve(&mut self, ch: char, wt: Weight) -> (u16, u16) {
        let pid = wt as u16;
        let g = self.primary[pid as usize].lookup_glyph_index(ch);
        if g != 0 || ch.is_whitespace() || ch.is_control() {
            return (pid, g);
        }
        for i in 0..self.fallbacks.len() {
            let fb = &mut self.fallbacks[i];
            if fb.font.is_none() {
                fb.font = Some(load(&fb.path, fb.index));
            }
            if let Some(Some(f)) = &fb.font {
                let g = f.lookup_glyph_index(ch);
                if g != 0 {
                    return (10 + i as u16, g);
                }
            }
        }
        (pid, 0)
    }

    /// Lays out `text` starting at pen `x` on `baseline` (physical pixels).
    /// Draws when `pm` is given, otherwise only measures. Returns advance width.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &mut self,
        mut pm: Option<&mut Pixmap>,
        mask: Option<&Mask>,
        text: &str,
        x: f32,
        baseline: f32,
        px: f32,
        wt: Weight,
        color: [f32; 4],
        clip: (f32, f32),
    ) -> f32 {
        if self.cache.len() > 6000 {
            self.cache.clear();
        }
        let size_key = (px * 4.0).round() as u32;
        let mut pen = x;
        let mut prev: Option<(u16, u16)> = None;
        for ch in text.chars() {
            let (fid, gi) = self.resolve(ch, wt);
            // No font has it (or it is a zero-width joiner / variation
            // selector): skip instead of drawing a tofu box.
            if gi == 0 && !ch.is_whitespace() {
                continue;
            }
            if let Some((pf, pg)) = prev {
                if pf == fid {
                    if let Some(k) = self.font(fid).horizontal_kern_indexed(pg, gi, px) {
                        pen += k;
                    }
                }
            }
            let key = (fid, gi, size_key);
            if !self.cache.contains_key(&key) {
                let (m, bmp) = self.font(fid).rasterize_indexed(gi, px);
                self.cache.insert(key, Glyph { m, bmp });
            }
            let g = &self.cache[&key];
            if let Some(pm) = pm.as_deref_mut() {
                if color[3] > 0.003 && g.m.width > 0 {
                    let gx = (pen + g.m.xmin as f32).round() as i32;
                    let gy = (baseline - g.m.ymin as f32 - g.m.height as f32).round() as i32;
                    blit(pm, mask, gx, gy, &g.m, &g.bmp, color, clip, &self.gamma);
                }
            }
            pen += g.m.advance_width;
            prev = Some((fid, gi));
        }
        pen - x
    }
}

#[allow(clippy::too_many_arguments)]
fn blit(
    pm: &mut Pixmap,
    mask: Option<&Mask>,
    gx: i32,
    gy: i32,
    m: &Metrics,
    cov: &[u8],
    color: [f32; 4],
    clip: (f32, f32),
    gamma: &[u8; 256],
) {
    let pw = pm.width() as i32;
    let ph = pm.height() as i32;
    let cx0 = clip.0.max(0.0).floor() as i32;
    let cx1 = clip.1.min(pw as f32).ceil() as i32;
    let mdata = mask.map(|m| m.data());
    let data = pm.data_mut();
    for row in 0..m.height as i32 {
        let py = gy + row;
        if py < 0 || py >= ph {
            continue;
        }
        for col in 0..m.width as i32 {
            let px = gx + col;
            if px < cx0 || px >= cx1 {
                continue;
            }
            let c = cov[(row * m.width as i32 + col) as usize];
            if c == 0 {
                continue;
            }
            let idx = (py * pw + px) as usize;
            let mut a = gamma[c as usize] as f32 / 255.0 * color[3];
            if let Some(md) = mdata {
                a *= md[idx] as f32 / 255.0;
            }
            if a <= 0.0 {
                continue;
            }
            let d = &mut data[idx * 4..idx * 4 + 4];
            let inv = 1.0 - a;
            d[0] = (color[0] * a * 255.0 + d[0] as f32 * inv).min(255.0) as u8;
            d[1] = (color[1] * a * 255.0 + d[1] as f32 * inv).min(255.0) as u8;
            d[2] = (color[2] * a * 255.0 + d[2] as f32 * inv).min(255.0) as u8;
            d[3] = (a * 255.0 + d[3] as f32 * inv).min(255.0) as u8;
        }
    }
}
