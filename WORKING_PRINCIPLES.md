# WORKING_PRINCIPLES.md — bộ nguyên tắc vĩnh viễn của workspace

> File này là **system prompt thường trực**. Mỗi request, trước khi làm bất cứ điều gì, phải đọc lại file này.
> Bản sao thứ hai nằm trong repo: `Vertical-ios-all-versions/WORKING_PRINCIPLES.md`.

## 0. Tư duy bắt buộc (áp dụng cho MỌI request)

1. **Đọc hết trước, sửa sau.** Không đoán. Không "chắc là". Trước khi sửa một hành vi, phải đọc *toàn bộ*
   đường đi của nó: file định nghĩa → file gọi → file test → file cấu hình. Nếu chưa đọc đủ thì **chưa được viết code**.
2. **Không tin 100% việc mình (hoặc request trước) đã làm.** Mỗi lần quay lại một vùng code cũ: rà lại từ đầu,
   giả định là nó *sai* cho tới khi chứng minh được nó đúng. Lỗi "trượt dài" sinh ra từ việc tin vào kết luận cũ.
3. **Đa nghi có kỷ luật.** Luôn hỏi: "Tại sao A = B? Liệu B có bằng C? A thực sự chạy như thế nào? Nếu giả thuyết này
   sai thì bằng chứng nào sẽ xuất hiện?" → rồi đi tìm bằng chứng đó trong code/log/benchmark.
4. **Không biết thì tra.** Tìm trên mạng (piston-meta, Modrinth, manga hướng dẫn Apple Metal, tài liệu wgpu/winit,
   wiki.vg), đọc nhiều mã nguồn khác nhau. Trích nguồn khi kết luận.
5. **Gặp khó không bỏ.** Đổi cách tiếp cận (grep khác, decompile khác, viết test nhỏ để cô lập) cho tới khi xong.
6. **Nói thật về giới hạn.** Không có môi trường build được (macOS/Xcode/thiết bị thật) thì phải nói rõ cái gì đã
   biên dịch/chạy test được, cái gì chỉ là thiết kế đã kiểm chứng bằng lý luận + đọc code.

## 1. Quy trình chuẩn cho một task sửa lỗi trong repo này

1. `git clone` / `git status` / `git log --stat` — biết mình đang ở commit nào, request trước đã đụng file nào.
2. Đọc `AGENTS.md`, `docs/ios/*.md` liên quan, và **toàn bộ** file sẽ sửa (không đọc lướt bằng grep rồi sửa).
3. Viết lại bằng chứng: "triệu chứng người dùng thấy" → "dòng code nào tạo ra nó" (file:line).
4. Kiểm chứng bằng `cargo check`/`cargo test` cho crate bị đụng; nếu môi trường không build được toàn bộ thì cô lập
   phần logic ra crate nhỏ, std-only, test được.
5. Viết test hồi quy *trước hoặc cùng lúc* với fix. Test phải fail nếu fix bị revert.
6. Ghi lại kết quả + rủi ro còn lại trong `docs/vi/` của repo.

## 2. Nguyên tắc kỹ thuật riêng cho port iOS này

- **Không fake version.** Một version được coi là "playable" chỉ khi simulation tables của nó được sinh từ chính jar
  của version đó (hoặc adapter đã kiểm chứng), không phải hardlink từ base 26.3 rồi đổi nhãn.
- **Không chặn main thread.** Bất cứ thứ gì >2 ms (giải nén, mesh, I/O, decode PNG) phải ra worker/thread riêng,
  main thread chỉ nhận kết quả qua kênh.
- **Ngân sách frame là luật.** Mỗi hệ thống (mesh, particle, entity, UI) có budget riêng; vượt budget thì *giảm chất
  lượng*, không được *trễ frame*.
- **Bộ nhớ là tài nguyên hữu hạn.** iOS giết app bằng Jetsam, không có quota. Mọi cache phải có trần byte và chính sách
  xả theo vùng-nhìn-thấy (visibility) + LRU, có hysteresis để không rung.
- **Chip Apple nóng là mất hiệu năng.** Thermal state phải làm *giảm* tải trước khi OS bóp xung: hạ dynamic resolution,
  tắt frame-gen, giảm particle, giãn tick phụ.
- **Touch là input hạng nhất**, không phải chuột giả lập: phải có coalescing, prediction, smoothing có thể tắt,
  và phân loại tap/hold/drag rõ ràng.

## 3. Nhật ký thực thi (append mỗi request)

- **2026-11-20 — request #1**: clone `LLOS-Lord/Vertical-ios-all-versions` (commit `ec84e17`), đọc
  `crates/horizon-client/src/{versions,launcher_mods,game/touch}.rs`, `crates/horizon-client-render/src/{touch,frame_gen}.rs`,
  `game/mods*`, `packaging/ios/Launcher/*.swift`. Kết quả: xem `docs/vi/PHAN-TICH-VA-FIX.md` +
  các crate mới `horizon-frame-sched`, `horizon-memory-governor`, `horizon-version-truth`,
  `horizon-mod-diagnostics`, `horizon-touch-pipeline`, `horizon-visibility-budget`.
- **2026-11-21 — request #2 (bốn yêu cầu lớn: đa luồng theo mức quan trọng / quản lý RAM / port mod /
  chặn GPU quá tải)**: đọc `horizon-visibility-budget/src/lib.rs`, `horizon-frame-sched/src/*.rs`,
  `horizon-threads/src/{jobs,process_memory}.rs`, `horizon-memory-governor/src/lib.rs`,
  `horizon-hooks/src/porter.rs`, `horizon-client/src/{game.rs,mod_api.rs,client_lib.rs}`,
  `horizon-client-render/src/gfx/{cull,census,visbuffer,device_probe}.rs`, `gfx.rs`
  (`occlusion_culling_requested`, `TILED_MIN_SECTIONS`, submit path).

  **Phát hiện cốt lõi (khác giả định ban đầu):** `horizon-visibility-budget` và
  `horizon-frame-sched::budgets` vẫn là **crate chết**, và vấn đề thật không phải "thiếu scheduler"
  mà là **bốn thang ưu tiên không biết nhau** (`Sort`, `FrameKind`, `jobs::Class`,
  `memory_governor::Class`) với bốn cách chia khoảng khác nhau, không chỗ nào chuyển đổi. Vì vậy
  việc đúng là viết **lớp điều phối**, không viết thêm scheduler thứ năm.

  Ba crate mới, std-only, `#![forbid(unsafe_code)]`, test đầy đủ:
  `horizon-importance` (**16/16**), `horizon-gpu-governor` (**22/22**), `horizon-mod-shim`
  (**12/12**) = **50 test pass**.

  Sửa gốc RAM: `game/ram.rs` viết lại — trần Jetsam suy từ `footprint + os_proc_available_memory()`
  tại runtime thay cho bảng tier chip (nghiên cứu: giới hạn Jetsam *không* suy từ RAM lắp máy).
  `game/thermal.rs` viết lại theo đúng API `game.rs` gọi.

  **Sự cố môi trường:** sandbox reset **hai lần** trong request này — xoá toolchain Rust (cài lại
  1.95.0 vì workspace yêu cầu) và prune toàn bộ 482 file `crates/horizon-client/src/game/*.rs`
  (phục hồi 637 file từ clone sạch `ec84e17`, `ram.rs`/`thermal.rs` viết lại thủ công).
  Đã xác nhận `horizon-importance`/`gpu-governor`/`mod-shim` + `game.rs` edits còn nguyên.

  Chi tiết + bảng nguồn: `docs/vi/PHAN-TICH-VA-FIX.md` **§9**.

### 2026-11-20 — request #4 (kiểm toán: "build success ≠ không có lỗi")

**Bài học, ghi thành luật:** một cơ chế chỉ được coi là "đã có" khi **log trên máy thật** chứng minh
nó chạy. Biên dịch được + test logic pass + CI xanh **không** phải bằng chứng. Warning `never used` /
`never read` trong log build là **lỗi chức năng**, không phải tiếng ồn: nó nói rằng thứ vừa viết ra
không có đường nào tới nó.

Kiểm toán request #2–#3 trên log thật cho kết quả: **0/4 cơ chế hoạt động**.
- RAM budget: không có phép đo Jetsam nào tồn tại trong cây ⇒ `limit_mib` luôn `None`; log chỉ có
  `RAM world closed — freed 0 MiB` (×5), còn dòng ngân sách **0 lần**.
- Nhiệt độ: `thermal` **0 hit** trong 9 giờ chơi; `heat_tick` return sớm vì lần poll đầu không phải
  "thay đổi".
- Draw budget: `scale()`/`scene_reset()` chết ⇒ không chứng minh được nó tác động lên hình.
- SoC: `hw.machine` đọc từ **biến môi trường** ⇒ mọi máy iOS bị xếp **A12** (A11 thật bị xếp sai).

Sửa gốc đã làm: thêm 5 phép đo kernel trong `horizon-threads::process_memory`
(`phys_footprint`, `os_proc_available_memory`, `jetsam_limit = footprint + available`, `hw.machine`,
`hw.memsize`), nối `Budget`/`Thermal`/`DrawBudget` vào đúng chỗ, log lần đọc đầu tiên mỗi session,
overlay F3 hiện số thật, và `memory` entry không còn trống trên iOS (nó đọc `/proc` trước đây).

Chi tiết + bảng bằng chứng + danh sách **giới hạn chưa kiểm chứng**: `docs/vi/PHAN-TICH-VA-FIX.md` **§11**.
