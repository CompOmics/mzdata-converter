mod bruker;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::info;
use mzdata::MZReader;
use mzdata::io::mzml::MzMLWriterType;
use mzdata::meta::{
    DataProcessing, FormatConversion, ProcessingMethod, Software, custom_software_name,
};
use mzdata::prelude::*;
use mzdata::spectrum::MultiLayerSpectrum;
use mzdata::spectrum::bindata::BinaryCompressionType;
use rayon::prelude::*;

const BATCH_SIZE: usize = 128;
const WRITER_BUF_SIZE: usize = 1024 * 1024;

/// Convert mass spectrometry raw files to indexed mzML.
///
/// Supports Thermo RAW, Bruker TDF, mzML, and MGF input formats.
/// By default, profile spectra are centroided and output is zlib-compressed.
///
/// Glob patterns are expanded automatically (e.g. *.RAW works on Windows).
#[derive(Parser)]
#[command(name = "mzdata-converter")]
struct Cli {
    /// Input file(s) or glob pattern(s) (e.g. *.RAW, data/*.d)
    #[arg(required = true)]
    inputs: Vec<String>,

    /// Output directory (defaults to same directory as each input file)
    #[arg(short, long)]
    output_dir: Option<PathBuf>,

    /// Number of files to process concurrently (default: number of input files)
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Disable peak picking (write profile data as-is)
    #[arg(long)]
    no_peak_picking: bool,

    /// Signal-to-noise threshold for peak picking
    #[arg(long, default_value_t = 1.0)]
    sn_threshold: f32,

    /// Disable zlib compression of binary data arrays
    #[arg(long)]
    no_compression: bool,
}

/// Expand glob patterns and collect unique file paths.
fn expand_inputs(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for pattern in patterns {
        let mut matched = false;
        for entry in
            glob::glob(pattern).with_context(|| format!("Invalid glob pattern: {pattern}"))?
        {
            let path = entry.with_context(|| format!("Error reading glob match for: {pattern}"))?;
            if !files.contains(&path) {
                files.push(path);
            }
            matched = true;
        }
        if !matched {
            bail!("No files matched: {pattern}");
        }
    }
    if files.is_empty() {
        bail!("No input files found");
    }
    Ok(files)
}

fn output_path_for(input: &Path, output_dir: Option<&Path>) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let filename = format!("{}.mzML", stem.to_string_lossy());
    match output_dir {
        Some(dir) => dir.join(filename),
        None => input.with_file_name(filename),
    }
}

/// Configure vendor-native centroiding on the reader.
fn configure_peak_picking(reader: &mut MZReader<fs::File>, input: &Path) {
    match reader {
        MZReader::ThermoRaw(thermo) => {
            thermo.set_centroiding(true);
            info!("Thermo native centroiding enabled for {}", input.display());
        }
        MZReader::BrukerTDF(bruker) => {
            bruker.set_consolidate_peaks(true);
            info!(
                "Bruker TDF peak consolidation enabled for {}",
                input.display()
            );
        }
        _ => {}
    }
}

/// Check if this is a Bruker .d directory.
fn is_bruker_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("d"))
        && path.join("analysis.tdf").exists()
}

/// Ensure the output has a valid data-processing record and corresponding
/// software reference, even when the input reader provides no processing
/// metadata (as is common for vendor RAW readers).
fn ensure_default_data_processing<W: std::io::Write>(writer: &mut MzMLWriterType<W>) {
    if !writer.data_processings().is_empty() {
        return;
    }

    let software_id = Software::find_unique_id("mzdata_converter", writer.softwares());
    let mut software = Software {
        id: software_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    };
    software.add_param(custom_software_name("mzdata-converter"));
    writer.softwares_mut().push(software);

    let mut method = ProcessingMethod {
        order: 0,
        software_reference: software_id,
        ..Default::default()
    };
    method.add_param(FormatConversion::ConversionToMzML.into());

    writer.data_processings_mut().push(DataProcessing {
        id: "mzdata_conversion".to_string(),
        methods: vec![method],
    });
}

/// Parameters for building a spectrum in the Bruker SDK path.
struct BrukerSpectrumParams<'a> {
    mzs: &'a [f64],
    intensities: &'a [f32],
    id: &'a str,
    index: usize,
    ms_level: u8,
    time_minutes: f64,
    ion_mobility: Option<f64>,
    precursor: Option<mzdata::spectrum::Precursor>,
    mz_range: Option<(f64, f64)>,
}

/// Build a CV param with accession, name, value, and unit.
fn cv_param(
    accession: u32,
    name: &str,
    value: impl ToString,
    unit: mzdata::params::Unit,
) -> mzdata::params::Param {
    let mut p = mzdata::params::Param::new_key_value(name, value.to_string());
    p.accession = Some(accession);
    p.controlled_vocabulary = Some(mzdata::params::ControlledVocabulary::MS);
    p.unit = unit;
    p
}

/// Build a MultiLayerSpectrum from raw m/z and intensity arrays.
fn build_spectrum(params: BrukerSpectrumParams) -> MultiLayerSpectrum {
    use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray as BinDataArray};

    let mz_bytes: Vec<u8> = params.mzs.iter().flat_map(|v| v.to_le_bytes()).collect();
    let int_bytes: Vec<u8> = params
        .intensities
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();

    let mut arrays = mzdata::spectrum::bindata::BinaryArrayMap::new();
    arrays.add(BinDataArray::wrap(
        &ArrayType::MZArray,
        BinaryDataArrayType::Float64,
        mz_bytes,
    ));
    arrays.add(BinDataArray::wrap(
        &ArrayType::IntensityArray,
        BinaryDataArrayType::Float32,
        int_bytes,
    ));

    // Compute TIC and base peak from arrays
    let tic: f64 = params.intensities.iter().map(|&v| v as f64).sum();
    let (bp_mz, bp_intensity) = if let Some(max_idx) = params
        .intensities
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
    {
        (params.mzs[max_idx], params.intensities[max_idx] as f64)
    } else {
        (0.0, 0.0)
    };

    let mut spectrum: MultiLayerSpectrum = MultiLayerSpectrum::default();
    spectrum.description.id = params.id.to_string();
    spectrum.description.index = params.index;
    spectrum.description.ms_level = params.ms_level;
    spectrum.description.signal_continuity = mzdata::spectrum::SignalContinuity::Centroid;

    // TIC and base peak as params
    use mzdata::params::Unit;
    spectrum.description.params.push(cv_param(
        1000285,
        "total ion current",
        tic,
        Unit::DetectorCounts,
    ));
    spectrum
        .description
        .params
        .push(cv_param(1000504, "base peak m/z", bp_mz, Unit::MZ));
    spectrum.description.params.push(cv_param(
        1000505,
        "base peak intensity",
        bp_intensity,
        Unit::DetectorCounts,
    ));

    // Scan event with retention time, ion mobility, scan windows
    let mut scan = mzdata::spectrum::ScanEvent {
        start_time: params.time_minutes,
        ..Default::default()
    };
    if let Some(ook0) = params.ion_mobility {
        let param = cv_param(
            1002815,
            "inverse reduced ion mobility",
            ook0,
            Unit::VoltSecondPerSquareCentimeter,
        );
        let params_list = scan.params.get_or_insert_with(|| Box::new(Vec::new()));
        params_list.push(param);
    }
    if let Some((mz_low, mz_high)) = params.mz_range {
        scan.scan_windows.push(mzdata::spectrum::ScanWindow::new(
            mz_low as f32,
            mz_high as f32,
        ));
    }

    // Sum of spectra (mobility scans are aggregated)
    spectrum.description.acquisition.combination = mzdata::spectrum::ScanCombination::Sum;
    spectrum.description.acquisition.scans.push(scan);

    spectrum.arrays = Some(arrays);
    if let Some(p) = params.precursor {
        spectrum.description.precursor.push(p);
    }
    spectrum
}

/// Convert a Bruker .d directory using the native SDK for fast centroiding.
/// Supports both DDA (MsMsType=8) and DIA (MsMsType=9) acquisition modes.
fn convert_bruker_sdk(
    input: &Path,
    output: &Path,
    compression: BinaryCompressionType,
    pb: &ProgressBar,
    sdk_path: &Path,
) -> Result<u64> {
    let tdf_path = input.join("analysis.tdf");
    let sdk = bruker::TimsDataHandle::open(sdk_path, input)?;
    let metadata = bruker::read_metadata(&tdf_path)?;
    let frames = bruker::read_frames(&tdf_path)?;
    let precursors = bruker::read_precursors(&tdf_path)?;
    let dia_windows = bruker::read_dia_windows(&tdf_path)?;
    let dia_frame_mapping = bruker::read_dia_frame_mapping(&tdf_path)?;

    // Group DDA precursors by parent MS1 frame
    let mut precursors_by_parent: std::collections::HashMap<i64, Vec<&bruker::PrecursorInfo>> =
        std::collections::HashMap::new();
    for p in &precursors {
        precursors_by_parent
            .entry(p.parent_frame)
            .or_default()
            .push(p);
    }

    // Estimate total spectra: MS1 + DDA precursors + DIA (frames × windows)
    let ms1_count = frames
        .iter()
        .filter(|f| f.msms_type == 0 && f.num_scans > 0)
        .count();
    let dia_spectrum_count: usize = frames
        .iter()
        .filter(|f| f.msms_type == 9)
        .filter_map(|f| dia_frame_mapping.get(&f.id))
        .filter_map(|group| dia_windows.get(group))
        .map(|windows| windows.len())
        .sum();
    let total = (ms1_count + precursors.len() + dia_spectrum_count) as u64;
    pb.set_length(total);

    let fh = BufWriter::with_capacity(
        WRITER_BUF_SIZE,
        fs::File::create(output)
            .with_context(|| format!("Failed to create: {}", output.display()))?,
    );
    let mut writer = MzMLWriterType::new_with_index_and_compression(fh, true, compression);
    ensure_default_data_processing(&mut writer);

    // Note: we don't populate source file or instrument metadata here because
    // MzMLWriterType panics on empty InstrumentConfiguration components.
    // The mzdata writer handles basic metadata automatically.

    writer.set_spectrum_count(total);

    let mz_range = Some((metadata.mz_acq_range_lower, metadata.mz_acq_range_upper));
    let mut spectrum_index: usize = 0;
    let mut last_ms1_id = String::new();

    for frame in &frames {
        if frame.num_scans == 0 {
            continue;
        }

        match frame.msms_type {
            // MS1: extract centroided spectrum across all mobility scans
            0 => {
                let (mzs, intensities) =
                    match sdk.extract_centroided_spectrum(frame.id, 0, frame.num_scans) {
                        Ok(result) => result,
                        Err(e) => {
                            log::warn!("Skipping MS1 frame {}: {e}", frame.id);
                            pb.inc(1);
                            continue;
                        }
                    };

                last_ms1_id = format!(
                    "merged=0 frame={} scanStart=0 scanEnd={}",
                    frame.id, frame.num_scans
                );
                let spectrum = build_spectrum(BrukerSpectrumParams {
                    mzs: &mzs,
                    intensities: &intensities,
                    id: &last_ms1_id,
                    index: spectrum_index,
                    ms_level: 1,
                    time_minutes: frame.time / 60.0,
                    ion_mobility: None,
                    precursor: None,
                    mz_range,
                });
                spectrum_index += 1;
                writer.write_owned(spectrum)?;
                pb.inc(1);

                // DDA: write child MS2 precursors for this MS1 frame
                if let Some(precs) = precursors_by_parent.get(&frame.id) {
                    let precursor_ids: Vec<i64> = precs.iter().map(|p| p.id).collect();
                    let msms_data = match sdk.read_pasef_msms(&precursor_ids) {
                        Ok(data) => data,
                        Err(e) => {
                            log::warn!("Skipping MS2 precursors for frame {}: {e}", frame.id);
                            pb.inc(precs.len() as u64);
                            continue;
                        }
                    };

                    for p in precs {
                        let (mzs, intensities) = match msms_data.get(&p.id) {
                            Some(data) => data,
                            None => {
                                pb.inc(1);
                                continue;
                            }
                        };

                        let ion_mobility =
                            sdk.scannum_to_oneoverk0(p.parent_frame, p.scan_number).ok();

                        let mut precursor = mzdata::spectrum::Precursor {
                            precursor_id: Some(last_ms1_id.clone()),
                            ..Default::default()
                        };
                        let selected_mz = p.monoisotopic_mz.unwrap_or(p.largest_peak_mz);
                        precursor.ions.push(mzdata::spectrum::SelectedIon {
                            mz: selected_mz,
                            charge: p.charge,
                            intensity: p.intensity as f32,
                            ..Default::default()
                        });
                        if let (Some(iso_mz), Some(iso_width)) = (p.isolation_mz, p.isolation_width)
                        {
                            precursor.isolation_window.target = iso_mz as f32;
                            let half = iso_width as f32 / 2.0;
                            precursor.isolation_window.lower_bound = half;
                            precursor.isolation_window.upper_bound = half;
                            precursor.isolation_window.flags =
                                mzdata::spectrum::IsolationWindowState::Offset;
                        }
                        if let Some(ce) = p.collision_energy {
                            precursor.activation.energy = ce as f32;
                            precursor.activation.methods_mut().push(
                                mzdata::meta::DissociationMethodTerm::CollisionInducedDissociation,
                            );
                        }

                        let spectrum = build_spectrum(BrukerSpectrumParams {
                            mzs,
                            intensities,
                            id: &format!("precursor={}", p.id),
                            index: spectrum_index,
                            ms_level: 2,
                            time_minutes: p.retention_time / 60.0,
                            ion_mobility,
                            precursor: Some(precursor),
                            mz_range,
                        });
                        spectrum_index += 1;
                        writer.write_owned(spectrum)?;
                        pb.inc(1);
                    }
                }
            }

            // DIA: extract one spectrum per isolation window within the frame
            9 => {
                let window_group = match dia_frame_mapping.get(&frame.id) {
                    Some(&group) => group,
                    None => {
                        log::warn!("No window group for DIA frame {}", frame.id);
                        continue;
                    }
                };
                let windows = match dia_windows.get(&window_group) {
                    Some(w) => w,
                    None => {
                        log::warn!("No windows for group {window_group} in frame {}", frame.id);
                        continue;
                    }
                };

                for window in windows {
                    let (mzs, intensities) = match sdk.extract_centroided_spectrum(
                        frame.id,
                        window.scan_start,
                        window.scan_end,
                    ) {
                        Ok(result) => result,
                        Err(e) => {
                            log::warn!("Skipping DIA window in frame {}: {e}", frame.id);
                            pb.inc(1);
                            continue;
                        }
                    };

                    let mut precursor = mzdata::spectrum::Precursor {
                        precursor_id: if last_ms1_id.is_empty() {
                            None
                        } else {
                            Some(last_ms1_id.clone())
                        },
                        ..Default::default()
                    };
                    precursor.isolation_window.target = window.mz_center as f32;
                    let half = window.mz_width as f32 / 2.0;
                    precursor.isolation_window.lower_bound = half;
                    precursor.isolation_window.upper_bound = half;
                    precursor.isolation_window.flags =
                        mzdata::spectrum::IsolationWindowState::Offset;
                    precursor.activation.energy = window.collision_energy as f32;
                    precursor
                        .activation
                        .methods_mut()
                        .push(mzdata::meta::DissociationMethodTerm::CollisionInducedDissociation);

                    let spectrum = build_spectrum(BrukerSpectrumParams {
                        mzs: &mzs,
                        intensities: &intensities,
                        id: &format!(
                            "merged=0 frame={} scanStart={} scanEnd={}",
                            frame.id, window.scan_start, window.scan_end
                        ),
                        index: spectrum_index,
                        ms_level: 2,
                        time_minutes: frame.time / 60.0,
                        ion_mobility: None,
                        precursor: Some(precursor),
                        mz_range,
                    });
                    spectrum_index += 1;
                    writer.write_owned(spectrum)?;
                    pb.inc(1);
                }
            }

            // DDA MS2 frames (type 8) — handled above after their parent MS1
            // Other frame types — skip
            _ => {}
        }
    }

    writer.close()?;
    Ok(spectrum_index as u64)
}

/// Convert a single file from its source format to indexed mzML.
fn convert_file(
    input: &Path,
    output: &Path,
    do_peak_picking: bool,
    sn_threshold: f32,
    compression: BinaryCompressionType,
    pb: &ProgressBar,
) -> Result<u64> {
    // Try Bruker native SDK for .d directories
    if is_bruker_dir(input) {
        let sdk_path = bruker::find_sdk_library().unwrap_or_else(|| {
            // Try bare library name — lets dlopen/LoadLibrary search system paths
            let name = if cfg!(windows) {
                "timsdata.dll"
            } else {
                "libtimsdata.so"
            };
            PathBuf::from(name)
        });
        match convert_bruker_sdk(input, output, compression, pb, &sdk_path) {
            Ok(total) => return Ok(total),
            Err(e) => {
                info!("Bruker SDK not available ({e:#}), falling back to timsrust");
            }
        }
    }

    let mut reader = MZReader::open_path(input)
        .with_context(|| format!("Failed to open: {}", input.display()))?;

    if do_peak_picking {
        configure_peak_picking(&mut reader, input);
    }

    let total = reader.len() as u64;
    pb.set_length(total);

    // mzdata bug workaround: thermorawfilereader stores the parent MS1's 1-based Thermo scan
    // number in PrecursorT::parent_index, but make_native_id() adds 1 treating it as 0-based.
    // Result: every MS2 spectrumRef points to its own scan instead of its parent MS1.
    // Fix: track the last MS1 native ID ourselves and override precursor_id for each MS2.
    let fix_thermo_precursor_refs = matches!(reader, MZReader::ThermoRaw(_));

    let fh = BufWriter::with_capacity(
        WRITER_BUF_SIZE,
        fs::File::create(output)
            .with_context(|| format!("Failed to create: {}", output.display()))?,
    );
    let mut writer = MzMLWriterType::new_with_index_and_compression(fh, true, compression);
    writer.copy_metadata_from(&reader);
    // Remove instrument configurations with unknown components that cause the writer to panic
    for ic in writer.instrument_configurations.values_mut() {
        ic.components
            .retain(|c| c.component_type != mzdata::meta::ComponentType::Unknown);
    }
    writer
        .instrument_configurations
        .retain(|_, ic| !ic.components.is_empty());
    ensure_default_data_processing(&mut writer);
    writer.set_spectrum_count(total);

    {
        // Reader thread pre-fetches batches; main thread peak-picks in parallel + writes.
        let (tx, rx) = sync_channel::<Vec<MultiLayerSpectrum>>(2);

        let reader_handle = thread::spawn(move || {
            let mut batch = Vec::with_capacity(BATCH_SIZE);
            for spectrum in reader.iter() {
                batch.push(spectrum);
                if batch.len() >= BATCH_SIZE {
                    if tx.send(batch).is_err() {
                        return;
                    }
                    batch = Vec::with_capacity(BATCH_SIZE);
                }
            }
            if !batch.is_empty() {
                let _ = tx.send(batch);
            }
        });

        let mut last_ms1_id = String::new();
        for mut batch in rx {
            if fix_thermo_precursor_refs {
                for spectrum in &mut batch {
                    if spectrum.description.ms_level == 1 {
                        last_ms1_id = spectrum.description.id.clone();
                    } else if spectrum.description.ms_level > 1 {
                        for precursor in &mut spectrum.description.precursor {
                            precursor.precursor_id = if last_ms1_id.is_empty() {
                                None
                            } else {
                                Some(last_ms1_id.clone())
                            };
                        }
                    }
                }
            }
            process_and_write_batch(batch, do_peak_picking, sn_threshold, &mut writer, pb)?;
        }

        reader_handle.join().expect("reader thread panicked");
    }

    writer.close()?;
    Ok(total)
}

/// Peak-pick a batch in parallel via rayon, then write sequentially.
fn process_and_write_batch<W: std::io::Write>(
    batch: Vec<MultiLayerSpectrum>,
    do_peak_picking: bool,
    sn_threshold: f32,
    writer: &mut MzMLWriterType<W>,
    pb: &ProgressBar,
) -> Result<()> {
    let batch = if do_peak_picking {
        batch
            .into_par_iter()
            .map(|mut s| {
                let _ = s.pick_peaks(sn_threshold);
                s
            })
            .collect()
    } else {
        batch
    };
    let count = batch.len() as u64;
    for spectrum in batch {
        writer.write_owned(spectrum)?;
    }
    pb.inc(count);
    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let files = expand_inputs(&cli.inputs)?;
    let num_files = files.len();
    eprintln!("Found {} file(s) to convert", num_files);

    let compression = if cli.no_compression {
        BinaryCompressionType::NoCompression
    } else {
        BinaryCompressionType::Zlib
    };

    if let Some(ref dir) = cli.output_dir {
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create output directory: {}", dir.display()))?;
    }

    let jobs = cli.jobs.unwrap_or(num_files).max(1);
    let do_peak_picking = !cli.no_peak_picking;
    let sn_threshold = cli.sn_threshold;
    let output_dir = &cli.output_dir;

    let start = Instant::now();
    let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10));
    let style = ProgressStyle::with_template(
        "{prefix:.bold} {spinner:.green} [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} spectra ({per_sec}, ETA {eta})",
    )
    .unwrap()
    .progress_chars("=>-");

    // Limit concurrent file conversions with a bounded channel as semaphore.
    // Each file gets its own OS thread; rayon's global pool handles peak picking.
    let (sem_tx, sem_rx) = sync_channel::<()>(jobs);
    for _ in 0..jobs {
        let _ = sem_tx.send(());
    }
    let sem_rx = std::sync::Arc::new(std::sync::Mutex::new(sem_rx));

    let handles: Vec<_> = files
        .into_iter()
        .map(|input| {
            let sem_rx = std::sync::Arc::clone(&sem_rx);
            let sem_tx = sem_tx.clone();
            let style = style.clone();
            let multi = multi.clone();
            let output_dir = output_dir.clone();

            thread::spawn(move || -> Result<(PathBuf, u64)> {
                sem_rx.lock().unwrap().recv().unwrap();
                let output = output_path_for(&input, output_dir.as_deref());

                let pb = multi.add(ProgressBar::new(0));
                pb.set_style(style);
                pb.set_prefix(
                    input
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                );

                let total = convert_file(
                    &input,
                    &output,
                    do_peak_picking,
                    sn_threshold,
                    compression,
                    &pb,
                )?;

                pb.finish_with_message("done");
                let _ = sem_tx.send(());
                Ok((output, total))
            })
        })
        .collect();

    let mut total_spectra: u64 = 0;
    let mut errors = 0;
    for handle in handles {
        match handle.join().expect("file thread panicked") {
            Ok((path, count)) => {
                total_spectra += count;
                eprintln!("  -> {}", path.display());
            }
            Err(e) => {
                eprintln!("Error: {e:#}");
                errors += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "Converted {} spectra across {} file(s) in {:.2}s ({} concurrent job(s)){}",
        total_spectra,
        num_files,
        elapsed.as_secs_f64(),
        jobs,
        if errors > 0 {
            format!(" ({errors} error(s))")
        } else {
            String::new()
        },
    );

    if errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}
