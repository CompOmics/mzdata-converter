# mzdata-converter

Bruker and Thermo raw file to mzML converter built with [mzdata](https://github.com/mobiusklein/mzdata).

## Supported Formats

- Thermo RAW
- Bruker TDF
- mzML
- MGF

## Installation

Download a binary from the [releases](https://github.com/ralfg/mzdata-converter/releases) page.

## Usage

```bash
# Single file
mzdata-converter sample.RAW

# Multiple files (all cores used)
mzdata-converter *.RAW *.d

# Custom output directory
mzdata-converter -o output/ *.RAW

# Disable peak picking
mzdata-converter --no-peak-picking sample.RAW
```

### Options

| Flag | Description |
|------|-------------|
| `-o, --output-dir` | Output directory (default: same as input) |
| `-j, --jobs` | Concurrent files (default: all) |
| `--no-peak-picking` | Disable centroiding |
| `--sn-threshold` | S/N threshold for peak picking (default: 1.0) |
| `--no-compression` | Disable zlib compression |

## Defaults

- Indexed mzML output with zlib compression
- Thermo: native vendor centroiding
- Bruker: peak consolidation across ion mobility (10 ppm)
- Other formats: mzsignal peak picker

## License

Apache-2.0. See [NOTICE](NOTICE) for third-party license information.
