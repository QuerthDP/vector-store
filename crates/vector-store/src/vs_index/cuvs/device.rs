/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

use anyhow::anyhow;
use anyhow::bail;
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
use tracing::error;

const CUDA_SUCCESS: c_int = 0;
const CUDA_MEMCPY_HOST_TO_DEVICE: c_int = 1;
const CUDA_MEMCPY_DEVICE_TO_HOST: c_int = 2;

#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaGetDevice(device: *mut c_int) -> c_int;
    fn cudaMalloc(ptr: *mut *mut c_void, size: usize) -> c_int;
    fn cudaFree(ptr: *mut c_void) -> c_int;
    fn cudaMemsetAsync(
        ptr: *mut c_void,
        value: c_int,
        count: usize,
        stream: cuvs_sys::cudaStream_t,
    ) -> c_int;
    fn cudaMemcpy2DAsync(
        dst: *mut c_void,
        dpitch: usize,
        src: *const c_void,
        spitch: usize,
        width: usize,
        height: usize,
        kind: c_int,
        stream: cuvs_sys::cudaStream_t,
    ) -> c_int;
    fn cudaGetErrorString(error: c_int) -> *const c_char;
}

fn check_cuda(status: c_int, context: &str) -> anyhow::Result<()> {
    if status == CUDA_SUCCESS {
        return Ok(());
    }
    // SAFETY: `cudaGetErrorString` always returns a static C string.
    let message = unsafe { CStr::from_ptr(cudaGetErrorString(status)) };
    Err(anyhow!(
        "{context} failed: {} (CUDA error {status})",
        message.to_string_lossy()
    ))
}

/// CAGRA searches a dataset only if its rows are padded to this many bytes.
const CAGRA_ROW_ALIGNMENT: usize = 16;

#[derive(Debug)]
pub(super) struct DeviceMatrix<T> {
    data: *mut c_void,
    shape: [i64; 2],
    /// Values from one row to the next, at least `shape[1]`.
    pitch: i64,
    device_id: c_int,
    _values: PhantomData<T>,
}

impl<T: DType + Copy + Default> DeviceMatrix<T> {
    /// Allocates a matrix whose values are left for the device to write.
    pub(super) fn new(rows: usize, columns: usize) -> anyhow::Result<Self> {
        Self::with_pitch(rows, columns, columns)
    }

    fn with_pitch(rows: usize, columns: usize, pitch: usize) -> anyhow::Result<Self> {
        let bytes = rows
            .checked_mul(pitch)
            .and_then(|values| values.checked_mul(size_of::<T>()))
            .ok_or_else(|| anyhow!("a {rows}x{pitch} device matrix overflows"))?;

        let mut device_id = 0;
        // SAFETY: `device_id` is a valid out-pointer.
        check_cuda(unsafe { cudaGetDevice(&mut device_id) }, "cudaGetDevice")?;
        let mut data = std::ptr::null_mut();
        // SAFETY: `data` is a valid out-pointer.
        check_cuda(unsafe { cudaMalloc(&mut data, bytes) }, "cudaMalloc")?;
        Ok(Self {
            data,
            shape: [rows as i64, columns as i64],
            pitch: pitch as i64,
            device_id,
            _values: PhantomData,
        })
    }

    pub(super) fn from_host(
        resources: &Resources,
        host: &[T],
        rows: usize,
        columns: usize,
    ) -> anyhow::Result<Self> {
        Self::upload(resources, host, rows, columns, columns)
    }

    pub(super) fn padded_from_host(
        resources: &Resources,
        host: &[T],
        rows: usize,
        columns: usize,
    ) -> anyhow::Result<Self> {
        let pitch =
            (columns * size_of::<T>()).next_multiple_of(CAGRA_ROW_ALIGNMENT) / size_of::<T>();
        Self::upload(resources, host, rows, columns, pitch)
    }

    fn upload(
        resources: &Resources,
        host: &[T],
        rows: usize,
        columns: usize,
        pitch: usize,
    ) -> anyhow::Result<Self> {
        if rows.checked_mul(columns) != Some(host.len()) {
            bail!(
                "host matrix has {} values, expected {rows}x{columns}",
                host.len()
            );
        }
        let matrix = Self::with_pitch(rows, columns, pitch)?;
        if pitch != columns {
            // Zeroed as cuVS zeroes its own padded copies.
            let stream = resources
                .stream()
                .map_err(|err| anyhow!("failed to get the cuVS stream: {err}"))?;
            // SAFETY: `matrix` holds `rows` rows of `pitch` values.
            check_cuda(
                unsafe { cudaMemsetAsync(matrix.data, 0, rows * pitch * size_of::<T>(), stream) },
                "cudaMemsetAsync",
            )?;
        }
        // SAFETY: `host` holds `rows` rows of `columns` values, and `matrix`
        // the same rows `pitch` values apart.
        unsafe {
            copy(
                resources,
                (matrix.data, pitch * size_of::<T>()),
                (host.as_ptr().cast(), columns * size_of::<T>()),
                (columns * size_of::<T>(), rows),
                CUDA_MEMCPY_HOST_TO_DEVICE,
            )
        }?;
        Ok(matrix)
    }

    pub(super) fn to_host(&self, resources: &Resources) -> anyhow::Result<Vec<T>> {
        let [rows, columns] = self.shape.map(|extent| extent as usize);
        let mut host = vec![T::default(); rows * columns];
        // SAFETY: as in `upload`, the other way round.
        unsafe {
            copy(
                resources,
                (host.as_mut_ptr().cast(), columns * size_of::<T>()),
                (self.data, self.pitch as usize * size_of::<T>()),
                (columns * size_of::<T>(), rows),
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        }?;
        Ok(host)
    }
}

impl<T> DeviceMatrix<T> {
    fn device(&self) -> DLDevice {
        DLDevice {
            device_type: DLDeviceType::kDLCUDA,
            device_id: self.device_id,
        }
    }

    /// `None` for contiguous rows, as cuVS expects of a matrix it writes.
    fn strides(&self) -> Option<[i64; 2]> {
        (self.pitch != self.shape[1]).then_some([self.pitch, 1])
    }
}

/// Copies `height` rows of `width` bytes from `src` to `dst` and waits for the
/// copy to finish. Each buffer comes with its pitch, the bytes from one row to
/// the next. The copy runs on the cuVS stream, after the kernels queued there,
/// so it sees what they wrote.
///
/// # Safety
///
/// `src` and `dst` must each hold `height` rows of `width` bytes, their pitch
/// apart, in the memory `kind` names for them: host or device.
unsafe fn copy(
    resources: &Resources,
    (dst, dst_pitch): (*mut c_void, usize),
    (src, src_pitch): (*const c_void, usize),
    (width, height): (usize, usize),
    kind: c_int,
) -> anyhow::Result<()> {
    let stream = resources
        .stream()
        .map_err(|err| anyhow!("failed to get the cuVS stream: {err}"))?;
    // SAFETY: the caller guarantees the extents of both buffers.
    check_cuda(
        unsafe { cudaMemcpy2DAsync(dst, dst_pitch, src, src_pitch, width, height, kind, stream) },
        "cudaMemcpy2DAsync",
    )?;
    resources
        .sync_stream()
        .map_err(|err| anyhow!("failed to sync the cuVS stream: {err}"))
}

impl<T> Drop for DeviceMatrix<T> {
    fn drop(&mut self) {
        // SAFETY: `data` came from `cudaMalloc` and is freed exactly once.
        if let Err(err) = check_cuda(unsafe { cudaFree(self.data) }, "cudaFree") {
            error!("failed to free a device matrix: {err}");
        }
    }
}

impl<T: DType> AsDlTensor for DeviceMatrix<T> {
    fn as_dl_tensor(&self) -> Result<DLTensorView<'_>, DLPackError> {
        // SAFETY: `data` is exactly the row-major matrix `shape` and `pitch`
        // declare, on `device_id`, and outlives the view, whose lifetime is the
        // `&self` borrow.
        unsafe {
            DLTensorView::from_raw_parts(
                self.data,
                self.device(),
                &self.shape,
                self.strides().as_ref().map(|strides| strides.as_slice()),
                T::dl_dtype(),
            )
        }
    }
}

impl<T: DType> AsDlTensorMut for DeviceMatrix<T> {
    fn as_dl_tensor_mut(&mut self) -> Result<DLTensorViewMut<'_>, DLPackError> {
        // SAFETY: as in `as_dl_tensor`, and the `&mut self` borrow makes the
        // view the only access to `data`.
        unsafe {
            DLTensorViewMut::from_raw_parts(
                self.data,
                self.device(),
                &self.shape,
                self.strides().as_ref().map(|strides| strides.as_slice()),
                T::dl_dtype(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_host_round_trips_the_values() {
        let resources = Resources::new().unwrap();
        let host: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let matrix = DeviceMatrix::from_host(&resources, &host, 2, 3).unwrap();

        assert_eq!(matrix.strides(), None);
        assert_eq!(matrix.to_host(&resources).unwrap(), host);
    }

    #[test]
    fn padded_from_host_round_trips_the_values() {
        let resources = Resources::new().unwrap();
        let host: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let matrix = DeviceMatrix::padded_from_host(&resources, &host, 2, 3).unwrap();

        assert_eq!(matrix.strides(), Some([4, 1]));
        assert_eq!(matrix.to_host(&resources).unwrap(), host);
    }

    #[test]
    fn from_host_rejects_mismatched_length() {
        let resources = Resources::new().unwrap();
        let host: Vec<f32> = vec![1.0, 2.0, 3.0];
        let err = DeviceMatrix::from_host(&resources, &host, 2, 3).unwrap_err();
        assert!(err.to_string().contains("expected 2x3"), "got: {err}");
    }
}
