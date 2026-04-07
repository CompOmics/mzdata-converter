mod sdk;

pub use sdk::TimsDataHandle;

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Frame metadata from the Frames table.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub id: i64,
    pub ms_level: u8,
    pub num_scans: u32,
    pub time: f64, // retention time in seconds
}

/// Precursor info for MS2 spectra (DDA).
/// One row per precursor (not per frame-precursor pair).
#[derive(Debug, Clone)]
pub struct PrecursorInfo {
    pub id: i64,
    pub largest_peak_mz: f64,
    pub monoisotopic_mz: Option<f64>,
    pub charge: Option<i32>,
    pub intensity: f64,
    pub parent_frame: i64,
    pub retention_time: f64,
    pub isolation_mz: Option<f64>,
    pub isolation_width: Option<f64>,
    pub collision_energy: Option<f64>,
}

/// Read all frame metadata from analysis.tdf.
pub fn read_frames(tdf_path: &Path) -> Result<Vec<FrameInfo>> {
    let conn = Connection::open(tdf_path).with_context(|| "Failed to open analysis.tdf")?;

    let mut stmt = conn
        .prepare(
            "SELECT Id, MsMsType, NumScans, Time \
             FROM Frames ORDER BY Id",
        )
        .with_context(|| "Failed to query Frames table")?;

    let frames = stmt
        .query_map([], |row| {
            let msms_type: i32 = row.get(1)?;
            Ok(FrameInfo {
                id: row.get(0)?,
                ms_level: if msms_type == 0 { 1 } else { 2 },
                num_scans: row.get(2)?,
                time: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| "Failed to read Frames")?;

    Ok(frames)
}

/// Read precursor info for DDA MS2 frames.
pub fn read_precursors(tdf_path: &Path) -> Result<Vec<PrecursorInfo>> {
    let conn = Connection::open(tdf_path).with_context(|| "Failed to open analysis.tdf")?;

    // One row per precursor. Use MIN to pick one representative isolation window
    // (all PASEF frames for the same precursor share the same isolation parameters).
    let mut stmt = conn
        .prepare(
            "SELECT p.Id, p.LargestPeakMz, p.MonoisotopicMz, p.Charge, p.Intensity, \
                    p.Parent, fr.Time, \
                    MIN(f.IsolationMz), MIN(f.IsolationWidth), MIN(f.CollisionEnergy) \
             FROM Precursors p \
             LEFT JOIN PasefFrameMsMsInfo f ON f.Precursor = p.Id \
             LEFT JOIN Frames fr ON fr.Id = p.Parent \
             GROUP BY p.Id \
             ORDER BY p.Parent, p.Id",
        )
        .with_context(|| "Failed to query Precursors table")?;

    let precursors = stmt
        .query_map([], |row| {
            Ok(PrecursorInfo {
                id: row.get(0)?,
                largest_peak_mz: row.get(1)?,
                monoisotopic_mz: row.get(2)?,
                charge: row.get(3)?,
                intensity: row.get(4)?,
                parent_frame: row.get(5)?,
                retention_time: row.get(6)?,
                isolation_mz: row.get(7)?,
                isolation_width: row.get(8)?,
                collision_energy: row.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| "Failed to read Precursors")?;

    Ok(precursors)
}

/// Find the SDK library (timsdata.dll / libtimsdata.so).
///
/// Search order:
/// 1. Next to the executable
/// 2. `libs/` next to the executable (for development)
/// 3. Current working directory
/// 4. `BRUKER_SDK_PATH` environment variable
pub fn find_sdk_library() -> Option<std::path::PathBuf> {
    let lib_name = if cfg!(windows) {
        "timsdata.dll"
    } else {
        "libtimsdata.so"
    };

    // Check next to executable, then libs/ subdir
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let beside = exe_dir.join(lib_name);
        if beside.exists() {
            return Some(beside);
        }
        let in_libs = exe_dir.join("libs").join(lib_name);
        if in_libs.exists() {
            return Some(in_libs);
        }
    }

    // Check current directory and libs/ subdir
    let cwd = std::path::PathBuf::from(lib_name);
    if cwd.exists() {
        return Some(cwd);
    }
    let cwd_libs = std::path::PathBuf::from("libs").join(lib_name);
    if cwd_libs.exists() {
        return Some(cwd_libs);
    }

    // Check BRUKER_SDK_PATH env var
    if let Ok(dir) = std::env::var("BRUKER_SDK_PATH") {
        let sdk_path = std::path::PathBuf::from(dir).join(lib_name);
        if sdk_path.exists() {
            return Some(sdk_path);
        }
    }

    None
}
