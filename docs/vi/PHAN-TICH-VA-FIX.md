# PHÂN TÍCH VÀ SỬA LỖI — Vertical iOS (Horizon Rust engine)

Tài liệu này ghi lại: (1) bằng chứng đọc từ mã nguồn cho từng lỗi người dùng báo, (2) nguyên nhân gốc,
(3) cách sửa đã viết, (4) những gì còn phải làm trên máy Mac/thiết bị thật.

Repo đã clone: `Vertical-ios-all-versions/`, commit gốc `ec84e17`
(`Fix touch gestures, Apple A11/1.21.1 performance, 1.21.1 server adapter & Modrinth/Horizon mod porter`).

---

## 0. Tóm tắt điều tra (đọc trước)

| # | Triệu chứng người dùng | Nguyên nhân gốc tìm được | Trạng thái |
|---|---|---|---|
| 1 | Cài mod xong vào game không thấy gì | 3 nguyên nhân độc lập: (a) mod Java chỉ port được asset, code bị skip mà UI vẫn báo "installed + on"; (b) native mod `.hnmod` không bao giờ được build vào game iOS; (c) đường dẫn `Documents` của launcher và thư mục làm việc của game có thể lệch nhau | Đã có crate chẩn đoán + API + sửa UI gợi ý (xem §1) |
| 2 | Bật Frame Generation → game đứng | `FrameGen::frames()` **block vô hạn** trên `presenter.free.recv()`; sinh frame ở mọi tốc độ bất kể GPU/panel; copy full-screen + present thêm làm hàng đợi GPU dài ra; iOS nhiệt độ cao làm mọi thứ tệ hơn | Crate `horizon-frame-sched` (admission control + thermal) + patch gfx (xem §2) |
| 3 | Version khác 26.3: sảnh ghi đúng, vào world vẫn là item 26.3 | `populate_from_simulation_base()` **hardlink bảng mô phỏng 26.3** vào thư mục version khác rồi chỉ đổi nhãn (`source_minecraft`), và cố tình **xoá `data/`, `models/`, `blockstates/`, `items/`, `atlases/`** của version thật | Crate `horizon-version-truth` + kế hoạch pipeline build thật (xem §3) |
| 4 | Cảm ứng chậm; cuộn menu bị "dật dật" | (a) tap chỉ được phân loại khi **nhấc ngón** ⇒ phản hồi trễ; (b) `Out::Look` đưa delta thô ⇒ camera giật khi UIKit gộp mẫu; (c) scroll dùng `.trunc()` trên `SCROLL_LINE = 12` ⇒ mất phần dư mỗi frame, list nhảy 24→48 px | Crate `horizon-touch-pipeline` + patch `game/touch.rs` (xem §4) |
| 5 | Thiếu nút mở giao dịch dân làng / cưỡi sinh vật | Engine **đã có** `open_mount_menu`, `interact_mount`, `merchant`, `start_riding`; nhưng overlay cảm ứng **không có control nào** cho 2 hành động này (chỉ có `key.use` = chuột phải) | Thêm 2 `Control` mới + logic (xem §5) |
| + | Muốn tối ưu sâu hơn | Crate `horizon-frame-sched::budgets` + `horizon-visibility-budget` (xem §6) |
| + | Quản lý RAM | Crate `horizon-memory-governor` (xem §7) |
| + | Cơ chế port mod | `horizon-mod-diagnostics` (xem §1) |
| + | Chống GPU quá tải / không vẽ thứ không thấy | `horizon-visibility-budget` (xem §6) |
| + | Tương thích chip Apple | `horizon-frame-sched::device` + `thermal` (xem §2, §6) |

---

## 1. Mod không hoạt động

### Bằng chứng

* `crates/horizon-client/src/launcher_mods.rs` — `install_from_modrinth()` / `import_local_mod_file()`:
  porter trả về `verdict` (`works` / `partly` / …) và danh sách `skipped`. Mod **luôn được**
  `set_enabled(home, &out.mod_id, true)` — bật, và UI chỉ hiển thị `verdict` như một nhãn nhỏ.
* `crates/horizon-port-pack/src/store.rs` — porter chuyển được *asset* (texture/model/âm thanh) và
  data JSON; **bytecode `.class` không chạy được** (không có JVM trên iOS). Một mod kiểu HUD/zoom/
  keybind → chỉ còn asset ⇒ "không thấy thay đổi gì" là đúng bản chất, nhưng UI không nói thế.
* `crates/horizon-client-render/src/mod_list.rs::NATIVE_DIR` + `NOT_BUILT`: native mod `.hnmod`
  phải được **biên dịch cùng game**; trên iOS launcher không build lại game ⇒ mod nằm im.
* Đường dẫn: game đọc `horizon-mods.txt` + `horizon-mods/` từ `Game::game_dir()` =
  `std::env::current_dir()`, trên iOS được `set_current_dir(Documents)` (entry.rs `into_app_sandbox`);
  launcher ghi vào `import::home()` = `current_dir()` (iOS). Trên Android/desktop hai giá trị này
  **khác nhau** (`import::home()` trả `logging::game_home()`), và nếu ai đó chạy game với `--gameDir`
  khác thì launcher vẫn ghi `Documents` ⇒ **hai danh sách mod khác nhau, không log gì**.

### Đã làm

* **Crate mới `crates/horizon-mod-diagnostics`**: đọc `horizon-mods/`, `manifest.json`,
  `.sources/<id>.json` (báo cáo porter) và `horizon-mods.txt` ở **cả hai** thư mục, rồi trả về cho
  từng mod: `kind`, `enabled`, `reachable`, `contributes`, `verdict` (một câu tiếng người) và
  `fix` (`AlignGameFolder{launcher, game}` / `NeedsDesktopBuild` / `AssetsOnly{skipped}` /
  `TurnItOn` / `Reinstall`). Toàn bộ luật là hàm thuần, 6 unit test.
* API mới cho launcher: `launcher-mods {"action":"diagnose"}` trả `Diagnosis[]` để SwiftUI hiện
  ngay dưới mỗi hàng mod: “Assets only: behaviour lives in Java code the porter could not carry
  (…)” — người dùng hiểu ngay vì sao không thấy gì, thay vì tưởng launcher hỏng.
* **Đã nối vào launcher** (`saves_control.rs`): `launcher-mods {"action":"list"}` giờ trả thêm
  `diagnostics`, và có `{"action":"diagnose"}` riêng. Hàm `mod_diagnostics()` dựng
  `horizon_mod_diagnostics::Inputs` từ **hai** thư mục — `import::home()` (launcher ghi) và
  `logging::game_home()` (game đọc) — cộng `horizon-mods.txt` của cả hai, danh sách mod đã cài và
  báo cáo của porter. Nhờ vậy câu trả lời "mod này có tác dụng không" là `effective`, tính từ file
  thật, không phải từ công tắc.
* `Mods.swift`: hàng mod có thêm `effective` + `diagnosis`; `badgeTone` trả `.warn` khi
  `effective == false` (thắng cả `verdict` đẹp của porter), và một dòng cảnh báo màu cam hiện ngay
  dưới tên mod. `reloadInstalled()` gộp `diagnostics` vào danh sách theo `id`; đường `toggle` giữ
  lại chẩn đoán đã có thay vì xoá trắng.

### Còn phải làm (đã ghi trong ROADMAP)

* Nút “Port lại với mixin” trên desktop (build lại `horizon-native-mods`).
* Với iOS: chỉ hiển thị các mod `Content`/`PortedJar` là “có hiệu lực”; `.hnmod` hiển thị badge
  “cần build trên Mac”.

---

## 2. Frame Generation làm đứng máy

### Bằng chứng (đọc kỹ `gfx/frame_gen.rs`)

```rust
// FrameGen::frames(), dòng ~228
let pair = if presenter.failed() { None } else {
    presenter.free.recv().ok().zip(presenter.free.recv().ok())   // <-- BLOCK, không timeout
};
```

* `recv()` trên `mpsc::Receiver` **chặn vô hạn**. Present thread phải: acquire swapchain → copy
  texture→texture full-screen → submit → present → sleep(interval/2) → present frame thật. Trên A11,
  mỗi bước này đủ chậm để render thread đứng chờ ⇒ **game đứng hẳn**, input không được xử lý
  (đúng triệu chứng “đứng không thao tác được gì”).
* `FrameGen::present()` gửi `Pair` vào `sync_channel(1)` **mỗi frame thật**, không có khái niệm
  “frame thật có đủ nhanh không”. Nếu frame thật là 30 ms trên panel 60 Hz, việc sinh thêm frame
  15 ms chỉ làm hàng đợi dài thêm: `FRAMES = 6` texture full-size (ở 3× scale, 2732×2048 ⇒ ~22 MB
  mỗi cái) ⇒ 130 MB VRAM/RAM chỉ cho frame-gen.
* Không có khái niệm nhiệt độ. iOS `thermalState` = `.serious` là lúc GPU đã mất ~30–50% xung; lúc
  đó frame-gen là thứ *đầu tiên* phải tắt, nhưng code hiện tại còn không đọc `thermalState`.

### Đã làm

* **Crate mới `crates/horizon-frame-sched`** (thuần logic, 21 test):
  * `admission::Admission` — quyết định mỗi frame: có sinh frame không, hệ số (1.5×/2×), và
    **timeout chờ present thread** (`present_timeout_ms`), dựa trên `real_ms`, `gpu_ms`,
    `made_share` (tỉ lệ frame tạo ra thực sự được hiển thị), `display_hz`, `present_wait_ms`.
    Có warm-up 45 frame, có hạ hệ số khi panel bỏ frame, có cool-down khi present thread làm
    render thread chờ (chống đứng máy), có lý do cụ thể (`Reason`) để log 1 lần mỗi lần đổi.
  * `thermal::ThermalGovernor` — `ProcessInfo.thermalState` → `ThermalAction`
    (resolution scale, particle budget, render distance cap, **suspend_frame_gen**,
    giãn tick nền, frame budget). Có hysteresis: tăng cấp chậm, hạ cấp theo từng bước sau 20 s mát.
  * `device::DeviceProfile::from_hw_machine("iPhone11,8") → A11 → Legacy` ⇒ biết ngay máy nào
    không nên bật frame-gen; `memory_ceiling_mib()` cho luôn trần RAM theo máy.
  * `budgets::FrameLedger` — ngân sách ms/frame chia theo hạng công việc
    (`PlayerCritical` → `Speculative`), tự hạ chất lượng khi vượt budget liên tục.

### Còn phải làm (patch `gfx.rs`, cần build macOS để xác nhận)

Đã ghi rõ trong `docs/vi/integration-notes.md`: thay `recv()` bằng `recv_timeout(decision.present_timeout_ms)`,
gọi `admission.observe(...)` mỗi frame, truyền `display_hz` từ `winit` monitor, đọc `thermalState`
từ `horizon-ios` shim.

---

## 3. Version “fake”

### Bằng chứng (`crates/horizon-client/src/versions.rs`)

```rust
fn populate_from_simulation_base(base, target_resources, version_id) -> usize {
    // hardlink facts.json, registries.json, items.json, worldgen/... từ base 26.3
    ...
    if version_id != horizon_core::VANILLA_VERSION {
        // XOÁ data/, models/, blockstates/, items/, atlases/, equipment/, shaders/, font/,
        // textures/gui/sprites/ của version thật rồi link từ base 26.3
        for schema_bound in [...] { fs::remove_dir_all(&dst); link_missing_tree(&base…, &dst); }
    }
    // manifest: minecraft=26.3, source_minecraft=<version>, full_simulation=true
}
```

⇒ Sảnh hiển thị `source_minecraft` (đúng version), nhưng **mọi thứ engine mô phỏng** (block, item,
entity, registry, model, blockstate) là của 26.3. Đây chính xác là hiện tượng “vào world item vẫn
như 26.3”. Tệ hơn: nó còn ghi 26.3 data vào save của version khác.

### Đã làm

* **Crate mới `crates/horizon-version-truth`**: đọc thư mục đã cài và **phán**:
  `Native` / `Ported` / `Cosmetic` / `Broken`, kèm bằng chứng (`minecraft` vs `source_minecraft`,
  `dataVersion` của `registries.json`, SHA-1 của `client.jar` so với `TruthStamp`), kèm `remaining:
  [Download, GenerateTables, AdaptTables, …]`. `stamp()` ghi dấu vết nguồn gốc để lần sau kiểm chứng lại được.
  Test quan trọng nhất: **`a_hardlinked_26_3_tree_labelled_1_20_1_is_caught_as_cosmetic`**.
* Nhờ crate này launcher/game không thể “nói dối” nữa: badge hiển thị và hành vi trong game lấy từ
  cùng một hàm `judge()`.
* **Đã nối vào launcher** (đây là phần trước còn thiếu):
  * `versions::InstalledVersion` có thêm `verdict`, `badge`, `truth_detail`, `remaining[]`;
    `list_installed()` gọi `horizon_version_truth::judge_dir(home, <resources>, VANILLA_VERSION)` cho
    **từng** thư mục (cả `versions/<id>/resources` lẫn root `resources/` legacy). Trường mới là
    `serde` thường nên bản launcher đang chạy vẫn parse được (chỉ hiện thêm).
  * `build_manifest_from_base()` giờ ghi luôn `truth: {version, client_jar_sha1, protocol,
    provenance}`: `generated` khi là 26.3, `borrowed` khi chép bảng 26.3 sang version khác. Nhờ vậy
    `judge()` **chứng minh** được thay vì suy đoán — test mới
    `a_borrowed_stamp_from_the_installer_is_caught_even_with_a_full_tree` khoá lại đúng hành vi này.
  * `packaging/ios/Launcher/Versions.swift`: chip của mỗi version giờ lấy `statedBadge` (badge thật,
    vd “Assets only”) chứ không phải suy từ `support_tier`; dòng lý do màu cam (`truthDetail`) +
    “N steps left: generate-tables, …” hiện ngay dưới tên; bấm **Use** trên một tree `cosmetic`/
    `broken` sẽ bung `alert` giải thích rõ (texture là của version đó, còn block/item/mob/định dạng
    world là của version khác) trước khi đổi version. Bảng màu thêm `Theme.warnText` để chữ cam đọc
    được ở cả light/dark.

### Còn phải làm để có version *thật* (đây là phần lớn nhất, cần máy Mac + jar Mojang)

Đường đi đúng (đã kiểm chứng bằng cách đọc `horizon-prepare`):

1. `horizon-prepare` **bắt buộc** có JVM vì nó chạy *exporter* — bytecode của chính Mojang
   (`exporters.rs`: `VanillaGameplayFactsExporter`, `VanillaEntityTypesExporter`, … là mã Java
   Horizon tự viết, `javac` + `java -cp client.jar`). Không có javac trên iOS ⇒ **không thể sinh
   bảng mô phỏng cho version mới trên máy**.
2. Vì vậy version thật phải được sinh **offline** (CI/desktop) rồi tải về dưới dạng bundle:
   `horizon-prepare --client-jar <id>.jar --assets-dir … --output bundles/<id>/` (xem
   `tools/ci/build_sim_base_release.py` đã có sẵn trong repo, mở rộng nó thành multi-version).
3. Launcher trên iOS: tải `<id>.zip` (SHA-256), giải nén vào `versions/<id>/`, chạy
   `horizon-version-truth::stamp(..., Generated, jar_sha1)` và **không bao giờ** hardlink base 26.3
   để giả làm version khác nữa. Với version chưa có bundle: hiển thị badge “Assets only” và
   **chặn tạo world mới** trên version đó (chống corrupt save).
4. Bỏ `populate_from_simulation_base` khỏi đường “Mojang official” (giữ nó chỉ cho repair của 26.3).

Tôi đã viết `rust` cho tầng quyết định + dấu vết + phán quyết; phần “sinh bảng” phụ thuộc JVM nên
phải chạy ngoài thiết bị — điều này là **giới hạn vật lý**, không phải lựa chọn thiết kế.

---

## 4. Cảm ứng chậm + menu “dật dật”

### Bằng chứng

* `crates/horizon-client/src/game/touch.rs`:
  * Tap: `Touches::end_at()` quyết định tap khi **nhấc ngón**; sau đó `play_mouse(press)` rồi
    `touch.late.push(...)` để nhả ở tick sau. Với một ngón giữ 200 ms, phản hồi tới sau khi nhấc.
  * Look: `Out::Look(dx,dy)` → `self.mouse.add_motion(dx,dy)` **thô**. UIKit gộp mẫu ⇒ 3 mẫu/frame,
    rồi 0 mẫu/frame ⇒ camera giật.
  * Scroll: `let lines = ((at.1 - last.1)/SCROLL_LINE).trunc();` với `SCROLL_LINE = 12.0` và sau đó
    `f.scrolled = (at.0, last.1 + lines*SCROLL_LINE)` ⇒ **phần lẻ bị bỏ**. Ngón tay đi 20 px/frame ⇒
    1 line (12 px) → bỏ 8 px; frame sau 20+8=28 px ⇒ 2 line (24 px) bỏ 4 px… menu nhảy 24→48 px:
    đúng “dật dật”.
* `Touches::moved()` dùng `SLOP = 8.0` cho cả “trở thành drag” và “còn là tap” ⇒ ngón rung 1 px quanh
  ngưỡng có thể đổi trạng thái qua lại; `end_at` kiểm tra lại slop lần nữa ⇒ “drag camera nhưng lại click”.

### Đã làm

* **Crate mới `crates/horizon-touch-pipeline`** (12 test):
  * `Press` + `PressTiming::Down` — nút sáng **ngay frame ngón chạm**, tách khỏi bookkeeping
    late-release của game (không mất input, không trễ cảm giác).
  * `LookSmoother` — chia đều delta qua các frame nhưng **bảo toàn tổng** (test chứng minh
    `handed == pushed`), có trần chống “camera tự quay” khi mẫu bị kẹt. Không dùng EMA ⇒ không mất
    độ chính xác ngắm.
  * `Scroll` — smooth mode giữ **cả phần lẻ** (list bám ngón từng pixel) và discrete mode (vanilla
    `discreteScrolling`) chỉ phát line nguyên, tối đa 3 line/frame, có `end()` nhả phần dư.
  * `Coalescer` — gộp `coalescedTouchesForTouch` thành 1 `Motion`/frame, giữ `peak` (phát hiện flick).
  * `Classifier` — tap/hold/drag có **hysteresis** và **một chiều** (đã drag thì không thể thành tap).

### Đã nối vào code (không còn là “đề xuất”)

* `game/touch.rs`: `TouchInput` có `look: LookSmoother`, `interact_sneak`, `holding`, `late_keys`;
  mỗi `Out::Look(dx,dy)` đi qua `look.push(dx,dy)`, và cuối `touch_frame()` gọi `look.next()` để
  nhả đúng phần còn lại của burst — tổng chuyển động **không đổi**, chỉ chia đều qua các frame.
  `set_spread()` đọc từ `settings.touch.look_smoothing` mỗi frame nên đổi trong options là có hiệu lực ngay.
* `MenuFinger` có thêm `scroll: Scroll` + `moved_at: (f32,f32)`; cả hai nhánh scroll (một ngón và
  ngón thứ hai) đều dùng `Scroll::moved()` ⇒ hết mất phần lẻ, menu hết nhảy 24→48 px.
* Options: thêm `Opt::TouchLookSmooth` (slider 4 nấc: Off/raw · Smooth · Smoother · Smoothest),
  ghi vào `horizon-touch.txt` dạng `look:<n>` (mặc định 2 thì không ghi), có lang key
  `horizon.options.touch_look_smoothing` ở cả `en_us.json` và `vi_vn.json`.

### Còn phải làm

`Present.swift` gọi `coalescedTouchesForTouch` khi gửi sự kiện (phía Swift, cần build iOS để xác nhận).

---

## 5. Thiếu nút giao dịch dân làng / cưỡi sinh vật

### Bằng chứng

* Engine **đã có** đủ logic: `game/mounts.rs::interact_mount()` (mở menu ngựa/lạc đà, cưỡi heo,
  strider, llama, ghast), `game/mount_menu.rs::open_mount_menu()`, `merchant.rs`,
  `game/interactions.rs`, `mobs/riders.rs::start_riding()`.
* Overlay cảm ứng: `Control` enum (`horizon-client-render/src/touch.rs`) chỉ có
  `Forward/Back/Left/Right/Sneak/Jump/Perspective/Chat/Pause/HideHud/Inventory/Hotbar/Custom/Stick`.
  ⇒ Trên điện thoại **không có nút nào** cho phải-chuột-mở-menu-dân-làng (dân làng cần
  `use` **không** sneak, ngựa đã thuần cần `use` + **sneak** để mở inventory), và không có nút
  “xuống ngựa” riêng. Người dùng chỉ có “tap vào thế giới = use”, nhưng tap chỉ gửi **một click ngắn**
  và `entity_targeted()` phụ thuộc hit test — không ổn định với đám đông.

### Đã làm

* Thêm 2 `Control` mới vào crate `horizon-touch-pipeline`-compatible layout:
  * `Control::Interact` — “dùng tay”: gửi **giữ** `key.use` (chuột phải) trong 2 tick rồi nhả, đủ để
    mở merchant/mở menu ngựa/cưỡi; kèm logic `sneak` khi đang cưỡi (vanilla: sneak + use = mở
    inventory ngựa).
  * `Control::Dismount` — gọi `dismount_mount()` trực tiếp (không phụ thuộc phím).
  Chi tiết + lý do thiết kế ở `docs/vi/integration-notes.md`.
* **Đã xong cả hai phía** (đây là phần trước còn treo):
  * Crate render: `Control::Interact` / `Control::Dismount` có đủ `control_name`,
    `control_from_name`, `control_caption`, `styled`, `can_hide`, `migrate_global_style`,
    `touch_draw::glyph`; `layout()` đặt chúng **cùng cột với Jump** (Use ngay trên, Get Off trên nữa)
    để ngón cái đi một đường; `TouchSettings.riding` + `visible()` làm nút Get Off **chỉ hiện khi
    đang cưỡi** (ẩn tay vẫn thắng).
  * Game: `key_of(Interact)` → `Key::Right` (giữ), `key_of(Dismount)` → `Out::Press/Release` xử lý
    riêng; sneak+use **chỉ** khi `ride_is_tamed()` (ngựa/lạc đà đã thuần) — dân voi hay chest-boat
    không bị “sneak” oan; `settings.touch.riding` được set trước khi gọi `layout()` mỗi frame.
  * Nhãn: “Use” / “Get Off” (đã có ở `en_us` + `vi_vn`).
  * Test: `dismount_is_only_on_screen_while_there_is_a_mount`,
    `look_smoothing_round_trips_and_only_writes_a_non_default`.

---

## 6. Hiệu năng / GPU / “không vẽ thứ không thấy”

* `horizon-frame-sched::budgets::FrameLedger` — ngân sách ms/frame theo hạng; việc hạng thấp bị
  **từ chối** thay vì làm frame dài ra; hạ chất lượng sau 30 frame vượt budget, phục hồi sau 600
  frame đạt; `may_speculate()` tắt hẳn việc “chuẩn bị trước” khi frame đã đắt.
* `horizon-visibility-budget` — chấm điểm từng ứng viên (frustum + tỉ lệ không bị che + độ gần tâm
  ngắm + khoảng cách + trọng số loại), sắp xếp và **cắt theo budget** trước khi submit ⇒ section bị
  che/bị quay lưng không tốn draw call, entity/particle xa không chiếm frame của terrain.
* `horizon-frame-sched::thermal` — chip nóng thì giảm resolution scale, particle, render distance,
  tắt frame-gen **trước khi** iOS bóp xung.
* `horizon-frame-sched::device` — phân loại A11/A12/A13/A14/A15/A16/A17/Modern → trần RAM, budget
  frame, có cho frame-gen hay không.

## 7. Quản lý RAM

* `horizon-memory-governor`:
  * `CacheRegistry` — cache có tên, `Class` (Irreplaceable/Costly/Rebuildable/Scratch),
    `visible` (renderer cập nhật mỗi frame), `idle_frames`.
  * `Governor` — trần `high`/`low` theo `DeviceProfile::memory_ceiling_mib()`, evict theo
    (class → visible thấp → idle cao), **hysteresis** (evict xuống `low`, không evict từng frame),
    `memory_warning()` hạ `low` xuống 55% trần trong 600 frame.
  * `world_closed()` — dọn sạch mọi cache rebuildable khi rời world (map cache).
  * `census()` cho F3 / `horizon-ctl memory`.

### Đã nối vào game (phần trước chỉ là crate rời)

* `game/ram.rs` — `Budget` + `Census`, và **`visible_share(renderer)`**: tỉ lệ section đang được vẽ
  trên tổng số section đang giữ. Đây là thứ biến "cache 40 MB" thành "cache 40 MB mà chỉ 20% nhìn
  thấy" — nếu không có nó, governor chỉ biết *lớn hay nhỏ*, không biết *đáng giữ hay không*.
  `frame()` chia `mesh_bytes` theo `visible_share` thành `chunk_meshes_near` (Costly) và
  `chunk_meshes_far` (Rebuildable) chứ không báo hai lần: phần bị bỏ chỉ được đếm một lần, và đúng
  phần mà `release_offscreen_sections()` thực sự giải phóng được.
* `game/thermal.rs` — `Thermal` + `reported()` (đọc `horizon_ios_launcher::thermal_state()` hoặc
  `HORIZON_THERMAL_STATE`), bảng hành động nằm trong `horizon-frame-sched::thermal`. `world_closed()`
  đưa `applied` về mức nguội để renderer của world mới được **bảo lại** thay vì kế thừa trạng thái
  của renderer đã chết.
* `game.rs` — `heat_tick()` chạy **trước** `ram_tick()`: một bước nhiệt cũng giải phóng bộ nhớ (ít
  section, ít hạt hơn), và frame-gen phải tắt trước khi renderer đo frame mà nó định nội suy.
* `control.rs` — overlay `status` có thêm `device`, `thermal`, `ram` (kèm **census từng dòng**:
  tên, class, byte, tỉ lệ nhìn thấy) và `frame_generation`. Đây là câu trả lời trực tiếp cho
  "tại sao iPhone của tôi chỉ được 30 fps": nhìn thấy tier thiết bị, mức nhiệt đang áp, và cache nào
  đang chiếm bao nhiêu.
* `graphics.rs` — thêm `FrameGeneration::is_on()`: một vị từ duy nhất cho overlay, bộ lập lịch và
  governor nhiệt, để "off" không bao giờ bị hiểu nhầm thành một chế độ.

## 8. Kiểm chứng (chạy được trong repo này)

| Kiểm tra | Kết quả |
| --- | --- |
| `cargo check -p horizon-client --no-default-features` | ✅ Finished (8 warning, đều là dead-code có sẵn) |
| `cargo check -p horizon-client-render --no-default-features` | ✅ Finished |
| `cargo check -p horizon-client-input` | ✅ Finished |
| `cargo test` 6 crate mới | ✅ 8 + 12 + 21 + 7 + 6 + 6 = **60 pass** |
| `cargo test -p horizon-client-render --lib touch::` | ✅ **46 pass** (thêm 2 test mới cho `look:` và Get Off) |
| `cargo test -p horizon-client --lib` | ❌ **hỏng sẵn trong repo**, không liên quan (xem dưới) |

**Ba test hỏng sẵn — không phải do thay đổi này** (đã kiểm bằng cách thay toàn bộ
`crates/horizon-client-render/src` bằng bản clone sạch rồi chạy lại: **vẫn hỏng y hệt**):

1. `chat::tests::chat_field_budgets_utf16_before_normalization_and_replaces_selected_units` —
   kỳ vọng chuỗi 200 ký tự có `?` ở cuối, nhận được 199 ký tự không có.
2. `extras::mobile_ui::tests::mobile_descriptions_keep_shared_keys_and_native_fallbacks` — kỳ vọng
   `wheel_cycles_list_settings`, nhận `wheel_cycles_settings`.
3. `gfx::hdr` — **SIGSEGV**, sập cả process test (nên mọi `cargo test -p horizon-client-render`
   đầy đủ đều dừng ở đó; chạy theo filter thì qua).

Cụ thể với `cargo test -p horizon-client --lib`: 74 lỗi biên dịch (`PlayerMotion::new` nhận
`Arc<MotionResources>` trong khi test truyền `&MotionResources`) — `motion.rs` và các file test
**giống hệt** bản clone sạch, tức repo tự nó không build được test target.

Số liệu trên lấy từ lần chạy cuối sau khi phục hồi workspace; nếu `target/` bị xoá thì phải build
lại từ đầu (~4 phút cho `horizon-client-render` với 4 GiB swap).

---

# Phần 9 — Bốn yêu cầu lớn: phân tích sâu và ba crate điều phối mới

Yêu cầu: (1) đa luồng theo mức quan trọng, (2) quản lý RAM, (3) cơ chế port mod, (4) chặn GPU quá tải.
Nguyên tắc bắt buộc: tuân thủ `WORKING_PRINCIPLES.md`.

## 9.1 Phát hiện cốt lõi: repo đã có *bốn* thang ưu tiên nhưng chúng không biết nhau

Đây là kết quả của việc đọc toàn bộ đường đi (định nghĩa → nơi gọi → test), và nó giải thích vì sao
"game chạy mượt nhưng vẫn còn nhiều dư địa":

| Thang ưu tiên | Ở đâu | Đo bằng gì | Ai dùng |
| --- | --- | --- | --- |
| `horizon_visibility_budget::Sort` | `crates/horizon-visibility-budget` | trọng số 0.25–1.0 theo *loại* | **không ai** (crate chết) |
| `horizon_frame_sched::FrameKind` | `crates/horizon-frame-sched/src/budgets.rs` | `share()` 1.0 / 0.75 / 0.5 / 0.25 / 0.1 | **không ai** (crate chết) |
| `horizon_threads::jobs::Class` | `crates/horizon-threads/src/jobs.rs` | `Urgent` / `Near` / `Far` | pool job (có dùng) |
| `horizon_memory_governor::Class` | `crates/horizon-memory-governor` | `Irreplaceable` / `Costly` / `Rebuildable` / `Scratch` | chỉ `game/ram.rs` |

Bốn thang, bốn cách chia khoảng, và **không chỗ nào chuyển đổi giữa chúng**. Hệ quả cụ thể:

* `Sort::EntityNear` (0.95) > `Sort::SectionNear` (0.75) ⇒ một con bò ở xa xếp trên mặt đất người chơi
  đang đứng — trong khi `Class::Near` (đúc chunk, *sinh ra* mặt đất đó) là enum khác, không liên quan.
* `FrameKind::PlayerCritical` tồn tại **chính xác** để bảo vệ mặt đất, nhưng đường vẽ không hề đọc nó.
  Đo khung hình rồi không hành động theo số đo — đó là toàn bộ lỗi.
* `jobs::Class::Urgent` (lưu game, mã hoá chunk) mặc nhiên *cao hơn* mọi thứ, kể cả địa hình người
  chơi đang nhìn. Đây là cơ chế "đói" (starvation) làm địa hình dưới chân tới muộn.

## 9.2 Crate 1 — `horizon-importance`: một thang duy nhất

`Importance` là 8 bậc, xếp theo đúng thứ tự người chơi sẽ phàn nàn nếu mất:

`Critical` → `Near` → `Visible` → `OwnBody` → `Animated` → `Ambient` → `Speculative` → `Invisible`

Mỗi từ vựng cũ ánh xạ vào bằng hàm toàn phần: `of_sort`, `of_frame_kind`, `of_job`, `of_cache`
(ánh xạ *đảo*: `Irreplaceable` → `Critical` vì đó là thứ phải giữ nhất). `Shedder::shed` trả lời câu
hỏi duy nhất khung hình thực sự có: *với chừng này mili-giây, bỏ cái nào?*

**Quy tắc quan trọng nhất phát hiện trong lúc viết test:** khung hình phải là một **tiền tố** của
thang, không bao giờ là tập con rời rạc. Nếu một bậc không vừa ngân sách thì mọi bậc dưới nó cũng
hết ngân sách (`closed` trong `shed`). Không có quy tắc này, một đám hạt rẻ tiền sẽ được vẽ trong khi
mặt đất bị bỏ — đúng lỗi mà crate này sinh ra để sửa. Test `the_ladder_outranks_cheapness` khoá tính
chất đó.

`jobs::Class::Urgent` ánh xạ xuống `Visible`, **không** lên `Critical`: việc nền khẩn cấp không phải
là thứ người chơi có thể mất game vì nó.

**16/16 test pass.**

## 9.3 Crate 2 — `horizon-gpu-governor`: chặn GPU quá tải

Nghiên cứu (xem §9.6) cho hai kết luận **ngược với trực giác từ renderer immediate-mode**:

1. **Hidden surface removal của Apple là miễn phí và chính xác từng pixel.** Depth prepass không mua
   được gì — Xcode 12 gắn cờ "unnecessary" trên GPU Apple; đo đạc xác nhận: một scheme occlusion hai
   pha trên GPU Apple **chậm hơn 0–1.9 ms/khung**.
2. **Tiler flush khi pipeline đổi theo cách kết thúc hidden-surface pass** — thủ phạm kinh điển là
   lệnh translucent xen giữa opaque, hoặc bất kỳ `discard` nào ghi depth. Mỗi lần flush nạp lại toàn
   bộ tile memory của khung hình.

⇒ **Thứ tự submit đáng giá hơn số pixel.** Do đó `Bucket` (`Opaque` → `AlphaTest` → `Translucent`)
là cốt lõi, và `Scheduler` tách hai quyết định: *cái gì* (thang `Importance`) và *khi nào* (bucket).
`Batches::flushes()` đếm số lần flush ⇒ overlay thấy được khi nội dung làm batching tệ đi.

Tiler thiếu **bin-list và geometry throughput**, không thiếu fragment ⇒ ngân sách `Budget` tính cả
draw call, tam giác và mili-giây; test `the_draw_ceiling_is_the_one_that_bites_first` chứng minh draw
call là giới hạn chạm trước.

`Governor` (frame-time → resolution scale) có bất đối xứng: hạ ngay (2 khung), nâng chậm (20 khung),
và `ceiling` ghi nhớ mức đã không giữ nổi. **Bug thật tìm được qua test:** nếu ghi `ceiling` cả khi
đã ở sàn, một giây tệ sẽ ghim game ở 50% độ phân giải *vĩnh viễn*. Sửa: chỉ ghi khi `scale > floor`,
và `ceiling` tự hồi lên sau `RECOVER_AFTER` (120 khung) bằng chứng sạch.

**22/22 test pass.**

## 9.4 RAM — sửa gốc: trần Jetsam phải đo tại runtime, không phải bảng

`game/ram.rs` cũ dùng bảng `Legacy => 700, Mobile => 900, …` theo tier chip. Nghiên cứu cho thấy
điều đó **sai về nguyên tắc**: giới hạn Jetsam **không** suy ra từ RAM lắp máy — iPhone SE3 ≈1384 MB,
iPhone 13 ≈2099 MB, iPad 9 ≈1024 MB. Hai máy cùng 4 GB có thể bị giới hạn cách nhau 50%.

Viết lại `game/ram.rs`:

* `Device { physical_mib, available_mib, footprint_mib, limit_mib }`.
* `Device::limit_from(footprint, available)` = tổng hai số đo. `os_proc_available_memory()` trả 0 khi
  process **đã** vượt ⇒ trần không bao giờ thấp hơn footprint (test khoá lại).
* Trần thiết kế = **65%** giới hạn (50% khi đã có cảnh báo) — chừa chỗ cho burst không lập lịch được.
* Cảnh báo RAM ⇒ **giảm một nửa**, không purge: purge toàn bộ gây stall decode lại lớn hơn áp lực ban
  đầu. Trần tự hồi sau `WARNING_HOLD` = 10 s yên tĩnh.
* `visible_share()` chia `mesh_bytes` thành near (Costly) / far (Rebuildable) ⇒ eviction chỉ nhắm phần
  thật sự thu hồi được.

## 9.5 Crate 3 — `horizon-mod-shim`: mod port xong phải *chạy*, và phải nói thật

Vấn đề: `horizon-port-pack` đã port, `horizon-mod-diagnostics` đã giải thích vì sao mod *trơ*, nhưng
không ai trả lời câu mod author hỏi: *"porter bảo thành công; cái gì trong mod tôi sẽ được gọi?"*

Bài học từ Sinytra Connector và Wrapper: lớp tương thích chỉ tốt bằng **bề mặt API nó thật sự shim**,
và mọi thứ không shim đều hỏng **âm thầm**.

`TABLE` là bảng 30 điểm mở rộng, mỗi điểm ghi: tên loader, family, `Fidelity`
(`Native`/`Adapted`/`Absent`), **đường dẫn tới nơi hiện thực trong repo này**, và một câu giải thích.

**Quy tắc chịu lực:** không bao giờ khai nhiều hơn sự thật. Mỗi `Native` là một khẳng định về một
facility *có thật trong repo hôm nay*. Test `every_entry_names_a_real_implementation_unless_absent`
buộc mọi mục khác `Absent` phải nêu đường dẫn.

Điểm thiết kế quan trọng thứ hai: `Coverage::unknown` **tách khỏi** `absent`. "Không biết" khác "biết
là không có"; gộp chúng sẽ làm mọi mod trên loader lạ đọc thành hỏng. Verdict vẫn nói thật: chỉ
`works` khi mọi điểm đều được phục vụ.

**12/12 test pass.**

## 9.6 Nguồn tham khảo

| Nguồn | Kết luận đã dùng |
| --- | --- |
| WWDC20-10605 "Gain insights into your Metal app with Xcode 12" | Xcode gắn cờ depth prepass là *không cần thiết* trên GPU Apple; TBDR/HSR đã bỏ overdraw opaque |
| "Tile-Based GPUs Explained: Apple, Mali, Adreno, PowerVR and TBDR" (safeguard.sh) | HSR flush khi gặp translucent/`discard` ⇒ opaque → alpha-test → translucent, không xen kẽ; nút cổ chai là geometry/bin list, không phải fragment |
| aszecsei/cinnabar PR #223, #253; Siroko/kansei PR #37 | Occlusion hai pha trên GPU Apple: **chậm hơn 0–1.9 ms**; "tiled profile" bỏ depth prepass + occlusion ⇒ M1 Pro 8.05→6.9 ms |
| MIKE TURITZIN / HZB thread (x.com/miketuritzin, 4/2025) | Two-pass HZB chỉ đúng cho IMR ⇒ chỉ bật sau capability gate, không bật mặc định |
| iOS Jetsam (rorklab.net; codoid.com; appxiom.com) | Giới hạn **không** suy từ RAM; dùng `os_proc_available_memory()` + `task_vm_info.phys_footprint`; `resident_size` sai |
| Sinytra Connector discussions #11, #133; theo.is-a.dev/blog/porting-fabric-to-forge; InsanityLabs/Wrapper | Shim theo bề mặt API; bảng trạng thái shim từng module; thất bại là âm thầm |
| breakingac.com "Why Your Mobile Game Stutters"; Mindful Chase "WaveEngine Performance Triage"; PulseGeek "Frame Rate vs Latency" | Ngân sách theo khung (2–4 ms cho việc nền); hysteresis chống pumping; p99 < 1.5× p50 là chỉ số đúng |

## 9.7 Việc còn lại (chưa nối vào đường chạy)

Ba crate này là **lớp chính sách**, đã test đầy đủ, nhưng **chưa được gọi từ `gfx.rs`**. Cụ thể còn:

1. Nối `Scheduler::schedule` vào đường submit của `gfx.rs` (trước `draw`, theo `Bucket`).
2. Nối `Shedder` vào chỗ dựng danh sách candidate của `Renderer`, lấy `FrameLedger` làm nguồn ngân sách.
3. Nối `horizon-mod-shim::judge` vào `horizon-mod-diagnostics` để verdict của launcher có dữ liệu shim.
4. `probe()` trong `game/ram.rs` hiện trả `limit_mib: None` (chưa có FFI iOS trong sandbox này);
   `horizon-ios` cần điền `os_proc_available_memory()` + `phys_footprint` thật.

## 9.8 Kiểm chứng (§9) — chạy thật trong sandbox này

| Kiểm tra | Kết quả |
| --- | --- |
| `cargo check -p horizon-client --no-default-features` | ✅ **Finished, 0 error** (10 warning, đều dead-code của API chưa được gọi) |
| `cargo test -p horizon-importance` | ✅ **16 pass** |
| `cargo test -p horizon-gpu-governor` | ✅ **22 pass** |
| `cargo test -p horizon-mod-shim` | ✅ **12 pass** |
| **Tổng** | ✅ **50 test pass, 0 fail** |

**Ba lỗi thật do test bắt được** (giữ lại vì chúng là bằng chứng thiết kế, không phải lỗi gõ):

1. `horizon-importance`: bản đầu cho phép hạt rẻ tiền lọt vào sau khi địa hình bị bỏ ngân sách →
   thêm quy tắc `closed` (khung hình phải là **tiền tố** của thang). Test `the_ladder_outranks_cheapness`.
2. `horizon-gpu-governor`: ghi `ceiling` cả khi đã ở sàn ⇒ một giây tệ ghim game ở 50% độ phân giải
   **vĩnh viễn** → chỉ ghi khi `scale > floor`, thêm `RECOVER_AFTER`. Test
   `the_ceiling_decays_back_up_after_a_bad_patch`.
3. `game/thermal.rs`: `resolution_scale` khai trong `impl` thay vì `struct`.

**Giới hạn phải nói thật:**

- **Chưa chạy trên macOS/Xcode/thiết bị thật.** Không có GPU, không có Jetsam, không có
  `ProcessInfo.thermalState`. Ba crate là logic thuần nên test được đầy đủ; `probe()` trong
  `game/ram.rs` trả `limit_mib: None` trên mọi nền tảng không phải iOS, nghĩa là **đường đo Jetsam
  thật chưa được chạy lần nào**. `horizon-ios` phải điền hai con số đó.
- Ba crate **chưa được gọi từ `gfx.rs`** (§9.7) — đúng như bảng "việc còn lại". Chúng là lớp chính
  sách đã test, không phải tính năng đang bật.
- Sandbox reset **hai lần** trong request này (xoá toolchain, prune 482 file `game/*.rs`, xoá swap).
  Đã phục hồi đầy đủ; `game/touch.rs` phải vá lại 2 arm `Interact`/`Dismount` đã mất trong lần prune.
