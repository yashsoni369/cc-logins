//! Tray icon rasterisation.
//!
//! The Windows system tray shows an image and a hover tooltip — it has no text
//! label (`set_title` is macOS-only, and Linux panels discourage titles). So the
//! utilisation number is drawn *into* the icon bitmap and the bitmap is redrawn
//! whenever the displayed value changes.
//!
//! Text is deliberately not rendered with a font. At 16px a system font's digits
//! collapse into grey mush; a hand-built 3x5 segment set stays pixel-crisp at
//! every integer scale and costs no dependency and no font licence.
//!
//! Design contract (see PRODUCT.md): colour encodes quota state and nothing
//! else. A healthy account is drawn in a near-neutral ink on the transparent
//! face with no colour at all.
//!
//! ## Ambient theme vs. app theme
//!
//! The icon must also read against the **OS taskbar/menu-bar background**,
//! which is an entirely different surface from this app's own day/night
//! setting. Someone can run the app in night mode while Windows itself uses a
//! light taskbar — the icon sits in the taskbar, not in the app window, so it
//! has to follow the taskbar's theme. [`AmbientTheme`] is that signal, probed
//! fresh from the OS and threaded through [`IconSpec`]; nothing in this module
//! reads the app's own settings.

use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// The background the tray icon is actually composited against: the OS
/// taskbar (Windows/Linux) or menu bar (macOS). Deliberately independent of
/// this app's own day/night preference — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbientTheme {
    Light,
    Dark,
}

impl AmbientTheme {
    /// Detect the OS's tray/taskbar background. Never used from tests —
    /// tests inject an [`AmbientTheme`] directly so the colour and
    /// cache-key logic can be exercised without touching the registry.
    #[cfg(windows)]
    pub fn detect() -> Self {
        detect_windows().unwrap_or(AmbientTheme::Dark)
    }

    #[cfg(target_os = "macos")]
    pub fn detect() -> Self {
        // Tray icons are template images on macOS (`icon_as_template(true)`,
        // set in `lib.rs`), so AppKit itself recolours the bitmap for the
        // current menu-bar appearance after the fact — this module's
        // light/dark ink branch never actually runs there. The constant only
        // needs to be *a* valid value so the shared rendering path and cache
        // key stay well-defined.
        AmbientTheme::Dark
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn detect() -> Self {
        // No reliable cross-desktop-environment signal on Linux: GNOME, KDE,
        // XFCE and friends each expose (or don't expose) panel theme
        // differently, and none of it is queryable from a background
        // process without a desktop-specific dependency. Default to dark,
        // matching both the common case for Linux panels and this app's
        // existing default everywhere else absent better information.
        AmbientTheme::Dark
    }
}

/// Read `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`,
/// `SystemUsesLightTheme` (DWORD; 1 = light taskbar, 0 = dark).
///
/// This shells out to `reg query` rather than calling the Win32 registry API
/// directly: the native route needs `windows-sys`'s `Win32_System_Registry`
/// feature, which isn't enabled on the dependency in `Cargo.toml` (out of
/// scope for this change — see the accompanying report). `reg query` is the
/// documented fallback and only runs when a caller asks for the current
/// theme, not per pixel.
#[cfg(windows)]
fn detect_windows() -> Option<AmbientTheme> {
    use std::process::Command;

    let output = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "SystemUsesLightTheme",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        // Missing value (older Windows, or a stripped-down profile) — the
        // key documents Windows' own default as dark.
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|l| l.contains("SystemUsesLightTheme"))?;
    let hex = line.split_whitespace().last()?;
    let value = u32::from_str_radix(hex.trim_start_matches("0x"), 16).ok()?;
    Some(if value != 0 { AmbientTheme::Light } else { AmbientTheme::Dark })
}

/// Quota state. Thresholds match the auto-switch defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Healthy — rendered monochrome, no colour.
    Ok,
    /// >= 75% on the binding window.
    Caution,
    /// >= 90% on the binding window.
    Critical,
    /// A switch is in flight.
    Switching,
    /// Usage data is too old to trust, or the environment is unreachable.
    Stale,
    /// No accounts configured yet.
    Unconfigured,
}

impl State {
    /// Classify by utilisation of the worst (binding) window.
    pub fn from_utilisation(pct: f32) -> Self {
        if pct >= 90.0 {
            State::Critical
        } else if pct >= 75.0 {
            State::Caution
        } else {
            State::Ok
        }
    }

    /// Foreground colour for the numeral and arc, tuned for the ambient
    /// taskbar background.
    ///
    /// Dark-taskbar values are sRGB, matching the `--ink` / `--caution` /
    /// `--danger` tokens in `src/styles/tokens.css` (night theme). Light-
    /// taskbar values are the *same* design system's **day**-theme tokens —
    /// `--ink`, `--caution`, `--danger`, `--muted` under `prefers-color-
    /// scheme: light` — converted from oklch to sRGB, since those are
    /// already the app's contrast-checked choices for ink on a light
    /// surface. Plain amber (`#E8A53A`) or the resting bone tone read as
    /// near-invisible on white; the day tokens are deliberately darker and
    /// more saturated for exactly that reason.
    fn ink(self, theme: AmbientTheme) -> Color {
        match theme {
            AmbientTheme::Dark => match self {
                // oklch(0.945 0.006 90) — night --ink
                State::Ok | State::Switching => Color::from_rgba8(0xEE, 0xEC, 0xE7, 255),
                // oklch(0.790 0.145 75) — night --caution
                State::Caution => Color::from_rgba8(0xE8, 0xA5, 0x3A, 255),
                // oklch(0.640 0.190 27) — night --danger
                State::Critical => Color::from_rgba8(0xE0, 0x5A, 0x47, 255),
                // dimmed bone
                State::Stale | State::Unconfigured => Color::from_rgba8(0x88, 0x86, 0x82, 255),
            },
            AmbientTheme::Light => match self {
                // oklch(0.220 0.006 250) — day --ink
                State::Ok | State::Switching => Color::from_rgba8(0x19, 0x1B, 0x1D, 255),
                // oklch(0.500 0.140 65) — day --caution
                State::Caution => Color::from_rgba8(0x97, 0x4D, 0x00, 255),
                // oklch(0.500 0.190 27) — day --danger
                State::Critical => Color::from_rgba8(0xB7, 0x19, 0x1C, 255),
                // oklch(0.470 0.006 250) — day --muted
                State::Stale | State::Unconfigured => Color::from_rgba8(0x58, 0x5B, 0x5E, 255),
            },
        }
    }

    /// The unfilled portion of the ring. "Dimmer than the primary ink,
    /// legible against the ambient background" in both themes: on a dark
    /// taskbar that means lighter-than-background grey; on a light taskbar
    /// it means darker-than-background grey. The stale variant moves a step
    /// *toward* the background (rather than away from it) to read as
    /// visibly de-emphasised in both cases.
    fn track(self, theme: AmbientTheme) -> Color {
        match theme {
            AmbientTheme::Dark => match self {
                State::Stale => Color::from_rgba8(0x55, 0x54, 0x51, 160),
                _ => Color::from_rgba8(0x66, 0x65, 0x62, 200),
            },
            AmbientTheme::Light => match self {
                State::Stale => Color::from_rgba8(0xB5, 0xB3, 0xAF, 160),
                _ => Color::from_rgba8(0x8C, 0x8A, 0x86, 200),
            },
        }
    }
}

/// What to draw. `utilisation` is the worst window's percentage, 0..=100.
#[derive(Debug, Clone, Copy)]
pub struct IconSpec {
    pub utilisation: Option<f32>,
    pub state: State,
    /// Rotation of the arc for the switching animation, in turns (0.0..1.0).
    pub spin: f32,
    /// Ambient OS taskbar/menu-bar background — see the module docs. Not the
    /// app's own day/night setting.
    pub theme: AmbientTheme,
}

impl IconSpec {
    pub fn resting(utilisation: f32, theme: AmbientTheme) -> Self {
        Self {
            utilisation: Some(utilisation),
            state: State::from_utilisation(utilisation),
            spin: 0.0,
            theme,
        }
    }

    pub fn unconfigured(theme: AmbientTheme) -> Self {
        Self { utilisation: None, state: State::Unconfigured, spin: 0.0, theme }
    }

    /// A cheap identity for the rendered result. The rasteriser is only re-run
    /// when this changes, which at 16px is a handful of times an hour rather
    /// than once per poll. Folding in `theme` means a taskbar theme change
    /// (e.g. Windows flipping light/dark) forces a redraw instead of leaving
    /// a stale, wrong-contrast bitmap on screen.
    pub fn cache_key(&self) -> u64 {
        let pct = self.utilisation.map(|p| p.round() as i64).unwrap_or(-1);
        let spin = (self.spin * 12.0).round() as i64;
        let theme = matches!(self.theme, AmbientTheme::Light) as u64;
        ((pct + 1) as u64) << 32 | (theme << 16) | (self.state as u64) << 8 | (spin as u64 & 0xFF)
    }
}

// ── 3x5 digit set ────────────────────────────────────────────────────────────
// Each digit is 5 rows of 3 bits, most-significant bit leftmost.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

/// Em dash, for the unconfigured state.
const DASH: [u8; 5] = [0b000, 0b000, 0b111, 0b000, 0b000];
/// Right arrow, for the switching state.
const ARROW: [u8; 5] = [0b010, 0b001, 0b111, 0b001, 0b010];

const GLYPH_W: u32 = 3;
const GLYPH_H: u32 = 5;
const GLYPH_GAP: u32 = 1;

/// Render an RGBA8 icon of `size` x `size` pixels.
///
/// Returns **straight** (non-premultiplied) RGBA, which is what
/// `tauri::image::Image` expects. tiny-skia works in premultiplied space, so
/// the buffer is demultiplied on the way out — without this the anti-aliased
/// edge of the arc renders too dark.
pub fn render(spec: IconSpec, size: u32) -> Vec<u8> {
    let mut pm = Pixmap::new(size, size).expect("non-zero icon size");

    // The ring only appears where there is room for it. At 16px the digits
    // need the whole face, and a 1px ring around them reads as noise.
    let draw_ring = size >= 24;
    if draw_ring {
        draw_arc(&mut pm, spec, size);
    }

    let glyphs = glyphs_for(spec);
    if !glyphs.is_empty() {
        // Fit the glyph run to the space left by the ring.
        let inner = if draw_ring { (size as f32 * 0.60) as u32 } else { size - 2 };
        let run_w = glyphs.len() as u32 * GLYPH_W + (glyphs.len() as u32 - 1) * GLYPH_GAP;
        let scale = ((inner / run_w.max(1)).min(inner / GLYPH_H)).max(1);

        let total_w = run_w * scale;
        let total_h = GLYPH_H * scale;
        let ox = (size.saturating_sub(total_w)) / 2;
        let oy = (size.saturating_sub(total_h)) / 2;

        let ink = spec.state.ink(spec.theme);
        for (i, g) in glyphs.iter().enumerate() {
            let gx = ox + i as u32 * (GLYPH_W + GLYPH_GAP) * scale;
            blit_glyph(&mut pm, g, gx, oy, scale, ink);
        }
    }

    demultiply(pm)
}

/// Convert tiny-skia's premultiplied buffer to straight RGBA.
fn demultiply(pm: Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pm.pixels().len() * 4);
    for px in pm.pixels() {
        let c = px.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

fn glyphs_for(spec: IconSpec) -> Vec<[u8; 5]> {
    match spec.state {
        State::Unconfigured => vec![DASH],
        State::Switching => vec![ARROW],
        _ => match spec.utilisation {
            None => vec![DASH],
            Some(p) => {
                let v = p.round().clamp(0.0, 99.0) as u32;
                if v >= 10 {
                    vec![DIGITS[(v / 10) as usize], DIGITS[(v % 10) as usize]]
                } else {
                    vec![DIGITS[v as usize]]
                }
            }
        },
    }
}

/// Draw the track and, for a known utilisation, the filled arc.
fn draw_arc(pm: &mut Pixmap, spec: IconSpec, size: u32) {
    let s = size as f32;
    let stroke_w = (s * 0.075).max(1.5);
    let r = s / 2.0 - stroke_w / 2.0 - 1.0;
    let cx = s / 2.0;
    let cy = s / 2.0;

    let mut stroke = Stroke::default();
    stroke.width = stroke_w;

    // Track. Stale draws it dashed to signal that the reading is not current.
    if let Some(path) = arc_path(cx, cy, r, 0.0, 1.0) {
        let mut paint = Paint::default();
        paint.set_color(spec.state.track(spec.theme));
        paint.anti_alias = true;
        if spec.state == State::Stale {
            stroke.dash = tiny_skia::StrokeDash::new(vec![stroke_w * 1.6, stroke_w * 1.6], 0.0);
        }
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        stroke.dash = None;
    }

    if spec.state == State::Unconfigured || spec.state == State::Stale {
        return;
    }

    let (start, sweep) = match spec.state {
        // A fixed 90-degree segment that the caller rotates each frame.
        State::Switching => (spec.spin, 0.25),
        _ => (0.0, spec.utilisation.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0),
    };

    if sweep > 0.001 {
        if let Some(path) = arc_path(cx, cy, r, start, sweep) {
            let mut paint = Paint::default();
            paint.set_color(spec.state.ink(spec.theme));
            paint.anti_alias = true;
            pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}

/// Build a circular arc starting at 12 o'clock, clockwise.
/// `start` and `sweep` are in turns.
fn arc_path(cx: f32, cy: f32, r: f32, start: f32, sweep: f32) -> Option<tiny_skia::Path> {
    const TAU: f32 = std::f32::consts::TAU;
    // Flatten to line segments; at icon sizes the error is far below a pixel.
    let steps = ((sweep.abs() * 96.0).ceil() as usize).max(2);
    let mut pb = PathBuilder::new();
    for i in 0..=steps {
        let t = start + sweep * (i as f32 / steps as f32);
        let a = t * TAU - TAU / 4.0; // rotate so 0 turns == 12 o'clock
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.finish()
}

/// Blit a 3x5 glyph at an integer scale. Nearest-neighbour by construction, so
/// edges stay hard at every size.
fn blit_glyph(pm: &mut Pixmap, glyph: &[u8; 5], ox: u32, oy: u32, scale: u32, color: Color) {
    let w = pm.width();
    let h = pm.height();
    let c = color.premultiply().to_color_u8();
    let data = pm.pixels_mut();

    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..GLYPH_W {
            // MSB is the leftmost column.
            if bits & (1 << (GLYPH_W - 1 - col)) == 0 {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = ox + col * scale + dx;
                    let y = oy + row as u32 * scale + dy;
                    if x < w && y < h {
                        data[(y * w + x) as usize] = c;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEMES: [AmbientTheme; 2] = [AmbientTheme::Dark, AmbientTheme::Light];

    fn luma(c: Color) -> f32 {
        0.299 * c.red() + 0.587 * c.green() + 0.114 * c.blue()
    }

    #[test]
    fn classifies_by_binding_window() {
        assert_eq!(State::from_utilisation(0.0), State::Ok);
        assert_eq!(State::from_utilisation(74.9), State::Ok);
        assert_eq!(State::from_utilisation(75.0), State::Caution);
        assert_eq!(State::from_utilisation(89.9), State::Caution);
        assert_eq!(State::from_utilisation(90.0), State::Critical);
        assert_eq!(State::from_utilisation(100.0), State::Critical);
    }

    #[test]
    fn renders_every_size_without_panicking() {
        for theme in THEMES {
            for size in [16u32, 20, 24, 32, 48, 64] {
                let out = render(IconSpec::resting(61.0, theme), size);
                assert_eq!(out.len(), (size * size * 4) as usize);
            }
        }
    }

    #[test]
    fn single_digit_utilisation_uses_one_glyph() {
        assert_eq!(glyphs_for(IconSpec::resting(4.0, AmbientTheme::Dark)).len(), 1);
        assert_eq!(glyphs_for(IconSpec::resting(13.0, AmbientTheme::Dark)).len(), 2);
        // 100% has to fit two glyphs, so it clamps to 99.
        assert_eq!(glyphs_for(IconSpec::resting(100.0, AmbientTheme::Dark)).len(), 2);
    }

    #[test]
    fn unconfigured_and_switching_use_symbols() {
        assert_eq!(glyphs_for(IconSpec::unconfigured(AmbientTheme::Dark)), vec![DASH]);
        let sw = IconSpec {
            utilisation: Some(50.0),
            state: State::Switching,
            spin: 0.0,
            theme: AmbientTheme::Dark,
        };
        assert_eq!(glyphs_for(sw), vec![ARROW]);
    }

    #[test]
    fn cache_key_changes_with_value_and_state() {
        let a = IconSpec::resting(61.0, AmbientTheme::Dark);
        let b = IconSpec::resting(62.0, AmbientTheme::Dark);
        let c = IconSpec::resting(91.0, AmbientTheme::Dark);
        assert_ne!(a.cache_key(), b.cache_key());
        assert_ne!(b.cache_key(), c.cache_key());
        // Sub-integer noise must not invalidate the cache.
        assert_eq!(
            IconSpec::resting(61.2, AmbientTheme::Dark).cache_key(),
            IconSpec::resting(61.4, AmbientTheme::Dark).cache_key()
        );
    }

    #[test]
    fn cache_key_changes_with_ambient_theme() {
        // A taskbar theme flip must force a redraw, or the icon can be left
        // rendered in the wrong (near-invisible) contrast.
        let dark = IconSpec::resting(61.0, AmbientTheme::Dark);
        let light = IconSpec::resting(61.0, AmbientTheme::Light);
        assert_ne!(dark.cache_key(), light.cache_key());

        let dark_unconf = IconSpec::unconfigured(AmbientTheme::Dark);
        let light_unconf = IconSpec::unconfigured(AmbientTheme::Light);
        assert_ne!(dark_unconf.cache_key(), light_unconf.cache_key());
    }

    #[test]
    fn healthy_icon_is_near_neutral_in_both_themes() {
        // The design contract: no colour at rest, in either ambient theme.
        for theme in THEMES {
            let ink = State::Ok.ink(theme);
            let (r, g, b) = (ink.red(), ink.green(), ink.blue());
            let spread = [r, g, b].iter().cloned().fold(f32::MIN, f32::max)
                - [r, g, b].iter().cloned().fold(f32::MAX, f32::min);
            assert!(spread < 0.05, "{theme:?}: resting ink should be near-neutral, spread was {spread}");
        }
    }

    #[test]
    fn healthy_ink_inverts_lightness_between_themes() {
        // The actual bug: on a light taskbar the healthy ink must be a dark
        // neutral, not the near-white bone used on a dark taskbar.
        let on_dark_taskbar = State::Ok.ink(AmbientTheme::Dark);
        let on_light_taskbar = State::Ok.ink(AmbientTheme::Light);
        assert!(luma(on_dark_taskbar) > 0.8, "healthy ink on a dark taskbar should be near-white");
        assert!(luma(on_light_taskbar) < 0.3, "healthy ink on a light taskbar should be near-black");
    }

    #[test]
    fn caution_and_critical_are_darkened_for_light_taskbars() {
        // Amber (#E8A53A) and the dark-taskbar red are both weak on white;
        // the light-taskbar variants must be meaningfully darker.
        assert!(luma(State::Caution.ink(AmbientTheme::Light)) < luma(State::Caution.ink(AmbientTheme::Dark)));
        assert!(luma(State::Critical.ink(AmbientTheme::Light)) < luma(State::Critical.ink(AmbientTheme::Dark)));
        // And still legibly lighter than pure black, i.e. not collapsed to ink.
        assert!(luma(State::Caution.ink(AmbientTheme::Light)) > 0.05);
        assert!(luma(State::Critical.ink(AmbientTheme::Light)) > 0.05);
    }

    #[test]
    fn state_ramp_is_distinguishable_within_each_theme() {
        for theme in THEMES {
            let colors = [
                State::Ok.ink(theme),
                State::Caution.ink(theme),
                State::Critical.ink(theme),
                State::Stale.ink(theme),
            ];
            for i in 0..colors.len() {
                for j in (i + 1)..colors.len() {
                    let a = colors[i];
                    let b = colors[j];
                    let dist = ((a.red() - b.red()).powi(2)
                        + (a.green() - b.green()).powi(2)
                        + (a.blue() - b.blue()).powi(2))
                    .sqrt();
                    assert!(dist > 0.08, "{theme:?}: states {i} and {j} are too close ({dist})");
                }
            }
        }
    }
}
