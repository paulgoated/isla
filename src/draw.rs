//! Drawing helpers: a `Canvas` working in design units (scaled by DPI and
//! offset to the content origin), shape paths and vector icons.

use crate::text::{Fonts, Weight};
use tiny_skia::{
    Color, FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pattern,
    Pixmap, SpreadMode, Stroke, StrokeDash, Transform,
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgba(pub f32, pub f32, pub f32, pub f32);

impl Rgba {
    pub const fn rgb(r: f32, g: f32, b: f32) -> Rgba {
        Rgba(r, g, b, 1.0)
    }
    pub fn a(self, a: f32) -> Rgba {
        Rgba(self.0, self.1, self.2, self.3 * a)
    }
}

pub const WHITE: Rgba = Rgba::rgb(1.0, 1.0, 1.0);
pub const GRAY: Rgba = Rgba::rgb(0.60, 0.60, 0.64);
pub const BLUE: Rgba = Rgba::rgb(0.04, 0.52, 1.0);
/// Second data colour (pink); no violet anywhere in Isla's own design.
pub const PURPLE: Rgba = Rgba::rgb(1.0, 0.42, 0.6);
pub const GREEN: Rgba = Rgba::rgb(0.19, 0.82, 0.35);
pub const ORANGE: Rgba = Rgba::rgb(1.0, 0.62, 0.04);
pub const RED: Rgba = Rgba::rgb(1.0, 0.27, 0.23);
pub const CYAN: Rgba = Rgba::rgb(0.39, 0.82, 1.0);

pub fn paint(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    p.set_color(
        Color::from_rgba(
            c.0.clamp(0.0, 1.0),
            c.1.clamp(0.0, 1.0),
            c.2.clamp(0.0, 1.0),
            c.3.clamp(0.0, 1.0),
        )
        .unwrap_or(Color::BLACK),
    );
    p
}

const K: f32 = 0.5523;

pub fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let c = r * (1.0 - K);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - c, y, x + w, y + c, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - c, x + w - c, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + c, y + h, x, y + h - c, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + c, x + c, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Notch attached to the top screen edge: rounded bottom corners plus
/// concave "ears" that blend into the top edge (physical pixels).
pub fn notch_path(cx: f32, w: f32, h: f32, r: f32, ear: f32) -> Option<Path> {
    let l = cx - w / 2.0;
    let rt = cx + w / 2.0;
    let r = r.min(h - ear).min(w / 2.0).max(0.0);
    let c = r * (1.0 - K);
    let e = ear * (1.0 - K);
    let mut pb = PathBuilder::new();
    pb.move_to(l - ear, 0.0);
    pb.cubic_to(l - e, 0.0, l, ear - e, l, ear);
    pb.line_to(l, h - r);
    pb.cubic_to(l, h - c, l + c, h, l + r, h);
    pb.line_to(rt - r, h);
    pb.cubic_to(rt - c, h, rt, h - c, rt, h - r);
    pb.line_to(rt, ear);
    pb.cubic_to(rt, ear - e, rt + e, 0.0, rt + ear, 0.0);
    pb.close();
    pb.finish()
}

fn arc_path(cx: f32, cy: f32, r: f32, start: f32, sweep: f32) -> Option<Path> {
    let n = ((sweep.abs() / std::f32::consts::TAU) * 64.0).ceil().max(2.0) as usize;
    let mut pb = PathBuilder::new();
    for i in 0..=n {
        let a = start + sweep * i as f32 / n as f32;
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.finish()
}

fn poly(pts: &[(f32, f32)]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for (i, (x, y)) in pts.iter().enumerate() {
        if i == 0 {
            pb.move_to(*x, *y);
        } else {
            pb.line_to(*x, *y);
        }
    }
    pb.close();
    pb.finish()
}

pub struct Canvas<'a> {
    pub pm: &'a mut Pixmap,
    pub mask: Option<&'a Mask>,
    pub fonts: &'a mut Fonts,
    pub ox: f32,
    pub oy: f32,
    pub s: f32,
    pub alpha: f32,
}

impl Canvas<'_> {
    fn tf(&self) -> Transform {
        Transform::from_row(self.s, 0.0, 0.0, self.s, self.ox, self.oy)
    }

    /// Design-unit rect to physical window rect.
    pub fn phys(&self, x: f32, y: f32, w: f32, h: f32) -> [f32; 4] {
        [self.ox + x * self.s, self.oy + y * self.s, w * self.s, h * self.s]
    }

    pub fn fill(&mut self, path: Option<Path>, c: Rgba) {
        let a = c.3 * self.alpha;
        if a <= 0.002 {
            return;
        }
        if let Some(p) = path {
            let t = self.tf();
            self.pm
                .fill_path(&p, &paint(c.a(self.alpha)), FillRule::Winding, t, self.mask);
        }
    }

    pub fn stroke(&mut self, path: Option<Path>, width: f32, c: Rgba) {
        if c.3 * self.alpha <= 0.002 {
            return;
        }
        if let Some(p) = path {
            let st = Stroke {
                width,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Default::default()
            };
            let t = self.tf();
            self.pm.stroke_path(&p, &paint(c.a(self.alpha)), &st, t, self.mask);
        }
    }

    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        self.fill(rrect_path(x, y, w, h, r), c);
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        self.fill(PathBuilder::from_circle(cx, cy, r), c);
    }

    pub fn image(&mut self, img: &Pixmap, x: f32, y: f32, size: f32, r: f32) {
        let Some(path) = rrect_path(x, y, size, size, r) else { return };
        let sc = size / img.width() as f32;
        let shader = Pattern::new(
            img.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Bicubic,
            self.alpha.clamp(0.0, 1.0),
            Transform::from_row(sc, 0.0, 0.0, sc, x, y),
        );
        let p = Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        let t = self.tf();
        self.pm.fill_path(&path, &p, FillRule::Winding, t, self.mask);
    }

    // ---------- text ----------

    pub fn measure(&mut self, size: f32, wt: Weight, s: &str) -> f32 {
        let px = size * self.s;
        self.fonts
            .run(None, None, s, 0.0, 0.0, px, wt, [0.0; 4], (0.0, 0.0))
            / self.s
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_clip(
        &mut self,
        x: f32,
        baseline: f32,
        size: f32,
        wt: Weight,
        c: Rgba,
        s: &str,
        clip: (f32, f32),
    ) -> f32 {
        let px = size * self.s;
        let color = [c.0, c.1, c.2, c.3 * self.alpha];
        let clip = (self.ox + clip.0 * self.s, self.ox + clip.1 * self.s);
        self.fonts.run(
            Some(&mut *self.pm),
            self.mask,
            s,
            self.ox + x * self.s,
            self.oy + baseline * self.s,
            px,
            wt,
            color,
            clip,
        ) / self.s
    }

    pub fn text(&mut self, x: f32, baseline: f32, size: f32, wt: Weight, c: Rgba, s: &str) -> f32 {
        self.text_clip(x, baseline, size, wt, c, s, (-1e5, 1e5))
    }

    pub fn text_right(&mut self, right: f32, baseline: f32, size: f32, wt: Weight, c: Rgba, s: &str) -> f32 {
        let w = self.measure(size, wt, s);
        self.text(right - w, baseline, size, wt, c, s);
        w
    }

    /// Truncates with an ellipsis so the text fits `max_w`.
    pub fn fit(&mut self, size: f32, wt: Weight, s: &str, max_w: f32) -> String {
        if self.measure(size, wt, s) <= max_w {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let cand: String = chars[..mid].iter().collect::<String>() + "…";
            if self.measure(size, wt, &cand) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
    }

    /// Scrolling text for titles longer than the available space.
    #[allow(clippy::too_many_arguments)]
    pub fn marquee(&mut self, x0: f32, x1: f32, baseline: f32, size: f32, wt: Weight, c: Rgba, s: &str, t: f32) {
        let tw = self.measure(size, wt, s);
        if tw <= x1 - x0 {
            self.text_clip(x0, baseline, size, wt, c, s, (x0, x1));
            return;
        }
        let gap = 44.0;
        let speed = 30.0;
        let pause = 2.2;
        let cycle = tw + gap;
        let period = pause + cycle / speed;
        let ph = t % period;
        let off = if ph < pause { 0.0 } else { (ph - pause) * speed };
        self.text_clip(x0 - off, baseline, size, wt, c, s, (x0, x1));
        self.text_clip(x0 - off + cycle, baseline, size, wt, c, s, (x0, x1));
    }

    // ---------- icons ----------

    pub fn play(&mut self, cx: f32, cy: f32, sz: f32, c: Rgba) {
        let p = || poly(&[
            (cx - sz * 0.32, cy - sz * 0.42),
            (cx + sz * 0.48, cy),
            (cx - sz * 0.32, cy + sz * 0.42),
        ]);
        self.fill(p(), c);
        self.stroke(p(), sz * 0.12, c);
    }

    pub fn pause(&mut self, cx: f32, cy: f32, sz: f32, c: Rgba) {
        let bw = sz * 0.26;
        let bh = sz * 0.86;
        let gap = sz * 0.2;
        self.rrect(cx - gap / 2.0 - bw, cy - bh / 2.0, bw, bh, bw * 0.35, c);
        self.rrect(cx + gap / 2.0, cy - bh / 2.0, bw, bh, bw * 0.35, c);
    }

    /// Double triangle (dir = 1 forward, -1 backward).
    pub fn skip(&mut self, cx: f32, cy: f32, sz: f32, dir: f32, c: Rgba) {
        let h = sz * 0.34;
        for i in 0..2 {
            let x0 = cx + dir * (-sz * 0.5 + i as f32 * sz * 0.48);
            let x1 = x0 + dir * sz * 0.5;
            let p = || poly(&[(x0, cy - h), (x1, cy), (x0, cy + h)]);
            self.fill(p(), c);
            self.stroke(p(), sz * 0.1, c);
        }
    }

    pub fn speaker(&mut self, x: f32, cy: f32, sz: f32, level: f32, muted: bool, c: Rgba) {
        self.rrect(x, cy - sz * 0.17, sz * 0.24, sz * 0.34, sz * 0.05, c);
        let cone = || poly(&[
            (x + sz * 0.2, cy - sz * 0.17),
            (x + sz * 0.5, cy - sz * 0.4),
            (x + sz * 0.5, cy + sz * 0.4),
            (x + sz * 0.2, cy + sz * 0.17),
        ]);
        self.fill(cone(), c);
        self.stroke(cone(), sz * 0.06, c);
        let ox = x + sz * 0.52;
        if muted {
            let (a, b) = (ox + sz * 0.14, ox + sz * 0.42);
            let mut pb = PathBuilder::new();
            pb.move_to(a, cy - sz * 0.14);
            pb.line_to(b, cy + sz * 0.14);
            pb.move_to(b, cy - sz * 0.14);
            pb.line_to(a, cy + sz * 0.14);
            self.stroke(pb.finish(), sz * 0.09, c);
            return;
        }
        let waves = if level <= 0.001 { 0 } else if level < 0.34 { 1 } else if level < 0.67 { 2 } else { 3 };
        for i in 0..waves {
            let r = sz * (0.16 + 0.15 * i as f32);
            self.stroke(arc_path(ox, cy, r, -0.85, 1.7), sz * 0.08, c);
        }
    }

    pub fn bolt(&mut self, cx: f32, cy: f32, sz: f32, c: Rgba) {
        let pts: Vec<(f32, f32)> = [
            (0.12, -0.5),
            (-0.3, 0.07),
            (-0.02, 0.07),
            (-0.12, 0.5),
            (0.3, -0.07),
            (0.02, -0.07),
        ]
        .iter()
        .map(|(x, y)| (cx + x * sz, cy + y * sz))
        .collect();
        self.fill(poly(&pts), c);
    }

    pub fn battery(&mut self, x: f32, cy: f32, w: f32, h: f32, pct: f32, charging: bool) {
        let fill = if charging {
            GREEN
        } else if pct < 0.2 {
            RED
        } else {
            WHITE
        };
        self.stroke(rrect_path(x, cy - h / 2.0, w, h, h * 0.3), 1.1, WHITE.a(0.45));
        self.rrect(x + w + 0.8, cy - h * 0.18, 1.6, h * 0.36, 0.8, WHITE.a(0.45));
        let iw = (w - 3.4) * pct.clamp(0.0, 1.0);
        if iw > 0.3 {
            self.rrect(x + 1.7, cy - h / 2.0 + 1.7, iw, h - 3.4, (h - 3.4) * 0.25, fill);
        }
    }

    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, width: f32, progress: f32, c: Rgba) {
        self.stroke(PathBuilder::from_circle(cx, cy, r), width, c.a(0.22));
        let p = progress.clamp(0.0, 1.0);
        if p > 0.002 {
            self.stroke(
                arc_path(cx, cy, r, -std::f32::consts::FRAC_PI_2, p * std::f32::consts::TAU),
                width,
                c,
            );
        }
    }

    pub fn stroke_dashed(&mut self, path: Option<Path>, width: f32, dash: f32, c: Rgba) {
        if c.3 * self.alpha <= 0.002 {
            return;
        }
        if let Some(p) = path {
            let st = Stroke {
                width,
                line_cap: LineCap::Round,
                dash: StrokeDash::new(vec![dash, dash * 0.8], 0.0),
                ..Default::default()
            };
            let t = self.tf();
            self.pm.stroke_path(&p, &paint(c.a(self.alpha)), &st, t, self.mask);
        }
    }

    pub fn text_center(&mut self, cx: f32, baseline: f32, size: f32, wt: Weight, c: Rgba, s: &str) -> f32 {
        let w = self.measure(size, wt, s);
        self.text(cx - w / 2.0, baseline, size, wt, c, s);
        w
    }

    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Rgba) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        self.stroke(pb.finish(), width, c);
    }

    pub fn mic(&mut self, cx: f32, cy: f32, sz: f32, muted: bool, c: Rgba) {
        let w = sz * 0.36;
        self.rrect(cx - w / 2.0, cy - sz * 0.48, w, sz * 0.58, w / 2.0, c);
        self.stroke(arc_path(cx, cy - sz * 0.08, sz * 0.32, 0.15, std::f32::consts::PI - 0.3), sz * 0.08, c);
        self.line(cx, cy + sz * 0.24, cx, cy + sz * 0.44, sz * 0.08, c);
        self.line(cx - sz * 0.16, cy + sz * 0.44, cx + sz * 0.16, cy + sz * 0.44, sz * 0.08, c);
        if muted {
            self.line(cx - sz * 0.38, cy - sz * 0.42, cx + sz * 0.38, cy + sz * 0.42, sz * 0.09, RED);
        }
    }

    pub fn camera(&mut self, cx: f32, cy: f32, sz: f32, c: Rgba) {
        self.rrect(cx - sz * 0.5, cy - sz * 0.3, sz * 0.7, sz * 0.6, sz * 0.12, c);
        self.fill(
            poly(&[
                (cx + sz * 0.22, cy - sz * 0.08),
                (cx + sz * 0.5, cy - sz * 0.26),
                (cx + sz * 0.5, cy + sz * 0.26),
                (cx + sz * 0.22, cy + sz * 0.08),
            ]),
            c,
        );
    }

    fn cloud(&mut self, cx: f32, cy: f32, sz: f32, c: Rgba) {
        let mut pb = PathBuilder::new();
        pb.push_circle(cx - sz * 0.2, cy + sz * 0.06, sz * 0.2);
        pb.push_circle(cx + sz * 0.02, cy - sz * 0.08, sz * 0.27);
        pb.push_circle(cx + sz * 0.26, cy + sz * 0.08, sz * 0.18);
        pb.push_circle(cx + sz * 0.05, cy + sz * 0.1, sz * 0.16);
        self.fill(pb.finish(), c);
    }

    fn sun(&mut self, cx: f32, cy: f32, sz: f32, c: Rgba) {
        self.circle(cx, cy, sz * 0.2, c);
        for i in 0..8 {
            let a = i as f32 * std::f32::consts::FRAC_PI_4;
            let (s, co) = a.sin_cos();
            self.line(cx + co * sz * 0.3, cy + s * sz * 0.3, cx + co * sz * 0.42, cy + s * sz * 0.42, sz * 0.07, c);
        }
    }

    fn moon(&mut self, cx: f32, cy: f32, sz: f32, c: Rgba) {
        // Crescent: outer circle minus an offset inner circle, sampled.
        let r = sz * 0.3;
        let (ix, iy, ir) = (cx + r * 0.5, cy - r * 0.35, r * 0.85);
        let mut outer = Vec::new();
        let mut inner = Vec::new();
        let n = 96;
        let base = (iy - cy).atan2(ix - cx) + std::f32::consts::PI;
        for i in 0..=n {
            let a = base - std::f32::consts::PI + std::f32::consts::TAU * i as f32 / n as f32;
            let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
            if (x - ix).hypot(y - iy) >= ir {
                outer.push((x, y));
            }
            let a2 = base + std::f32::consts::PI - std::f32::consts::TAU * i as f32 / n as f32;
            let (x2, y2) = (ix + ir * a2.cos(), iy + ir * a2.sin());
            if (x2 - cx).hypot(y2 - cy) <= r {
                inner.push((x2, y2));
            }
        }
        outer.extend(inner);
        self.fill(poly(&outer), c);
    }

    /// WMO weather code icon.
    pub fn weather(&mut self, cx: f32, cy: f32, sz: f32, code: i32, day: bool) {
        let sun_col = Rgba::rgb(1.0, 0.8, 0.2);
        let cloud_col = Rgba::rgb(0.86, 0.88, 0.92);
        let rain_col = Rgba::rgb(0.35, 0.68, 1.0);
        let celestial = |cv: &mut Self, x: f32, y: f32, s: f32| {
            if day {
                cv.sun(x, y, s, sun_col)
            } else {
                cv.moon(x, y, s, Rgba::rgb(0.95, 0.92, 0.75))
            }
        };
        match code {
            0 => celestial(self, cx, cy, sz),
            1 | 2 => {
                celestial(self, cx - sz * 0.14, cy - sz * 0.14, sz * 0.8);
                self.cloud(cx + sz * 0.04, cy + sz * 0.1, sz * 0.8, cloud_col);
            }
            45 | 48 => {
                for i in 0..3 {
                    let y = cy - sz * 0.2 + i as f32 * sz * 0.2;
                    self.line(cx - sz * 0.38, y, cx + sz * 0.38, y, sz * 0.08, cloud_col);
                }
            }
            51..=67 | 80..=82 => {
                self.cloud(cx, cy - sz * 0.1, sz, cloud_col);
                for i in 0..3 {
                    let x = cx - sz * 0.2 + i as f32 * sz * 0.2;
                    self.line(x, cy + sz * 0.24, x - sz * 0.07, cy + sz * 0.42, sz * 0.07, rain_col);
                }
            }
            71..=77 | 85 | 86 => {
                self.cloud(cx, cy - sz * 0.1, sz, cloud_col);
                for i in 0..3 {
                    self.circle(cx - sz * 0.2 + i as f32 * sz * 0.2, cy + sz * 0.33, sz * 0.05, WHITE);
                }
            }
            95..=99 => {
                self.cloud(cx, cy - sz * 0.12, sz, cloud_col.a(0.9));
                self.bolt(cx, cy + sz * 0.28, sz * 0.4, sun_col);
            }
            _ => self.cloud(cx, cy, sz, cloud_col),
        }
    }

    /// Animated equalizer bars inside (x, y, w, h).
    pub fn equalizer(&mut self, x: f32, y: f32, w: f32, h: f32, t: f32, playing: bool, c: Rgba) {
        let n = 4;
        let bw = w / (n as f32 * 1.6 - 0.6);
        let freqs = [7.3, 9.1, 6.2, 8.4];
        let phases = [0.0, 1.7, 3.1, 4.6];
        for i in 0..n {
            let lvl = if playing {
                let a = (t * freqs[i] + phases[i]).sin().abs();
                let b = (t * freqs[i] * 0.53 + phases[i] * 2.0).sin().abs();
                0.25 + 0.75 * (a * 0.6 + b * 0.4)
            } else {
                0.22
            };
            let bh = (h * lvl).max(bw);
            let bx = x + i as f32 * bw * 1.6;
            self.rrect(bx, y + (h - bh) / 2.0, bw, bh, bw / 2.0, c);
        }
    }
}
