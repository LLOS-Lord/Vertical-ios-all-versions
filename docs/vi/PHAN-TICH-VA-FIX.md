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

---

# Phần 10 — Nối `Scheduler` vào `gfx.rs` + build trên GitHub Actions

## 10.1 Điều chỉnh thiết kế sau khi đọc đường submit thật

Kế hoạch ban đầu (§9.7) là "gọi `Scheduler::schedule` trước khi submit". Đọc `gfx.rs` cho thấy điều
đó **sai** ở hai điểm, và sửa lại là phần lớn công việc của phần này:

1. **Renderer đã submit đúng thứ tự cho TBDR.** Solid terrain đi pass riêng, translucent đi pass
   riêng (`draw_sections(.., 0, solid_draws, translucent_draws)`), mirror và shadow camera cũng vậy.
   Không có translucent nào xen giữa opaque ⇒ **không có gì để `Bucket` sửa**. Thêm một lượt sort
   chỉ tốn CPU mỗi khung.
2. **`Renderer::draws` không phải một túi draw độc lập.** Nó là một dãy **run liền mạch**, mỗi run
   chia sẻ một arena page + index page, vì `draw_sections_from` bind một pipeline và một cặp buffer
   cho mỗi page rồi phát cả run. Solid là **tiền tố** `0..solid_draws`, translucent nối sau, và trong
   solid thì `phase1_draws..phase2_cutout` là opaque, phần còn lại là cutout.

⇒ Bỏ lẻ tẻ một draw ở giữa là vô nghĩa (tiết kiệm đúng một `draw_indexed`, thứ GPU vốn đã bỏ qua khi
bin rỗng). Cắt **đuôi** mới đáng: một run 200 draw trên một page là **một** lệnh
`multi_draw_indexed_indirect`; giảm nửa danh sách là giảm nửa lệnh đó — và trên GPU Apple nút cổ chai
là geometry/bin-list chứ không phải fragment.

## 10.2 Cái đã làm: `gfx/draw_budget.rs`

`horizon_gpu_governor::Governor` (đo thời gian GPU, trả về scale) + `Plan::of(solid, translucent,
scale)`:

* Scale áp lên **solid range** dưới dạng **độ dài tiền tố**, không bao giờ tạo lỗ.
* **Translucent giữ nguyên độ dài**: đó là nước và kính quanh người chơi, số lượng ít so với solid, và
  làm rỗng hồ trước khi rút ngắn chân trời là đánh đổi sai.
* `MIN_SOLID_DRAWS = 8`: không bao giờ cắt vào near-field.
* **Khung hình bình thường giữ nguyên toàn bộ danh sách** — `Plan::full`, mọi phép tính range quy về
  như trước. Đường đo là **ngoại lệ**, không phải mặc định.
* Một chỗ duy nhất: `let solid_draws = plan.solid;` — mọi pass đọc `solid_draws` nên không pass nào
  vẽ quá mốc.
* `target` của governor lấy theo `display_hz()` thật.

**Test bắt được một lỗi thật:** `struggling()` ban đầu đo "plan có cắt không" — nhưng plan còn cắt
suốt hàng trăm khung sau khi máy đã khoẻ (scale chưa về 1.0). Đổi sang đo "còn vượt target không"
(`governor.frame_ms() > target_ms()`). **9/9 test pass.**

## 10.3 Guardrail của CI — hai cái đỏ sẵn phải sửa trước khi push

| Check | Trước | Sau |
| --- | --- | --- |
| `check_android_manifest.py` | ❌ thiếu **8** dependency (5 của request trước + 3 crate mới) | ✅ pass |
| `check_layers.py` | ❌ **9** crate không có layer | ✅ 69 crate, 28 exception |
| `check_public_tree.py` | cần git history → chỉ chạy được trên CI | ✅ pass trên CI |

## 10.4 Push và kết quả Actions (commit `91bfa45`)

| Workflow / job | Kết quả |
| --- | --- |
| `build` → Compact 26.3 Sim Runtime Pack | ✅ success |
| `build` → **Rust Guardrails & Functional Tests** | ✅ **success** (guardrails, `horizon-hooks`, `horizon-addresses`, `horizon-protocol`, `horizon-port-pack`, `horizon-official`, `horizon-client-lib`, e2e ios_all_versions, check official) |
| `build` → iOS Swift Tests & Native ARM64 IPA | ⏳ đang chạy |
| `android` → apk | ❌ **fail ở "Build the game"** |

### Lỗi Android và cách sửa

```
error: cannot update the lock file .../crates/horizon-android/Cargo.lock
       because --locked was passed to prevent this
```

`crates/horizon-android` là **workspace riêng** với **lockfile riêng** (`crates/horizon-android/
Cargo.lock`, có track trong git). `android.yml` build bằng `--manifest-path crates/horizon-android/
Cargo.toml --locked`. Thêm 8 dependency vào manifest mà không cập nhật lockfile đó ⇒ cargo muốn ghi
⇒ `--locked` chặn.

**Cách sửa — và một cái bẫy đã tránh được:** `cargo update --manifest-path` **nâng cấp 93 crate**
(gồm `wgpu`, `naga`, `tokio`, `zip`, `arboard`…) vì lockfile của Android đã cũ so với registry. Đó là
thay đổi khổng lồ không liên quan và rất dễ làm hỏng build ARM64.

Thay vào đó: chép **đúng 9 package block** của crate nội bộ từ `Cargo.lock` gốc sang, sắp xếp lại
theo (name, version) đúng thứ tự cargo, rồi để `cargo metadata --locked` thêm các **cạnh phụ thuộc**
còn thiếu trong những package đã có. Kết quả: **+73 dòng, 0 dòng xoá** — chỉ thêm. `cargo metadata
--locked` exit=0 và lockfile **không đổi**.

## 10.5 Kết quả cuối — commit `601448c`, tất cả workflow xanh

| Workflow | Kết quả |
| --- | --- |
| `build` | ✅ **completed / success** |
| ⤷ Rust Guardrails & Functional Tests | ✅ success |
| ⤷ Compact 26.3 Simulation Runtime Pack | ✅ success |
| ⤷ **iOS Swift Tests & Native ARM64 IPA** | ✅ **success** — Xcode 26 + SDK 26, Swift LauncherLogicTests trên Simulator, **build engine ARM64 thật**, đóng gói + audit IPA unsigned và TrollStore |
| ⤷ release | ✅ success |
| `android` → apk | ✅ **success** — build ARM64 `--locked`, đóng gói + ký APK |

**Điểm quan trọng:** job iOS đã **biên dịch `horizon-official` cho `aarch64-apple-ios` thành công**.
Đây là lần đầu code trong request này được biên dịch cho target Apple thật (không chỉ host Linux), và
`gfx/draw_budget.rs` + 9 crate mới đều qua được. Nó **không** chứng minh đường render chạy đúng trên
GPU thật — chỉ chứng minh nó build được và test logic pass.

**Vẫn chưa kiểm chứng được (phải nói thật):**
- Chưa có thiết bị iOS thật: `draw_budget` chưa đo được `gpu_ms` thật, `governor` chưa chạy với số đo
  GPU thật. Logic đã test bằng số giả lập trong unit test.
- `probe()` trong `game/ram.rs` vẫn trả `limit_mib: None` trên mọi nền tảng — **đường đo Jetsam thật
  chưa chạy lần nào**. `horizon-ios` cần gọi `os_proc_available_memory()` + `phys_footprint`.

---

# §11 — Kiểm toán request #4: cơ chế đã thêm có thật sự chạy? (đọc log, không chỉ đọc "success")

Yêu cầu của bạn: *"cần chú ý thêm trong log build action, ko phải bảo build success là ko có gì lỗi cả,
những warn cx rất quan trọng"*. §11 làm đúng việc đó: đọc **cả** 274 dòng log build Actions **và**
9411 dòng log chẩn đoán trên máy thật (iOS 16.7.16, app 0.1.44 build 14.1, A11), rồi đối chiếu từng
cơ chế với bằng chứng "có dòng log / không có dòng log".

## 11.0 Kết luận trước, ngắn

**Build success KHÔNG có nghĩa là cơ chế hoạt động.** Bốn cơ chế của request #2–#3 được nối dây, biên
dịch sạch cho `aarch64-apple-ios`, test logic pass — và **không cái nào chạy trên máy thật**. Cả bốn
đều thất bại theo cùng một kiểu: chúng phụ thuộc một phép đo từ kernel mà **không có dòng code nào
thực hiện phép đo đó**, và bộ biên dịch đã nói ra điều này bằng warning "never used" — đúng loại
warning mà "build success" che mất.

## 11.1 Bằng chứng từ log build (274 dòng, job `Build native ARM64 Rust engine`, 17m01s, success)

| Warning | Vị trí | Nghĩa thật |
| --- | --- | --- |
| `limit_from`, `total_bytes`, `is_empty`, field `measured`, `profile`/`measured`/`limit_mib`/`describe`/`stats` **never used** | `crates/horizon-client/src/game/ram.rs:83,128,153,168,197–307` | **Toàn bộ phần đọc Jetsam là code chết.** Không chỗ nào lấy limit ⇒ `Budget` luôn dùng bảng tier |
| field `tier` `:41`; `level`, `may_relax_ticks`, `reset`, `tier` **never used** | `crates/horizon-client/src/game/thermal.rs:85–179` | Nhiệt độ đo được nhưng **không ai đọc**. Chỉ `describe()`/`action()` được gọi |
| `scene_reset`, `scale` **never used** | `crates/horizon-client-render/src/gfx/draw_budget.rs:138,143` | Draw budget tự chỉnh độ phân giải, **không ai đọc `scale()`** ⇒ không có gì chứng minh nó tác động lên hình |
| `missing [lints] to inherit [workspace.lints]` | `horizon-prepare`, `horizon-theme`, `horizon-bench` | 3 crate **thoát khỏi bộ lint của workspace** — CI vẫn xanh vì chúng không bị kiểm |
| `http_get` never used | `horizon-client-render/src/skins.rs:1803` | Code tải skin qua HTTP chưa nối |

## 11.2 Bằng chứng từ log thiết bị (9411 dòng, 21:24–23:09)

| Cơ chế | Chuỗi kỳ vọng | Thực tế trong log | Kết luận |
| --- | --- | --- | --- |
| Ngân sách RAM | `Horizon: RAM … ceiling … freed N MiB` | **0 lần**. Dòng RAM duy nhất: `Horizon: RAM world closed — freed 0 MiB` (×5) | ❌ |
| Nhiệt độ | `Horizon: thermal …` | **0 lần** (`thermal` 0 hit, `heat` 0 hit) trong 9 giờ chơi | ❌ |
| Draw budget | đổi `scale` khi GPU quá tải | không có log nào, không có accessor | ❌ |
| Máy/SoC | — | `Horizon renderer: Apple A11 GPU` (đúng), nhưng profile nội bộ lại là **A12** (xem 11.3) | ❌ |

Chi tiết đáng chú ý khác:
- `frame generation off` ×8 / `unsupported` ×6 — trên A11 là **đúng thiết kế** (Legacy tier tắt FG),
  không phải lỗi. Nhưng nó được quyết bởi tier… mà tier lại sai (11.3).
- Version skew: launcher ghi `Horizon 0.1.44`, log game ghi `Horizon 0.1.0` — xem 11.5.
- Chỉ 2 dòng ERROR, cả hai vô hại: `Error while loading the narrator: Unsupported platform ios`.

## 11.3 Nguyên nhân gốc (đã xác minh bằng đọc mã, không suy đoán)

**Gốc 1 — không có phép đo Jetsam nào tồn tại.**
`game/ram.rs::probe()` hard-code `available_mib: None`, `limit_mib: None`. Trong toàn bộ
`crates/horizon-threads`, lần gọi `sysctlbyname` **duy nhất** là `hw.perflevel0.physicalcpu`
(`lib.rs:202`); **không có** `os_proc_available_memory`, **không có** `phys_footprint`.
⇒ `Device::ceiling_mib()` luôn rơi về bảng tier, `Budget::frame()` không bao giờ vượt ngưỡng, nên
**không có plan, không có eviction, không có log**. Đây là lý do dòng `RAM world closed — freed 0 MiB`
luôn là `0`: không có gì được giải phóng vì không có gì được yêu cầu giải phóng.

**Gốc 2 — định danh máy đọc từ biến môi trường.**
`ram::hw_machine()` chỉ đọc `HORIZON_HW_MACHINE`, thứ không ai set trên điện thoại ⇒
`DeviceProfile::from_hw_machine("")` ⇒ nhánh cuối `Self::A12`. **Mọi** thiết bị iOS đều bị xếp A12:
A11 thật bị xếp cao hơn thực tế (ngân sách khung hình sai), máy yếu hơn cũng bị xếp A12.

**Gốc 3 — `heat_tick` coi "chưa có gì thay đổi" là "không cần làm gì".**
`Thermal::tick()` trả `false` cho lần quan sát **đầu tiên** (nó chỉ *seed* governor). `heat_tick` thấy
`false` ⇒ `return` **trước cả khi** đọc/tính, và không log. Cộng thêm: chuỗi này chỉ chạy trong
`memory_tick` khi world đã tồn tại ⇒ một session 9 giờ vẫn không có một dòng nhiệt độ nào.

**Gốc 4 — `ram_tick` tính census trước, kiểm tra world sau.**
Census (`world_bytes`, `lod_bytes`, `visible_share`) được tính rồi mới tới nhánh `player.is_none()`,
nên khi không có world nó ghi "world closed" và bỏ luôn phần ngân sách thật.

## 11.4 Đã sửa gì (đúng gốc, không vá ngọn)

1. `crates/horizon-threads/src/process_memory.rs` — thêm phép đo thật, có `SAFETY:` cho từng `unsafe`:
   `footprint_bytes()` (`task_info(TASK_VM_INFO).phys_footprint`),
   `available_memory_bytes()` (`os_proc_available_memory`, iOS 13+),
   `jetsam_limit_bytes()` = `footprint + available` (`available == 0` ⇒ limit = footprint, không ratchet
   về 0), `hw_machine()` (`sysctlbyname("hw.machine")`), `physical_memory_bytes()` (`hw.memsize`).
   Toàn bộ có bản `#[cfg]` rỗng cho nền tảng khác, nên API là toàn phần.
2. `game/ram.rs::probe()` — dùng 5 phép đo trên; `limit_mib` giờ là số thật trên máy.
   `hw_machine()` — `sysctlbyname` trước, biến môi trường chỉ để soak harness giả lập máy.
3. `game/thermal.rs` — thêm `settled()`, `described()`, `note_described()`; `reset()` xoá cờ.
   `game.rs::heat_tick` — không `return` sớm khi governor chưa settle, và **log lần đọc đầu tiên** của
   mỗi session (nếu không, "mát" và "chưa từng đọc" không phân biệt được trong log).
4. `game.rs::ram_tick` — chặn `player.is_none()` **trước** khi tính census, và log `ram.describe()`
   một lần mỗi session (nguồn limit: `measured` hay `tier`).
5. Nối các accessor từng chết vào đúng chỗ dùng: overlay F3 giờ in `RAM … ceiling`, `RAM plan: N MiB
   over M plan(s)`, `Thermal: <level> tier (<name>)`, `Draw resolution: N%`; entry point log
   `Horizon: device iPhone…, RAM … MiB, jetsam limit … MiB` ngay khi mở log.
6. `gfx.rs::set_thermal` — chuyển `frame_budget_ms` của governor nhiệt xuống `draw_budget.set_target()`
   (chip nóng ⇒ ngân sách khung hình ngắn hơn ⇒ mục tiêu GPU thật), và `invalidate_presentation()` gọi
   `draw_budget.scene_reset()`.
7. Overlay `memory` trên iOS: `ProcessMemory::read()` đọc `/proc` — **không tồn tại trên iOS**, nên
   entry này **trống trên máy**. Thêm `debug_overlay::jetsam_lines()` dùng chính hai số jetsam.

## 11.5 Phát hiện thêm, chưa sửa (nói thật)

- **Version skew** `0.1.44` vs `0.1.0`: `crash::version()` = `RELEASE` (`option_env!("HORIZON_VERSION")`)
  hoặc `CARGO_PKG_VERSION`. `.github/workflows/build.yml` (luồng build IPA này) **không** set
  `HORIZON_VERSION`; chỉ `game-build.yml` set. ⇒ bản IPA 0.1.44 thực chất là crate `0.1.0` và tự khai
  là `0.1.0`. Đây là mùi của nguyên tắc **không fake version** và cần một quyết định riêng (truyền
  `HORIZON_VERSION` vào job IPA, hay để bản dev tự nhận là dev). **Chưa sửa** vì nó đổi số version
  của artifact — cần bạn đồng ý.
- `missing [lints] to inherit [workspace.lints]` ở 3 crate: chưa thêm, vì bật bộ lint workspace có thể
  làm đỏ CI với hàng loạt cảnh báo có sẵn. Đề xuất làm thành một PR riêng.
- 74 lỗi `cargo check -p horizon-client --all-targets` (test lib của crate) là **có sẵn từ trước**,
  không do §11 (đã đối chiếu: 75 → 74, lỗi còn lại đều ở `motion.rs`/test cũ). CI không chạy target này.
- 4 test host hỏng có sẵn: `gfx::hdr` (2), `chat_field_budgets_utf16`, `extras::mobile_ui`;
  `cargo test -p horizon-threads` 13 lỗi ở `profile`/`sampler` (đo CPU, không liên quan).

## 11.6 Giới hạn — điều gì đã kiểm chứng, điều gì chưa

**Đã kiểm chứng:**
- `cargo check -p horizon-client` (host, `--no-default-features`): **0 error**.
- `cargo check -p horizon-client-render --all-targets`: **0 error**, còn đúng 1 warning (`http_get`).
- Dead-code của `ram.rs`/`thermal.rs`/`draw_budget.rs` **đã biến mất** khỏi danh sách warning; chỉ còn
  `ride_is_tamed`, `hud_editor_drag_target`, `used_mouse`, `http_get` (không liên quan request này).
- `cargo test -p horizon-frame-sched`: **21/21 pass** (kể cả `DeviceTier::name` mới).
- `cargo test -p horizon-client-render --lib`: 135 test, chỉ 4 lỗi **có sẵn** như trên.

**CHƯA kiểm chứng (phải nói rõ):**
- **Chưa chạy trên máy thật.** `phys_footprint`/`os_proc_available_memory`/`hw.machine` chỉ được biên
  dịch và kiểm tra bằng đọc mã; nhánh `unsafe` **chưa được thực thi lần nào** trong lần sửa này.
  Bằng chứng duy nhất sẽ là log thiết bị lần tới, phải có hai dòng:
  `Horizon: device iPhone…` và `Horizon: RAM … ceiling (measured…`.
- Chưa build cho `aarch64-apple-ios` sau khi sửa (sẽ push để Actions làm — bước tiếp theo).
- Test hồi quy mới (`a_device_that_reports_its_counters_gets_a_real_ceiling`,
  `a_reported_machine_identifier_selects_its_own_tier`,
  `the_first_poll_counts_as_unsettled_so_it_is_reported`) nằm trong `--lib` test của `horizon-client`,
  target **vốn không compile được từ trước** (74 lỗi cũ) ⇒ chưa chạy được. Đã kiểm bằng type-check:
  chúng không thêm lỗi nào (75 → 74 sau khi sửa `Budget::new` → `Budget::for_device`).

## 11.7 Kết quả CI cho các sửa của §11 — và một lỗi mà "build success" ở host đã che

### Lần push 1 — `b02651b` → **FAILED** (đúng loại lỗi bạn cảnh báo)

`cargo check` trên host Linux: **0 error**. Actions, job `Build native ARM64 Rust engine`,
target `aarch64-apple-ios`: **4 error**.

```
error[E0425]: cannot find type `task_vm_info` in crate `libc`        (x2, process_memory.rs:104,107)
error[E0425]: cannot find function `mach_task_self` in crate `libc`  (process_memory.rs:114)
error[E0308]: mismatched types — expected `u32`, found `i32`         (TASK_VM_INFO flavour)
warning: unused doc comment                                          (process_memory.rs:141)
```

Nguyên nhân: `libc 0.2.190` gắn `target_os = "macos"` cho **cả** struct `task_vm_info` **và**
`mach_task_self()`. Trên iOS hai thứ đó **không tồn tại trong crate**, dù libSystem có thật.
Đây là kiểu lỗi mà host không bao giờ thấy: nhánh `#[cfg(target_vendor = "apple")]` không được
biên dịch trên Linux, nên "0 error ở máy" là **vô nghĩa** với target Apple.

**Đã sửa đúng gốc**, không đoán: tải `osfmk/mach/task_info.h` từ `apple-oss-distributions/xnu`,
đọc layout thật —
`TASK_VM_INFO = 22`, `phys_footprint` là field **thêm ở rev1**, và chính header tự xác nhận phép
tính: `TASK_VM_INFO_REV0_COUNT = 2+1+1+2+2+28 = 36` natural_t word ⇒ rev1 đặt `phys_footprint` tại
**byte 144**, kích thước buffer **152 byte = 38 word**. `task_info` trả về **ít word hơn** trên
kernel trước rev1 ⇒ nhánh đó **trả `None`** thay vì đọc field chưa khởi tạo và coi đó là số đo.

Để CI Linux kiểm được layout (nơi không có Apple SDK), layout được đưa ra module
`task_vm_info_layout` + test khoá `144 / 152 / 38`. **Test này đã bắt hai lỗi số học của tôi**
(đầu tiên tôi viết 160, rồi 148) trước khi push — tức là nó có giá trị thật, không phải test trang trí.

### Lần push 2 — `cf73b92` → **SUCCESS**, và đây là phần quan trọng: **warning**

| Job | Kết quả |
| --- | --- |
| Rust Guardrails & Functional Tests | ✅ success |
| Compact 26.3 Simulation Runtime Pack | ✅ success |
| iOS Swift Tests & Native ARM64 IPA | ✅ **success** — `Finished release profile in 16m 35s`, IPA + audit + upload OK |
| release | ✅ success |

**Warning trong log Apple — so sánh trước/sau (đây mới là bằng chứng cơ chế đã được nối):**

| Crate | Trước (§11.1) | Sau | Ý nghĩa |
| --- | --- | --- | --- |
| `horizon-client` | **11 warning** | **4** | 7 warning của `ram.rs`/`thermal.rs` **đã biến mất** ⇒ code có đường tới nó |
| `horizon-client-render` | **2** | **1** | `draw_budget::{scale, scene_reset}` **đã được dùng** |
| `horizon-threads` | — | **0** | 5 phép đo mới compile sạch, không warning |

Warning còn lại trong `horizon-client` (4): `ride_is_tamed`, `hud_editor_drag_target`,
`native_recipe_lookup`, `recipe_lookups_now` — **không liên quan** request #2–#4, chưa sửa.
`horizon-client-render` (1): `http_get` — code tải skin qua HTTP chưa nối.

**3 warning `missing [lints] to inherit [workspace.lints]`** (`horizon-prepare`, `horizon-theme`,
`horizon-bench`): đã thêm `[lints] workspace = true` cho cả ba. `cargo check` lại: **0 warning**,
không phát sinh warning mới. (Trước khi sửa, ba crate này **thoát** bộ lint workspace — nghĩa là
CI xanh một phần vì chúng không bị kiểm.)

### Điều vẫn CHƯA chứng minh được

Build xanh + warning giảm **chứng minh code có đường tới nó và compile đúng trên target Apple**.
Nó **không** chứng minh `task_info`/`os_proc_available_memory`/`sysctlbyname` trả số đúng trên máy.
Bằng chứng duy nhất cho việc đó là **log thiết bị lần chạy tới**, phải chứa đúng hai dòng:

```
Horizon: device iPhone10,x (…); RAM 3072 MiB, jetsam limit ~1400 MiB
Horizon: RAM 900 MiB ceiling (measured, jetsam ~1400 MiB), peak …
Horizon: nominal (…)
```

Nếu `jetsam limit None` xuất hiện trên máy thật thì nghĩa là `task_info` trả thiếu word hoặc
`os_proc_available_memory` bị chặn — lúc đó phải đọc lại §11.3 chứ không được đoán.

### Lần push 3 — `5a7ef88` → **SUCCESS**, warning `[lints]` đã hết

| Job | Kết quả |
| --- | --- |
| Rust Guardrails & Functional Tests | ✅ success |
| Compact 26.3 Simulation Runtime Pack | ✅ success |
| iOS Swift Tests & Native ARM64 IPA | ✅ success — `Finished release profile in 15m 14s` |
| release | ✅ success |

**Warning trong log Apple sau khi thêm `[lints]`:**

```
horizon-client        (lib) generated 4 warnings
horizon-client-render (lib) generated 1 warning
```

Không còn dòng `missing [lints] to inherit [workspace.lints]` nào. Năm warning còn lại đều **không
liên quan** request #2–#4 và **có sẵn từ trước**: `ride_is_tamed`, `hud_editor_drag_target`,
`native_recipe_lookup`, `recipe_lookups_now`, `http_get`.

**Con số để so sánh:** trước request #4, log Apple có **13 warning dead-code** của đúng các module
này + **3 warning `[lints]`** = 16 vấn đề. Sau: **5**, và không cái nào thuộc request này.

### Bài học ghi vào quy trình

`cargo check` trên host Linux cho ra **0 error** trong khi target `aarch64-apple-ios` có **4 error**.
Lý do: nhánh `#[cfg(target_vendor = "apple")]` **không được biên dịch** trên Linux, nên kết quả
"xanh ở máy" không nói gì về Apple. Từ nay, với bất kỳ thay đổi nào trong nhánh Apple:
1. phần **không phụ thuộc SDK** phải được đưa ra chỗ CI Linux kiểm được (như `task_vm_info_layout`),
   kèm test khoá offset/kích thước;
2. khẳng định "đã sửa xong" chỉ được đưa ra **sau khi job iOS xanh**, không phải sau `cargo check`.

---

# §12 — Request #5: bạn đã chạy bản đó. Log nói gì, và vì sao "hiệu năng không khác gì"

Cảm ơn bạn đã gửi `Horizon-diagnostics (6).txt` — nó trả lời dứt khoát ba câu hỏi khác nhau, và
câu trả lời **không** giống nhau cho cả ba.

## 12.1 Cơ chế đã chạy thật (đây là tin tốt, có bằng chứng)

```
[11:42:24] Horizon: device iPhone10,5 (DeviceProfile { tier: Legacy, soc: Some(A11),
           promotion: false, physical_mib: 3072, available_mib: Some(1200) });
           RAM Some(2973) MiB, jetsam limit Some(2098) MiB
[11:40:45] Horizon: RAM 1363 MiB ceiling (measured, jetsam 2098 MiB), peak 8%, 0 warning(s)
```

Ba nhánh `unsafe` của §11 — `task_info(TASK_VM_INFO).phys_footprint`,
`os_proc_available_memory()`, `sysctlbyname("hw.machine")` — **đã chạy và trả số thật**:
- `hw.machine` = **`iPhone10,5`** ⇒ `A11` ⇒ `tier: Legacy`. Trước §11 nó rơi về `A12` cho mọi máy.
- `phys_footprint + available` = **2098 MiB** đúng bằng iPhone X-class jetsam (nghiên cứu §9 dự đoán
  ~2.1 GiB cho máy 3 GB, thực đo 2098).
- Trần làm việc 1363 MiB = 2098 − 700 reserve của Legacy. Đỉnh dùng **8 %**.

**Nhưng đó cũng chính là lý do hiệu năng không đổi:** đỉnh 8 % nghĩa là game **chưa bao giờ** chạm
trần RAM. Cơ chế RAM đúng, chỉ là nó không phải nút thắt trên A11. Cùng lý do với nhiệt độ: máy ở
`nominal` suốt. Bốn cơ chế "quản lý toàn diện" hiện **không có gì để cắt** trên máy này.

## 12.2 Lỗi tôi gây ra ở §11, log bắt được: spam 131 dòng

```
Horizon: thermal nominal (100% resolution, 65% particles, 32 chunks)   × 131 trong 40 giây
```

Nguyên nhân: `Thermal::tick` **vừa** ghi `announced` **vừa** trả "có thay đổi", nên `take_change()`
luôn trả `None`; `heat_tick` lại log khi `!described()`, và `take_change` không bao giờ đánh dấu
⇒ log mỗi 2 giây, mãi mãi. Đã sửa: `tick` sở hữu đúng một lần chuyển trạng thái, `take_change` sở hữu
`announced`, và test `a_level_is_announced_once_and_then_left_alone` khoá lại.

## 12.3 Nhiệt độ: **chưa từng đọc được trên máy** — đã sửa

`platform_level()` chỉ đọc biến môi trường `HORIZON_THERMAL`, thứ không ai set trên điện thoại.
`horizon-ios-launcher::thermal_state()` có tồn tại nhưng **không có một dòng nào gọi**
`set_thermal_state` trong toàn bộ repo, và phía Swift (`packaging/ios/Launcher`) **không có** chỗ nào
nghe `thermalStateDidChangeNotification`. Nghĩa là: 131 dòng "nominal" kia là **hằng số giả**, không
phải số đo. Nhiệt độ chưa bao giờ là tín hiệu thật.

Đã sửa (3 lớp):
1. `horizon-ios-launcher`: export `horizon_thermal_state(i32)` (`#[no_mangle]`, chỉ iOS).
2. `Bridge.swift`: `Thermal.watch()` — đọc `ProcessInfo.thermalState` một lần và nghe notification,
   đẩy sang Rust; gọi từ `horizonLauncherPresent`.
3. `game/thermal.rs::platform_level()`: `thermal_state()` là nguồn thật; env chỉ để QA.
   Thêm chống nhấp nháy: mức **xấu hơn** phải sống qua 1 lần poll mới được cắt chất lượng
   (hạ res là hướng đắt tiền: một lần tạo lại target), mức tốt hơn thì tin ngay.

## 12.4 Hiệu năng: nút thắt thật không nằm ở bốn cơ chế đó

Log 5 phút này **không có một dòng** `pipelines ready`, `frame generation`, `first frame`,
`GPU over budget`, `Draw resolution`. Bundle là `Game current` chỉ 2 dòng, nên **không thể** kết luận
gì về FPS từ nó — nói thẳng: **tôi chưa có số đo FPS/frame time nào của bạn**. Muốn chẩn đoán hiệu
năng thì cần một bundle có phần "Game current" đầy đủ (chơi ≥ 60 giây rồi mới xuất).

Điều đọc được từ mã, và nó là nút thắt thật trên A11:
- `budget = (count / 8).max(15)` — số section được vẽ lại mỗi frame, **không phụ thuộc GPU**.
- `draw_budget` chỉ cắt **đuôi** danh sách solid khi `gpu_ms` vượt ngân sách. Trên iOS điều kiện tiên
  quyết là `TIMESTAMP_QUERY`; nếu adapter Metal không cấp, `gpu_timer = None`,
  `gpu_ms = None`, governor **không bao giờ** chạy ⇒ draw budget thành số trang trí. Đây là giả thuyết
  số 1 cho "hiệu năng không cải thiện", và log lần này không đủ để xác nhận (thiếu dòng
  `pipelines ready`). Dòng đó **đã có sẵn** trong `gfx.rs:4966` và in đúng
  `GPU timer on|off`, nên chỉ cần một bundle có phần "Game current" đầy đủ là biết ngay. Nếu là `off`
  thì `draw_budget` không có số đo và **không bao giờ** cắt gì — đó là lỗi phải sửa, không phải tinh chỉnh.

## 12.5 Fabric: câu trả lời trung thực, và phần tôi đã làm được

Yêu cầu của bạn là "port cho mod Fabric hoạt động". Tôi phải nói thẳng ranh giới kỹ thuật, vì hứa sai
sẽ tệ hơn: **Horizon là build Rust native, không có JVM và không có class Java của game.** Trong toàn
bộ repo, porter là **carry nội dung, không phải chạy code** — `carry.rs` tự ghi: *"Files are copied,
never code"*. Vì vậy:

| Loại mod Fabric | Kết quả thật trên Horizon |
| --- | --- |
| Chỉ `assets/` + `data/` (recipe, loot, tag, model, texture, lang) | **Chạy được**, porter carry sang content mod |
| Thêm block/item khai báo bằng JSON | **Chạy được** (overlay registry, `data/<ns>/horizon/`) |
| Có `mixins` (Sodium, Lithium, Iris, hầu hết mod tối ưu & mod cơ chế) | **Không thể chạy code**. Nội dung vẫn port, hành vi thì không |
| Có `accessWidener` | Như trên |
| Event/keybind/HUD bằng Java | Chỉ chạy nếu mod được viết lại thành **native mod** (Rust mixin bundle) — đó là con đường chính thức của Horizon (`docs/native-mods.md`) |

Vì vậy "port Fabric" theo nghĩa **chạy được mọi mod Fabric** là điều không thể làm đúng; bất kỳ ai
hứa điều đó là đang fake. Cái **làm được thật** và tôi đã làm ở vòng này:

**`crates/horizon-mod-shim`: `FabricDeclaration`** — đọc chính `fabric.mod.json` của jar và trả lời
trước khi cài, thay vì để người chơi đoán:
- `entrypoints` (`main`/`client`/`server`/`modmenu`) → loader point tương ứng;
- `depends`/`suggests` → các module Fabric API mà **code sẽ gọi** (`fabric-key-binding-api-v1`,
  `fabric-lifecycle-events-v1`, …) ⇒ chấm theo bảng `TABLE` (13 native / 22 adapted / 8 absent);
- `mixins`, `accessWidener` → **absent by construction**, và `needs_class_patching()`;
- `verdict(&coverage, carries_content)` tách hai lớp: **nội dung** (porter quyết) và **hành vi**
  (bảng shim quyết) — mod có mixin + có nội dung ⇒ `"partly"`, không có nội dung ⇒ `"no"`;
- `summary()` nói bằng lời người chơi: cái gì port, cái gì không, vì sao.

**17/17 test pass** trên crate này.

**Giới hạn (nói thật):** `FabricDeclaration` hiện là **thư viện đã test**, chưa được launcher gọi.
Bước tiếp theo cần nối vào `launcher_mods::port_jar` để verdict hiện trong app — tôi chưa làm vì nó
đụng luồng cài đặt (một thay đổi lớn hơn, và cần bạn xác nhận hướng đi trước).
