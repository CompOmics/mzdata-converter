# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

mzdata-converter is a Rust CLI tool that converts multi-vendor mass spectrometry raw files to indexed mzML format. It uses the `mzdata` crate for I/O and `mzsignal` (via mzdata's `nalgebra` feature) for peak picking.

**Supported input formats:** Thermo RAW (.raw, requires .NET 8 runtime), Bruker TDF, mzML, MGF.

**Default behavior** (modeled after ThermoRawFileParser):
- Output: indexed mzML with zlib-compressed binary data arrays
- Peak picking enabled: Thermo native centroiding for .RAW, Bruker peak consolidation (10 ppm) for TDF, mzsignal fallback for other formats
- All MS levels processed, no filtering
- Multi-file support with built-in glob expansion and parallel processing

## Build Commands

```bash
cargo build --release    # always use release (LTO + SIMD matter significantly)
cargo check              # type-check
cargo clippy             # lint
```

## Key Dependencies and Feature Flags

mzdata is used with `default-features = false` to avoid `zlib-ng-compat` (requires CMake + C toolchain on Windows).

Enabled mzdata features: `mzml`, `mgf`, `thermo`, `bruker_tdf`, `parallelism`, `nalgebra`, `zlib-rs`.

- `zlib-rs` — pure-Rust SIMD-accelerated zlib-ng port. Critical for performance; `miniz_oxide` is 2-5x slower
- `nalgebra` — activates `mzsignal`, enabling `pick_peaks()` on `MultiLayerSpectrum`
- `parallelism` — enables rayon for batch peak picking
- `thermo` — requires .NET 8 runtime at execution time
- `mimalloc` — global allocator replacing Windows default; ~20% faster due to multi-threaded allocation patterns

Release profile uses `lto = "thin"` and `codegen-units = 1` for better cross-crate inlining.

## Architecture

Single-binary CLI (`src/main.rs`).

### Per-file pipeline:
1. **Reader thread** — `MZReader::open_path()` auto-detects format, batches 128 spectra, sends via `sync_channel(2)` to keep rayon fed
2. **Main thread** — receives batches, peak-picks in parallel via `rayon::par_iter`, writes sequentially to `MzMLWriterType` with 1MB `BufWriter`
3. **Writer** — `MzMLWriterType::new_with_index_and_compression()` produces indexed mzML with zlib

### Multi-file concurrency:
- One OS thread per file, limited by `--jobs` (defaults to file count) via channel-based semaphore
- Rayon's global pool is shared across files for peak picking (work-stealing)
- File-level concurrency uses OS threads (not rayon) to avoid nested rayon deadlock
- Progress bars rate-limited to 10Hz via `ProgressDrawTarget::stderr_with_hz`

### Vendor-specific handling (`configure_peak_picking()`):
- **Thermo RAW**: `set_centroiding(true)` — native .NET peak picking (instrument-aware)
- **Bruker TDF**: `set_consolidate_peaks(true)` — merges peaks across ion mobility dimension (10 ppm)
- **Other formats**: `spectrum.pick_peaks(sn_threshold)` — mzsignal quadratic fit fallback

### Performance-critical decisions:
- `zlib-rs` over `miniz_oxide` (SIMD compression)
- `mimalloc` global allocator (allocation-heavy XML serialization)
- `write_owned()` over `write()` (avoids internal spectrum clone)
- 1MB `BufWriter` (reduces syscall overhead for large mzML output)
- `lto = "thin"` + `codegen-units = 1` (cross-crate inlining)

Key mzdata types: `MZReader` (dispatch enum), `MultiLayerSpectrum` (spectrum with raw + centroid + deconvoluted layers), `MzMLWriterType` (writer implementing `SpectrumWriter`). Import `mzdata::prelude::*` for all key traits.
