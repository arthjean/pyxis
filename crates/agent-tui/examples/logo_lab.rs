//! Test bench of cosmic logos for Pyxis: renders several concepts in truecolor
//! ANSI to pick one for real. `cargo run -p agent-tui --example logo_lab`.
//! None is wired into the TUI yet: this is exploratory. The winner will be
//! ported into `render.rs` (geometric generator -> bi-color half-blocks).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::f32::consts::{FRAC_PI_2, PI, TAU};

/// Side of the grid in pixels (-> N/2 cells tall once stacked).
const N: usize = 24;

/// Grid of intensities 0.0 (empty) .. 1.0 (brightest core).
type Grid = Vec<Vec<f32>>;

fn blank() -> Grid {
    vec![vec![0.0; N]; N]
}

/// Spiral galaxy: bright gaussian nucleus + two logarithmic arms fading
/// toward the rim.
fn galaxy() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let k = 2.3; // arm tightness
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            let rn = rr / c;
            let nucleus = (-(rn / 0.22).powi(2)).exp();
            let mut arm = 0.0_f32;
            if rr > 1.0 && rn < 1.08 {
                let phi = dy.atan2(dx);
                let base = phi - k * rr.ln();
                for a in [0.0, PI] {
                    let mut dphase = (base - a).rem_euclid(TAU);
                    if dphase > PI {
                        dphase -= TAU;
                    }
                    let width = 0.42 + 0.40 * rn; // the arms widen going outward
                    let along = (-(dphase / width).powi(2)).exp();
                    let radial = (-(rn / 0.62).powi(2)).exp() * (rn * 3.2).min(1.0);
                    arm = arm.max(along * radial);
                }
            }
            *cell = nucleus.max(arm * 0.92);
        }
    }
    g
}

/// Pulsar: neutron star (very bright point) emitting two opposite tilted
/// beams (the lighthouse of the universe).
fn pulsar() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let axis = FRAC_PI_2 + 0.5; // tilted magnetic axis
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            let rn = rr / c;
            let core = (-(rn / 0.12).powi(2)).exp();
            let phi = dy.atan2(dx);
            let mut beam = 0.0_f32;
            for a in [axis, axis + PI] {
                let mut d = (phi - a).rem_euclid(TAU);
                if d > PI {
                    d -= TAU;
                }
                let cone = 0.14 + 0.06 * rn;
                let along = (-(d / cone).powi(2)).exp();
                let radial = (-(rn / 0.85).powi(2)).exp() * (rn * 4.0).min(1.0);
                beam = beam.max(along * radial);
            }
            *cell = core.max(beam * 0.95);
        }
    }
    g
}

/// Supernova: incandescent core + star-shaped rays + shock shell.
fn supernova() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let rays = 6.0;
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            let rn = rr / c;
            let phi = dy.atan2(dx);
            let core = (-(rn / 0.14).powi(2)).exp();
            let sector = TAU / rays;
            let mut dphi = phi.rem_euclid(sector);
            if dphi > sector / 2.0 {
                dphi -= sector;
            }
            let raywidth = 0.10 + 0.10 * rn;
            let ray = (-(dphi / raywidth).powi(2)).exp() * (-(rn / 0.78).powi(2)).exp();
            let ring = (-(((rn - 0.66) / 0.05).powi(2))).exp() * 0.5;
            *cell = core.max(ray * 0.95).max(ring);
        }
    }
    g
}

/// Gravitational wave: crisp concentric rings fading at the rim
/// (a presence that propagates). The most abstract one.
fn ripple() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            let rn = rr / c;
            let phase = rn * 7.0 * PI; // ~3.5 rings over the radius
            let crest = (((phase).cos() + 1.0) / 2.0).powf(3.0);
            let fade = (-(rn / 0.82).powi(2)).exp();
            let core = (-(rn / 0.09).powi(2)).exp();
            *cell = core.max(crest * fade);
        }
    }
    g
}

/// Saturn: planetary disk + tilted elliptical ring (the back part of
/// the ring is occluded by the planet).
fn saturn() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let tilt = 0.40_f32;
    let (ct, st) = (tilt.cos(), tilt.sin());
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rn = (dx * dx + dy * dy).sqrt() / c;
            let planet = (-(rn / 0.34).powi(2)).exp();
            // Rotated then flattened coordinates -> ellipse.
            let u = dx * ct + dy * st;
            let v = -dx * st + dy * ct;
            let e = ((u / (0.98 * c)).powi(2) + (v / (0.30 * c)).powi(2)).sqrt();
            let ring = (-(((e - 0.82) / 0.14).powi(2))).exp();
            // The back of the ring (v < 0) is hidden in the planetary silhouette.
            let ring = if v < 0.0 && rn < 0.36 { 0.0 } else { ring };
            *cell = planet.max(ring * 0.9);
        }
    }
    g
}

/// Comet: bright head at the top right, trail widening and fading
/// toward the opposite corner.
fn comet() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let (hx, hy) = (c * 0.42, -c * 0.42); // head position
    let (mut ux, mut uy) = (-1.0_f32, 1.0_f32); // trail direction
    let dl = (ux * ux + uy * uy).sqrt();
    ux /= dl;
    uy /= dl;
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let rx = (x as f32 - c) - hx;
            let ry = (y as f32 - c) - hy;
            let along = rx * ux + ry * uy; // > 0 behind the head (trail)
            let perp = (-rx * uy + ry * ux).abs();
            let head = (-((rx * rx + ry * ry).sqrt() / (0.16 * c)).powi(2)).exp();
            let tail = if along > 0.0 {
                let w = 0.05 * c + 0.22 * along;
                (-(perp / w).powi(2)).exp() * (-(along / (0.95 * c))).exp()
            } else {
                0.0
            };
            *cell = head.max(tail * 0.85);
        }
    }
    g
}

/// Golden spiral (nautilus): a single logarithmic spiral, bright nucleus,
/// fading toward the rim. Sparer than the two-armed galaxy.
fn nautilus() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let a = 0.7; // starting radius
    let k = 0.45; // growth (larger = looser)
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            let rn = rr / c;
            let core = (-(rn / 0.11).powi(2)).exp();
            let mut arm = 0.0_f32;
            if rr > 0.8 {
                let phi = dy.atan2(dx);
                let theta = (rr / a).ln() / k;
                let mut dphase = (phi - theta).rem_euclid(TAU);
                if dphase > PI {
                    dphase -= TAU;
                }
                let width = 0.30 + 0.12 * rn;
                arm = (-(dphase / width).powi(2)).exp()
                    * (-(rn / 0.82).powi(2)).exp()
                    * (rn * 3.0).min(1.0);
            }
            *cell = core.max(arm);
        }
    }
    g
}

/// Distance from a point to the segment [a,b] (pixels).
fn dist_point_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len2 = vx * vx + vy * vy;
    let t = if len2 <= 0.0 {
        0.0
    } else {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (ax + t * vx, ay + t * vy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Constellation: stars (bright gaussians) linked by thin threads.
/// Abstract drawing, signable as a mark.
fn constellation() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let pts = [
        (-0.72, 0.42),
        (-0.30, -0.45),
        (0.08, 0.18),
        (0.46, -0.55),
        (0.70, 0.30),
        (0.02, 0.70),
    ];
    let to_px = |nx: f32, ny: f32| (c + nx * c, c + ny * c);
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let (px, py) = (x as f32, y as f32);
            let mut v = 0.0_f32;
            for w in pts.windows(2) {
                let (ax, ay) = to_px(w[0].0, w[0].1);
                let (bx, by) = to_px(w[1].0, w[1].1);
                let d = dist_point_seg(px, py, ax, ay, bx, by);
                v = v.max((-(d / 0.85).powi(2)).exp() * 0.32);
            }
            for p in pts {
                let (sx, sy) = to_px(p.0, p.1);
                let d = ((px - sx).powi(2) + (py - sy).powi(2)).sqrt();
                v = v.max((-(d / 1.15).powi(2)).exp());
            }
            *cell = v;
        }
    }
    g
}

fn sharp(x: f32, p: i32) -> f32 {
    (((x + 1.0) / 2.0).clamp(0.0, 1.0)).powi(p)
}

/// Wireframe globe: sphere in latitudes/longitudes, denser toward the limb,
/// edge halo. A world.
fn globe() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let radius = 0.94 * c;
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            if rr > radius {
                continue;
            }
            let z = (radius * radius - dx * dx - dy * dy).max(0.0).sqrt();
            let lat = (dy / radius).clamp(-1.0, 1.0).asin();
            let lon = dx.atan2(z);
            let lat_lines = sharp((lat * 9.0).cos(), 14);
            let lon_lines = sharp((lon * 5.0).cos(), 14);
            let depth = 0.35 + 0.65 * (z / radius); // fades toward the limb
            let mesh = lat_lines.max(lon_lines) * depth;
            let rim = (-(((rr / radius - 0.97) / 0.05).powi(2))).exp() * 0.6;
            *cell = mesh.max(rim);
        }
    }
    g
}

/// Dyson sphere: central star wrapped in a mesh of panels
/// (incomplete swarm, the star blazing through the missing panels) + surface
/// glow and silhouette rim. Type II megastructure (Kardashev).
fn dyson() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let radius = 0.92 * c;
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            let rn = rr / c;
            if rr > radius {
                // Beyond the shell: faint stellar corona.
                *cell = (-(((rr - radius) / (0.12 * c)).powi(2))).exp() * 0.22;
                continue;
            }
            let z = (radius * radius - dx * dx - dy * dy).max(0.0).sqrt();
            let depth = z / radius; // 1 at the center, 0 at the limb
            let lat = (dy / radius).clamp(-1.0, 1.0).asin();
            let lon = dx.atan2(z);
            // Panels: an incomplete swarm, ~25% missing (deterministic pattern).
            let pi = (lat * 8.0 / PI).floor() as i32;
            let pj = (lon * 8.0 / PI).floor() as i32;
            let missing = ((pi * 73 + pj * 131) & 7) < 2;
            // Transmitted starlight: full through the holes, attenuated by the panels.
            let star_broad = (-(rn / 0.55).powi(2)).exp();
            let star_core = (-(rn / 0.14).powi(2)).exp();
            let transmit = if missing { 0.95 } else { 0.30 };
            let glow = (star_broad * transmit).max(star_core * 0.6);
            // Structural mesh (beams) + surface glow + rim.
            let seam = sharp((lat * 9.0).cos(), 12).max(sharp((lon * 7.0).cos(), 12))
                * (0.45 + 0.55 * depth);
            let ambient = 0.09 * depth;
            let rim = (-(((rr / radius - 0.97) / 0.04).powi(2))).exp() * 0.55;
            *cell = glow.max(seam * 0.9).max(rim).max(ambient);
        }
    }
    g
}

/// Dyson sphere, minimalist version: a crisp core + two thin tilted rings of
/// collectors, each with a gap (swarm still assembling). Thin lines,
/// lots of emptiness, no panel.
fn dyson_min() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    // (tilt, minor axis ratio, gap start, gap end) in radians.
    let rings = [
        (0.50_f32, 0.30_f32, 1.1_f32, 2.3_f32),
        (-0.62, 0.26, 4.0, 5.0),
    ];
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rn = (dx * dx + dy * dy).sqrt() / c;
            let core = (-(rn / 0.12).powi(2)).exp();
            let mut ring = 0.0_f32;
            for (tilt, br, gap_start, gap_end) in rings {
                let (ct, st) = (tilt.cos(), tilt.sin());
                let u = dx * ct + dy * st;
                let v = -dx * st + dy * ct;
                let e = ((u / (0.88 * c)).powi(2) + (v / (br * c)).powi(2)).sqrt();
                let line = (-(((e - 1.0) / 0.06).powi(2))).exp();
                let phi = v.atan2(u).rem_euclid(TAU);
                if !(phi > gap_start && phi < gap_end) {
                    ring = ring.max(line);
                }
            }
            *cell = core.max(ring * 0.9);
        }
    }
    g
}

/// Continuous (resolution-independent) field of the minimalist Dyson, in
/// normalized coordinates nx,ny in [-1,1] (radius 1 = edge). `line_w` = ring thickness
/// (larger = thicker strokes). Used by the braille rendering (stippling).
fn dyson_min_at(nx: f32, ny: f32, line_w: f32, core_w: f32) -> f32 {
    let rn = (nx * nx + ny * ny).sqrt();
    let core = (-(rn / core_w).powi(2)).exp();
    let rings = [
        (0.50_f32, 0.30_f32, 1.1_f32, 2.3_f32),
        (-0.62, 0.26, 4.0, 5.0),
    ];
    let mut ring = 0.0_f32;
    for (tilt, br, gap_start, gap_end) in rings {
        let (ct, st) = (tilt.cos(), tilt.sin());
        let u = nx * ct + ny * st;
        let v = -nx * st + ny * ct;
        let e = ((u / 0.88).powi(2) + (v / br).powi(2)).sqrt();
        let line = (-(((e - 1.0) / line_w).powi(2))).exp();
        let phi = v.atan2(u).rem_euclid(TAU);
        if !(phi > gap_start && phi < gap_end) {
            ring = ring.max(line);
        }
    }
    core.max(ring * 0.9)
}

/// 4x4 Bayer matrix (ordered dithering): converts intensity into dot
/// density (the Grok-like "more or less packed" pattern).
const BAYER4: [[f32; 4]; 4] = [
    [0.0, 8.0, 2.0, 10.0],
    [12.0, 4.0, 14.0, 6.0],
    [3.0, 11.0, 1.0, 9.0],
    [15.0, 7.0, 13.0, 5.0],
];

/// Layout of the 8 dots of a braille cell -> bit (base U+2800).
const DOTS: [(usize, usize, u8); 8] = [
    (0, 0, 0x01),
    (0, 1, 0x02),
    (0, 2, 0x04),
    (0, 3, 0x40),
    (1, 0, 0x08),
    (1, 1, 0x10),
    (1, 2, 0x20),
    (1, 3, 0x80),
];

/// Renders a field as dithered braille dots: `cols x rows` cells, each
/// sampled over 2x4 subdots. Dot density follows the intensity,
/// boosted by `gamma` (< 1 = richer edges, denser; the true background stays
/// empty since 0^gamma = 0). Monochrome: grey depending on the cell peak.
fn render_braille(
    name: &str,
    cols: usize,
    rows: usize,
    scale: f32,
    gamma: f32,
    f: impl Fn(f32, f32) -> f32,
) {
    let (sw, sh) = (cols * 2, rows * 4); // subgrid (square when cols = 2*rows)
    println!("\n  ◆ {name}");
    let mut cur = 0u8;
    for cy in 0..rows {
        let mut line = String::from("      \x1b[0m");
        let mut have = false;
        for cx in 0..cols {
            let mut bits = 0u8;
            let mut peak = 0.0_f32;
            for (ddx, ddy, bit) in DOTS {
                let (sx, sy) = (cx * 2 + ddx, cy * 4 + ddy);
                let nx = (sx as f32 + 0.5 - sw as f32 / 2.0) / (sw as f32 / 2.0) * scale;
                let ny = (sy as f32 + 0.5 - sh as f32 / 2.0) / (sh as f32 / 2.0) * scale;
                let inten = f(nx, ny).powf(gamma);
                let thr = (BAYER4[sy & 3][sx & 3] + 0.5) / 16.0;
                if inten > thr {
                    bits |= bit;
                    peak = peak.max(inten);
                }
            }
            if bits == 0 {
                if have {
                    line.push_str("\x1b[0m");
                    have = false;
                }
                line.push(' ');
                continue;
            }
            // Grey in a middle band (neither too dark, nor pure white).
            let v = lerp(0x6a, 0xde, peak.clamp(0.0, 1.0));
            if !have || cur != v {
                line.push_str(&format!("\x1b[38;2;{v};{v};{v}m"));
                cur = v;
                have = true;
            }
            line.push(char::from_u32(0x2800 + bits as u32).unwrap_or(' '));
        }
        line.push_str("\x1b[0m");
        println!("{line}");
    }
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Continuous monochrome ramp: dark grey -> mid grey -> almost white
/// (aligned on the theme greys: faint/dim/fg).
fn shade(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let (a, b, tt) = if t < 0.5 {
        (0x2c, 0x8a, t / 0.5)
    } else {
        (0x8a, 0xf2, (t - 0.5) / 0.5)
    };
    let v = lerp(a, b, tt);
    (v, v, v)
}

/// Stacks the grid into bi-color half-blocks (fg = top pixel, bg = bottom pixel) and
/// prints in truecolor ANSI, codes emitted only on change.
fn render_grid(name: &str, g: &Grid) {
    const EPS: f32 = 0.07; // below this threshold: empty (transparent)
    let indent = "      ";
    println!("\n  ◆ {name}");
    let mut r = 0;
    while r < N {
        let mut line = String::from(indent);
        line.push_str("\x1b[0m");
        let (mut fg, mut bg) = ((255u8, 255u8, 255u8), (0u8, 0u8, 0u8));
        let (mut have_fg, mut have_bg) = (false, false);
        let row_bot = if r + 1 < N { Some(&g[r + 1]) } else { None };
        for (x, &top) in g[r].iter().enumerate() {
            let bot = row_bot.map(|rb| rb[x]).unwrap_or(0.0);
            let (t_on, b_on) = (top >= EPS, bot >= EPS);
            if !t_on && !b_on {
                if have_fg || have_bg {
                    line.push_str("\x1b[0m");
                    have_fg = false;
                    have_bg = false;
                }
                line.push(' ');
                continue;
            }
            let (ch, want_fg, want_bg) = match (t_on, b_on) {
                (true, false) => ('▀', Some(shade(top)), None),
                (false, true) => ('▄', Some(shade(bot)), None),
                _ => ('▀', Some(shade(top)), Some(shade(bot))),
            };
            if let Some(c) = want_fg
                && (!have_fg || fg != c)
            {
                line.push_str(&format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2));
                fg = c;
                have_fg = true;
            }
            match want_bg {
                Some(c) if !have_bg || bg != c => {
                    line.push_str(&format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2));
                    bg = c;
                    have_bg = true;
                }
                None if have_bg => {
                    line.push_str("\x1b[49m");
                    have_bg = false;
                }
                _ => {}
            }
            line.push(ch);
        }
        line.push_str("\x1b[0m");
        println!("{line}");
        r += 2;
    }
}

fn main() {
    println!("\n=== Banc d'essai logos Pyxis (cosmique / abstrait) ===");
    render_grid("1. Galaxie spirale", &galaxy());
    render_grid("2. Pulsar (deux faisceaux)", &pulsar());
    render_grid("3. Supernova", &supernova());
    render_grid("4. Onde gravitationnelle", &ripple());
    render_grid("5. Saturne (planete a anneaux)", &saturn());
    render_grid("6. Comete", &comet());
    render_grid("7. Spirale d'or (nautilus)", &nautilus());
    render_grid("8. Constellation", &constellation());
    render_grid("9. Globe filaire", &globe());
    render_grid("10. Sphere de Dyson", &dyson());
    render_grid("11. Sphere de Dyson (minimaliste, blocs)", &dyson_min());
    render_braille("11c. Dyson 30x15 (reference)", 30, 15, 1.05, 1.0, |x, y| {
        dyson_min_at(x, y, 0.075, 0.12)
    });
    render_braille(
        "11d. Dyson 30x15 (+ epais / + dense)",
        30,
        15,
        1.05,
        0.7,
        |x, y| dyson_min_at(x, y, 0.11, 0.15),
    );
    render_braille(
        "11e. Dyson 30x15 (max epais / dense)",
        30,
        15,
        1.05,
        0.5,
        |x, y| dyson_min_at(x, y, 0.15, 0.18),
    );
    println!();
}
