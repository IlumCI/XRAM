//! Runtime binding to the CUDA driver API.
//!
//! XRAM links against nothing at build time: `libcuda.so.1` is opened with
//! `dlopen` on first use. That keeps the entire workspace compiling and
//! unit-testing on machines with no NVIDIA driver (CI, containers, the dev box
//! this was written on), while still reaching the driver on a real GPU host.
//!
//! Only the subset XRAM needs is bound: context setup, device/pinned-host
//! allocation, async copies on explicit streams, and free-memory queries for
//! VRAM ballooning. Everything is the `_v2` symbol where CUDA versions one.

mod error;

pub use error::{CuResult, CudaError, Result, CUDA_SUCCESS};

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::ptr;
use std::sync::OnceLock;

pub type CuDevice = c_int;
pub type CuDevicePtr = u64;

opaque_handle!(CuContext);
opaque_handle!(CuStream);

/// Declares an opaque driver handle as a newtype over a raw pointer.
///
/// The driver hands these back as pointers we never dereference; wrapping them
/// keeps them distinct in signatures and lets us assert `Send` deliberately
/// rather than by accident.
macro_rules! opaque_handle {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(transparent)]
        pub struct $name(pub *mut c_void);
        impl $name {
            pub const NULL: Self = Self(ptr::null_mut());
            pub fn is_null(&self) -> bool {
                self.0.is_null()
            }
        }
        // SAFETY: CUDA contexts and streams are driver-side objects addressed by
        // handle. The driver serialises access internally, and XRAM only moves
        // handles between threads, never aliasing the objects they name.
        unsafe impl Send for $name {}
        unsafe impl Sync for $name {}
    };
}
use opaque_handle;

/// `CU_CTX_SCHED_BLOCKING_SYNC` - block the calling thread on sync rather than
/// spinning. XRAM's transfer threads are I/O threads, not compute threads; a
/// spin-wait here would burn a core that the swap path needs.
pub const CTX_SCHED_BLOCKING_SYNC: c_uint = 0x04;

/// `CU_STREAM_NON_BLOCKING` - do not implicitly synchronise with the legacy
/// default stream.
pub const STREAM_NON_BLOCKING: c_uint = 0x01;

macro_rules! cuda_api {
    ($( fn $field:ident : $symbol:literal = $ty:ty; )*) => {
        /// Resolved entry points into `libcuda.so.1`.
        #[allow(non_snake_case)]
        pub struct Cuda {
            handle: *mut c_void,
            $( $field: $ty, )*
        }

        impl Cuda {
            unsafe fn resolve(handle: *mut c_void) -> Result<Self> {
                Ok(Self {
                    handle,
                    $( $field: std::mem::transmute::<*mut c_void, $ty>(
                        sym(handle, concat!($symbol, "\0"))
                            .ok_or(CudaError::MissingSymbol($symbol))?,
                    ), )*
                })
            }
        }
    };
}

cuda_api! {
    fn cuInit: "cuInit" = unsafe extern "C" fn(c_uint) -> CuResult;
    fn cuGetErrorName: "cuGetErrorName" = unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult;
    fn cuGetErrorString: "cuGetErrorString" = unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult;
    fn cuDeviceGetCount: "cuDeviceGetCount" = unsafe extern "C" fn(*mut c_int) -> CuResult;
    fn cuDeviceGet: "cuDeviceGet" = unsafe extern "C" fn(*mut CuDevice, c_int) -> CuResult;
    fn cuDeviceGetName: "cuDeviceGetName" = unsafe extern "C" fn(*mut c_char, c_int, CuDevice) -> CuResult;
    fn cuDeviceGetAttribute: "cuDeviceGetAttribute" = unsafe extern "C" fn(*mut c_int, c_int, CuDevice) -> CuResult;
    fn cuCtxCreate: "cuCtxCreate_v2" = unsafe extern "C" fn(*mut *mut c_void, c_uint, CuDevice) -> CuResult;
    fn cuCtxDestroy: "cuCtxDestroy_v2" = unsafe extern "C" fn(*mut c_void) -> CuResult;
    fn cuCtxSetCurrent: "cuCtxSetCurrent" = unsafe extern "C" fn(*mut c_void) -> CuResult;
    fn cuCtxSynchronize: "cuCtxSynchronize" = unsafe extern "C" fn() -> CuResult;
    fn cuMemGetInfo: "cuMemGetInfo_v2" = unsafe extern "C" fn(*mut usize, *mut usize) -> CuResult;
    fn cuMemAlloc: "cuMemAlloc_v2" = unsafe extern "C" fn(*mut CuDevicePtr, usize) -> CuResult;
    fn cuMemFree: "cuMemFree_v2" = unsafe extern "C" fn(CuDevicePtr) -> CuResult;
    fn cuMemAllocHost: "cuMemAllocHost_v2" = unsafe extern "C" fn(*mut *mut c_void, usize) -> CuResult;
    fn cuMemFreeHost: "cuMemFreeHost" = unsafe extern "C" fn(*mut c_void) -> CuResult;
    fn cuMemcpyHtoDAsync: "cuMemcpyHtoDAsync_v2" = unsafe extern "C" fn(CuDevicePtr, *const c_void, usize, *mut c_void) -> CuResult;
    fn cuMemcpyDtoHAsync: "cuMemcpyDtoHAsync_v2" = unsafe extern "C" fn(*mut c_void, CuDevicePtr, usize, *mut c_void) -> CuResult;
    fn cuStreamCreate: "cuStreamCreate" = unsafe extern "C" fn(*mut *mut c_void, c_uint) -> CuResult;
    fn cuStreamDestroy: "cuStreamDestroy_v2" = unsafe extern "C" fn(*mut c_void) -> CuResult;
    fn cuStreamSynchronize: "cuStreamSynchronize" = unsafe extern "C" fn(*mut c_void) -> CuResult;
}

// SAFETY: every field is a plain function pointer into a library we keep loaded
// for the process lifetime; the driver itself is thread-safe.
unsafe impl Send for Cuda {}
unsafe impl Sync for Cuda {}

unsafe fn sym(handle: *mut c_void, name: &'static str) -> Option<*mut c_void> {
    debug_assert!(name.ends_with('\0'));
    let p = libc::dlsym(handle, name.as_ptr() as *const c_char);
    (!p.is_null()).then_some(p)
}

static CUDA: OnceLock<std::result::Result<Cuda, String>> = OnceLock::new();

/// Returns the process-wide driver binding, loading it on first call.
///
/// The result is cached either way: a machine without a driver pays one failed
/// `dlopen`, not one per call.
pub fn cuda() -> Result<&'static Cuda> {
    CUDA.get_or_init(|| {
        // SAFETY: called once, before any other entry point is used.
        unsafe { Cuda::load_uncached() }.map_err(|e| match e {
            // Unwrap the reason rather than the formatted error, so re-wrapping
            // below does not stutter the prefix.
            CudaError::Unavailable(why) => why,
            other => other.to_string(),
        })
    })
    .as_ref()
    .map_err(|e| CudaError::Unavailable(e.clone()))
}

/// True when a CUDA driver is present and reports at least one device.
pub fn available() -> bool {
    cuda().and_then(|c| c.device_count()).is_ok_and(|n| n > 0)
}

impl Cuda {
    unsafe fn load_uncached() -> Result<Self> {
        // `libcuda.so.1` is the driver's stable SONAME; `libcuda.so` only exists
        // when the development package is installed, so try it second.
        let mut last = String::new();
        for name in ["libcuda.so.1\0", "libcuda.so\0"] {
            let h = libc::dlopen(
                name.as_ptr() as *const c_char,
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if !h.is_null() {
                let this = Self::resolve(h)?;
                this.check("cuInit", (this.cuInit)(0))?;
                return Ok(this);
            }
            let err = libc::dlerror();
            if !err.is_null() {
                last = CStr::from_ptr(err).to_string_lossy().into_owned();
            }
        }
        Err(CudaError::Unavailable(if last.is_empty() {
            "libcuda.so.1 not found".to_owned()
        } else {
            last
        }))
    }

    fn check(&self, call: &'static str, code: CuResult) -> Result<()> {
        if code == CUDA_SUCCESS {
            return Ok(());
        }
        Err(CudaError::Call {
            call,
            code,
            name: self.err_text(self.cuGetErrorName, code),
            desc: self.err_text(self.cuGetErrorString, code),
        })
    }

    fn err_text(
        &self,
        f: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
        code: CuResult,
    ) -> String {
        let mut p: *const c_char = ptr::null();
        // SAFETY: `f` is a resolved driver entry point; it writes a pointer to a
        // static, driver-owned string, or leaves `p` null on an unknown code.
        unsafe {
            if f(code, &mut p) == CUDA_SUCCESS && !p.is_null() {
                return CStr::from_ptr(p).to_string_lossy().into_owned();
            }
        }
        "unknown".to_owned()
    }

    pub fn device_count(&self) -> Result<i32> {
        let mut n = 0;
        // SAFETY: resolved entry point, out-param is a valid initialised i32.
        unsafe { self.check("cuDeviceGetCount", (self.cuDeviceGetCount)(&mut n))? };
        Ok(n)
    }

    pub fn device(&self, ordinal: i32) -> Result<CuDevice> {
        let mut d: CuDevice = 0;
        // SAFETY: resolved entry point, out-param is a valid initialised CuDevice.
        unsafe { self.check("cuDeviceGet", (self.cuDeviceGet)(&mut d, ordinal))? };
        Ok(d)
    }

    pub fn device_name(&self, dev: CuDevice) -> Result<String> {
        let mut buf = [0u8; 256];
        // SAFETY: buffer is 256 bytes and we pass that exact length; the driver
        // NUL-terminates within it.
        unsafe {
            self.check(
                "cuDeviceGetName",
                (self.cuDeviceGetName)(buf.as_mut_ptr() as *mut c_char, buf.len() as c_int, dev),
            )?;
            Ok(CStr::from_ptr(buf.as_ptr() as *const c_char)
                .to_string_lossy()
                .into_owned())
        }
    }

    pub fn device_attribute(&self, attr: c_int, dev: CuDevice) -> Result<i32> {
        let mut v = 0;
        // SAFETY: resolved entry point, out-param is a valid initialised i32.
        unsafe {
            self.check(
                "cuDeviceGetAttribute",
                (self.cuDeviceGetAttribute)(&mut v, attr, dev),
            )?
        };
        Ok(v)
    }

    /// Creates a context and makes it current on the calling thread.
    pub fn create_context(&self, dev: CuDevice, flags: c_uint) -> Result<CuContext> {
        let mut ctx = ptr::null_mut();
        // SAFETY: resolved entry point, out-param is a valid initialised pointer.
        unsafe { self.check("cuCtxCreate", (self.cuCtxCreate)(&mut ctx, flags, dev))? };
        Ok(CuContext(ctx))
    }

    /// Binds `ctx` to the calling thread.
    ///
    /// Every thread that issues copies needs this; a context is current to one
    /// thread at a time unless explicitly pushed.
    pub fn set_current(&self, ctx: CuContext) -> Result<()> {
        // SAFETY: `ctx` came from `create_context` and has not been destroyed.
        unsafe { self.check("cuCtxSetCurrent", (self.cuCtxSetCurrent)(ctx.0)) }
    }

    /// # Safety
    /// No allocation, stream, or copy belonging to `ctx` may be in flight or
    /// subsequently used.
    pub unsafe fn destroy_context(&self, ctx: CuContext) -> Result<()> {
        self.check("cuCtxDestroy", (self.cuCtxDestroy)(ctx.0))
    }

    pub fn sync_context(&self) -> Result<()> {
        // SAFETY: resolved entry point, operates on the thread's current context.
        unsafe { self.check("cuCtxSynchronize", (self.cuCtxSynchronize)()) }
    }

    /// Returns `(free, total)` device memory in bytes.
    ///
    /// This is what VRAM ballooning polls: when `free` drops, XRAM gives VRAM
    /// back rather than making a GPU application fail to start.
    pub fn mem_info(&self) -> Result<(usize, usize)> {
        let (mut free, mut total) = (0usize, 0usize);
        // SAFETY: resolved entry point, both out-params are valid initialised usize.
        unsafe { self.check("cuMemGetInfo", (self.cuMemGetInfo)(&mut free, &mut total))? };
        Ok((free, total))
    }

    /// # Safety
    /// The returned pointer must be released with [`Cuda::mem_free`] exactly
    /// once, under the same context.
    pub unsafe fn mem_alloc(&self, bytes: usize) -> Result<CuDevicePtr> {
        let mut p: CuDevicePtr = 0;
        self.check("cuMemAlloc", (self.cuMemAlloc)(&mut p, bytes))?;
        Ok(p)
    }

    /// # Safety
    /// `p` must come from [`Cuda::mem_alloc`] and not have been freed.
    pub unsafe fn mem_free(&self, p: CuDevicePtr) -> Result<()> {
        self.check("cuMemFree", (self.cuMemFree)(p))
    }

    /// Allocates page-locked host memory.
    ///
    /// Pinned staging buffers are not optional for XRAM: pageable-source copies
    /// force the driver through its own bounce buffer, which is most of the gap
    /// between advertised and achieved PCIe bandwidth.
    ///
    /// # Safety
    /// The returned pointer must be released with [`Cuda::mem_free_host`]
    /// exactly once.
    pub unsafe fn mem_alloc_host(&self, bytes: usize) -> Result<*mut c_void> {
        let mut p = ptr::null_mut();
        self.check("cuMemAllocHost", (self.cuMemAllocHost)(&mut p, bytes))?;
        Ok(p)
    }

    /// # Safety
    /// `p` must come from [`Cuda::mem_alloc_host`] and not have been freed.
    pub unsafe fn mem_free_host(&self, p: *mut c_void) -> Result<()> {
        self.check("cuMemFreeHost", (self.cuMemFreeHost)(p))
    }

    /// # Safety
    /// `src` must be valid for `bytes` reads and must stay alive and unmodified
    /// until `stream` is synchronised. `dst` must be a device allocation with at
    /// least `bytes` of room.
    pub unsafe fn memcpy_h2d_async(
        &self,
        dst: CuDevicePtr,
        src: *const c_void,
        bytes: usize,
        stream: CuStream,
    ) -> Result<()> {
        self.check(
            "cuMemcpyHtoDAsync",
            (self.cuMemcpyHtoDAsync)(dst, src, bytes, stream.0),
        )
    }

    /// # Safety
    /// `dst` must be valid for `bytes` writes and must stay alive until `stream`
    /// is synchronised. `src` must be a device allocation with at least `bytes`.
    pub unsafe fn memcpy_d2h_async(
        &self,
        dst: *mut c_void,
        src: CuDevicePtr,
        bytes: usize,
        stream: CuStream,
    ) -> Result<()> {
        self.check(
            "cuMemcpyDtoHAsync",
            (self.cuMemcpyDtoHAsync)(dst, src, bytes, stream.0),
        )
    }

    pub fn create_stream(&self, flags: c_uint) -> Result<CuStream> {
        let mut s = ptr::null_mut();
        // SAFETY: resolved entry point, out-param is a valid initialised pointer.
        unsafe { self.check("cuStreamCreate", (self.cuStreamCreate)(&mut s, flags))? };
        Ok(CuStream(s))
    }

    /// # Safety
    /// No work may be outstanding on `stream`, and it must not be used again.
    pub unsafe fn destroy_stream(&self, stream: CuStream) -> Result<()> {
        self.check("cuStreamDestroy", (self.cuStreamDestroy)(stream.0))
    }

    pub fn sync_stream(&self, stream: CuStream) -> Result<()> {
        // SAFETY: `stream` came from `create_stream` and has not been destroyed.
        unsafe { self.check("cuStreamSynchronize", (self.cuStreamSynchronize)(stream.0)) }
    }
}

impl Drop for Cuda {
    fn drop(&mut self) {
        // Only reached if the OnceLock is ever torn down, which it is not in
        // practice; kept so the handle is not leaked in tests that build one.
        if !self.handle.is_null() {
            // SAFETY: `handle` came from `dlopen` and is closed exactly once.
            unsafe { libc::dlclose(self.handle) };
        }
    }
}

/// Reports the PCIe link the GPU is actually negotiated at, read from sysfs.
///
/// The CUDA driver API exposes no link-speed attribute, and the negotiated
/// width matters more than the slot's nominal one: a card in an x16 slot wired
/// x4 caps XRAM's VRAM tier at a quarter of the expected bandwidth, and we would
/// rather report that than have it show up as an unexplained benchmark result.
pub fn pcie_link(bus_id: &str) -> Option<(String, String)> {
    let base = format!("/sys/bus/pci/devices/{}", bus_id.to_ascii_lowercase());
    let rd = |f: &str| std::fs::read_to_string(format!("{base}/{f}")).ok();
    Some((
        rd("current_link_speed")?.trim().to_owned(),
        rd("current_link_width")?.trim().to_owned(),
    ))
}

/// Formats a CUDA PCI bus/device/function triple the way sysfs names it.
pub fn bus_id(domain: i32, bus: i32, device: i32) -> String {
    format!("{domain:04x}:{bus:02x}:{device:02x}.0")
}

/// `CU_DEVICE_ATTRIBUTE_PCI_BUS_ID`
pub const ATTR_PCI_BUS_ID: c_int = 33;
/// `CU_DEVICE_ATTRIBUTE_PCI_DEVICE_ID`
pub const ATTR_PCI_DEVICE_ID: c_int = 34;
/// `CU_DEVICE_ATTRIBUTE_PCI_DOMAIN_ID`
pub const ATTR_PCI_DOMAIN_ID: c_int = 50;
/// `CU_DEVICE_ATTRIBUTE_TOTAL_CONSTANT_MEMORY` - used only as a liveness probe.
pub const ATTR_INTEGRATED: c_int = 18;

#[allow(dead_code)]
fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<CuContext>();
    f::<CuStream>();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the dlopen design: this must not panic or fail to
    /// build anywhere, GPU or not.
    #[test]
    fn loading_is_infallible_as_a_query() {
        match cuda() {
            Ok(c) => {
                let n = c.device_count().expect("device count");
                assert!(n >= 0);
            }
            Err(CudaError::Unavailable(_)) => {} // expected without a driver
            Err(e) => panic!("unexpected error shape: {e}"),
        }
    }

    #[test]
    fn available_matches_cuda_state() {
        // Must agree with itself and never panic.
        let a = available();
        assert_eq!(a, cuda().and_then(|c| c.device_count()).unwrap_or(0) > 0);
    }

    #[test]
    fn pcie_link_missing_device_is_none() {
        assert!(pcie_link("ffff:ff:ff.0").is_none());
    }

    #[test]
    fn bus_id_formats_like_sysfs() {
        assert_eq!(bus_id(0, 1, 0), "0000:01:00.0");
        assert_eq!(bus_id(0, 0x2f, 0), "0000:2f:00.0");
    }
}
