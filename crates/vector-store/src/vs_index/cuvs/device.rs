/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

//! Device-resident matrices handed to cuVS.
//!
//! The `cuvs` crate ships no public tensor type ("To hand your own GPU (or host)
//! buffer to cuVS, implement `AsDlTensor`/`AsDlTensorMut` on top of
//! `DLTensorView::from_raw_parts`"), so the GPU backend brings its own. Memory is
//! managed through the CUDA runtime directly rather than `cuvs-sys`'s RMM
//! helpers, because those take a `cuvsResources_t` and `cuvs::Resources` keeps
//! its handle private -- the safe and raw APIs cannot be mixed.
//!
//! Matrices are stored contiguously, which is not merely the simple choice.
//! cuVS classifies a dataset as `DevicePadded` or `DeviceStandard` from its row
//! width, treating a row as padded when it is a multiple of 16 bytes -- so a
//! contiguous `f32` matrix is already padded whenever its dimension is divisible
//! by 4, which covers the usual embedding sizes (128, 256, 512, 768, 1024).
//!
//! Expressing padding as a DLPack stride does *not* work: RAFT's interop rejects
//! any non-contiguous matrix with "Expected a row-major matrix", so a genuinely
//! padded buffer has to come from `cuvsDatasetMakePadded` (the crate's
//! `PaddedDataset`), which allocates and copies a second time. CAGRA accepts a
//! standard-layout dataset for building, so that copy is not needed here; if
//! search turns out to require a padded layout for unaligned dimensions, the
//! conversion belongs at that point rather than on every upload.

use anyhow::anyhow;
use cuvs::Resources;
use cuvs::dlpack::AsDlTensor;
use cuvs::dlpack::AsDlTensorMut;
use cuvs::dlpack::DLDevice;
use cuvs::dlpack::DLDeviceType;
use cuvs::dlpack::DLPackError;
use cuvs::dlpack::DLTensorView;
use cuvs::dlpack::DLTensorViewMut;
use cuvs::dlpack::DType;
use std::ffi::CStr;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::marker::PhantomData;

#[allow(non_camel_case_types)]
type cudaError_t = c_int;

const CUDA_SUCCESS: cudaError_t = 0;
const CUDA_MEMCPY_HOST_TO_DEVICE: c_int = 1;
#[allow(
    dead_code,
    reason = "the device-to-host path is exercised by tests today; search reads results with it"
)]
const CUDA_MEMCPY_DEVICE_TO_HOST: c_int = 2;

/// CAGRA aligns dataset rows to 16 bytes; a row width that is a multiple of this
/// is what makes cuVS classify a dataset as padded rather than standard.
const CAGRA_ROW_ALIGN_BYTES: usize = 16;

#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaMalloc(ptr: *mut *mut c_void, size: usize) -> cudaError_t;
    fn cudaFree(ptr: *mut c_void) -> cudaError_t;
    fn cudaMemsetAsync(
        ptr: *mut c_void,
        value: c_int,
        count: usize,
        stream: cuvs_sys::cudaStream_t,
    ) -> cudaError_t;
    fn cudaMemcpyAsync(
        dst: *mut c_void,
        src: *const c_void,
        count: usize,
        kind: c_int,
        stream: cuvs_sys::cudaStream_t,
    ) -> cudaError_t;
    fn cudaGetErrorString(error: cudaError_t) -> *const c_char;
}

fn check_cuda(status: cudaError_t, context: &str) -> anyhow::Result<()> {
    if status == CUDA_SUCCESS {
        return Ok(());
    }
    // SAFETY: cudaGetErrorString returns a pointer to a static NUL-terminated
    // string for any value, including unrecognized ones.
    let message = unsafe { CStr::from_ptr(cudaGetErrorString(status)) };
    Err(anyhow!(
        "{context} failed: {} (CUDA error {status})",
        message.to_string_lossy()
    ))
}

/// Whether a contiguous row of `columns` values of `T` already meets CAGRA's
/// row-width rule, i.e. whether cuVS will classify the matrix as padded rather
/// than standard. For `f32` this is every dimension divisible by 4.
fn is_cagra_aligned<T>(columns: usize) -> bool {
    (columns * size_of::<T>()).is_multiple_of(CAGRA_ROW_ALIGN_BYTES)
}

/// A contiguous row-major `rows x columns` matrix in device memory.
///
/// Not `Send`/`Sync`: the buffer is freed on drop and is tied to the CUDA
/// context of the thread that owns it. The backend confines all cuVS state to a
/// single dedicated thread, so this is never moved.
pub(super) struct DeviceMatrix<T: DType> {
    data: *mut c_void,
    bytes: usize,
    /// Extent handed to cuVS: `[rows, columns]`, contiguous.
    shape: [i64; 2],
    _marker: PhantomData<T>,
}

impl<T: DType> DeviceMatrix<T> {
    /// Allocates a zeroed `rows x columns` matrix.
    pub(super) fn zeros(res: &Resources, rows: usize, columns: usize) -> anyhow::Result<Self> {
        let bytes = rows
            .checked_mul(columns)
            .and_then(|elements| elements.checked_mul(size_of::<T>()))
            .ok_or_else(|| anyhow!("device matrix {rows}x{columns} overflows a usize"))?;

        let mut data: *mut c_void = std::ptr::null_mut();
        // SAFETY: `data` is a valid out-pointer; `bytes` is the size we want.
        check_cuda(unsafe { cudaMalloc(&mut data, bytes) }, "cudaMalloc")?;

        let matrix = Self {
            data,
            bytes,
            shape: [rows as i64, columns as i64],
            _marker: PhantomData,
        };

        let stream = res.stream().map_err(|err| anyhow!("cuVS stream: {err}"))?;
        // SAFETY: `data` is a live allocation of `bytes` bytes, and `stream`
        // belongs to `res`, which outlives the synchronization below.
        check_cuda(
            unsafe { cudaMemsetAsync(matrix.data, 0, matrix.bytes, stream) },
            "cudaMemsetAsync",
        )?;
        res.sync_stream()
            .map_err(|err| anyhow!("cuVS stream sync: {err}"))?;

        Ok(matrix)
    }

    /// Uploads a contiguous row-major host matrix into a fresh device matrix.
    pub(super) fn from_host(
        res: &Resources,
        host: &[T],
        rows: usize,
        columns: usize,
    ) -> anyhow::Result<Self> {
        let expected = rows * columns;
        if host.len() != expected {
            return Err(anyhow!(
                "host matrix has {} values, expected {rows}x{columns} = {expected}",
                host.len()
            ));
        }

        let matrix = Self::zeros(res, rows, columns)?;
        if matrix.bytes == 0 {
            return Ok(matrix);
        }

        let stream = res.stream().map_err(|err| anyhow!("cuVS stream: {err}"))?;
        // SAFETY: source and destination both hold exactly `rows * columns`
        // values of `T`, checked above, and `stream` belongs to `res`.
        check_cuda(
            unsafe {
                cudaMemcpyAsync(
                    matrix.data,
                    host.as_ptr() as *const c_void,
                    matrix.bytes,
                    CUDA_MEMCPY_HOST_TO_DEVICE,
                    stream,
                )
            },
            "cudaMemcpyAsync (host to device)",
        )?;
        res.sync_stream()
            .map_err(|err| anyhow!("cuVS stream sync: {err}"))?;

        Ok(matrix)
    }

    /// Downloads into a contiguous row-major host buffer, dropping the padding.
    #[allow(
        dead_code,
        reason = "exercised by tests today; search reads neighbours and distances back with it"
    )]
    pub(super) fn to_host(&self, res: &Resources, host: &mut [T]) -> anyhow::Result<()> {
        let rows = self.rows();
        let columns = self.columns();
        let expected = rows * columns;
        if host.len() != expected {
            return Err(anyhow!(
                "host matrix has {} values, expected {rows}x{columns} = {expected}",
                host.len()
            ));
        }
        if self.bytes == 0 {
            return Ok(());
        }

        let stream = res.stream().map_err(|err| anyhow!("cuVS stream: {err}"))?;
        // SAFETY: mirror of the upload above, with source and destination
        // swapped; `host` is sized to exactly `rows * columns` values.
        check_cuda(
            unsafe {
                cudaMemcpyAsync(
                    host.as_mut_ptr() as *mut c_void,
                    self.data,
                    self.bytes,
                    CUDA_MEMCPY_DEVICE_TO_HOST,
                    stream,
                )
            },
            "cudaMemcpyAsync (device to host)",
        )?;
        res.sync_stream()
            .map_err(|err| anyhow!("cuVS stream sync: {err}"))?;

        Ok(())
    }

    pub(super) fn rows(&self) -> usize {
        self.shape[0] as usize
    }

    pub(super) fn columns(&self) -> usize {
        self.shape[1] as usize
    }

    /// Bytes occupied by the device allocation.
    pub(super) fn allocated_bytes(&self) -> usize {
        self.bytes
    }

    /// Whether cuVS will see this matrix as CAGRA-padded rather than standard.
    #[allow(
        dead_code,
        reason = "asserted by tests; search will branch on it if it needs a padded dataset"
    )]
    pub(super) fn is_cagra_padded(&self) -> bool {
        is_cagra_aligned::<T>(self.columns())
    }

    fn device() -> DLDevice {
        DLDevice {
            device_type: DLDeviceType::kDLCUDA,
            device_id: 0,
        }
    }
}

/// Written by hand rather than derived: deriving would demand `T: Debug`, and
/// the device buffer itself cannot be shown without copying it back to the host.
impl<T: DType> std::fmt::Debug for DeviceMatrix<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceMatrix")
            .field("rows", &self.rows())
            .field("columns", &self.columns())
            .field("cagra_padded", &self.is_cagra_padded())
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl<T: DType> Drop for DeviceMatrix<T> {
    fn drop(&mut self) {
        if self.data.is_null() {
            return;
        }
        // SAFETY: `data` came from `cudaMalloc` and is freed exactly once.
        let status = unsafe { cudaFree(self.data) };
        if status != CUDA_SUCCESS {
            // Nothing actionable is left at drop time, but a leaked device
            // allocation is worth surfacing -- it eats VRAM until restart.
            tracing::error!("failed to free device matrix: CUDA error {status}");
        }
    }
}

impl<T: DType> AsDlTensor for DeviceMatrix<T> {
    fn as_dl_tensor(&self) -> Result<DLTensorView<'_>, DLPackError> {
        // SAFETY: `data` is a live device allocation matching `shape`, and the
        // view borrows `self`, so it cannot outlive the allocation. `None`
        // strides declare the contiguous row-major layout the buffer has.
        unsafe {
            DLTensorView::from_raw_parts(
                self.data,
                Self::device(),
                &self.shape,
                None,
                T::dl_dtype(),
            )
        }
    }
}

impl<T: DType> AsDlTensorMut for DeviceMatrix<T> {
    fn as_dl_tensor_mut(&mut self) -> Result<DLTensorViewMut<'_>, DLPackError> {
        // SAFETY: as above, with unique access guaranteed by `&mut self`.
        unsafe {
            DLTensorViewMut::from_raw_parts(
                self.data,
                Self::device(),
                &self.shape,
                None,
                T::dl_dtype(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_rows_are_cagra_aligned_when_divisible_by_four() {
        assert!(!is_cagra_aligned::<f32>(1));
        assert!(!is_cagra_aligned::<f32>(3));
        assert!(is_cagra_aligned::<f32>(4));
        assert!(!is_cagra_aligned::<f32>(5));
        // The dimensions real embeddings use.
        for dimensions in [128, 256, 512, 768, 1024] {
            assert!(is_cagra_aligned::<f32>(dimensions), "{dimensions}");
        }
    }

    #[test]
    fn u32_rows_align_like_f32() {
        assert!(!is_cagra_aligned::<u32>(3));
        assert!(is_cagra_aligned::<u32>(8));
    }

    #[test]
    fn round_trip_preserves_unaligned_rows() {
        let res = Resources::new().unwrap();
        // 3 columns is not a multiple of 4, so cuVS sees a standard layout.
        let host: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let matrix = DeviceMatrix::from_host(&res, &host, 2, 3).unwrap();

        assert_eq!(matrix.rows(), 2);
        assert_eq!(matrix.columns(), 3);
        assert_eq!(matrix.allocated_bytes(), 6 * size_of::<f32>());
        assert!(!matrix.is_cagra_padded());

        let mut out = vec![0.0; 6];
        matrix.to_host(&res, &mut out).unwrap();
        assert_eq!(out, host);
    }

    #[test]
    fn round_trip_preserves_aligned_rows() {
        let res = Resources::new().unwrap();
        let host: Vec<f32> = (0..8).map(|value| value as f32).collect();
        let matrix = DeviceMatrix::from_host(&res, &host, 2, 4).unwrap();

        assert_eq!(matrix.allocated_bytes(), 8 * size_of::<f32>());
        assert!(matrix.is_cagra_padded());

        let mut out = vec![0.0; 8];
        matrix.to_host(&res, &mut out).unwrap();
        assert_eq!(out, host);
    }

    #[test]
    fn zeros_starts_zeroed() {
        let res = Resources::new().unwrap();
        let matrix = DeviceMatrix::<f32>::zeros(&res, 2, 3).unwrap();

        let mut out = vec![1.0; 6];
        matrix.to_host(&res, &mut out).unwrap();
        assert_eq!(out, vec![0.0; 6]);
    }

    #[test]
    fn from_host_rejects_mismatched_length() {
        let res = Resources::new().unwrap();
        let host: Vec<f32> = vec![1.0, 2.0, 3.0];
        let err = DeviceMatrix::from_host(&res, &host, 2, 3).unwrap_err();
        assert!(err.to_string().contains("expected 2x3"), "got: {err}");
    }
}
