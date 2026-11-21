# Ghi chú tích hợp (chính xác từng chỗ cần sửa)

Tài liệu này là bản hướng dẫn thay thế từng dòng, để lần tích hợp trên máy macOS không phải đọc lại
toàn bộ mã nguồn. Mọi đường dẫn tính từ gốc repo.

---

## A. Frame Generation — `crates/horizon-client-render/src/gfx/frame_gen.rs`

### A1. Chặn đứng vô hạn (nguyên nhân số 1 của “game đứng”)

Hiện tại (`FrameGen::frames`, khoảng dòng 226–242):

```rust
let pair = if presenter.failed() { None } else {
    presenter.free.recv().ok().zip(presenter.free.recv().ok())
};
```

Thay bằng một `recv` có timeout do `horizon_frame_sched::Admission` quyết định:

```rust
// crates/horizon-client-render/Cargo.toml: horizon-frame-sched = { path = "../horizon-frame-sched" }
use std::time::Duration;

let timeout = Duration::from_secs_f32(self.admission_timeout_ms / 1000.0);
let first = presenter.free.recv_timeout(timeout).ok();
let second = first.and_then(|a| presenter.free.recv_timeout(timeout).ok().map(|b| (a, b)));
if second.is_none() {
    // The present thread is late: stop generating this frame, keep presenting normally.
    self.stop();
    self.paused = true;
    self.stalled_at = Some(Instant::now());
}
let pair = second;
```

`self.stalled_at` là trường mới; `FrameGen::present()` đọc nó để báo cho `Admission` biết
`present_wait_ms` và để **không** sinh frame trong cool-down (`Admission::observe` đã làm việc đó).

### A2. Quyết định có sinh frame hay không

Trong `gfx.rs` (~dòng 8137), thay điều kiện `let fg = self.frame_gen.on && …` bằng:

```rust
let measurements = frame_gen::Measurements {
    real_ms: self.frame_ms_smoothed,        // đã có: FrameGraph/self.last_frame
    gpu_ms: self.gpu_times.last().copied(),
    made_share: self.frame_gen.made_share.unwrap_or(0.0),
    display_hz: self.display_hz,            // từ winit: monitor.refresh_rate_millihertz() / 1000
    present_wait_ms: self.last_wait.as_secs_f32() * 1000.0,
};
let decision = self.frame_gen.admission.observe(
    &measurements,
    /* in_world: */ !frame.title && frame.menu_blur.is_none(),
    /* external_post: */ self.pack_post.is_some() || self.native.as_ref().is_some_and(|n| n.post.is_some()),
);
if let Some(reason) = self.frame_gen.admission.take_reason_change(decision.reason) {
    log::info!("Horizon: frame generation {} ({} ms real, {} Hz)", reason.as_str(), measurements.real_ms, measurements.display_hz);
}
let fg = decision.generate && self.hdr.is_none() && … /* các điều kiện cũ về format/usage */;
```

### A3. Nhiệt độ (iOS)

`crates/horizon-ios/src/lib.rs` thêm một hàm export:

```rust
#[unsafe(no_mangle)]
pub extern "C" fn horizon_thermal_state() -> i32 {
    // Swift gọi ProcessInfo.processInfo.thermalState.rawValue và ghi vào một atomic;
    // hàm này chỉ đọc nó. (Xem packaging/ios/Launcher/Bridge.swift.)
    0
}
```
Swift phía `Bridge.swift`:
```swift
@_cdecl("horizon_ios_thermal_start")
public func horizonIOSProvideThermal() -> Bool {
    // NotificationCenter thermalStateDidChangeNotification → horizon_thermal_set(Int32(state.rawValue))
    true
}
```

Trong `game.rs` mỗi giây:
```rust
let level = ThermalLevel::from_raw(horizon_thermal_state() as i64);
if let Some(changed) = self.thermal.observe(level, dt) {
    let action = self.thermal.action();
    if let Some(r) = self.renderer.as_mut() {
        if action.suspend_frame_gen { r.set_frame_generation(false); self.settings_saved_frame_gen = …; }
        r.set_dynamic_world_scale(action.resolution_scale);
    }
    self.effective_render_distance_cap = action.render_distance_cap;
    log::info!("Horizon: thermal {changed:?}: {}", action.resolution_scale);
}
```

### A4. Bộ nhớ frame-gen

`const FRAMES: usize = 6;` → `3` trên `DeviceTier::Legacy|Mobile`, và texture dùng
`RENDER_ATTACHMENT|TEXTURE_BINDING` (không cần `COPY_SRC|COPY_DST` cho cặp đôi). Ở 3× scale mỗi
texture 2732×2048 RGBA8 = 22 MB; 6 cái là 130 MB — quá nhiều cho máy 3 GB RAM.

---

## B. Version thật — `crates/horizon-client/src/versions.rs`

1. Trong `prepare_mojang_on_device()`, **bỏ** nhánh hardlink base 26.3 khi `version_id != 26.3`:

```rust
// TRƯỚC
if let Some(base) = sim_base.as_deref() { let linked = populate_from_simulation_base(base, &staging, &clean); … }
// SAU
match horizon_version_truth::bundle_for(&home, &clean) {
    Some(bundle) => { /* giải nén bundle đã sinh offline; kiểm SHA-256 từng file */ }
    None => { /* full_simulation = false; ghi truth = None; badge “Assets only” */ }
}
```

2. Sau khi cài bundle: `horizon_version_truth::stamp(&final_root, &clean, &jar_sha1, protocol, Provenance::Generated)?;`

3. `list_installed()` / `installed_json()` thêm:
```rust
let truth = horizon_version_truth::judge_dir(home, &resources, horizon_core::VANILLA_VERSION);
json!({ "verdict": truth.verdict.as_str(), "badge": truth.verdict.badge(), "detail": truth.detail, "remaining": truth.remaining })
```

   ✅ **ĐÃ LÀM** — nhưng theo cách ít rủi ro hơn: 4 trường (`verdict`, `badge`, `truth_detail`,
   `remaining`) được thêm **thẳng vào struct `InstalledVersion`** (có `serde`), không thêm endpoint
   mới, nên `launcher-versions {"action":"list"}` tự mang chúng và bản Swift đang chạy vẫn parse
   được (chỉ bỏ qua trường lạ). `permissions` không đổi. Cả hai nhánh (`versions/<id>/resources` và
   root `resources/` legacy) đều được judge.

   Thêm: `build_manifest_from_base()` ghi `truth.provenance = "generated" | "borrowed"` ⇒ `judge()`
   có bằng chứng thay vì suy đoán (test `a_borrowed_stamp_from_the_installer_is_caught_even_with_a_full_tree`).

   Phía Swift (`packaging/ios/Launcher/Versions.swift`): chip dùng `statedBadge` (badge thật), thêm
   dòng `truthDetail` + `remainingText`, và `alert` xác nhận khi bấm **Use** trên tree
   `cosmetic`/`broken`.

4. `select_version()` từ chối `Verdict::Broken`, cảnh báo (nhưng cho phép) `Cosmetic`, và trong
   `game/launcher.rs` khi mở world: nếu `Cosmetic` thì ghi log + hiện toast
   `“Assets only: this world still plays 26.3”` trước khi vào.

   ⏳ **CHƯA LÀM (cố ý)** — hiện `select_version()` **vẫn cho chọn** `Cosmetic`, và cảnh báo nằm ở
   launcher (alert xác nhận) chứ chưa phải ở `game/launcher.rs`. Lý do: chặn cứng version trong
   `select_version()` sẽ làm người dùng đang có save của version đó **không mở lại được** cho tới khi
   có bundle thật; alert + nhãn “Assets only” đã đủ để không ai bị lừa. Khi có bundle thật (mục 5)
   thì siết thành lỗi cứng là một dòng.

5. `tools/ci/build_sim_base_release.py` mở rộng thành matrix: mỗi version có
   `(id, client.jar sha1, bundle sha256, protocol)`, upload lên release cùng
   `versions-catalog.json`. Đây là nơi *sinh* bảng mô phỏng thật (cần JDK 21 — không thể trên iPhone).

---

## C. Touch — `crates/horizon-client/src/game/touch.rs`

### C1. Look smoothing

```rust
// Game struct: look: horizon_touch_pipeline::LookSmoother (khởi tạo với spread = 2)
Out::Look(dx, dy) => {
    self.touch_grab();
    self.touch.look.push(dx * scale, dy * scale);
    let (dx, dy) = self.touch.look.next();
    self.mouse.add_motion(f64::from(dx), f64::from(dy));
}
```
Trong `touch_frame`, sau khi xử lý `out`:
```rust
let (dx, dy) = self.touch.look.next();   // phần còn nợ của frame trước
if dx != 0.0 || dy != 0.0 { self.mouse.add_motion(f64::from(dx), f64::from(dy)); }
```
Khi ngón nhấc: `self.touch.look.flush()` để camera dừng đúng chỗ ngón chỉ.

Tuỳ chọn người dùng: `horizon-touch.txt` dòng mới `look_smoothing:<1..4>`, đọc trong
`TouchSettings::parse`; 1 = tắt (PvP).

✅ **ĐÃ LÀM** (khác chi tiết nhỏ so với bản nháp ở trên):
* Dòng file là `look:<1..4>` (không phải `look_smoothing:`), và **không ghi khi bằng 2** (mặc định)
  — file cũ vẫn đọc y nguyên.
* `set_spread(self.settings.touch.look_smoothing)` được gọi mỗi frame trong `touch_frame` nên
  đổi slider là có tác dụng ngay, không cần reload.
* Không còn `look.flush()` khi nhấc ngón: `LookSmoother::next()` tự trả hết phần còn lại trong
  `spread` frame, và `touch_frame` gọi `next()` mỗi frame kể cả khi không có `Out::Look` nào.
* Slider trong options: `Opt::TouchLookSmooth` (4 nấc, `slider_steps() == Some(3)`), nhãn
  Off (raw) / Smooth / Smoother / Smoothest, key `horizon.options.touch_look_smoothing`.

### C2. Scroll theo pixel

Thay khối scroll trong `touch_event` Moved/Ended:

```rust
// MenuFinger: bỏ `scrolled`, thêm `scroll: horizon_touch_pipeline::Scroll`
// bắt đầu: self.touch.menu = Some(MenuFinger { scroll: Scroll::new(SCROLL_LINE, self.settings.discrete_scroll), … })
TouchPhase::Moved => {
    let lines = finger_scroll.moved(at.1, dt);
    if scrolls { for _ in 0..lines.abs().ceil() as u32 { self.touch_wheel(event_loop, lines.signum()); } }
}
TouchPhase::Ended => { let rest = finger_scroll.end(); … }
```

`touch_wheel` vẫn nhận line (game chỉ hiểu wheel theo line), nhưng vì `Scroll` phát *đúng* số line
tương ứng với pixel đã đi, list bám ngón. Với `discrete_scroll` giữ nguyên hành vi quantize của vanilla.

✅ **ĐÃ LÀM** — `MenuFinger` có `scroll: horizon_touch_pipeline::Scroll` + `moved_at`, và **cả hai**
nhánh scroll trong `touch_event` (một ngón cuộn menu, và ngón thứ hai cuộn trong khi ngón đầu giữ)
đều đi qua `Scroll::moved(at.1)`. Không nhánh nào còn `trunc()`.

### C3. Phân loại gesture

Dùng `Classifier` thay cho `SLOP`/`HOLD` rải rác: một `Classifier` cho mỗi ngón `Kind::World`, và
`end()` trả về `Gesture` cuối cùng. Điều này loại bỏ “drag rồi vẫn click”.

⏳ **CHƯA LÀM** — `Touches` trong crate render vẫn dùng `SLOP`/`HOLD`. `Classifier` mới chỉ sống
trong `horizon-touch-pipeline` (có test). Lý do trì hoãn: thay nó là việc sửa *máy trạng thái ngón
tay* — thứ mà 46 test của `touch.rs` đang khoá từng hành vi; làm cùng lúc với việc thêm nút mới sẽ
khiến không phân biệt được hỏng do đâu. Đây là việc sạch, tách riêng, và **không** phải nguyên nhân
của “menu dật dật” (nguyên nhân là `trunc()`, đã sửa).

### C4. Nút Interact / Dismount

`horizon-client-render/src/touch.rs`:
```rust
pub enum Control { …, Interact, Dismount }
```
* ~~`layout_with_affordances(w, h, &Affordances{riding, entity})`~~ → **đã làm đơn giản hơn**:
  `TouchSettings` có một field `pub riding: bool`, `Game` set trước mỗi lần gọi `layout()`, và
  `visible()` trả `false` cho `Dismount` khi `!riding` (hàm `conditional()` đánh dấu nút nào phụ
  thuộc trạng thái). Lý do: `layout()` được gọi từ ~6 chỗ (HUD, editor, menu, layout preview…), thêm
  tham số là sửa cả 6; một field thì chỗ nào quên set cũng chỉ mất nút Get Off, không hỏng layout.
* `control_name`: `"interact"`, `"dismount"`; `control_caption`: `horizon.options.touch.interact`,
  `horizon.options.touch.dismount`; `styled`/`can_hide` như các nút khác.
* `touch_draw`: `Interact` vẽ bàn tay (5 cell), `Dismount` vẽ mũi tên xuống khỏi yên.

`crates/horizon-client/src/game/touch.rs` (**đã làm**, đúng như dưới):
```rust
Out::Press(Control::Interact) => {
    self.play_mouse(MouseButton::Right, true)?;   // giữ phải chuột
    if self.ride_is_tamed() { self.touch.interact_sneak = true; /* ShiftLeft xuống */ }
    self.touch.holding = Some(MouseButton::Right);
}
Out::Release(Control::Interact) => { /* nhả ShiftLeft, rồi nhả chuột ở tick sau (late_keys) */ }
Out::Press(Control::Dismount) => { self.dismount_mount(); }
```
`ride_is_tamed()` chỉ đúng với ngựa/lạc đà **đã thuần** (vanilla: sneak+use mở inventory ngựa);
dân làng/chest-boat nhận đúng một cú right-click, không dính sneak.
Với ngựa đã thuần: giữ **sneak + use** (vanilla). Nút `Interact` khi `self.riding.is_some()`
gửi `ShiftLeft` xuống cùng `Right` (2 tick) để mở `open_mount_menu`. Với dân làng: chỉ `Right`
(không sneak) ⇒ `MerchantMenu`. Không có nhánh mới nào trong engine — chỉ *gọi* đúng hàm đã có.

---

## D. Mod — API cho launcher

`crates/horizon-client/src/game/saves_control.rs`, trong `handle_launcher_mods`:

```rust
"diagnose" => {
    let home = horizon_client::import::home();
    let game_home = horizon_client::logging::game_home();
    let inputs = horizon_mod_diagnostics::Inputs {
        launcher_home: home.clone(),
        game_home: game_home.clone(),
        enabled: horizon_mod_diagnostics::read_mod_list(&home),
        game_enabled: Some(horizon_mod_diagnostics::read_mod_list(&game_home)),
        installed: horizon_mod_diagnostics::scan(&home),
        porter: horizon_mod_diagnostics::read_porter_reports(&home),
    };
    json!({ "diagnostics": horizon_mod_diagnostics::diagnose(&inputs) })
}
```
Và trong `launcher_mods::list_installed` thêm 2 trường `effective: bool`, `diagnosis: Option<String>`
để `Mods.swift` hiện dưới mỗi hàng.

---

## E. RAM — nối vào game

```rust
// Game struct
memory: horizon_memory_governor::Governor::for_device(&device_profile),
caches: horizon_memory_governor::CacheRegistry::new(),

// mỗi frame, sau khi streaming cập nhật:
self.caches.upsert("chunk_meshes", Costly, self.renderer.mesh_bytes(), visible_share(&self.renderer), 0);
self.caches.upsert("textures", Rebuildable, self.gui.atlas.bytes(), 0.8, 0);
self.caches.upsert("scratch", Scratch, self.streaming.scratch_bytes(), 0.0, 0);
if let Some(plan) = self.memory.frame(&self.caches, dt) {
    let freed = self.apply_eviction(&plan);   // renderer.release_sections(...) / atlas.drop(...)
    self.memory.plan_done(freed);
    log::info!("Horizon: memory {} → freed {} MiB", plan.cause.as_str(), plan.freed_bytes() >> 20);
}
```
`didReceiveMemoryWarning` (đã có `game/memory_warning.rs`) chỉ cần thêm 1 dòng:
`self.memory.memory_warning();` — governor lo phần còn lại (hạ trần và evict ngay).

Khi đóng world: `let plan = self.memory.world_closed(&self.caches); self.apply_eviction(&plan);`

---

## F. Trạng thái nối dây thực tế (cập nhật sau khi build lại)

Toàn bộ A–E ở trên **đã nằm trong code**, không còn là đề xuất. Bảng đối chiếu nhanh:

| Mục | Trạng thái | Chỗ nằm |
| --- | --- | --- |
| A. Frame Generation | ✅ xong | `gfx/frame_gen.rs` (`recv_timeout`, `STALL_STRIKES`, `FRAMES` 6→3, `set_setting`, `profile`), `gfx.rs` (`set_frame_generation`, `set_thermal`, `heat_scale_ceiling`) |
| B. Version thật | ✅ xong phần quyết định | `horizon-version-truth`, `versions.rs` (`verdict`/`badge`/`truth_detail`/`remaining`, stamp `borrowed`), `Versions.swift` (chip + alert) |
| C. Touch | ✅ xong (trừ `Classifier`) | `game/touch.rs` (`look`, `Scroll` ở cả 2 nhánh, `interact_sneak`), `render/touch.rs` (`Interact`/`Dismount`, `look_smoothing`, `riding`) |
| D. Mod diagnostics | ✅ xong | `saves_control.rs::mod_diagnostics` → `launcher-mods` `list` + `diagnose`, `Mods.swift` (`effective`, `diagnosis`) |
| E. RAM/thermal | ✅ xong | `game/ram.rs`, `game/thermal.rs`, `game.rs` (`heat_tick`/`ram_tick`/`world_closed`), `control.rs` (overlay `device`/`thermal`/`ram`) |

### Còn treo (biết rõ, không phải bỏ sót)

1. `Present.swift` gọi `coalescedTouchesForTouch` — phía Swift, cần build iOS để kiểm chứng.
2. `Classifier` thay `SLOP`/`HOLD` trong `Touches` — việc tách riêng, rủi ro cao (46 test đang khoá
   hành vi ngón tay), **không** phải nguyên nhân của lỗi nào đã báo.
3. `select_version()` chưa chặn cứng `Cosmetic` — cố ý (xem B.4).
4. Sinh bảng mô phỏng thật cho version ≠ 26.3 — cần JDK trên CI, không thể trên máy.

### Một bài học về kho làm việc (ghi lại để không mất lần nữa)

Snapshot của workspace đã **xoá `crates/horizon-client/src/game/*.rs` và `target/`** hai lần trong
phiên này. Cách phục hồi đã dùng, theo thứ tự:

1. `git clone --depth 1 https://github.com/LLOS-Lord/Vertical-ios-all-versions /home/user/.fresh-src`
   — **đúng repo**, HEAD `ec84e17` khớp với cây làm việc. (`heyskylo/horizon-ios` là repo khác:
   nó có `packaging/ios/` nhưng **không** có `crates/horizon-client/src/game/` hay `versions.rs` —
   clone nhầm repo sẽ làm mất thêm thời gian.)
2. So danh sách file rồi `rsync --ignore-existing` (hoặc `cp -a` từng file trong `.missing.txt`).
3. **Không** sửa code chỉ vì nó "trông lạ": `diff` với `.fresh-src` trước, kết luận sau.
4. `cargo check` lại từ đầu; các file đã sửa ở phía render (`touch.rs`, `options_ui.rs`,
   `settings.rs`, `gfx*`, `streaming.rs`, `touch_draw.rs`, `graphics.rs`) **sống sót**; các file
   mới thêm trong `game/` (`ram.rs`, `thermal.rs`) thì **mất** và phải viết lại từ API mà `game.rs`
   đang gọi — đọc các lời gọi trong `game.rs` trước, vì nó là bản đặc tả.
