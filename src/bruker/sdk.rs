//! FFI bindings to Bruker's TDF-SDK (timsdata.dll / libtimsdata.so).
//!
//! Loaded at runtime via `libloading`. Falls back gracefully if the SDK is not found.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use libloading::{Library, Symbol};

/// Callback function type used by the SDK for spectrum data.
type SdkCallback = unsafe extern "C" fn(i64, u32, *const f64, *const f32, *mut std::ffi::c_void);

/// Handle to the Bruker TDF-SDK library and an open .d dataset.
///
/// The library is intentionally leaked on drop — unloading timsdata.dll
/// causes STATUS_ACCESS_VIOLATION on Windows due to background threads
/// in the SDK that haven't cleaned up yet.
pub struct TimsDataHandle {
    _lib: std::mem::ManuallyDrop<Library>,
    handle: u64,
    extract_centroid_fn:
        unsafe extern "C" fn(u64, i64, u32, u32, SdkCallback, *mut std::ffi::c_void) -> u32,
    read_pasef_msms_fn:
        unsafe extern "C" fn(u64, *const i64, u32, SdkCallback, *mut std::ffi::c_void) -> u32,
    scannum_to_ook0_fn: unsafe extern "C" fn(u64, i64, *const f64, *mut f64, u32) -> u32,
}

// SDK calls are not thread-safe per handle, but we use one handle per file/thread.
unsafe impl Send for TimsDataHandle {}

/// Data passed through the SDK callback via user_data pointer.
struct CentroidCallbackData {
    mzs: Vec<f64>,
    intensities: Vec<f32>,
}

/// Data passed through the PASEF MSMS callback — collects per-precursor spectra.
struct PasefCallbackData {
    spectra: HashMap<i64, (Vec<f64>, Vec<f32>)>,
}

impl TimsDataHandle {
    /// Load the SDK library and open a .d directory.
    pub fn open(sdk_path: &Path, analysis_dir: &Path) -> Result<Self> {
        let lib = unsafe { Library::new(sdk_path) }
            .with_context(|| format!("Failed to load Bruker SDK from {}", sdk_path.display()))?;

        let open_fn: Symbol<unsafe extern "C" fn(*const i8, u32, u32) -> u64> =
            unsafe { lib.get(b"tims_open_v2\0") }
                .with_context(|| "Failed to find tims_open_v2 in SDK")?;

        type ExtractFn =
            unsafe extern "C" fn(u64, i64, u32, u32, SdkCallback, *mut std::ffi::c_void) -> u32;

        let extract_centroid_fn =
            *unsafe { lib.get::<ExtractFn>(b"tims_extract_centroided_spectrum_for_frame_v2\0") }
                .with_context(|| "Failed to find tims_extract_centroided_spectrum_for_frame_v2")?;

        type PasefFn =
            unsafe extern "C" fn(u64, *const i64, u32, SdkCallback, *mut std::ffi::c_void) -> u32;

        let read_pasef_msms_fn = *unsafe { lib.get::<PasefFn>(b"tims_read_pasef_msms_v2\0") }
            .with_context(|| "Failed to find tims_read_pasef_msms_v2")?;

        type ConvertFn = unsafe extern "C" fn(u64, i64, *const f64, *mut f64, u32) -> u32;

        let scannum_to_ook0_fn = *unsafe { lib.get::<ConvertFn>(b"tims_scannum_to_oneoverk0\0") }
            .with_context(|| "Failed to find tims_scannum_to_oneoverk0")?;

        let error_fn: Symbol<unsafe extern "C" fn(*mut i8, u32) -> u32> =
            unsafe { lib.get(b"tims_get_last_error_string\0") }
                .with_context(|| "Failed to find tims_get_last_error_string")?;

        let dir_str = CString::new(
            analysis_dir
                .to_str()
                .with_context(|| "Path contains invalid UTF-8")?,
        )?;

        // use_recalibrated_state=1, pressure_compensation=2 (per-frame)
        let handle = unsafe { (open_fn)(dir_str.as_ptr(), 1, 2) };
        if handle == 0 {
            let mut buf = vec![0i8; 1024];
            unsafe { (error_fn)(buf.as_mut_ptr(), buf.len() as u32) };
            let err = unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .to_string();
            bail!("Failed to open TDF dataset: {err}");
        }

        Ok(Self {
            _lib: std::mem::ManuallyDrop::new(lib),
            handle,
            extract_centroid_fn,
            read_pasef_msms_fn,
            scannum_to_ook0_fn,
        })
    }

    /// Extract centroided spectrum for a frame (MS1), aggregated across mobility scans.
    pub fn extract_centroided_spectrum(
        &self,
        frame_id: i64,
        scan_begin: u32,
        scan_end: u32,
    ) -> Result<(Vec<f64>, Vec<f32>)> {
        unsafe extern "C" fn callback(
            _id: i64,
            n: u32,
            mz_ptr: *const f64,
            intensity_ptr: *const f32,
            user_data: *mut std::ffi::c_void,
        ) {
            unsafe {
                let data = &mut *(user_data as *mut CentroidCallbackData);
                let n = n as usize;
                if n > 0 && !mz_ptr.is_null() && !intensity_ptr.is_null() {
                    data.mzs = std::slice::from_raw_parts(mz_ptr, n).to_vec();
                    data.intensities = std::slice::from_raw_parts(intensity_ptr, n).to_vec();
                }
            }
        }

        let mut data = CentroidCallbackData {
            mzs: Vec::new(),
            intensities: Vec::new(),
        };

        let ret = unsafe {
            (self.extract_centroid_fn)(
                self.handle,
                frame_id,
                scan_begin,
                scan_end,
                callback,
                &mut data as *mut CentroidCallbackData as *mut std::ffi::c_void,
            )
        };

        if ret == 0 {
            bail!("SDK centroid extraction returned 0 for frame {frame_id}");
        }

        Ok((data.mzs, data.intensities))
    }

    /// Read PASEF MSMS data for a batch of precursor IDs (DDA MS2).
    /// Returns a map of precursor_id -> (mz_values, intensity_values).
    #[allow(clippy::type_complexity)]
    pub fn read_pasef_msms(
        &self,
        precursor_ids: &[i64],
    ) -> Result<HashMap<i64, (Vec<f64>, Vec<f32>)>> {
        unsafe extern "C" fn callback(
            precursor_id: i64,
            n: u32,
            mz_ptr: *const f64,
            intensity_ptr: *const f32,
            user_data: *mut std::ffi::c_void,
        ) {
            unsafe {
                let data = &mut *(user_data as *mut PasefCallbackData);
                let n = n as usize;
                if n > 0 && !mz_ptr.is_null() && !intensity_ptr.is_null() {
                    let mzs = std::slice::from_raw_parts(mz_ptr, n).to_vec();
                    let intensities = std::slice::from_raw_parts(intensity_ptr, n).to_vec();
                    data.spectra.insert(precursor_id, (mzs, intensities));
                }
            }
        }

        let mut data = PasefCallbackData {
            spectra: HashMap::new(),
        };

        let ret = unsafe {
            (self.read_pasef_msms_fn)(
                self.handle,
                precursor_ids.as_ptr(),
                precursor_ids.len() as u32,
                callback,
                &mut data as *mut PasefCallbackData as *mut std::ffi::c_void,
            )
        };

        if ret == 0 {
            bail!("SDK read_pasef_msms returned 0");
        }

        Ok(data.spectra)
    }

    /// Convert a scan number to 1/K0 value for a given frame.
    pub fn scannum_to_oneoverk0(&self, frame_id: i64, scan_number: f64) -> Result<f64> {
        let input = [scan_number];
        let mut output = [0.0f64];
        let ret = unsafe {
            (self.scannum_to_ook0_fn)(
                self.handle,
                frame_id,
                input.as_ptr(),
                output.as_mut_ptr(),
                1,
            )
        };
        if ret == 0 {
            bail!("SDK scannum_to_oneoverk0 failed for frame {frame_id}");
        }
        Ok(output[0])
    }
}

// Note: we intentionally do not call tims_close on drop — it causes
// STATUS_ACCESS_VIOLATION on some SDK versions. The OS reclaims resources
// on process exit. The Library (_lib) stays loaded to keep function pointers valid.
