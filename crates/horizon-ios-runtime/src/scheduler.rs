//! Priority scheduler: what must run now, what may wait, what must be dropped.
//!
//! The tree already has a good shared job pool (`horizon-threads/src/jobs.rs`,
//! `Rayon`-style `Class`/`Rank`) and `GameAllocator`. What it lacks on iOS is a硬性
//! frame budget: chunk generation, lighting, meshing, network, telemetry, cosmetics and
//! far terrain all compete, and a slow tick makes input + render late. Apple's Tech Talk
//! 110147: scale thread count to the core count, merge tiny jobs, avoid `sleep`, use QoS
//! so the scheduler can put background work on E cores.
//!
//! This scheduler is the *decision layer* (deterministic, testable, zero-dependency).
//! The engine plugs its existing worker pool into `FrameOutcome::executed`.

use crate::apple::{Chip, ThermalState, WorkerBudget, qos_for};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Background = 0,
    Low = 1,
    Normal = 2,
    High = 3,
    Critical = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkKind {
    Input,
    Render,
    GameTick,
    VisibleMesh,
    Lighting,
    ChunkGenerate,
    Network,
    Io,
    FarTerrain,
    Cosmetics,
    Telemetry,
    ModContentReload,
}
impl WorkKind {
    pub fn priority(self) -> Priority {
        match self {
            Self::Input | Self::Render | Self::GameTick => Priority::Critical,
            Self::VisibleMesh | Self::Network | Self::ModContentReload => Priority::High,
            Self::Lighting | Self::ChunkGenerate | Self::Io => Priority::Normal,
            Self::FarTerrain | Self::Cosmetics => Priority::Low,
            Self::Telemetry => Priority::Background,
        }
    }
    /// Dropping this job cannot change what the player sees this frame.
    pub fn droppable(self) -> bool {
        matches!(self, Self::FarTerrain | Self::Cosmetics | Self::Telemetry)
    }
    /// Deferring this job keeps correctness but not this frame's visuals.
    pub fn deferrable(self) -> bool {
        matches!(
            self,
            Self::Lighting | Self::ChunkGenerate | Self::Io | Self::FarTerrain
        )
    }
    /// Tie-break inside one QoS band. Input must be sampled before the renderer
    /// reads it; a network packet or a mod reload must not jump ahead of the tick.
    pub fn urgency(self) -> u8 {
        match self {
            Self::Input => 5,
            Self::Render | Self::GameTick => 4,
            Self::VisibleMesh | Self::Network | Self::ModContentReload => 3,
            Self::Lighting | Self::ChunkGenerate | Self::Io => 2,
            Self::FarTerrain | Self::Cosmetics => 1,
            Self::Telemetry => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: u64,
    pub kind: WorkKind,
    /// Estimated CPU microseconds.
    pub cost_us: u32,
}
impl Job {
    pub fn new(id: u64, kind: WorkKind, cost_us: u32) -> Self {
        Self { id, kind, cost_us }
    }
    pub fn priority(&self) -> Priority {
        self.kind.priority()
    }
    pub fn qos(&self) -> crate::apple::Qos {
        qos_for(self.priority())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub budget: WorkerBudget,
    /// CPU microseconds allowed for one frame's worker jobs.
    pub frame_budget_us: u32,
    /// Hard cap on queued jobs, so a slow tick cannot grow an unbounded backlog.
    pub max_queue: usize,
}
impl SchedulerConfig {
    pub fn for_chip(chip: &Chip, thermal: ThermalState, frame_ms: f32) -> Self {
        let budget = WorkerBudget::for_chip(chip, thermal);
        Self {
            budget,
            frame_budget_us: (frame_ms * 1000.0 * 0.7) as u32,
            max_queue: budget.total * 32,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitResult {
    Queued,
    /// Non-essential work was refused at enqueue time (backpressure).
    Dropped,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    pub executed: Vec<Job>,
    pub deferred: usize,
    pub dropped: usize,
    pub used_us: u32,
    pub workers_used: usize,
}

#[derive(Clone, Debug)]
pub struct Scheduler {
    config: SchedulerConfig,
    queues: BTreeMap<Priority, VecDeque<Job>>,
    next_id: u64,
    pub dropped_total: u64,
    pub deferred_total: u64,
}
impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            queues: BTreeMap::new(),
            next_id: 0,
            dropped_total: 0,
            deferred_total: 0,
        }
    }
    pub fn with_chip(chip: &Chip, thermal: ThermalState, frame_ms: f32) -> Self {
        Self::new(SchedulerConfig::for_chip(chip, thermal, frame_ms))
    }
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }
    pub fn set_thermal(&mut self, chip: &Chip, thermal: ThermalState, frame_ms: f32) {
        let frame_budget_us = self.config.frame_budget_us;
        let mut c = SchedulerConfig::for_chip(chip, thermal, frame_ms);
        c.frame_budget_us = ((frame_budget_us as f32) * thermal.scale()) as u32;
        self.config = c;
    }

    /// Bounded submit: never grow an unbounded backlog behind a slow frame.
    pub fn submit(&mut self, kind: WorkKind, cost_us: u32) -> SubmitResult {
        let queued: usize = self.queues.values().map(VecDeque::len).sum();
        if queued >= self.config.max_queue {
            if kind.droppable() {
                self.dropped_total += 1;
                return SubmitResult::Dropped;
            }
            // Make room by dropping the oldest droppable job first.
            let mut made_room = false;
            for p in [Priority::Background, Priority::Low] {
                if let Some(q) = self.queues.get_mut(&p)
                    && q.pop_front().is_some()
                {
                    self.dropped_total += 1;
                    made_room = true;
                    break;
                }
            }
            // An essential job may also preempt a deferrable chunk/lighting/IO job:
            // those are regenerable, while input/render/mod-reload are not.
            if !made_room
                && kind.priority() >= Priority::High
                && let Some(q) = self.queues.get_mut(&Priority::Normal)
                && let Some(pos) = q.iter().position(|j| j.kind.deferrable())
            {
                q.remove(pos);
                self.dropped_total += 1;
                made_room = true;
            }
            if !made_room {
                self.dropped_total += 1;
                return SubmitResult::Dropped;
            }
        }
        self.next_id += 1;
        let job = Job::new(self.next_id, kind, cost_us);
        let q = self.queues.entry(job.priority()).or_default();
        let urgency = job.kind.urgency();
        let pos = q
            .iter()
            .position(|queued| queued.kind.urgency() < urgency)
            .unwrap_or(q.len());
        q.insert(pos, job);
        SubmitResult::Queued
    }

    /// Run one frame's worth of work. Critical jobs always run first; non-essential
    /// jobs are dropped when the frame budget is exhausted, and deferrable jobs stay
    /// queued for the next frame instead of overloading this one.
    pub fn run_frame(&mut self) -> FrameOutcome {
        let mut out = FrameOutcome::default();
        let budget_us = self.config.frame_budget_us;
        let max_workers = self.config.budget.total;
        for p in [
            Priority::Critical,
            Priority::High,
            Priority::Normal,
            Priority::Low,
            Priority::Background,
        ] {
            let Some(q) = self.queues.get_mut(&p) else {
                continue;
            };
            while let Some(job) = q.pop_front() {
                if out.used_us + job.cost_us > budget_us {
                    if job.kind.droppable() {
                        out.dropped += 1;
                        self.dropped_total += 1;
                        continue;
                    }
                    q.push_front(job);
                    out.deferred += q.len();
                    self.deferred_total += q.len() as u64;
                    break;
                }
                if out.workers_used >= max_workers {
                    if job.kind.droppable() {
                        out.dropped += 1;
                        self.dropped_total += 1;
                        continue;
                    }
                    q.push_front(job);
                    out.deferred += q.len();
                    self.deferred_total += q.len() as u64;
                    return out;
                }
                out.used_us += job.cost_us;
                out.workers_used += 1;
                out.executed.push(job);
            }
        }
        out
    }

    pub fn pending(&self) -> usize {
        self.queues.values().map(VecDeque::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apple::chips;

    fn sched() -> Scheduler {
        SchedulerConfig {
            budget: WorkerBudget::for_chip(&chips::m1(), ThermalState::Nominal),
            frame_budget_us: 11_000,
            max_queue: 32,
        }
        .into()
    }
    impl From<SchedulerConfig> for Scheduler {
        fn from(c: SchedulerConfig) -> Self {
            Scheduler::new(c)
        }
    }

    #[test]
    fn critical_input_and_render_always_run_first() {
        let mut s = sched();
        s.submit(WorkKind::Telemetry, 100);
        s.submit(WorkKind::ChunkGenerate, 100);
        s.submit(WorkKind::Render, 100);
        s.submit(WorkKind::Input, 100);
        let o = s.run_frame();
        let kinds: Vec<_> = o.executed.iter().map(|j| j.kind).collect();
        assert_eq!(kinds[0], WorkKind::Input);
        assert_eq!(kinds[1], WorkKind::Render);
        assert_eq!(o.dropped, 0);
    }

    #[test]
    fn non_essential_work_is_dropped_under_a_frame_budget_overload() {
        let mut s = Scheduler::new(SchedulerConfig {
            budget: WorkerBudget::for_chip(&chips::a14_bionic(), ThermalState::Nominal),
            frame_budget_us: 1000,
            max_queue: 64,
        });
        s.submit(WorkKind::Input, 200);
        s.submit(WorkKind::Render, 700);
        for _ in 0..10 {
            s.submit(WorkKind::FarTerrain, 500);
            s.submit(WorkKind::Cosmetics, 500);
            s.submit(WorkKind::Telemetry, 500);
        }
        let o = s.run_frame();
        assert_eq!(o.used_us, 900);
        assert!(
            o.dropped > 0,
            "non-essential work must be dropped, not queued forever"
        );
        assert!(o.executed.iter().any(|j| j.kind == WorkKind::Input));
        assert!(o.executed.iter().any(|j| j.kind == WorkKind::Render));
        assert!(o.executed.iter().all(|j| !j.kind.droppable()));
    }

    #[test]
    fn essential_work_is_deferred_not_dropped_when_the_budget_is_exhausted() {
        let mut s = Scheduler::new(SchedulerConfig {
            budget: WorkerBudget::for_chip(&chips::m1(), ThermalState::Nominal),
            frame_budget_us: 500,
            max_queue: 64,
        });
        s.submit(WorkKind::GameTick, 400);
        s.submit(WorkKind::ChunkGenerate, 400);
        let o = s.run_frame();
        assert_eq!(o.executed.len(), 1);
        assert!(o.deferred > 0);
        assert_eq!(s.pending(), 1);
        // A later frame with more budget finishes it.
        s.config.frame_budget_us = 1000;
        let o2 = s.run_frame();
        assert_eq!(o2.executed.len(), 1);
        assert_eq!(o2.executed[0].kind, WorkKind::ChunkGenerate);
    }

    #[test]
    fn queues_are_bounded_and_backpressure_drops_cosmetics() {
        let mut s = Scheduler::new(SchedulerConfig {
            budget: WorkerBudget::for_chip(&chips::a14_bionic(), ThermalState::Nominal),
            frame_budget_us: 11_000,
            max_queue: 4,
        });
        for _ in 0..4 {
            assert_eq!(s.submit(WorkKind::ChunkGenerate, 10), SubmitResult::Queued);
        }
        assert_eq!(s.submit(WorkKind::Telemetry, 10), SubmitResult::Dropped);
        assert_eq!(
            s.submit(WorkKind::Input, 10),
            SubmitResult::Queued,
            "essential work still gets in"
        );
        assert!(s.pending() <= 5);
    }

    #[test]
    fn qos_maps_critical_to_p_cores_and_background_to_e_cores() {
        use crate::apple::Qos;
        let j = Job::new(1, WorkKind::Input, 10);
        assert_eq!(j.qos(), Qos::UserInteractive);
        assert!(!j.qos().confined_to_efficiency_cores());
        let bg = Job::new(2, WorkKind::ChunkGenerate, 10);
        assert_eq!(bg.qos(), Qos::Default);
        let tel = Job::new(3, WorkKind::Telemetry, 10);
        assert_eq!(tel.qos(), Qos::Background);
        assert!(tel.qos().confined_to_efficiency_cores());
    }

    #[test]
    fn thermal_throttling_reduces_the_frame_budget_not_the_critical_jobs() {
        let mut s = Scheduler::with_chip(&chips::m1(), ThermalState::Nominal, 16.67);
        let cold = s.config().frame_budget_us;
        s.set_thermal(&chips::m1(), ThermalState::Critical, 16.67);
        assert!(s.config().frame_budget_us < cold);
        s.submit(WorkKind::Input, 10);
        assert_eq!(s.run_frame().executed[0].kind, WorkKind::Input);
    }

    #[test]
    fn total_workers_never_exceed_the_core_count() {
        let mut s = Scheduler::with_chip(&chips::a14_bionic(), ThermalState::Nominal, 16.67);
        for _ in 0..64 {
            s.submit(WorkKind::ChunkGenerate, 1);
        }
        let o = s.run_frame();
        assert!(
            o.workers_used <= s.config().budget.total,
            "{} workers",
            o.workers_used
        );
    }
}
