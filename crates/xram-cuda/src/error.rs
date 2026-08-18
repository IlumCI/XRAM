use std::ffi::c_int;

/// Raw `CUresult` code returned by the CUDA driver API.
pub type CuResult = c_int;

pub const CUDA_SUCCESS: CuResult = 0;

#[derive(Debug, thiserror::Error)]
pub enum CudaError {
    /// `libcuda.so.1` is not present, or the loader refused it.
    ///
    /// This is the expected outcome on any machine without the NVIDIA driver
    /// stack, including CI. Callers should treat it as "no VRAM tier available"
    /// rather than a hard failure.
    #[error("CUDA driver library unavailable: {0}")]
    Unavailable(String),

    /// The library loaded, but a symbol we require is missing. Usually means a
    /// driver far older than we support.
    #[error("CUDA driver is missing symbol `{0}` (driver too old?)")]
    MissingSymbol(&'static str),

    /// A driver call returned a non-success `CUresult`.
    #[error("{call} failed: {name} ({code}) - {desc}")]
    Call {
        call: &'static str,
        code: CuResult,
        name: String,
        desc: String,
    },

    #[error("no CUDA-capable device found")]
    NoDevice,
}

pub type Result<T> = std::result::Result<T, CudaError>;
