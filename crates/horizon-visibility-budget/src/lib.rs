//! What is worth drawing this frame, and in what order.
//!
//! # The waste on a phone
//!
//! A 12-chunk render distance loads 625 columns, each with up to 24 sections: ~15 000 sections, of
//! which a phone's camera can see perhaps a tenth. The renderer already walks them and the GPU
//! already culls them, but *walking* is the cost that matters on an A11: every section submission
//! is a bind group, a draw, a uniform write, and the CPU side of that dominates at 8 ms/frame.
//! Beyond that, entities, block entities and particles are drawn without any notion of how much of
//! the frame they are entitled to, so a villager crowd or an explosion can take the frame and the
//! terrain the player is standing on arrives late.
//!
//! # The rule
//!
//! [`Score`] gives every candidate a number in `0..=1` from three facts the caller already has —
//! inside the frustum, how much of it survives occlusion (a cheeky depth-histogram test the
//! renderer's occlusion data already supports), and how close it is to the crosshair — times a
//! weight for its kind. [`Ranker`] then hands candidates out in descending score until the budget
//! for that kind is spent: the player always gets the crosshair first, then the near field, then
//! the rest, and *nothing invisible is ever ranked*.
//!
//! This is not a replacement for the GPU's own culling: it runs before the submit, so a culled
//! section never costs a draw call at all. Pure arithmetic, tested without a GPU.

#![forbid(unsafe_code)]

/// A camera for the frame, as far as scoring is concerned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// The forward direction, normalised.
    pub fx: f64,
    pub fy: f64,
    pub fz: f64,
    /// Half the diagonal field of view, radians.
    pub half_fov: f64,
    /// How far the camera can see, blocks.
    pub range: f64,
    /// The player's vertical field, for the near-field bonus below the camera.
    pub eye_height: f64,
}

impl Camera {
    /// A camera looking along -Z (Minecraft's yaw 0), 70° vertical FOV, 12 chunks.
    pub fn yaw_pitch(x: f64, y: f64, z: f64, yaw: f64, pitch: f64, range: f64) -> Self {
        // Minecraft's yaw: 0 = -Z, 90 = -X (turning right), matching `Mth.cos(yaw * π/180)`.
        let (sy, cy) = yaw.to_radians().sin_cos();
        let (sp, cp) = pitch.to_radians().sin_cos();
        Self {
            x,
            y,
            z,
            fx: -sy * cp,
            fy: -sp,
            fz: -cy * cp,
            half_fov: 35.0_f64.to_radians(),
            range,
            eye_height: 1.62,
        }
    }
}

/// What a candidate is, which sets how much a given visibility is worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sort {
    /// A block the player is looking at or breaking: always first.
    Targeted,
    /// An entity within reach or in the crosshair.
    EntityNear,
    /// A chunk section the crosshair is inside or pointing at.
    SectionTargeted,
    /// Terrain in the near field.
    SectionNear,
    /// Terrain further out.
    SectionFar,
    /// Block entity, item frame, sign: small, cheap, but not the world.
    BlockEntity,
    /// Particles.
    Particle,
    /// A section behind the camera or fully occluded: never submitted.
    Hidden,
}

/// What the caller knows about one candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub sort: Sort,
    /// The candidate's centre, blocks.
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// The share of the candidate the depth histogram says is visible, 0..=1. `1.0` when the
    /// renderer has no occlusion data for it.
    pub unoccluded: f32,
    /// Whether the frustum test passed.
    pub in_frustum: bool,
    /// The frame cost of submitting it, milliseconds (a draw plus its uniforms).
    pub cost_ms: f32,
    /// The caller's own identity for it: a section index, an entity id, a particle slot.
    pub id: u64,
}

/// A scored candidate: what the ranker hands back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ranked {
    pub id: u64,
    pub score: f32,
    pub sort: Sort,
    pub cost_ms: f32,
}

/// How much each kind is worth before distance and occlusion scale it.
fn weight(sort: Sort) -> f32 {
    match sort {
        Sort::Targeted => 1.0,
        Sort::EntityNear => 0.95,
        Sort::SectionTargeted => 0.9,
        Sort::SectionNear => 0.75,
        Sort::SectionFar => 0.5,
        Sort::BlockEntity => 0.4,
        Sort::Particle => 0.25,
        Sort::Hidden => 0.0,
    }
}

/// Score a candidate against a camera. Zero means "do not submit".
///
/// The score combines: the kind's weight, how central the candidate is in the view (a cosine
/// falloff over the FOV), how much of it survives occlusion, and a near-field bonus for terrain
/// under and immediately around the player — a section at the camera's feet is always worth more
/// than one at the horizon at the same angle, because the player notices its absence.
pub fn score(camera: &Camera, candidate: &Candidate) -> f32 {
    if candidate.sort == Sort::Hidden || !candidate.in_frustum {
        return 0.0;
    }
    if candidate.unoccluded <= 0.0 {
        return 0.0;
    }
    let (dx, dy, dz) = (
        candidate.x - camera.x,
        candidate.y - camera.y,
        candidate.z - camera.z,
    );
    let dist = (dx * dx + dy * dy + dz * dz).sqrt();
    if dist > camera.range {
        return 0.0;
    }
    // Centrality: 1 on the crosshair, 0 at the edge of the FOV. A candidate behind the camera has
    // a negative dot and is hidden, whatever the frustum test said.
    let dot = if dist > 1e-6 {
        (dx * camera.fx + dy * camera.fy + dz * camera.fz) / dist
    } else {
        1.0
    };
    if dot <= 0.0 {
        return 0.0;
    }
    let angle = dot.clamp(-1.0, 1.0).acos();
    let central = (1.0 - (angle / (camera.half_fov * 1.6)).clamp(0.0, 1.0)) as f32;
    // Distance falloff: linear in range, so a section at half the range is worth half again as
    // much as one at the edge.
    let near = 1.0 - (dist / camera.range).clamp(0.0, 1.0) as f32;
    let occlusion = candidate.unoccluded.clamp(0.0, 1.0);
    // Under the player's feet: within two blocks of eye height, any distance.
    let underfoot = if dist > 0.5
        && candidate.y <= camera.y
        && candidate.y >= camera.y - camera.eye_height - 2.0
    {
        0.15
    } else {
        0.0
    };
    let base = weight(candidate.sort);
    (base * (0.35 + 0.4 * central + 0.25 * near) * (0.35 + 0.65 * occlusion) + underfoot)
        .clamp(0.0, 1.0)
}

/// The frame's budgets per kind, in milliseconds, and the ranking.
#[derive(Clone, Debug, Default)]
pub struct Ranker {
    /// The frame's total budget for submissions, ms.
    pub budget_ms: f32,
    /// The smallest score worth submitting at all.
    pub floor: f32,
    /// How many candidates were hidden by scoring last frame, for the overlay.
    pub hidden: u32,
    /// How many were dropped for budget, for the overlay.
    pub dropped: u32,
}

impl Ranker {
    pub fn new(budget_ms: f32) -> Self {
        Self {
            budget_ms,
            floor: 0.04,
            hidden: 0,
            dropped: 0,
        }
    }

    /// Rank `candidates` against `camera` and keep those that fit `budget_ms`. The result is in
    /// submit order: highest score first. `always` candidates (the block being broken, the entity
    /// being attacked) are prepended regardless of budget.
    pub fn rank(&mut self, camera: &Camera, candidates: &[Candidate]) -> Vec<Ranked> {
        self.hidden = 0;
        self.dropped = 0;
        let mut scored: Vec<Ranked> = Vec::with_capacity(candidates.len());
        for c in candidates {
            let s = score(camera, c);
            if s <= 0.0 {
                self.hidden += 1;
                continue;
            }
            if s < self.floor {
                self.hidden += 1;
                continue;
            }
            scored.push(Ranked {
                id: c.id,
                score: s,
                sort: c.sort,
                cost_ms: c.cost_ms.max(0.0),
            });
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.sort.cmp(&b.sort))
                .then(a.id.cmp(&b.id))
        });
        let mut out = Vec::with_capacity(scored.len());
        let mut spent = 0.0f32;
        for entry in scored {
            // The targeted things are free: the player is interacting with them, so what they cost
            // is spent whether or not the budget has room, and it never comes out of the sections'
            // share either.
            if entry.sort == Sort::Targeted {
                out.push(entry);
            } else if spent + entry.cost_ms <= self.budget_ms {
                spent += entry.cost_ms;
                out.push(entry);
            } else {
                self.dropped += 1;
            }
        }
        out
    }

    /// A short census for the performance overlay: `(submitted, hidden, dropped, ms)`.
    pub fn census(&self, submitted: usize, ms: f32) -> (usize, u32, u32, f32) {
        (submitted, self.hidden, self.dropped, ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> Camera {
        // At the origin looking along -Z, 12-chunk range.
        Camera::yaw_pitch(0.0, 64.0, 0.0, 0.0, 0.0, 192.0)
    }

    fn at(sort: Sort, x: f64, y: f64, z: f64) -> Candidate {
        Candidate {
            sort,
            x,
            y,
            z,
            unoccluded: 1.0,
            in_frustum: true,
            cost_ms: 0.02,
            id: 0,
        }
    }

    #[test]
    fn what_is_behind_the_camera_scores_nothing() {
        let c = camera();
        assert_eq!(score(&c, &at(Sort::SectionNear, 0.0, 64.0, 50.0)), 0.0);
        assert!(score(&c, &at(Sort::SectionNear, 0.0, 64.0, -50.0)) > 0.0);
    }

    #[test]
    fn what_is_occluded_scores_nothing() {
        let c = camera();
        let mut cand = at(Sort::SectionNear, 0.0, 64.0, -30.0);
        cand.unoccluded = 0.0;
        assert_eq!(score(&c, &cand), 0.0);
        cand.unoccluded = 0.5;
        assert!(score(&c, &cand) > 0.0);
    }

    #[test]
    fn the_crosshair_beats_the_edge_and_the_near_field_beats_the_horizon() {
        let c = camera();
        let centre = at(Sort::SectionNear, 0.0, 64.0, -40.0);
        let edge = at(Sort::SectionNear, 90.0, 64.0, -40.0);
        let horizon = at(Sort::SectionNear, 0.0, 64.0, -190.0);
        assert!(score(&c, &centre) > score(&c, &edge));
        assert!(score(&c, &centre) > score(&c, &horizon));
    }

    #[test]
    fn ranking_puts_the_targeted_thing_first_even_when_the_budget_is_spent() {
        let mut ranker = Ranker::new(0.01);
        let mut targeted = at(Sort::Targeted, 0.0, 64.0, -2.0);
        targeted.id = 7;
        let mut a = at(Sort::SectionFar, 0.0, 64.0, -100.0);
        a.id = 1;
        let mut b = at(Sort::SectionFar, 0.0, 64.0, -120.0);
        b.id = 2;
        let ranked = ranker.rank(&camera(), &[a, b, targeted]);
        assert_eq!(ranked[0].id, 7);
        // 10 µs of budget and sections costing 20 µs each: the targeted one is free and comes
        // first, and neither section fits, so both are dropped rather than the frame growing.
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranker.dropped, 2);
        assert_eq!(ranker.hidden, 0);
    }

    #[test]
    fn everything_hidden_is_counted_and_nothing_is_submitted() {
        let mut ranker = Ranker::new(100.0);
        let behind = at(Sort::SectionNear, 0.0, 64.0, 80.0);
        let ranked = ranker.rank(&camera(), &[behind]);
        assert!(ranked.is_empty());
        assert_eq!(ranker.hidden, 1);
        assert_eq!(ranker.dropped, 0);
    }

    #[test]
    fn a_section_under_the_players_feet_is_worth_more_than_one_at_the_edge_of_the_range() {
        let c = camera();
        let feet = at(Sort::SectionNear, 0.0, 63.0, -3.0);
        let far = at(Sort::SectionNear, 60.0, 100.0, -150.0);
        assert!(score(&c, &feet) > score(&c, &far));
    }
}
