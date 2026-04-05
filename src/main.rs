#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::info;
use mzdata::io::mzml::MzMLWriterType;
use mzdata::prelude::*;
use mzdata::spectrum::bindata::BinaryCompressionType;
use mzdata::spectrum::MultiLayerSpectrum;
use mzdata::MZReader;
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
        for entry in glob::glob(pattern)
            .with_context(|| format!("Invalid glob pattern: {pattern}"))?
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

/// Configure vendor-native centroiding/consolidation on the reader.
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

/// Convert a single file from its source format to indexed mzML.
fn convert_file(
    input: &Path,
    output: &Path,
    do_peak_picking: bool,
    sn_threshold: f32,
    compression: BinaryCompressionType,
    pb: &ProgressBar,
) -> Result<u64> {
    let mut reader = MZReader::open_path(input)
        .with_context(|| format!("Failed to open: {}", input.display()))?;

    if do_peak_picking {
        configure_peak_picking(&mut reader, input);
    }

    let total = reader.len() as u64;
    pb.set_length(total);

    let fh = BufWriter::with_capacity(
        WRITER_BUF_SIZE,
        fs::File::create(output)
            .with_context(|| format!("Failed to create: {}", output.display()))?,
    );
    let mut writer = MzMLWriterType::new_with_index_and_compression(fh, true, compression);
    writer.copy_metadata_from(&reader);
    writer.set_spectrum_count(total);

    // Pre-fetch batches on a reader thread so rayon always has work
    // while the main thread writes the previous batch.
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

    for batch in rx {
        process_and_write_batch(batch, do_peak_picking, sn_threshold, &mut writer, pb)?;
    }

    reader_handle.join().expect("reader thread panicked");
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
                pb.set_prefix(input.file_name().unwrap_or_default().to_string_lossy().to_string());

                let total =
                    convert_file(&input, &output, do_peak_picking, sn_threshold, compression, &pb)?;

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
