//! The launcher's look, modelled on the game's own menus: heavy italic uppercase items with the festival magenta
//! highlight, over one of FH1's loading backdrops (from the user's converted data, so nothing from the game ships
//! with the launcher; before install, a painted dusk gradient stands in).
//!
//! Fonts come from the system like the engine's UiFont (ui.rs): Segoe UI Black Italic for titles and menu items and
//! Bahnschrift for body text on Windows, DejaVu / Liberation / Noto on Linux. None is redistributed; egui's default
//! font is the last fallback.

use std::path::Path;
use std::sync::Arc;

use eframe::egui::{self, pos2, vec2, Color32, FontFamily, FontId, Rect, Sense, Shape, Stroke, TextureHandle};

/// The festival magenta of the game's menus (ui.rs ACCENT).
pub const MAGENTA: Color32 = Color32::from_rgb(237, 41, 122);
pub const ORANGE: Color32 = Color32::from_rgb(255, 138, 60);
pub const PANEL: Color32 = Color32::from_rgba_premultiplied(8, 6, 16, 205);
pub const DIM: Color32 = Color32::from_rgb(170, 168, 182);
pub const OK: Color32 = Color32::from_rgb(110, 230, 150);
pub const BAD: Color32 = Color32::from_rgb(255, 110, 110);

pub fn heavy(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("heavy".into()))
}

pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Fonts and dark visuals with magenta selection.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windir = std::env::var_os("WINDIR").map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), std::path::PathBuf::from);
    // The first font file that exists: Windows' Fonts folder, else common Linux locations.
    let font = |names: &[&str]| {
        names.iter().find_map(|n| {
            let p = std::path::Path::new(n);
            let p = if p.is_absolute() { p.to_path_buf() } else { windir.join("Fonts").join(n) };
            std::fs::read(p).ok()
        })
    };
    let heavy_files = [
        "seguibli.ttf",
        "seguibl.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-BoldOblique.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-BoldOblique.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-BoldItalic.ttf",
        "/usr/share/fonts/liberation/LiberationSans-BoldItalic.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-BlackItalic.ttf",
        "/usr/share/fonts/noto/NotoSans-BlackItalic.ttf",
    ];
    let body_files = [
        "bahnschrift.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    ];
    let mut heavy_family = Vec::new();
    if let Some(b) = font(&heavy_files) {
        fonts.font_data.insert("heavy".into(), Arc::new(egui::FontData::from_owned(b)));
        heavy_family.push("heavy".to_owned());
    }
    if let Some(b) = font(&body_files) {
        fonts.font_data.insert("body".into(), Arc::new(egui::FontData::from_owned(b)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "body".into());
    }
    heavy_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("heavy".into()), heavy_family);
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = PANEL;
    v.selection.bg_fill = MAGENTA;
    v.hyperlink_color = ORANGE;
    v.extreme_bg_color = Color32::from_rgba_premultiplied(0, 0, 0, 170);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgba_premultiplied(40, 36, 56, 220);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgba_premultiplied(120, 24, 70, 230);
    v.widgets.active.weak_bg_fill = MAGENTA;
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(2);
    v.widgets.hovered.corner_radius = egui::CornerRadius::same(2);
    v.widgets.active.corner_radius = egui::CornerRadius::same(2);
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.text_styles.insert(egui::TextStyle::Body, body(16.0));
        s.text_styles.insert(egui::TextStyle::Button, body(16.0));
        s.spacing.item_spacing = vec2(10.0, 8.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
    });
}

/// A loading backdrop from the converted data (`ui/textures/horizon/loading/bgloadingimages/*.png`), picked by the
/// clock so each launch shows a different one.
pub fn load_backdrop(ctx: &egui::Context, private: &Path) -> Option<TextureHandle> {
    let dir = private.join("ui/textures/horizon/loading/bgloadingimages");
    let mut files: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        n.starts_with("image") && n.ends_with(".png")
    }).collect();
    files.sort();
    let pick = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as usize).unwrap_or(0);
    let path = files.get(pick % files.len().max(1))?;
    let bytes = std::fs::read(path).ok()?;
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..w * h * 4].to_vec(),
        png::ColorType::GrayscaleAlpha => buf[..w * h * 2].chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => return None,
    };
    Some(ctx.load_texture("backdrop", egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba), egui::TextureOptions::LINEAR))
}

/// The backdrop (cover-fitted) or the dusk gradient, then a dark wash from the left so the menu reads.
pub fn paint_backdrop(painter: &egui::Painter, rect: Rect, tex: Option<&TextureHandle>) {
    match tex {
        Some(t) => {
            let [tw, th] = t.size().map(|v| v as f32);
            let (sx, sy) = (rect.width() / tw, rect.height() / th);
            let s = sx.max(sy);
            let (uw, uh) = (rect.width() / (tw * s), rect.height() / (th * s));
            let uv = Rect::from_min_size(pos2((1.0 - uw) * 0.5, (1.0 - uh) * 0.5), vec2(uw, uh));
            painter.image(t.id(), rect, uv, Color32::WHITE);
        }
        None => {
            gradient_v(painter, rect, &[(0.0, Color32::from_rgb(22, 14, 54)), (0.55, Color32::from_rgb(92, 30, 96)), (1.0, Color32::from_rgb(214, 70, 92))]);
            // A slanted festival stripe.
            let (t, r, b) = (rect.top(), rect.right(), rect.bottom());
            painter.add(Shape::convex_polygon(
                vec![pos2(r - 420.0, t), pos2(r - 300.0, t), pos2(r - 560.0, b), pos2(r - 680.0, b)],
                Color32::from_rgba_unmultiplied(237, 41, 122, 40),
                Stroke::NONE,
            ));
            painter.add(Shape::convex_polygon(
                vec![pos2(r - 270.0, t), pos2(r - 230.0, t), pos2(r - 490.0, b), pos2(r - 530.0, b)],
                Color32::from_rgba_unmultiplied(255, 138, 60, 40),
                Stroke::NONE,
            ));
        }
    }
    gradient_h(painter, rect, Color32::from_rgba_unmultiplied(6, 4, 14, 225), Color32::from_rgba_unmultiplied(6, 4, 14, 40), 0.62);
}

fn gradient_v(painter: &egui::Painter, rect: Rect, stops: &[(f32, Color32)]) {
    let mut mesh = egui::Mesh::default();
    for (i, &(t, c)) in stops.iter().enumerate() {
        let y = rect.top() + rect.height() * t;
        mesh.colored_vertex(pos2(rect.left(), y), c);
        mesh.colored_vertex(pos2(rect.right(), y), c);
        if i > 0 {
            let k = (i as u32) * 2;
            mesh.add_triangle(k - 2, k - 1, k);
            mesh.add_triangle(k - 1, k + 1, k);
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// `from` at the left edge fading to `to` at `frac` of the width (and `to` beyond).
fn gradient_h(painter: &egui::Painter, rect: Rect, from: Color32, to: Color32, frac: f32) {
    let mut mesh = egui::Mesh::default();
    let mid = rect.left() + rect.width() * frac;
    for (x, c) in [(rect.left(), from), (mid, to), (rect.right(), to)] {
        mesh.colored_vertex(pos2(x, rect.top()), c);
        mesh.colored_vertex(pos2(x, rect.bottom()), c);
    }
    for k in [0u32, 2] {
        mesh.add_triangle(k, k + 1, k + 2);
        mesh.add_triangle(k + 1, k + 3, k + 2);
    }
    painter.add(Shape::mesh(mesh));
}

/// A slanted parallelogram behind `rect` (the game's menu highlight).
fn slant(painter: &egui::Painter, rect: Rect, color: Color32) {
    let k = rect.height() * 0.28;
    painter.add(Shape::convex_polygon(
        vec![
            pos2(rect.left() + k, rect.top()),
            pos2(rect.right() + k, rect.top()),
            pos2(rect.right() - k, rect.bottom()),
            pos2(rect.left() - k, rect.bottom()),
        ],
        color,
        Stroke::NONE,
    ));
}

/// The game logo lockup: "FH1" over "REWRITE" with the magenta-to-orange bar.
pub fn logo(ui: &mut egui::Ui, scale: f32) {
    let p = ui.painter().clone();
    let g1 = p.layout_no_wrap("FH1".into(), heavy(84.0 * scale), Color32::WHITE);
    let g2 = p.layout_no_wrap("REWRITE".into(), heavy(42.0 * scale), Color32::WHITE);
    let w = g1.size().x.max(g2.size().x) + 20.0;
    let h = g1.size().y * 0.86 + g2.size().y + 18.0 * scale;
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let shadow = Color32::from_black_alpha(140);
    let o1 = rect.left_top();
    let o2 = o1 + vec2(4.0, g1.size().y * 0.86);
    for (g, o) in [(&g1, o1), (&g2, o2)] {
        p.galley_with_override_text_color(o + vec2(3.0, 4.0), g.clone(), shadow);
        p.galley(o, g.clone(), Color32::WHITE);
    }
    let bar_y = o2.y + g2.size().y + 4.0 * scale;
    let bar = Rect::from_min_size(pos2(o1.x + 6.0, bar_y), vec2(220.0 * scale, 6.0 * scale));
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(bar.left_top(), MAGENTA);
    mesh.colored_vertex(bar.left_bottom(), MAGENTA);
    mesh.colored_vertex(bar.right_top(), Color32::from_rgb(255, 210, 63));
    mesh.colored_vertex(bar.right_bottom(), Color32::from_rgb(255, 210, 63));
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    p.add(Shape::mesh(mesh));
}

/// A main-menu item: heavy italic uppercase, magenta slant on hover (always on when `hot`). Greyed when disabled.
pub fn menu_item(ui: &mut egui::Ui, text: &str, size: f32, enabled: bool, hot: bool) -> egui::Response {
    let painter = ui.painter().clone();
    let color = if enabled { Color32::WHITE } else { Color32::from_gray(120) };
    let galley = painter.layout_no_wrap(text.to_uppercase(), heavy(size), color);
    let pad = vec2(18.0, 2.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, if enabled { Sense::click() } else { Sense::hover() });
    let lit = enabled && (hot || resp.hovered());
    if lit {
        slant(&painter, rect.shrink2(vec2(0.0, 3.0)), MAGENTA);
    } else if enabled {
        slant(&painter, rect.shrink2(vec2(0.0, 3.0)), Color32::from_rgba_unmultiplied(0, 0, 0, 90));
    }
    let pos = rect.left_top() + pad;
    painter.galley_with_override_text_color(pos + vec2(2.0, 3.0), galley.clone(), Color32::from_black_alpha(150));
    painter.galley(pos, galley, color);
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// A heavy italic section header.
pub fn header(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).font(heavy(20.0)).color(Color32::WHITE));
}

/// A translucent dark panel like the game's menu backing.
pub fn panel<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(PANEL)
        .inner_margin(egui::Margin::symmetric(22, 16))
        .corner_radius(egui::CornerRadius::same(4))
        .show(ui, add)
        .inner
}

/// The magenta progress bar.
pub fn progress(ui: &mut egui::Ui, frac: f32, text: &str) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_rgba_unmultiplied(0, 0, 0, 150));
    let fill = Rect::from_min_size(rect.min, vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()));
    p.rect_filled(fill, 2.0, MAGENTA);
    p.text(rect.left_center() + vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, text.to_uppercase(), heavy(14.0), Color32::WHITE);
}
