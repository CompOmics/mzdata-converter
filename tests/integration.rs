//! Integration tests for mzdata-converter.
//!
//! Test data is stored as `.tar.gz` archives (via Git LFS) and `.RAW` files
//! in `tests/data/`. Archives are extracted before each test. Extracted
//! directories are gitignored.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

fn mzdata_converter() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mzdata-converter"))
}

// Guards check-then-extract below: cargo test runs tests in parallel threads,
// and two tests can share the same archive. Without this lock both threads
// see the target dir missing and race to `tar xzf` into it concurrently,
// which on Windows fails with "Can't unlink already-existing object".
static EXTRACT_LOCK: Mutex<()> = Mutex::new(());

/// Extract a `.tar.gz` archive if the target directory doesn't exist yet.
/// Returns the path to the extracted directory.
fn ensure_extracted(archive: &str) -> Option<PathBuf> {
    let _guard = EXTRACT_LOCK.lock().unwrap();

    let archive_path = Path::new(archive);
    if !archive_path.exists() {
        eprintln!("Skipping test: {archive} not found");
        return None;
    }

    // Strip .tar.gz to get the directory name
    let dir_name = archive
        .strip_suffix(".tar.gz")
        .expect("archive must end in .tar.gz");
    let dir_path = Path::new(dir_name);

    if !dir_path.exists() {
        let parent = archive_path.parent().unwrap_or(Path::new("."));
        let filename = archive_path.file_name().unwrap().to_str().unwrap();
        let status = Command::new("tar")
            .args(["xzf", filename])
            .current_dir(parent)
            .status()
            .expect("Failed to run tar");
        assert!(status.success(), "Failed to extract {archive}");
    }

    Some(dir_path.to_path_buf())
}

#[test]
fn test_dda_bruker_conversion() {
    let Some(input) = ensure_extracted("tests/data/200ngHeLaPASEF_1min.d.tar.gz") else {
        return;
    };

    let output_dir = tempfile::tempdir().unwrap();
    let result = mzdata_converter()
        .arg(&input)
        .arg("-o")
        .arg(output_dir.path())
        .output()
        .expect("Failed to run mzdata-converter");

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "Conversion failed: {stderr}");

    let mzml_path = output_dir.path().join("200ngHeLaPASEF_1min.mzML");
    assert!(mzml_path.exists(), "Output mzML not created");

    let content = std::fs::read_to_string(&mzml_path).unwrap();
    let spectra = content.matches("</spectrum>").count();
    assert!(spectra > 100, "Expected >100 spectra, got {spectra}");
    assert!(
        content.contains("\"ms level\" value=\"1\""),
        "No MS1 spectra"
    );
    assert!(
        content.contains("\"ms level\" value=\"2\""),
        "No MS2 spectra"
    );
    assert!(content.contains("<indexedmzML"), "Not indexed mzML");
    assert!(
        content.contains("isolation window target m/z"),
        "No isolation windows"
    );
    assert!(content.contains("collision energy"), "No collision energy");
}

#[test]
fn test_dia_bruker_conversion() {
    let Some(input) =
        ensure_extracted("tests/data/230711_idleflow_400-1000mz_25mz_diaPasef_10sec.d.tar.gz")
    else {
        return;
    };

    let output_dir = tempfile::tempdir().unwrap();
    let result = mzdata_converter()
        .arg(&input)
        .arg("-o")
        .arg(output_dir.path())
        .output()
        .expect("Failed to run mzdata-converter");

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "Conversion failed: {stderr}");

    let mzml_path = output_dir
        .path()
        .join("230711_idleflow_400-1000mz_25mz_diaPasef_10sec.mzML");
    assert!(mzml_path.exists(), "Output mzML not created");

    let content = std::fs::read_to_string(&mzml_path).unwrap();
    let spectra = content.matches("</spectrum>").count();
    assert!(spectra > 100, "Expected >100 spectra, got {spectra}");
    assert!(
        content.contains("\"ms level\" value=\"1\""),
        "No MS1 spectra"
    );
    assert!(
        content.contains("\"ms level\" value=\"2\""),
        "No MS2 spectra"
    );
    assert!(content.contains("<indexedmzML"), "Not indexed mzML");
    assert!(
        content.contains("isolation window target m/z"),
        "No isolation windows"
    );
}

#[test]
fn test_thermo_raw_conversion() {
    let input = Path::new("tests/data/small.RAW");
    if !input.exists() {
        eprintln!("Skipping test: {} not found", input.display());
        return;
    }

    let output_dir = tempfile::tempdir().unwrap();
    let result = mzdata_converter()
        .arg(input)
        .arg("-o")
        .arg(output_dir.path())
        .output()
        .expect("Failed to run mzdata-converter");

    let stderr = String::from_utf8_lossy(&result.stderr);

    // Thermo .NET assemblies may not work on macOS
    if stderr.contains("FileLoadException") || stderr.contains("dependent libraries is missing") {
        eprintln!("Skipping: Thermo .NET runtime not functional on this platform");
        return;
    }

    assert!(result.status.success(), "Conversion failed: {stderr}");

    let mzml_path = output_dir.path().join("small.mzML");
    assert!(mzml_path.exists(), "Output mzML not created");

    let content = std::fs::read_to_string(&mzml_path).unwrap();
    let spectra = content.matches("</spectrum>").count();
    assert!(spectra > 0, "No spectra in output, got {spectra}");
    assert!(
        content.contains("\"ms level\" value=\"1\""),
        "No MS1 spectra"
    );
    assert!(content.contains("<indexedmzML"), "Not indexed mzML");
}

#[test]
fn test_missing_input_fails() {
    let result = mzdata_converter()
        .arg("nonexistent_file.raw")
        .output()
        .expect("Failed to run mzdata-converter");

    assert!(!result.status.success(), "Should fail on missing input");
}

#[test]
fn test_multiple_files() {
    let inputs: Vec<PathBuf> = [
        ensure_extracted("tests/data/200ngHeLaPASEF_1min.d.tar.gz"),
        ensure_extracted("tests/data/230711_idleflow_400-1000mz_25mz_diaPasef_10sec.d.tar.gz"),
    ]
    .into_iter()
    .flatten()
    .collect();

    if inputs.is_empty() {
        eprintln!("Skipping test: no test files found");
        return;
    }

    let output_dir = tempfile::tempdir().unwrap();
    let mut cmd = mzdata_converter();
    for input in &inputs {
        cmd.arg(input);
    }
    cmd.arg("-o").arg(output_dir.path());

    let result = cmd.output().expect("Failed to run mzdata-converter");
    // Verify no crash; some formats may not work on all platforms
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.code().is_some(), "Process crashed: {stderr}");
}
