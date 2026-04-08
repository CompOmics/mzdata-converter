# mzdata-converter

Bruker and Thermo raw file to mzML converter built with [mzdata](https://github.com/mobiusklein/mzdata).

## Supported Formats

- Thermo RAW
- Bruker TDF
- mzML
- MGF

## Installation

Download a binary from the [releases](https://github.com/compomics/mzdata-converter/releases) page.

Or use Docker:

```bash
docker pull ghcr.io/compomics/mzdata-converter:latest
docker run -v $(pwd):/data -w /data ghcr.io/compomics/mzdata-converter sample.RAW
```

## Usage

```bash
# Single file
mzdata-converter sample.RAW

# Multiple files
mzdata-converter *.RAW *.d

# Custom output directory
mzdata-converter -o output/ *.RAW
```

### Options

| Flag | Description |
|------|-------------|
| `-o, --output-dir` | Output directory (default: same as input) |
| `-j, --jobs` | Concurrent files (default: all) |
| `--no-peak-picking` | Disable centroiding |
| `--sn-threshold` | S/N threshold for peak picking (default: 1.0) |
| `--no-compression` | Disable zlib compression |

### Defaults

- Indexed mzML output with zlib compression
- Thermo: native vendor centroiding (through the [`thermorawfilereader`](https://docs.rs/crate/thermorawfilereader/) crate)
- Bruker: native SDK centroiding (bundled `timsdata.dll`/`libtimsdata.so`)
- Other formats: mzsignal peak picker

## Performance

Single-file benchmarks on a 32-core Windows workstation:

| Format | Throughput | ~100k spectra |
|--------|-----------|---------------|
| Thermo RAW | ~4,000 spectra/s | ~25s |
| Bruker TDF | ~550 spectra/s | ~3 min |

Multi-file workloads benefit from concurrent processing.

## Acknowledgements

Built on the following projects:

- [mzdata](https://github.com/mobiusklein/mzdata) — Rust mass spectrometry I/O library by Joshua Klein
- [thermorawfilereader](https://github.com/mobiusklein/thermorawfilereader.rs) — Rust bindings for Thermo's RawFileReader
- [timsrust](https://github.com/MannLabs/timsrust) — Pure Rust Bruker TDF reader (fallback when SDK is unavailable)
- [Bruker TDF-SDK](https://github.com/bruker-daltonics) — Native centroiding library for timsTOF data

## Related Projects

- [ThermoRawFileParser](https://github.com/compomics/ThermoRawFileParser) — Thermo RAW to mzML converter (C#)
- [tdf2mzml](https://github.com/mafreitas/tdf2mzml) — Bruker TDF to mzML converter (Python)
- [msconvert (ProteoWizard)](https://proteowizard.sourceforge.io/) — Multi-vendor converter supporting Waters, SCIEX, Agilent, and more

## License

Apache-2.0. See [NOTICE](NOTICE) for third-party license information.
