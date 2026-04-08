mod sdk;

pub use sdk::TimsDataHandle;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Frame metadata from the Frames table.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub id: i64,
    /// MsMsType: 0=MS1, 8=DDA PASEF, 9=DIA PASEF
    pub msms_type: i32,
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
    pub scan_number: f64,
    pub charge: Option<i32>,
    pub intensity: f64,
    pub parent_frame: i64,
    pub retention_time: f64,
    pub isolation_mz: Option<f64>,
    pub isolation_width: Option<f64>,
    pub collision_energy: Option<f64>,
}

/// DIA isolation window definition from DiaFrameMsMsWindows.
#[derive(Debug, Clone)]
pub struct DiaWindow {
    pub scan_start: u32,
    pub scan_end: u32,
    pub mz_center: f64,
    pub mz_width: f64,
    pub collision_energy: f64,
}

/// Global metadata from the analysis.tdf GlobalMetadata table.
#[derive(Debug, Clone)]
pub struct TdfMetadata {
    #[allow(dead_code)]
    pub acquisition_software: String,
    #[allow(dead_code)]
    pub acquisition_software_version: String,
    #[allow(dead_code)]
    pub instrument_serial_number: String,
    pub mz_acq_range_lower: f64,
    pub mz_acq_range_upper: f64,
    #[allow(dead_code)]
    pub ook0_acq_range_lower: f64,
    #[allow(dead_code)]
    pub ook0_acq_range_upper: f64,
    #[allow(dead_code)]
    pub instrument_name: String,
    #[allow(dead_code)]
    pub sample_name: String,
}

/// Read global metadata from analysis.tdf.
pub fn read_metadata(tdf_path: &Path) -> Result<TdfMetadata> {
    let conn = Connection::open(tdf_path).with_context(|| "Failed to open analysis.tdf")?;

    let mut stmt = conn
        .prepare("SELECT Key, Value FROM GlobalMetadata")
        .with_context(|| "Failed to query GlobalMetadata")?;

    let kv: HashMap<String, String> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let get = |key: &str| kv.get(key).cloned().unwrap_or_default();
    let get_f64 = |key: &str| get(key).parse::<f64>().unwrap_or(0.0);

    Ok(TdfMetadata {
        acquisition_software: get("AcquisitionSoftware"),
        acquisition_software_version: get("AcquisitionSoftwareVersion"),
        instrument_serial_number: get("InstrumentSerialNumber"),
        mz_acq_range_lower: get_f64("MzAcqRangeLower"),
        mz_acq_range_upper: get_f64("MzAcqRangeUpper"),
        ook0_acq_range_lower: get_f64("OneOverK0AcqRangeLower"),
        ook0_acq_range_upper: get_f64("OneOverK0AcqRangeUpper"),
        instrument_name: get("InstrumentName"),
        sample_name: get("SampleName"),
    })
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
            Ok(FrameInfo {
                id: row.get(0)?,
                msms_type: row.get(1)?,
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
            "SELECT p.Id, p.LargestPeakMz, p.MonoisotopicMz, p.ScanNumber, \
                    p.Charge, p.Intensity, p.Parent, fr.Time, \
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
                scan_number: row.get(3)?,
                charge: row.get(4)?,
                intensity: row.get(5)?,
                parent_frame: row.get(6)?,
                retention_time: row.get(7)?,
                isolation_mz: row.get(8)?,
                isolation_width: row.get(9)?,
                collision_energy: row.get(10)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| "Failed to read Precursors")?;

    Ok(precursors)
}

/// Read DIA isolation window definitions, grouped by window group.
pub fn read_dia_windows(tdf_path: &Path) -> Result<HashMap<i32, Vec<DiaWindow>>> {
    let conn = Connection::open(tdf_path).with_context(|| "Failed to open analysis.tdf")?;

    // Table may not exist in DDA-only files
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='DiaFrameMsMsWindows'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !table_exists {
        return Ok(HashMap::new());
    }

    let mut stmt = conn
        .prepare(
            "SELECT WindowGroup, ScanNumBegin, ScanNumEnd, IsolationMz, IsolationWidth, CollisionEnergy \
             FROM DiaFrameMsMsWindows ORDER BY WindowGroup",
        )
        .with_context(|| "Failed to query DiaFrameMsMsWindows")?;

    let mut windows: HashMap<i32, Vec<DiaWindow>> = HashMap::new();
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, i32>(0)?,
            DiaWindow {
                scan_start: row.get(1)?,
                scan_end: row.get(2)?,
                mz_center: row.get(3)?,
                mz_width: row.get(4)?,
                collision_energy: row.get(5)?,
            },
        ))
    })?
    .filter_map(|r| r.ok())
    .for_each(|(group, window)| {
        windows.entry(group).or_default().push(window);
    });

    Ok(windows)
}

/// Read DIA frame-to-window-group mapping.
pub fn read_dia_frame_mapping(tdf_path: &Path) -> Result<HashMap<i64, i32>> {
    let conn = Connection::open(tdf_path).with_context(|| "Failed to open analysis.tdf")?;

    // Table may not exist in DDA-only files
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='DiaFrameMsMsInfo'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !table_exists {
        return Ok(HashMap::new());
    }

    let mut stmt = conn
        .prepare("SELECT Frame, WindowGroup FROM DiaFrameMsMsInfo")
        .with_context(|| "Failed to query DiaFrameMsMsInfo")?;

    let mapping = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(mapping)
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
