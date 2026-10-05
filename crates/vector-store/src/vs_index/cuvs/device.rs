/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

use anyhow::anyhow;
use anyhow::bail;
use cuvs::Resources;
use cuvs::dlpack::AsDlTensor;
use cuvs::dlpack::DLDevice;
use cuvs::dlpack::DLDeviceType;
use cuvs::dlpack::DLPackError;
use cuvs::dlpack::DLTensorView;
use cuvs::dlpack::DType;
use std::ffi::CStr;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::marker::PhantomData;
use tracing::error;

const CUDA_SUCCESS: c_int = 0;
const CUDA_MEMCPY_HOST_TO_DEVICE: c_int = 1;

#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaGetDevice(device: *mut c_int) -> c_int;
    fn cudaMalloc(ptr: *mut *mut c_void, size: usize) -> c_int;
    fn cudaFree(ptr: *mut c_void) -> c_int;
    fn cudaMemcpyAsync(
        dst: *mut c_void,
        src: *const c_void,
        count: usize,
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

#[derive(Debug)]
pub(super) struct DeviceMatrix<T> {
    data: *mut c_void,
    shape: [i64; 2],
    device_id: c_int,
    _values: PhantomData<T>,
}

impl<T: DType> DeviceMatrix<T> {
    pub(super) fn from_host(
        resources: &Resources,
        host: &[T],
        rows: usize,
        columns: usize,
    ) -> anyhow::Result<Self> {
        if rows.checked_mul(columns) != Some(host.len()) {
            bail!(
                "host matrix has {} values, expected {rows}x{columns}",
                host.len()
            );
        }
        let bytes = size_of_val(host);

        let mut device_id = 0;
        // SAFETY: `device_id` is a valid out-pointer.
        check_cuda(unsafe { cudaGetDevice(&mut device_id) }, "cudaGetDevice")?;
        let mut data = std::ptr::null_mut();
        // SAFETY: `data` is a valid out-pointer.
        check_cuda(unsafe { cudaMalloc(&mut data, bytes) }, "cudaMalloc")?;
        let matrix = Self {
            data,
            shape: [rows as i64, columns as i64],
            device_id,
            _values: PhantomData,
        };

        let stream = resources
            .stream()
            .map_err(|err| anyhow!("failed to get the cuVS stream: {err}"))?;
        // SAFETY: `data` was just allocated with as many bytes as `host` holds.
        check_cuda(
            unsafe {
                cudaMemcpyAsync(
                    matrix.data,
                    host.as_ptr().cast(),
                    bytes,
                    CUDA_MEMCPY_HOST_TO_DEVICE,
                    stream,
                )
            },
            "cudaMemcpyAsync",
        )?;
        resources
            .sync_stream()
            .map_err(|err| anyhow!("failed to sync the cuVS stream: {err}"))?;

        Ok(matrix)
    }
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
        // SAFETY: `data` is exactly the contiguous row-major matrix `shape`
        // declares, on `device_id`, and outlives the view, whose lifetime is the
        // `&self` borrow.
        unsafe {
            DLTensorView::from_raw_parts(
                self.data,
                DLDevice {
                    device_type: DLDeviceType::kDLCUDA,
                    device_id: self.device_id,
                },
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

    const CUDA_MEMCPY_DEVICE_TO_HOST: c_int = 2;

    #[test]
    fn from_host_round_trips_the_values() {
        let resources = Resources::new().unwrap();
        let host: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let matrix = DeviceMatrix::from_host(&resources, &host, 2, 3).unwrap();

        let mut copy = vec![0.0; host.len()];
        let stream = resources.stream().unwrap();
        // SAFETY: `copy` holds as many bytes as `matrix`.
        check_cuda(
            unsafe {
                cudaMemcpyAsync(
                    copy.as_mut_ptr().cast(),
                    matrix.data,
                    size_of_val(copy.as_slice()),
                    CUDA_MEMCPY_DEVICE_TO_HOST,
                    stream,
                )
            },
            "cudaMemcpyAsync",
        )
        .unwrap();
        resources.sync_stream().unwrap();
        assert_eq!(copy, host);
    }

    #[test]
    fn from_host_rejects_mismatched_length() {
        let resources = Resources::new().unwrap();
        let host: Vec<f32> = vec![1.0, 2.0, 3.0];
        let err = DeviceMatrix::from_host(&resources, &host, 2, 3).unwrap_err();
        assert!(err.to_string().contains("expected 2x3"), "got: {err}");
    }
}
