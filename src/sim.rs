//! Interactive water: the classic two-buffer height-field wave equation on a
//! grid downsampled from the taskbar, cheap enough to run on the CPU. The
//! renderer uploads the field as a texture and shades it with normals derived
//! in the pixel shader.

/// Simulation cell size in output pixels. 2 keeps ripple shapes smooth on a
/// 48px-tall bar while the grid stays tiny (~960x24 at 1080p).
pub const CELL: u32 = 2;

pub struct RippleSim {
    pub width: u32,
    pub height: u32,
    curr: Vec<f32>,
    prev: Vec<f32>,
    /// Total energy estimate, used to decide when the water has settled.
    energy: f32,
}

impl RippleSim {
    pub fn new(surface_w: u32, surface_h: u32) -> Self {
        let width = (surface_w / CELL).max(8);
        let height = (surface_h / CELL).max(4);
        let n = (width * height) as usize;
        Self { width, height, curr: vec![0.0; n], prev: vec![0.0; n], energy: 0.0 }
    }

    /// Drop a disturbance at surface-pixel coordinates. `radius` in pixels,
    /// `strength` roughly -1..1 (negative = push down, like something landing).
    pub fn splash(&mut self, px: f32, py: f32, radius: f32, strength: f32) {
        let cx = px / CELL as f32;
        let cy = py / CELL as f32;
        let r = (radius / CELL as f32).max(1.0);
        let x0 = ((cx - r).floor().max(0.0)) as u32;
        let x1 = ((cx + r).ceil().min(self.width as f32 - 1.0)) as u32;
        let y0 = ((cy - r).floor().max(0.0)) as u32;
        let y1 = ((cy + r).ceil().min(self.height as f32 - 1.0)) as u32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d2 = (dx * dx + dy * dy) / (r * r);
                if d2 < 1.0 {
                    // Smooth cosine bump.
                    let w = 0.5 + 0.5 * (std::f32::consts::PI * d2.sqrt()).cos();
                    self.curr[(y * self.width + x) as usize] += strength * w;
                }
            }
        }
        self.energy += strength.abs();
    }

    /// One fixed step of the wave equation. Call at the active frame rate.
    pub fn step(&mut self) {
        const DAMPING: f32 = 0.985;
        let w = self.width as usize;
        let h = self.height as usize;
        let mut energy = 0.0f32;
        for y in 0..h {
            let up = if y == 0 { 0 } else { y - 1 } * w;
            let down = if y == h - 1 { y } else { y + 1 } * w;
            let row = y * w;
            for x in 0..w {
                let left = row + x.saturating_sub(1);
                let right = row + (x + 1).min(w - 1);
                let neighbors =
                    self.curr[up + x] + self.curr[down + x] + self.curr[left] + self.curr[right];
                let next = (neighbors * 0.5 - self.prev[row + x]) * DAMPING;
                self.prev[row + x] = next;
                energy += next * next;
            }
        }
        std::mem::swap(&mut self.curr, &mut self.prev);
        self.energy = energy;
    }

    /// Water height in [-1, 1]-ish units at a surface-pixel x, sampled along
    /// the vertical middle of the pool — used to bob the character.
    pub fn height_at(&self, px: f32) -> f32 {
        let x = ((px / CELL as f32) as u32).min(self.width - 1);
        let y = self.height / 2;
        self.curr[(y * self.width + x) as usize]
    }

    /// True once ripples have decayed below perception; lets the app drop to
    /// the idle frame rate.
    pub fn is_calm(&self) -> bool {
        self.energy < 1e-4
    }

    pub fn field(&self) -> &[f32] {
        &self.curr
    }
}
