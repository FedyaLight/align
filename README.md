# Align — кроссплатформенный синхронизатор медиа на Rust + GPUI

Cargo workspace с 4 крейтами (`crates/`). Статус: **sync + метаданные +
XML + экспорт + drift-render + CLI + GPUI + metadata/LTC timecode, параллелизм
×4 с бит-идентичными результатами, cooperative cancel, sequence picker
+ сохранённые результаты всех sequences и совместный XML/FCPXML/AAF export
+ Common → current sequence → track наследование всех sync-настроек
+ per-track Preserve basic editing для фиксированных cuts/trims/duplicates/gaps
+ сохранение исправленных media paths в отдельную копию XML/FCPXML/AAF
+ отдельные Final Cut timeline/multicam outputs и независимые labels для
  synchronized/unsynchronized clips
+ форматы + drag&drop в GUI, системные меню с шорткатами, единый
тулбар, timeline preview с линейкой/зумом/навигацией/V-A дорожками,
контекстные меню у курсора, export sheet, quit-time cleanup следов**
(сохранённые Path Fixer redirections применяются автоматически и редактируются
в `Align → Path Fixer…`, там же задаются игнорируемые расширения и через
`Save Fixed Copy…` сохраняется отдельная копия XML/FCPXML/AAF с исправленными
media paths; неразрешённые timeline media можно выбрать через `Relink…` в GUI;
`File → Analysis Cache` очищает текущий проект или весь cache и сохраняет
срок хранения 7/30/90 дней либо до ручной очистки;
custom sequence name, synchronized/unsynchronized symbol, XML color и
Final Cut audio role доступны в Export)
(Swift-реализация удалена; исследование и история — в `work/`).
Rust edition 2024 / `cargo fmt` чист, тесты зелёные,
`clippy -D warnings` чист, GUI висит idle на 0% CPU.

## Карта портирования (Swift -> Rust)

| Swift (Apple-only) | Rust (Win/macOS/Linux) | Статус |
|---|---|---|
| `Model.swift` | `align-core/src/model.rs` — те же поля, serde-ключи = Swift JSON (`clipID`, `sourceIn`…) | ✅ |
| `Fingerprint.swift` (Accelerate/vDSP FFT, Hann) | `fingerprint.rs` (realfft, тот же frame 1024/hop 512, bands, top-2, score ≥ 2.5, deltas [8,20,36]) | ✅ |
| `GCCPHAT.swift` (vDSP DFT) | `gccphat.rs` (rustfft, те же пороги 16k/128k, peak/RMS ≥ 4, prominence ≥ 1.015, параболическая интерполяция) | ✅ |
| `FingerprintCache.swift` (CryptoKit SHA256 + plist) | `cache.rs` (BLAKE3 + bincode, v6 + media identity/тег движка, project-only clear, retention, head+tail 1 МБ, atomic write) | ✅ |
| `AudioDecoder.swift` (AVAssetReader + AVAudioConverter, gate, hysteresis 0.9) | `decode/mono.rs` + `sym.rs` + `ff.rs` + `apple.rs` за `MediaBackend` (adaptive channel + явный multichannel mix, выбор тира до первого сэмпла) | ✅ |
| `inspect` (AVURLAsset, AVSampleCursor VFR, Sony meta, BWF) | `align-decode`: ffprobe/AVFoundation timing + Sony + BWF/iXML + LTC-audio | ✅ |
| `FingerprintMatcher` + `MatchGraph` + `FineMatcher` + drift/piecewise | `align-core`: coarse/fine/graph + per-track Linear/Takes, threshold и strict clip order, drift 10 мин/50 якорей | ✅ |
| `TimelineExport` + OTIO/Premiere/FCP XML writers + drift-render WAV | `align-core` (quick-xml + serde_json) + `align-decode` (hound + bext writer, 32-bit float) | ✅ |
| `FingerprintMatcher` + `MatchGraph` + constraints | `matcher.rs` + `graph.rs` (те же 12 голосов, margin 1.35, confidence blend, IRLS×4, пороги 0.55/3/3/0.1) | ✅ |
| `PiecewiseTimeMapping` + `TimelineTrackAllocator` | `piecewise.rs` + `allocator.rs` (tolerance 0.0005/0.005, обе фазы) | ✅ |
| `FineMatcher` (multi-window GCC-PHAT, drift rate, piecewise rescue, forest) | `fine.rs` за `WindowProvider`-трейтом (окна 16 кГц ≤ 8.3 с, residual 12/6 мс, цикл-guard 0.85) | ✅ |
| `AudioDecoder` + `MonoSampleRateConverter` + inspect | `decode/mono.rs` (hysteresis 0.9, rubato SincFixed, group-delay compensation) + `decode/sym.rs` (Symphonia) + `decode/ff.rs` (ffprobe/pipe) за `MediaBackend` | ✅ |
| `SyncEngine` orchestration | `decode/pipeline.rs` (expand/inspect/fingerprint/match/refine/solve, канонический ClipID) | ✅ |
| AppleNative движок (AVAssetReader → discrete channels → общий rubato) | `decode/apple.rs` (conformance: inspect равен, 8 кГц бит-в-бит, окна по GCC, e2e тот же остров) | ✅ |
| BWF/RF64/BW64 + `link`-сеты, Sony tail, timecode-строки | `core/meta.rs` (UTC вместо local zone — только дельты влияют на матчинг) | ✅ |
| Spanned + Timecode матчеры | `core/spanned.rs`, `core/timecode.rs` (timecode-рёбра вне solve — паритет Swift, для упорядочивания островов) | ✅ |
| VFR classify/canonical + оба walk | `core/timing.rs`, ffprobe packet-walk, AVSampleCursor-walk | ✅ |
| FCP7 + FCPXML импорт (cuts/retime/transitions/links/relink/resolve) | `core/xml.rs` (verbatim payload round-trip, DOM-спэны) | ✅ |
| `AlignCLI/main.swift` | `align-cli` (`sync/export/export-json`, прогресс в stderr, exit 2/1) | ✅ |
| `TimelineExport` + OTIO/Premiere/FCPXML/precision-script | `core/export/` (модель, 3 врайтера, скрипт дословно) | ✅ |
| `AudioDriftCorrector` + `ChannelRenderer` + `WAVMetadataPreserver` | `decode/render.rs` (посегментный sinc, exact-length, int32-stems) + `core/wav.rs` + `core/drift.rs` | ✅ |
| `AlignApp` SwiftUI/AppKit (AppModel 844 строки, ContentView, TimelinePreview Canvas, ExportSheet) | `align-gpui` (GPUI 0.2: DropZone/SourceList/TimelineCanvas/ExportSheet entities) | ✅ |

## Гибридная архитектура: нативная обработка на Apple, общий код везде

На macOS обработка идёт через нативный стек (AVFoundation + AVAudioConverter
через `objc2-av-foundation` из Rust — без Swift-тулчейна), на
Windows/Linux — через portable движок (Symphonia + FFmpeg sidecar + rubato).
Шов — один трейт `MediaBackend` (`align-decode/src/backend.rs`):
`inspect` / `decode_mono_8k` / `decode_window_16k` с теми же контрактами,
что Swift (гейт 4 ридера, блоки 32768, hysteresis 0.9, quality `.max`).

Общее для всех ОС: матчер, граф, drift/piecewise, экспорт, DSP
(fingerprint + GCC-PHAT на `realfft`/`rustfft`), кэш-формат, UI на GPUI.
FFT специально НЕ раздваивается: 1024-pt FFT — микросекунды, декод тяжелее
на порядки, а общий DSP даёт бит-идентичные фингерпринты и сравнимые SHA.

- Выбор: на macOS по умолчанию AppleNative, `ALIGN_BACKEND=portable`
  (он же `pure-rust`) форсирует portable — для CI-проверок детерминизма.
- Кэш v5 изолирован тегом движка (`portable1` / `apple1`): записи разных
  движков не пересекаются ни по имени, ни по payload.
- ClipID остаётся SHA256 (идентичен Swift при том же duration).
- Почему не Swift-dylib по FFI: два тулчейна, Swift runtime в процессе,
  portable бэкенд всё равно нужен для Win/Linux. Rust + objc2 = один
  `cargo build` везде.

- **Декод:** Symphonia = ноль системных зависимостей для аудио; FFmpeg sidecar
  (отдельный процесс с bundled бинарником) для видеоконтейнеров — вместо
  линковки `ffmpeg-next` под 3 ОС и адского matrix CI. Падение ffmpeg не
  роняет основной процесс, память изолирована ОС.
- **FFmpeg sidecars:** release CI и macOS package собирают FFmpeg 9.0.1
  через `script/build-ffmpeg-minimal.sh`: file/pipe, монтажные контейнеры,
  аудиодекодеры и видеодекодеры для метаданных AAF. Network/devices отключены.
  На arm64 macOS бинарники занимают 6.3 + 6.1 МБ и используют только системные
  библиотеки. В macOS package они входят по умолчанию вместе с лицензией.
- **DSP на CPU, не на GPU:** 8 кГц mono FFT 1024 — это микросекунды на CPU;
  гонять через Metal/DX/Vulkan = PCIe-трансфер + wake GPU ради ничего.
  GPU (Blade в GPUI) только композитит UI: Metal/macOS, DirectX/Windows,
  Vulkan/Linux.
- **Только звук, видео в память не грузится никогда.** Инвариант зашит в
  трейт `MediaBackend`: методов, возвращающих видеосэмплы, нет в принципе —
  только mono `f32` (`decode_*`) и скалярные метаданные (`ProbeReport`).
  Контейнеры открываются ради аудиодорожек и таймингов, как в Swift
  (`AVAssetReaderTrackOutput` только на audio, `AVSampleCursor` без чтения
  сэмплов). Бюджеты резидентной памяти на job — константами в
  `backend.rs` с compile-time проверкой: стрим-блок 128 КиБ + FFT-скрэтч
  ≤ 512 КиБ. Трёхчасовой рекордер проходит через тот же резидентный набор,
  что и джингл: pending компактифицируется каждые 8 фреймов, как в Swift.
  Полные файлы в память не грузятся. Кэш content-addressed, сэмплирует
  head+tail 1 МБ вместо хэширования 119 ГБ корпусов.
- **CPU:** `std::thread::scope` work-stealing (`parallel::par_map`)
  для FFT/матчинга, чтобы UI-поток GPUI не stalls. Семафор 4 =
  `AVAssetReaderGate`. Без tokio/rayon — ноль лишнего рантайма.
- **Аллокатор:** mimalloc в `align-cli`/`align-gpui` (меньше фрагментации,
  чем системный, на всех 3 ОС). В `align-core` — только `Vec` с
  `reserve`/`with_capacity`, ноль аллокаций в hot loop (`scratch` reuse).
- **Утечки:** Rust ownership + отсутствие `Arc`-циклов (UI держит `Weak`
  на jobs), atomic cache write (tmp+rename), `cargo clippy -D warnings`,
  `cargo test` на каждый milestone. Санитайзеры: `cargo +nightly fuzz`,
  valgrind/asan на Linux CI.

## Сборка

```bash
cargo test -p align-core
cargo run -p align-cli
cargo run -p align-gpui
# timeline path can also be passed at launch
cargo run -p align-gpui -- /path/to/project.xml
```

Релиз под 3 ОС:

```bash
cargo build --release -p align-cli -p align-gpui
# macOS: script/package-macos.sh [dir] (без ffmpeg по умолчанию —
# Apple backend + системный PATH; BUNDLE_FFMPEG=1 чтобы встроить)
# Windows: cargo-wix (msi) — длинные пути, bundled ffmpeg.exe
# Linux: AppImage/Flatpak + bundled ffmpeg, Wayland+X11 через GPUI
# Windows/Linux release CI: script/build-ffmpeg-minimal.sh
```

## Детерминизм

- `FingerprintExtractor` и `GCC-PHAT` тесты фиксируют бит-стабильность
  на всех ОС (LCG-шум вместо RNG, sine вместо аудиофайлов).
- ClipID остаётся SHA256 от `path\0duration` — result SHA из HANDOFF
  (`b2863828…`) должен совпасть после полного порта матчера.
- FFT отличается от vDSP в последнем ulp → кэш v5 с тегом движка,
  поведение то же; DSP общий для всех ОС, раздваивается только decode.
- Conformance: один набор тестов гоняется против обоих движков на macOS
  (`ALIGN_BACKEND=portable` vs native) — сравниваются острова/офсеты
  в пределах epsilon, а не бит-равенство.
- Два найденных и убитых бага портирования (оба пойманы тестами, а не
  ревью): инвертированный знак лага GCC-PHAT (сопряжение не в ту сторону;
  юнит-тест маскировал его через `abs()`, теперь знак asserted) и
  неограниченный flush ресемплера (runaway-хвост; теперь граница
  по контенту + group-delay компенсация, точнее Swift).

## Дальше (по порядку)

1. ~~Экспорт, drift-render, CLI, GPUI~~ — готово (см. выше).
2. ~~Nested/multicam импорт (FCP7 inline sequences + multiclip с
   active-маркерами, FCPXML compound/sync-clip/multicam/аудишн) и
   VFR-предупреждения portable (Variable + Unknown, оба с тестами)~~ —
   готово.
3. Упаковка: `script/package-macos.sh [dir]` собирает `Align.app`
   (ad-hoc sign, LSMinimum 15.0, с FFmpeg sidecars по умолчанию) + `align-cli`
   в `~/Downloads/Align-macOS`; CI делает то же на 3 ОС
   (`.github/workflows/align-rs.yml`). Дальше — dmg/msi/AppImage,
   Developer ID и нотаризация для распространения.
4. AAF: реализованы импорт и экспорт видеодорожек с внешними медиа,
   выбор последовательности, сохранение физических аудиоканалов и экспорт
   подготовленных моностемов. Автономный модуль `align-aaf` на основе
   pyaaf2 входит в пакет; устанавливать Python пользователю не нужно.
   Импорт сохраняет кадровые/сэмпловые границы и поддерживает отмену.
   В R20 проверены видео `30000/1001`, таймкод композиции и фактический
   рендер в Resolve. В текущем исходном коде дополнительно поддержаны
   вложенные SourceClip/Sequence/Filler, активные Selector и извлечение
   встроенного моно PCM в кэш. Эти дополнения включены в R21.
   Встроенное многоканальное PCM с явными номерами физических каналов
   также поддержано (проверено стерео). Разные edit rate в цепочке AAF
   пересчитываются через точные дроби с сохранением кадровой частоты
   композиции (см. work/AAF-MIXED-CLOCK-ACCEPTANCE.md). Остались встроенное видео,
   сжатое встроенное аудио,
   эффекты, переходы и изменение скорости; неподдерживаемые конструкции
   завершаются явной ошибкой. Проверки: `script/check-aaf-smoke.py`,
   `script/check-aaf-picture-smoke.py`, `script/check-aaf-embedded-smoke.py`.
   Для неоднозначных проектов Resolve GUI и CLI предлагают Automatic либо
   явный AAF timeline FPS: 23.976, 24, 25, 29.97, 30, 50, 59.94 или 60.
   Override меняет только composition timecode; исходные picture edit rates
   сохраняются без retime (см. work/AAF-FPS-OVERRIDE-ACCEPTANCE.md).
   R21 включает `ffprobe`: экспорт и повторный импорт видео AAF проверены
   с пустым PATH, без системного FFmpeg и Python.

## Использование

```bash
# GUI (Metal/DirectX/Vulkan через GPUI/Blade)
cargo run --release -p align-gpui

# CLI: sync → result.json (прогресс в stderr)
./target/release/align-cli sync /path/to/media > result.json
./target/release/align-cli sync --match-threshold conservative /path/to/media > result.json
./target/release/align-cli sync --clip-order by-file-name /path/to/media > result.json
./target/release/align-cli sync --track-content linear /path/to/media > result.json
./target/release/align-cli sync --preserve-basic-editing A1 /path/to/project.xml > result.json
./target/release/align-cli sync --all-sequences /path/to/project.xml > results.json
./target/release/align-cli sync --write-fixed-project /path/to/project-fixed.xml /path/to/project.xml /path/to/media > result.json
./target/release/align-cli export /path/to/output /path/to/media
./target/release/align-cli export --all-sequences /path/to/output /path/to/project.xml
./target/release/align-cli export --write-fixed-project /path/to/project-fixed.aaf /path/to/output /path/to/project.aaf /path/to/media
./target/release/align-cli export --unmatched order-only --disable-unmatched --prevent-group-overlaps /path/to/output /path/to/media
./target/release/align-cli export --time-source timecode --match-threshold conservative --clip-order by-file-name /path/to/output /path/to/media
./target/release/align-cli export --preserve-basic-editing V1 --preserve-basic-editing A1 /path/to/output /path/to/project.xml
./target/release/align-cli export --no-drift --replaced-audio /path/to/output /path/to/media
./target/release/align-cli export --aaf --aaf-fps 25 /path/to/output /path/to/project.aaf
./target/release/align-cli export --no-fcpxml-multicam --label-synced --synced-symbol '[SYNCED]' --synced-color Iris --synced-role dialogue /path/to/output /path/to/media
./target/release/align-cli export --no-fcpxml-timeline /path/to/output /path/to/media
./target/release/align-cli export --export-media /path/to/output /path/to/media
./target/release/align-cli export-json result.json /path/to/output
./target/release/align-cli export-json --no-drift --replaced-audio result.json /path/to/output
```

Нужны только Rust stable + системный ffmpeg/ffprobe в PATH
(контейнерный тир portable-движка и mp4-фикстуры тестов).
