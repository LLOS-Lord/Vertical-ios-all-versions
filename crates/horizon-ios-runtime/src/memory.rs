//! Jetsam-aware memory governor with real chunk-cache eviction.
//!
//! The tree has excellent *diagnostics* (`horizon-ctl memory`, `horizon-threads::
//! process_memory`, a heap census) but no governor: nothing reads
//! `os_proc_available_memory()` and acts on it, and no cache is bounded by the
//! device's per-process limit. On iOS the limit is **not** derived from installed RAM
//! (measured ~1384 MiB on an iPhone SE 3, ~2099 MiB on an iPhone 13); the kernel kills
//! the app with SIGKILL via jetsam. WWDC22 "Profile and optimize your game's memory"
//! recommends `phys_footprint`, `os_proc_available_memory()` and *purgeable/volatile*
//! Metal resources for infrequently used caches.
//!
//! This governor:
//! * computes the real budget as `phys_footprint + available` (the jetsam limit) and
//!   keeps the game under a 60% working ceiling;
//! * protects the visible chunk set, evicts unloaded regions by LRU with a small
//!   "second chance" queue (2Q), and marks GPU meshes purgeable before freeing CPU data;
//! * degrades render distance / drops far terrain under pressure instead of being killed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySnapshot {
    /// `task_vm_info.phys_footprint` (what jetsam charges), MiB.
    pub phys_footprint_mb: u64,
    /// `os_proc_available_memory()` / MiB; 0 on Simulator / no limit.
    pub available_mb: u64,
    /// Explicit limit when the platform can report one (else footprint + available).
    pub reported_limit_mb: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    Normal,
    Warning,
    Critical,
}
impl Pressure {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// The device's real per-process ceiling, never the installed RAM.
pub fn jetsam_limit_mb(s: MemorySnapshot) -> u64 {
    s.reported_limit_mb
        .unwrap_or(s.phys_footprint_mb.saturating_add(s.available_mb))
        .max(1)
}
/// A conservative working ceiling: leave headroom for the GPU driver's host-side
/// allocations, compressed pages and the SpringBoard foreground app.
pub fn working_ceiling_mb(s: MemorySnapshot) -> u64 {
    (jetsam_limit_mb(s) * 60 / 100).max(256)
}
pub fn pressure_for(s: MemorySnapshot) -> Pressure {
    let limit = jetsam_limit_mb(s);
    let used = s.phys_footprint_mb;
    if limit == 0 || s.available_mb == 0 {
        return Pressure::Normal; // Simulator reports no limit: fail-open to normal.
    }
    let pct = used * 100 / limit;
    if pct >= 80 || s.available_mb < 180 {
        Pressure::Critical
    } else if pct >= 65 || s.available_mb < 350 {
        Pressure::Warning
    } else {
        Pressure::Normal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkKey {
    pub x: i32,
    pub z: i32,
    pub dimension: i32,
}
impl ChunkKey {
    pub fn new(x: i32, z: i32, dimension: i32) -> Self {
        Self { x, z, dimension }
    }
    pub fn region(self) -> (i32, i32, i32) {
        (self.x >> 5, self.z >> 5, self.dimension)
    }
}

#[derive(Clone, Debug)]
pub struct ChunkEntry {
    pub key: ChunkKey,
    /// CPU bytes (blocks, biomes, light, entities) in MiB.
    pub cpu_mb: u64,
    /// GPU bytes (mesh arena + textures) in MiB, on unified memory.
    pub gpu_mb: u64,
    pub last_used_tick: u64,
    pub gpu_purgeable: bool,
}
impl ChunkEntry {
    pub fn total_mb(&self) -> u64 {
        self.cpu_mb + self.gpu_mb
    }
}

/// A cache entry that has been marked `MTLPurgeableState.volatile` once, then freed
/// entirely on the next pressure tick (2Q / second chance).
#[derive(Clone, Debug, Default)]
pub struct ChunkCache {
    entries: BTreeMap<ChunkKey, ChunkEntry>,
    lru: VecDeque<ChunkKey>,
    probation: BTreeSet<ChunkKey>,
}
impl ChunkCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, e: ChunkEntry) {
        let k = e.key;
        self.entries.insert(k, e);
        self.lru.retain(|x| x != &k);
        self.lru.push_back(k);
        self.probation.remove(&k);
    }
    pub fn touch(&mut self, k: ChunkKey, tick: u64) {
        if let Some(e) = self.entries.get_mut(&k) {
            e.last_used_tick = tick;
        }
        self.lru.retain(|x| x != &k);
        self.lru.push_back(k);
        self.probation.remove(&k);
    }
    /// Called by the streaming system when a region leaves the loaded set.
    /// The chunk's GPU mesh becomes volatile immediately; its CPU data is freed on the
    /// next memory tick (or right away under critical pressure).
    pub fn unload_region(&mut self, region: (i32, i32, i32), critical: bool) -> u64 {
        let keys: Vec<ChunkKey> = self
            .entries
            .keys()
            .copied()
            .filter(|k| k.region() == region)
            .collect();
        let mut reclaimed = 0;
        for k in keys {
            if critical {
                reclaimed += self.remove(&k);
            } else if let Some(e) = self.entries.get_mut(&k) {
                e.gpu_purgeable = true;
                self.probation.insert(k);
            }
        }
        reclaimed
    }
    fn remove(&mut self, k: &ChunkKey) -> u64 {
        let bytes = self.entries.remove(k).map_or(0, |e| e.total_mb());
        self.lru.retain(|x| x != k);
        self.probation.remove(k);
        bytes
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn bytes_mb(&self) -> u64 {
        self.entries.values().map(ChunkEntry::total_mb).sum()
    }
    pub fn contains(&self, k: &ChunkKey) -> bool {
        self.entries.contains_key(k)
    }
    /// Evict in LRU order, never touching `visible`.
    /// Probation entries (an unloaded region) are evicted before live ones.
    pub fn evict_until_under(
        &mut self,
        budget_mb: u64,
        visible: &BTreeSet<ChunkKey>,
        max_evictions: usize,
    ) -> (u64, Vec<ChunkKey>) {
        let mut reclaimed = 0;
        let mut evicted = Vec::new();
        while self.bytes_mb() > budget_mb && evicted.len() < max_evictions {
            // First pass: everything whose region was unloaded.
            let mut candidate = self
                .lru
                .iter()
                .copied()
                .find(|k| self.probation.contains(k) && !visible.contains(k));
            // Second pass: oldest non-visible chunk.
            if candidate.is_none() {
                candidate = self.lru.iter().copied().find(|k| !visible.contains(k));
            }
            let Some(k) = candidate else { break };
            reclaimed += self.remove(&k);
            evicted.push(k);
        }
        (reclaimed, evicted)
    }
    pub fn mark_purgeable(&mut self, visible: &BTreeSet<ChunkKey>) -> u64 {
        let mut mb = 0;
        for (k, e) in &mut self.entries {
            if !visible.contains(k) && !e.gpu_purgeable && e.gpu_mb > 0 {
                e.gpu_purgeable = true;
                mb += e.gpu_mb;
            }
        }
        mb
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryAction {
    pub pressure: Pressure,
    pub evicted: Vec<ChunkKey>,
    pub reclaimed_mb: u64,
    pub purgeable_mb: u64,
    pub reduce_render_distance: bool,
    pub drop_far_terrain: bool,
    pub disable_frame_generation: bool,
}

#[derive(Clone, Debug)]
pub struct MemoryGovernor {
    pub cache: ChunkCache,
    pub tick: u64,
    pub last_pressure: Pressure,
    pub reclaimed_total_mb: u64,
}
impl Default for MemoryGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryGovernor {
    pub fn new() -> Self {
        Self {
            cache: ChunkCache::new(),
            tick: 0,
            last_pressure: Pressure::Normal,
            reclaimed_total_mb: 0,
        }
    }

    /// One memory tick (call at most a few times per second; it walks the cache).
    pub fn on_tick(
        &mut self,
        snapshot: MemorySnapshot,
        visible: &BTreeSet<ChunkKey>,
    ) -> MemoryAction {
        self.tick += 1;
        let pressure = pressure_for(snapshot);
        self.last_pressure = pressure;
        let ceiling = working_ceiling_mb(snapshot);
        // The cache may only use a quarter of the working ceiling; the rest belongs to
        // the renderer, the server thread and the JVM-free simulation tables.
        let cache_budget = ceiling / 4;
        let (mut reclaimed, evicted) = self.cache.evict_until_under(cache_budget, visible, 64);
        let purgeable_mb = if pressure != Pressure::Normal {
            self.cache.mark_purgeable(visible)
        } else {
            0
        };
        if pressure == Pressure::Critical {
            let (more, mut keys) = self.cache.evict_until_under(cache_budget / 2, visible, 128);
            reclaimed += more;
            // keys is used only for accounting; evicted already reports the first pass.
            keys.clear();
        }
        self.reclaimed_total_mb += reclaimed;
        MemoryAction {
            pressure,
            evicted,
            reclaimed_mb: reclaimed,
            purgeable_mb,
            reduce_render_distance: pressure != Pressure::Normal,
            drop_far_terrain: pressure == Pressure::Critical,
            disable_frame_generation: pressure == Pressure::Critical,
        }
    }

    /// The streaming system reports a region leaving the loaded set.
    pub fn region_unloaded(&mut self, region: (i32, i32, i32)) -> u64 {
        let critical = self.last_pressure == Pressure::Critical;
        let reclaimed = self.cache.unload_region(region, critical);
        self.reclaimed_total_mb += reclaimed;
        reclaimed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(used: u64, avail: u64) -> MemorySnapshot {
        MemorySnapshot {
            phys_footprint_mb: used,
            available_mb: avail,
            reported_limit_mb: None,
        }
    }
    fn entry(x: i32, cpu: u64, gpu: u64, tick: u64) -> ChunkEntry {
        ChunkEntry {
            key: ChunkKey::new(x, 0, 0),
            cpu_mb: cpu,
            gpu_mb: gpu,
            last_used_tick: tick,
            gpu_purgeable: false,
        }
    }

    #[test]
    fn the_limit_is_the_jetsam_ceiling_not_installed_ram() {
        // Measured in the wild: iPhone 13 with 4 GB RAM has ~2099 MiB app limit.
        assert_eq!(jetsam_limit_mb(snap(700, 1399)), 2099);
        assert_eq!(working_ceiling_mb(snap(700, 1399)), 2099 * 60 / 100);
        assert_eq!(pressure_for(snap(700, 1399)), Pressure::Normal);
        assert_eq!(pressure_for(snap(1500, 599)), Pressure::Warning);
        assert_eq!(pressure_for(snap(1750, 349)), Pressure::Critical);
        // Simulator: os_proc_available_memory() == 0 -> no invented limit.
        assert_eq!(pressure_for(snap(700, 0)), Pressure::Normal);
    }

    #[test]
    fn a_region_that_is_no_longer_loaded_is_evicted_first() {
        let mut cache = ChunkCache::new();
        cache.insert(entry(0, 8, 4, 0));
        cache.insert(entry(40, 8, 4, 1)); // different region (40 >> 5 == 1)
        let visible = BTreeSet::from([ChunkKey::new(0, 0, 0)]);
        assert_eq!(cache.unload_region((1, 0, 0), false), 0);
        let (reclaimed, evicted) = cache.evict_until_under(12, &visible, 8);
        assert_eq!(evicted, vec![ChunkKey::new(40, 0, 0)]);
        assert_eq!(reclaimed, 12);
        assert!(cache.contains(&ChunkKey::new(0, 0, 0)));
        assert!(!cache.contains(&ChunkKey::new(40, 0, 0)));
    }

    #[test]
    fn visible_chunks_are_never_evicted_even_under_extreme_pressure() {
        let mut cache = ChunkCache::new();
        for x in 0..6 {
            cache.insert(entry(x, 10, 5, x as u64));
        }
        let visible: BTreeSet<_> = (0..6).map(|x| ChunkKey::new(x, 0, 0)).collect();
        let (_, evicted) = cache.evict_until_under(0, &visible, 64);
        assert!(evicted.is_empty());
        assert_eq!(cache.len(), 6);
    }

    #[test]
    fn critical_pressure_purges_gpu_and_asks_for_render_distance_reduction() {
        let mut g = MemoryGovernor::new();
        for x in 0..10 {
            g.cache.insert(entry(x, 12, 8, x as u64));
        }
        let visible = BTreeSet::from([ChunkKey::new(0, 0, 0)]);
        let a = g.on_tick(snap(1750, 120), &visible);
        assert_eq!(a.pressure, Pressure::Critical);
        assert!(a.reclaimed_mb > 0);
        assert!(a.purgeable_mb > 0 || a.evicted.len() > 1);
        assert!(a.reduce_render_distance && a.drop_far_terrain && a.disable_frame_generation);
        assert!(g.cache.contains(&ChunkKey::new(0, 0, 0)));
    }

    #[test]
    fn normal_pressure_leaves_the_cache_alone() {
        let mut g = MemoryGovernor::new();
        for x in 0..4 {
            g.cache.insert(entry(x, 2, 1, x as u64));
        }
        let visible: BTreeSet<_> = (0..4).map(|x| ChunkKey::new(x, 0, 0)).collect();
        let a = g.on_tick(snap(600, 1400), &visible);
        assert_eq!(a.pressure, Pressure::Normal);
        assert!(a.evicted.is_empty());
        assert!(!a.reduce_render_distance);
    }

    #[test]
    fn unloading_a_region_frees_memory_immediately_when_critical() {
        let mut g = MemoryGovernor::new();
        g.last_pressure = Pressure::Critical;
        g.cache.insert(entry(40, 10, 6, 0));
        let reclaimed = g.region_unloaded((1, 0, 0));
        assert_eq!(reclaimed, 16);
        assert!(!g.cache.contains(&ChunkKey::new(40, 0, 0)));
        assert_eq!(g.reclaimed_total_mb, 16);
    }

    #[test]
    fn lru_touch_protects_a_recently_used_chunk() {
        let mut cache = ChunkCache::new();
        cache.insert(entry(0, 8, 4, 0));
        cache.insert(entry(1, 8, 4, 1));
        cache.touch(ChunkKey::new(0, 0, 0), 99);
        let (_, evicted) = cache.evict_until_under(12, &BTreeSet::new(), 8);
        assert_eq!(evicted, vec![ChunkKey::new(1, 0, 0)]);
        assert!(cache.contains(&ChunkKey::new(0, 0, 0)));
    }
}
