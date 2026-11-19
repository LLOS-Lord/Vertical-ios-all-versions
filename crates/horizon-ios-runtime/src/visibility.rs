//! Frustum / occlusion / GPU-budget culling: never draw what cannot be seen.
//!
//! The tree already has serious culling (`gfx/gpu_culling.rs`, `gfx/gpu_culling.wgsl`,
//! `game/occlusion_culling.rs`, `entity_culling.rs`, `particle_culling.rs`,
//! `mesher/leaf_culling.rs`) — this module is the *policy* layer on top: a bounded GPU
//! budget and a deterministic skip/LOD decision for blocks, entities and particles,
//! aligned with Sodium's visibility pipeline (frustum + graph occlusion BFS, async
//! traversal, batched draws). It is intentionally pure so it can be unit-tested on Linux.

/// A plane `nx*x + ny*y + nz*z + d >= 0` is inside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plane {
    pub nx: f32,
    pub ny: f32,
    pub nz: f32,
    pub d: f32,
}
impl Plane {
    pub fn distance(&self, p: [f32; 3]) -> f32 {
        self.nx * p[0] + self.ny * p[1] + self.nz * p[2] + self.d
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}
impl Aabb {
    pub fn new(min: [f32; 3], max: [f32; 3]) -> Self {
        Self { min, max }
    }
    pub fn center(&self) -> [f32; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }
}

/// Six-plane frustum. `from_camera` builds a simple symmetric perspective frustum in
/// view space; the renderer should supply the exact `view_proj` planes instead.
#[derive(Clone, Debug, PartialEq)]
pub struct Frustum {
    pub planes: [Plane; 6],
}
impl Frustum {
    pub fn from_camera(
        position: [f32; 3],
        forward: [f32; 3],
        near: f32,
        far: f32,
        tan_half_fov: f32,
    ) -> Self {
        let f = Self::normalize(forward);
        let up = [0.0, 1.0, 0.0];
        let right = Self::cross(f, up);
        let right = Self::normalize(right);
        let up = Self::normalize(Self::cross(right, f));
        let at_near = |i| position[i] + f[i] * near;
        let at_far = |i| position[i] + f[i] * far;
        let near_p = [at_near(0), at_near(1), at_near(2)];
        let far_p = [at_far(0), at_far(1), at_far(2)];
        let plane = |n: [f32; 3], p: [f32; 3]| Plane {
            nx: n[0],
            ny: n[1],
            nz: n[2],
            d: -(n[0] * p[0] + n[1] * p[1] + n[2] * p[2]),
        };
        // Left/right/top/bottom planes through the eye (conservative pyramid), plus near/far.
        // Inward normal = slope * forward - outward_axis, so dot(n, p) >= 0 inside.
        let side = |axis: [f32; 3], slope: f32| {
            let n = Self::normalize([
                f[0] * slope - axis[0],
                f[1] * slope - axis[1],
                f[2] * slope - axis[2],
            ]);
            plane(n, position)
        };
        Self {
            planes: [
                side(right, tan_half_fov),
                side([-right[0], -right[1], -right[2]], tan_half_fov),
                side(up, tan_half_fov),
                side([-up[0], -up[1], -up[2]], tan_half_fov),
                plane(f, near_p),
                plane([-f[0], -f[1], -f[2]], far_p),
            ],
        }
    }
    fn normalize(v: [f32; 3]) -> [f32; 3] {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
        [v[0] / l, v[1] / l, v[2] / l]
    }
    fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    }
    /// Standard p-vertex / n-vertex AABB test: pick the corner furthest along the
    /// plane normal; if even that corner is outside, the whole box is outside.
    pub fn intersects(&self, box_: Aabb) -> bool {
        for p in &self.planes {
            let n = [p.nx, p.ny, p.nz];
            let mut pv = [0.0; 3];
            for i in 0..3 {
                pv[i] = if n[i] >= 0.0 {
                    box_.max[i]
                } else {
                    box_.min[i]
                };
            }
            if p.distance(pv) < 0.0 {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    ChunkSection,
    BlockEntity,
    Entity,
    Particle,
    FarTile,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub key: u64,
    pub kind: CandidateKind,
    pub aabb: Aabb,
    pub distance: f32,
    /// Projected size in device pixels; below 1 px an entity/particle is invisible.
    pub pixel_size: f32,
    /// The upstream occlusion graph (or Hi-Z) says nothing behind it can be seen.
    pub occluded: bool,
    /// This section is needed for the simulation (tick), even if not drawn.
    pub simulation_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CullDecision {
    Draw,
    /// Draw a cheaper LOD mesh (far tiles / distant sections).
    Lod,
    /// Skip rendering entirely. `simulation_required` candidates still tick.
    Skip,
}

#[derive(Clone, Debug)]
pub struct GpuBudget {
    pub max_draws_per_frame: usize,
    pub max_gpu_ms: f32,
    pub measured_gpu_ms: f32,
    pub lod_distance: f32,
    pub max_render_distance: f32,
    pub pressure_reduce: bool,
}
impl GpuBudget {
    pub fn new(max_draws: usize, max_gpu_ms: f32) -> Self {
        Self {
            max_draws_per_frame: max_draws,
            max_gpu_ms,
            measured_gpu_ms: 0.0,
            lod_distance: 96.0,
            max_render_distance: 256.0,
            pressure_reduce: false,
        }
    }
    /// When the GPU is already over budget, cut the visible set instead of pushing more
    /// command buffers at a chip that cannot keep up.
    pub fn effective_draws(&self) -> usize {
        let gpu_scale = if self.measured_gpu_ms > self.max_gpu_ms {
            0.6
        } else {
            1.0
        };
        let mem_scale = if self.pressure_reduce { 0.7 } else { 1.0 };
        ((self.max_draws_per_frame as f32) * gpu_scale * mem_scale).floor() as usize
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisibilitySet {
    pub drawn: Vec<u64>,
    pub lod: Vec<u64>,
    pub skipped: Vec<u64>,
}
impl VisibilitySet {
    pub fn draw_count(&self) -> usize {
        self.drawn.len() + self.lod.len()
    }
}

/// Sort by priority (near, large, not occluded), frustum-cull, occlusion-cull, then
/// apply the bounded GPU draw budget. Deterministic: no frame-to-frame flicker.
pub fn cull(candidates: &[Candidate], frustum: &Frustum, budget: &GpuBudget) -> VisibilitySet {
    let mut order: Vec<&Candidate> = candidates.iter().collect();
    order.sort_by(|a, b| {
        b.kind
            .kind_priority()
            .cmp(&a.kind.kind_priority())
            .then_with(|| a.occluded.cmp(&b.occluded))
            .then_with(|| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.key.cmp(&b.key))
    });
    let mut out = VisibilitySet::default();
    let limit = budget.effective_draws();
    let render_distance = if budget.pressure_reduce {
        budget.max_render_distance * 0.7
    } else {
        budget.max_render_distance
    };
    for c in order {
        let invisible_detail =
            matches!(c.kind, CandidateKind::Entity | CandidateKind::Particle) && c.pixel_size < 1.0;
        let skip = !frustum.intersects(c.aabb)
            || c.occluded
            || c.distance > render_distance
            || invisible_detail;
        if skip {
            out.skipped.push(c.key);
            continue;
        }
        if out.draw_count() >= limit {
            // The GPU budget is a hard cap: even a cheap LOD mesh is still a draw call.
            // Lower-priority candidates (sorted far/small last) are skipped outright.
            out.skipped.push(c.key);
            continue;
        }
        if c.distance > budget.lod_distance && c.kind != CandidateKind::ChunkSection {
            out.lod.push(c.key);
        } else {
            out.drawn.push(c.key);
        }
    }
    out
}

impl CandidateKind {
    fn kind_priority(self) -> u8 {
        match self {
            Self::ChunkSection => 4,
            Self::BlockEntity => 3,
            Self::Entity => 2,
            Self::Particle => 1,
            Self::FarTile => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frustum() -> Frustum {
        // Eye at origin looking down -Z, 90-degree-ish cone.
        Frustum::from_camera([0.0, 0.0, 0.0], [0.0, 0.0, -1.0], 0.1, 300.0, 1.0)
    }
    fn box_at(x: f32, y: f32, z: f32, s: f32) -> Aabb {
        Aabb::new([x - s, y - s, z - s], [x + s, y + s, z + s])
    }
    fn cand(key: u64, z: f32, kind: CandidateKind) -> Candidate {
        Candidate {
            key,
            kind,
            aabb: box_at(0.0, 0.0, z, 8.0),
            distance: -z,
            pixel_size: 20.0,
            occluded: false,
            simulation_required: false,
        }
    }

    #[test]
    fn a_box_behind_the_camera_is_culled() {
        let f = frustum();
        assert!(f.intersects(box_at(0.0, 0.0, -50.0, 8.0)));
        assert!(!f.intersects(box_at(0.0, 0.0, 50.0, 8.0)));
    }

    #[test]
    fn occluded_and_out_of_range_candidates_are_skipped() {
        let f = frustum();
        let mut budget = GpuBudget::new(100, 12.0);
        budget.max_render_distance = 100.0;
        let mut occluded = cand(1, -20.0, CandidateKind::ChunkSection);
        occluded.occluded = true;
        let far = cand(2, -250.0, CandidateKind::ChunkSection);
        let near = cand(3, -20.0, CandidateKind::ChunkSection);
        let out = cull(&[occluded, far, near], &f, &budget);
        assert!(out.skipped.contains(&1));
        assert!(out.skipped.contains(&2));
        assert!(out.drawn.contains(&3));
    }

    #[test]
    fn sub_pixel_entities_and_particles_are_not_drawn() {
        let f = frustum();
        let budget = GpuBudget::new(100, 12.0);
        let mut tiny = cand(7, -10.0, CandidateKind::Entity);
        tiny.pixel_size = 0.4;
        let out = cull(&[tiny], &f, &budget);
        assert_eq!(out.skipped, vec![7]);
        assert_eq!(out.draw_count(), 0);
    }

    #[test]
    fn gpu_budget_and_memory_pressure_shrink_the_visible_set() {
        let f = frustum();
        let mut budget = GpuBudget::new(3, 12.0);
        let items: Vec<_> = (0..10)
            .map(|i| cand(i, -(10.0 + i as f32), CandidateKind::ChunkSection))
            .collect();
        let normal = cull(&items, &f, &budget);
        assert_eq!(normal.draw_count(), 3);
        budget.measured_gpu_ms = 20.0;
        budget.pressure_reduce = true;
        let stressed = cull(&items, &f, &budget);
        assert!(stressed.draw_count() < normal.draw_count());
        assert!(stressed.draw_count() <= 3);
    }

    #[test]
    fn distant_candidates_degrade_to_lod_before_being_dropped() {
        let f = frustum();
        let mut budget = GpuBudget::new(100, 12.0);
        budget.lod_distance = 50.0;
        let near = cand(1, -10.0, CandidateKind::ChunkSection);
        let far = cand(2, -80.0, CandidateKind::FarTile);
        let out = cull(&[near, far], &f, &budget);
        assert!(out.drawn.contains(&1));
        assert!(out.lod.contains(&2));
    }

    #[test]
    fn culling_is_deterministic_regardless_of_input_order() {
        let f = frustum();
        let budget = GpuBudget::new(5, 12.0);
        let a: Vec<_> = (0..12)
            .map(|i| cand(i, -(5.0 + i as f32 * 3.0), CandidateKind::ChunkSection))
            .collect();
        let mut b = a.clone();
        b.reverse();
        assert_eq!(cull(&a, &f, &budget), cull(&b, &f, &budget));
    }
}
