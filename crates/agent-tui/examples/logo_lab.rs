//! Banc d'essai de logos cosmiques pour Numen : rend plusieurs concepts en ANSI
//! truecolor pour choisir sur pièce. `cargo run -p agent-tui --example logo_lab`.
//! Aucun n'est encore câblé dans la TUI — c'est exploratoire. Le gagnant sera
//! porté dans `render.rs` (générateur géométrique → demi-blocs bi-color).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::f32::consts::{FRAC_PI_2, PI, TAU};

/// Côté de la grille en pixels (→ N/2 cellules de haut une fois empilé).
const N: usize = 24;

/// Grille d'intensités 0.0 (vide) .. 1.0 (cœur le plus brillant).
type Grid = Vec<Vec<f32>>;

fn blank() -> Grid {
    vec![vec![0.0; N]; N]
}

/// Galaxie spirale : noyau gaussien brillant + deux bras logarithmiques qui
/// s'estompent vers la lisière.
fn galaxy() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let k = 2.3; // serrage des bras
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
                    let width = 0.42 + 0.40 * rn; // les bras s'élargissent en sortant
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

/// Pulsar : étoile à neutrons (point très brillant) émettant deux faisceaux
/// opposés inclinés (le phare de l'univers).
fn pulsar() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let axis = FRAC_PI_2 + 0.5; // axe magnétique incliné
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

/// Supernova : cœur incandescent + rayons en étoile + coquille de choc.
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

/// Onde gravitationnelle : anneaux concentriques nets s'estompant en lisière
/// (une présence qui se propage). Le plus abstrait.
fn ripple() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let rr = (dx * dx + dy * dy).sqrt();
            let rn = rr / c;
            let phase = rn * 7.0 * PI; // ~3.5 anneaux sur le rayon
            let crest = (((phase).cos() + 1.0) / 2.0).powf(3.0);
            let fade = (-(rn / 0.82).powi(2)).exp();
            let core = (-(rn / 0.09).powi(2)).exp();
            *cell = core.max(crest * fade);
        }
    }
    g
}

/// Saturne : disque planétaire + anneau elliptique incliné (la partie arrière de
/// l'anneau est occultée par la planète).
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
            // Coordonnées tournées puis aplaties → ellipse.
            let u = dx * ct + dy * st;
            let v = -dx * st + dy * ct;
            let e = ((u / (0.98 * c)).powi(2) + (v / (0.30 * c)).powi(2)).sqrt();
            let ring = (-(((e - 0.82) / 0.14).powi(2))).exp();
            // L'arrière de l'anneau (v < 0) est masqué dans la silhouette planétaire.
            let ring = if v < 0.0 && rn < 0.36 { 0.0 } else { ring };
            *cell = planet.max(ring * 0.9);
        }
    }
    g
}

/// Comète : tête brillante en haut à droite, traînée qui s'élargit et s'estompe
/// vers le coin opposé.
fn comet() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let (hx, hy) = (c * 0.42, -c * 0.42); // position de la tête
    let (mut ux, mut uy) = (-1.0_f32, 1.0_f32); // sens de la traînée
    let dl = (ux * ux + uy * uy).sqrt();
    ux /= dl;
    uy /= dl;
    for (y, row) in g.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let rx = (x as f32 - c) - hx;
            let ry = (y as f32 - c) - hy;
            let along = rx * ux + ry * uy; // > 0 derrière la tête (traînée)
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

/// Spirale d'or (nautilus) : une seule spirale logarithmique, noyau brillant,
/// estompée vers la lisière. Plus épurée que la galaxie à deux bras.
fn nautilus() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    let a = 0.7; // rayon de départ
    let k = 0.45; // croissance (plus grand = plus lâche)
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

/// Distance d'un point au segment [a,b] (pixels).
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

/// Constellation : étoiles (gaussiennes brillantes) reliées par des filets ténus.
/// Tracé abstrait, signable comme une marque.
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

/// Globe filaire : sphère en latitudes/longitudes, plus dense vers le limbe,
/// halo de bord. Un monde.
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
            let depth = 0.35 + 0.65 * (z / radius); // estompe vers le limbe
            let mesh = lat_lines.max(lon_lines) * depth;
            let rim = (-(((rr / radius - 0.97) / 0.05).powi(2))).exp() * 0.6;
            *cell = mesh.max(rim);
        }
    }
    g
}

/// Sphère de Dyson : étoile centrale enserrée dans une résille de panneaux
/// (essaim incomplet, l'étoile flamboie par les panneaux manquants) + lueur de
/// surface et liseré de silhouette. Mégastructure de type II (Kardashev).
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
                // Au-delà de la coque : faible couronne stellaire.
                *cell = (-(((rr - radius) / (0.12 * c)).powi(2))).exp() * 0.22;
                continue;
            }
            let z = (radius * radius - dx * dx - dy * dy).max(0.0).sqrt();
            let depth = z / radius; // 1 au centre, 0 au limbe
            let lat = (dy / radius).clamp(-1.0, 1.0).asin();
            let lon = dx.atan2(z);
            // Panneaux : un essaim incomplet, ~25 % manquants (motif déterministe).
            let pi = (lat * 8.0 / PI).floor() as i32;
            let pj = (lon * 8.0 / PI).floor() as i32;
            let missing = ((pi * 73 + pj * 131) & 7) < 2;
            // Lumière de l'étoile transmise : pleine par les trous, atténuée par les panneaux.
            let star_broad = (-(rn / 0.55).powi(2)).exp();
            let star_core = (-(rn / 0.14).powi(2)).exp();
            let transmit = if missing { 0.95 } else { 0.30 };
            let glow = (star_broad * transmit).max(star_core * 0.6);
            // Résille structurelle (poutres) + lueur de surface + liseré.
            let seam = sharp((lat * 9.0).cos(), 12).max(sharp((lon * 7.0).cos(), 12))
                * (0.45 + 0.55 * depth);
            let ambient = 0.09 * depth;
            let rim = (-(((rr / radius - 0.97) / 0.04).powi(2))).exp() * 0.55;
            *cell = glow.max(seam * 0.9).max(rim).max(ambient);
        }
    }
    g
}

/// Sphère de Dyson, version minimaliste : un cœur net + deux anneaux fins de
/// collecteurs inclinés, chacun avec une brèche (essaim en assemblage). Lignes
/// fines, beaucoup de vide, aucun panneau.
fn dyson_min() -> Grid {
    let mut g = blank();
    let c = (N as f32 - 1.0) / 2.0;
    // (inclinaison, ratio petit axe, début de brèche, fin de brèche) en radians.
    let rings = [(0.50_f32, 0.30_f32, 1.1_f32, 2.3_f32), (-0.62, 0.26, 4.0, 5.0)];
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

/// Champ continu (résolution-indépendant) du Dyson minimaliste, en coordonnées
/// normalisées nx,ny ∈ [-1,1] (rayon 1 = bord). `line_w` = épaisseur des anneaux
/// (plus grand = traits plus épais). Sert au rendu braille (stippling).
fn dyson_min_at(nx: f32, ny: f32, line_w: f32, core_w: f32) -> f32 {
    let rn = (nx * nx + ny * ny).sqrt();
    let core = (-(rn / core_w).powi(2)).exp();
    let rings = [(0.50_f32, 0.30_f32, 1.1_f32, 2.3_f32), (-0.62, 0.26, 4.0, 5.0)];
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

/// Matrice de Bayer 4×4 (tramage ordonné) : convertit l'intensité en densité de
/// points (le motif « plus ou moins resserré » façon Grok).
const BAYER4: [[f32; 4]; 4] = [
    [0.0, 8.0, 2.0, 10.0],
    [12.0, 4.0, 14.0, 6.0],
    [3.0, 11.0, 1.0, 9.0],
    [15.0, 7.0, 13.0, 5.0],
];

/// Disposition des 8 points d'une cellule braille → bit (base U+2800).
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

/// Rend un champ en points braille tramés : `cols × rows` cellules, chacune
/// échantillonnée sur 2×4 sous-points. La densité des points suit l'intensité,
/// boostée par `gamma` (< 1 = bords plus fournis, plus dense ; le fond vrai reste
/// vide car 0^gamma = 0). Monochrome : gris fonction du pic de la cellule.
fn render_braille(
    name: &str,
    cols: usize,
    rows: usize,
    scale: f32,
    gamma: f32,
    f: impl Fn(f32, f32) -> f32,
) {
    let (sw, sh) = (cols * 2, rows * 4); // sous-grille (carrée si cols = 2·rows)
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
            // Gris dans une bande médiane (ni trop sombre, ni blanc pur).
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

/// Rampe monochrome continue : gris sombre → gris moyen → presque blanc
/// (calée sur les gris du thème : faint/dim/fg).
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

/// Empile la grille en demi-blocs bi-color (fg = pixel haut, bg = pixel bas) et
/// imprime en ANSI truecolor, codes émis seulement sur changement.
fn render_grid(name: &str, g: &Grid) {
    const EPS: f32 = 0.07; // sous ce seuil : vide (transparent)
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
    println!("\n=== Banc d'essai logos Numen (cosmique / abstrait) ===");
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
    render_braille("11d. Dyson 30x15 (+ epais / + dense)", 30, 15, 1.05, 0.7, |x, y| {
        dyson_min_at(x, y, 0.11, 0.15)
    });
    render_braille("11e. Dyson 30x15 (max epais / dense)", 30, 15, 1.05, 0.5, |x, y| {
        dyson_min_at(x, y, 0.15, 0.18)
    });
    println!();
}
