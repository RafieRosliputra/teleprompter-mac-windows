//! Drawing helpers: mirrored text, reading-line arrows, and the row map that
//! keeps the prompter, the preview and the main script view in sync.

use std::sync::Arc;

use eframe::egui::{
    Align, Color32, Galley, Mesh, Painter, Pos2, Rect, Shape, Stroke, Vec2, epaint::Vertex, pos2,
};

/// Row geometry of a laid-out galley, indexed by character.
pub struct RowMap {
    starts: Vec<usize>,
    y0: Vec<f32>,
    h: Vec<f32>,
    #[allow(dead_code)]
    pub total_chars: usize,
    pub height: f32,
}

impl RowMap {
    pub fn new(g: &Galley) -> Self {
        let mut starts = Vec::with_capacity(g.rows.len());
        let mut y0 = Vec::with_capacity(g.rows.len());
        let mut h = Vec::with_capacity(g.rows.len());
        let mut c = 0usize;
        for r in &g.rows {
            starts.push(c);
            y0.push(r.pos.y);
            h.push(r.row.size.y.max(1.0));
            c += r.row.glyphs.len() + usize::from(r.ends_with_newline);
        }
        if starts.is_empty() {
            starts.push(0);
            y0.push(0.0);
            h.push(1.0);
        }
        let height = y0.last().unwrap() + h.last().unwrap();
        Self { starts, y0, h, total_chars: c, height }
    }

    fn row_at_y(&self, y: f32) -> usize {
        match self.y0.iter().rposition(|&r| r <= y) {
            Some(i) => i,
            None => 0,
        }
    }

    fn row_of_char(&self, c: usize) -> usize {
        self.starts.iter().rposition(|&s| s <= c).unwrap_or(0)
    }

    /// y (galley space) → (char index at row start, fraction inside row)
    pub fn y_to_pos(&self, y: f32) -> (usize, f32) {
        let i = self.row_at_y(y);
        let f = ((y - self.y0[i]) / self.h[i]).clamp(0.0, 1.0);
        (self.starts[i], f)
    }

    /// (char, fraction) → y (galley space)
    pub fn pos_to_y(&self, c: usize, frac: f32) -> f32 {
        let i = self.row_of_char(c);
        self.y0[i] + frac * self.h[i]
    }

    /// Centre y of the row that contains char `c`.
    pub fn row_center_of_char(&self, c: usize) -> f32 {
        let i = self.row_of_char(c);
        self.y0[i] + self.h[i] * 0.5
    }

    /// Row band (y0, height) at y.
    pub fn row_band(&self, y: f32) -> (f32, f32) {
        let i = self.row_at_y(y);
        (self.y0[i], self.h[i])
    }

    pub fn first_row_center(&self) -> f32 {
        self.y0[0] + self.h[0] * 0.5
    }
}

/// x of the galley anchor for the given alignment inside [left, right].
pub fn anchor_x(align: Align, left: f32, right: f32) -> f32 {
    match align {
        Align::Min => left,
        Align::Center => (left + right) * 0.5,
        Align::Max => right,
    }
}

#[derive(Clone, Copy)]
pub struct Flip {
    pub h: bool,
    pub v: bool,
    /// The area that is mirrored (the whole prompter screen / preview box).
    pub area: Rect,
}

impl Flip {
    pub fn none(area: Rect) -> Self {
        Self { h: false, v: false, area }
    }
    pub fn apply(&self, p: Pos2) -> Pos2 {
        let x = if self.h { self.area.left() + self.area.right() - p.x } else { p.x };
        let y = if self.v { self.area.top() + self.area.bottom() - p.y } else { p.y };
        pos2(x, y)
    }
}

/// Paint a galley at `pos` with optional mirroring, by copying the glyph
/// meshes egui already built for each row and transforming the vertices.
pub fn paint_galley(
    painter: &Painter,
    galley: &Arc<Galley>,
    pos: Pos2,
    flip: Flip,
    font_tex_size: [usize; 2],
    fallback: Color32,
) {
    let uvn = Vec2::new(1.0 / font_tex_size[0] as f32, 1.0 / font_tex_size[1] as f32);
    let clip = painter.clip_rect();
    // Unflipped visible band, so we can skip rows that are off-screen.
    let vis = Rect::from_two_pos(flip.apply(clip.min), flip.apply(clip.max));
    let mut mesh: Option<Mesh> = None;
    for row in &galley.rows {
        let vm = &row.row.visuals.mesh;
        if vm.is_empty() {
            continue;
        }
        let rp = pos + row.pos.to_vec2();
        let rb = row.row.visuals.mesh_bounds.translate(rp.to_vec2());
        if rb.bottom() < vis.top() - 4.0 || rb.top() > vis.bottom() + 4.0 {
            continue;
        }
        let m = mesh.get_or_insert_with(|| Mesh::with_texture(vm.texture_id));
        let base = m.vertices.len() as u32;
        m.indices.extend(vm.indices.iter().map(|i| i + base));
        m.vertices.extend(vm.vertices.iter().map(|v| {
            let mut color = v.color;
            if color == Color32::PLACEHOLDER {
                color = fallback;
            }
            Vertex {
                pos: flip.apply(rp + v.pos.to_vec2()),
                uv: (v.uv.to_vec2() * uvn).to_pos2(),
                color,
            }
        }));
    }
    if let Some(m) = mesh {
        painter.add(Shape::mesh(m));
    }
}

/// Two triangles pointing inwards at the reading line, at the left and right edge.
pub fn reading_arrows(painter: &Painter, area: Rect, y: f32, size: f32, color: Color32, flip: Flip) {
    let s = size;
    let l = [
        pos2(area.left() + 2.0, y - s * 0.6),
        pos2(area.left() + 2.0 + s, y),
        pos2(area.left() + 2.0, y + s * 0.6),
    ];
    let r = [
        pos2(area.right() - 2.0, y - s * 0.6),
        pos2(area.right() - 2.0 - s, y),
        pos2(area.right() - 2.0, y + s * 0.6),
    ];
    for tri in [l, r] {
        let pts: Vec<Pos2> = tri.iter().map(|p| flip.apply(*p)).collect();
        painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
    }
}
