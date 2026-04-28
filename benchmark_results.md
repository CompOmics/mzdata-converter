# Bruker TDF Conversion Benchmark: SDK vs timsrust

## Setup

**File:** `20250314_025_S909622_LFQ_Ultra2_PASEF_15min_50ng_Condition_B_REP3_Slot2-51_1_552.d`
(DDA PASEF, 15 min gradient, 50 ng input)

**Tool:** `mzdata-converter` (this repo), release build with LTO + mimalloc

**Method:** `hyperfine --warmup 1 --runs 3` on an otherwise idle system

Three reader paths were compared:

| Path | Description |
|---|---|
| **Bruker SDK** | Native `timsdata.dll` via FFI; .NET 8 runtime required |
| **mzdata/timsrust** | mzdata's built-in TDF reader (`MZReader::open_path`), mzsignal consolidation |
| **timsrust direct** | Direct use of `timsrust` crate; rayon-parallel MS2 loading; manual MS1 scan summing |

---

## Results

### Wall-clock time (3 runs, 1 warmup)

| Command | Mean [s] | Min [s] | Max [s] | Relative |
|:---|---:|---:|---:|---:|
| `Bruker SDK` | 117.114 ± 1.853 | 115.488 | 119.131 | 1.02 ± 0.02 |
| `timsrust (direct, parallel MS2)` | 114.588 ± 1.018 | 113.556 | 115.591 | 1.00 |

**Summary:** within measurement noise — effectively a tie at wall-clock level.

### CPU utilisation

| Path | User CPU | System time | Effective cores |
|---|---|---|---|
| Bruker SDK | 808 s | 1891 s | ~23 |
| timsrust direct | 195 s | 19 s | ~1.7 |

The SDK exploits heavy internal parallelism from the .NET runtime. The timsrust path uses rayon only for MS2 loading; MS1 frame processing and mzML writing remain single-threaded.

### Output equivalence

| Metric | Bruker SDK | timsrust direct | mzdata/timsrust |
|---|---|---|---|
| Spectra written | 95,650 | 95,650 | **206,123** |
| File size | 1.7 GB | 2.1 GB | 4.1 GB |
| Mean peaks/spectrum (first 20) | ~896 | ~1,260 | ~5,348 |
| Centroided | yes | yes | yes |

The mzdata/timsrust path is **not equivalent**: it produces 2.15× more spectra and ~6× more peaks per spectrum. The cause is twofold:

1. **Spectrum granularity** — mzdata emits one spectrum per mobility scan rather than one per frame/precursor.
2. **Consolidation aggressiveness** — mzdata uses mzsignal's `IMMSMapExtracter` with `minimum_length=2` (hardcoded in `arrays.rs:168`), which is far less aggressive than either the Bruker SDK or timsrust's native scan summing.

The timsrust direct path produces the same spectrum count and structure as the SDK. The remaining ~40% difference in peak density reflects different centroiding algorithms (Bruker native C++ vs timsrust's sliding-window local-maxima centroid).

---

## Implementation notes

### `--no-sdk` flag

Added on branch `benchmark/sdk-vs-timsrust` to force the timsrust direct path regardless of SDK availability:

```
mzdata-converter --no-sdk input.d -o output/
```

### timsrust direct path (`convert_timsrust_direct`)

- **MS2**: `timsrust::readers::SpectrumReader` handles scan summing across PASEF mobility slices and centroiding natively. MS2 spectra are loaded in parallel via `rayon::par_iter` (since `SpectrumReaderTrait: Sync + Send`).
- **MS1**: `timsrust::readers::FrameReader` per-frame; all mobility scans summed by sorting TOF indices and accumulating intensities at the same bin (`tof_group_and_sum`), followed by the same sliding-window smooth + local-maxima centroid used internally by timsrust (`tof_smooth_and_centroid`, window = 1).
- MS2 spectra are pre-grouped by `precursor.frame_index` (1-based SQL frame ID) and written immediately after their parent MS1 spectrum, matching standard mzML convention.

### mzdata/timsrust API finding

`mzdata`'s `TDFSpectrumReaderType::set_consolidate_peaks(true)` uses mzsignal's `IMMSMapExtracter` with `minimum_length=2` hardcoded in `arrays.rs:168`. This parameter is not exposed through the public API. Increasing it would reduce peak count significantly and bring output closer to SDK quality. Worth raising upstream with the mzdata author.

---

## Conclusion

The Bruker native SDK offers no wall-clock advantage over a pure-Rust timsrust implementation once MS2 loading is parallelised with rayon. The SDK uses ~10× more CPU to achieve the same throughput. The main remaining difference is centroiding algorithm quality (SDK produces ~30% fewer peaks per spectrum), not speed.
