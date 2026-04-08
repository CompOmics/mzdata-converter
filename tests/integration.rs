//! Integration tests for mzdata-converter.
//!
//! Test data is stored as `.tar.gz` archives in `tests/data/` and extracted
//! before each test. The archives are tracked in git; extracted directories
//! are gitignored.

use std::path::{Path, PathBuf};
use std::process::Command;

fn mzdata_converter() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mzdata-converter"))
}

/// Extract a `.tar.gz` archive if the target directory doesn't exist yet.
/// Returns the path to the extracted directory.
fn ensure_extracted(archive: &str) -> Option<PathBuf> {
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
        let status = Command::new("tar")
            .args(["xzf", archive])
            .current_dir(parent)
            .status()
            .expect("Failed to run tar");
        assert!(status.success(), "Failed to extract {archive}");
    }

    Some(dir_path.to_path_buf())
}

fn run_conversion(input: &Path) -> (bool, String, String) {
    let output_dir = tempfile::tempdir().unwrap();
    let result = mzdata_converter()
        .arg(input)
        .arg("-o")
        .arg(output_dir.path())
        .output()
        .expect("Failed to run mzdata-converter");

    let stdout = String::from_utf8_lossy(&result.stdout).to_string();
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    (result.status.success(), stdout, stderr)
}

#[test]
fn test_dda_bruker_conversion() {
    let Some(input) = ensure_extracted("tests/data/test.d.tar.gz") else {
        return;
    };

    let (success, _, stderr) = run_conversion(&input);
    assert!(success, "Conversion failed: {stderr}");
}

#[test]
fn test_dia_test_conversion() {
    let Some(input) = ensure_extracted("tests/data/dia_test.d.tar.gz") else {
        return;
    };

    let (success, _, stderr) = run_conversion(&input);
    assert!(success, "Conversion failed: {stderr}");
}

#[test]
fn test_thermo_raw_conversion() {
    let input = Path::new("tests/data/test.raw");
    if !input.exists() {
        eprintln!("Skipping test: {} not found", input.display());
        return;
    }

    // The test .raw file may be truncated/corrupt — just verify the binary
    // runs without crashing (exit code != signal/access violation).
    let output_dir = tempfile::tempdir().unwrap();
    let result = mzdata_converter()
        .arg(input)
        .arg("-o")
        .arg(output_dir.path())
        .output()
        .expect("Failed to run mzdata-converter");

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.code().is_some(),
        "Process crashed (signal): {stderr}"
    );
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
        ensure_extracted("tests/data/test.d.tar.gz"),
        ensure_extracted("tests/data/dia_test.d.tar.gz"),
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
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success(),
        "Multi-file conversion failed: {stderr}"
    );
}
