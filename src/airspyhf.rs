//! Airspy HF+ / HF+ Discovery (USB 03eb:800c) via libairspyhf.
//!
//! The R2/Mini driver in `rs-spy` uses a different VID/PID (1d50:60a1) and protocol.

use crate::demod::{centered_iq_settings_with_rates, DemodConfig, RadioConfig, MPX_SAMPLE_RATE};
use crossbeam_channel::Sender;
use nusb::MaybeFuture;
use std::os::raw::{c_int, c_void};
use std::ptr;

pub const VID: u16 = 0x03eb;
pub const PID: u16 = 0x800c;

/// Count HF+ devices visible on USB (works without libairspyhf).
pub fn usb_count() -> usize {
    nusb::list_devices()
        .wait()
        .map(|devs| {
            devs.filter(|d| d.vendor_id() == VID && d.product_id() == PID)
                .count()
        })
        .unwrap_or(0)
}

#[cfg(has_airspyhf)]
pub fn list_devices() -> usize {
    let n = unsafe { ffi::airspyhf_list_devices(ptr::null_mut(), 0) };
    if n > 0 {
        n as usize
    } else {
        usb_count()
    }
}

#[cfg(not(has_airspyhf))]
pub fn list_devices() -> usize {
    usb_count()
}

/// Pick sample rate / downsample closest to the FM demod's target MPX rate.
pub fn hf_settings_for_mpx(freq_hz: u32, supported: &[u32]) -> (RadioConfig, DemodConfig) {
    centered_iq_settings_with_rates(freq_hz, MPX_SAMPLE_RATE, supported)
}

#[cfg(has_airspyhf)]
mod ffi {
    use std::os::raw::{c_int, c_uint};

    #[repr(C)]
    pub struct ComplexFloat {
        pub re: f32,
        pub im: f32,
    }

    #[repr(C)]
    pub struct Transfer {
        pub device: *mut Device,
        pub ctx: *mut std::ffi::c_void,
        pub samples: *mut ComplexFloat,
        pub sample_count: c_int,
        pub dropped_samples: u64,
    }

    pub enum Device {}

    pub type SampleCb = extern "C" fn(*mut Transfer) -> c_int;

    extern "C" {
        pub fn airspyhf_list_devices(serials: *mut u64, count: c_int) -> c_int;
        pub fn airspyhf_open(device: *mut *mut Device) -> c_int;
        pub fn airspyhf_close(device: *mut Device) -> c_int;
        pub fn airspyhf_start(device: *mut Device, cb: SampleCb, ctx: *mut std::ffi::c_void) -> c_int;
        pub fn airspyhf_stop(device: *mut Device) -> c_int;
        pub fn airspyhf_set_freq(device: *mut Device, freq_hz: c_uint) -> c_int;
        pub fn airspyhf_get_samplerates(device: *mut Device, buffer: *mut c_uint, len: c_uint) -> c_int;
        pub fn airspyhf_set_samplerate(device: *mut Device, samplerate: c_uint) -> c_int;
        pub fn airspyhf_set_hf_agc(device: *mut Device, flag: u8) -> c_int;
        pub fn airspyhf_set_hf_att(device: *mut Device, value: u8) -> c_int;
        pub fn airspyhf_set_lib_dsp(device: *mut Device, flag: u8) -> c_int;
    }
}

#[cfg(has_airspyhf)]
struct StreamCtx {
    tx: Sender<Vec<u8>>,
}

#[cfg(has_airspyhf)]
extern "C" fn sample_callback(transfer: *mut ffi::Transfer) -> c_int {
    unsafe {
        let t = &*transfer;
        let ctx = &*(t.ctx as *const StreamCtx);
        let samples = std::slice::from_raw_parts(t.samples, t.sample_count as usize);
        let iq = complex_to_rtl_iq(samples);
        if ctx.tx.send(iq).is_err() {
            return -1;
        }
    }
    0
}

#[cfg(has_airspyhf)]
fn complex_to_rtl_iq(samples: &[ffi::ComplexFloat]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let i = ((s.re * 127.0).clamp(-127.0, 127.0) + 127.0) as u8;
        let q = ((s.im * 127.0).clamp(-127.0, 127.0) + 127.0) as u8;
        out.push(i);
        out.push(q);
    }
    out
}

#[cfg(has_airspyhf)]
pub struct AirspyHf {
    dev: *mut ffi::Device,
    supported_rates: Vec<u32>,
    stream_ctx: Option<*mut StreamCtx>,
}

#[cfg(has_airspyhf)]
impl AirspyHf {
    pub fn open_first() -> Result<Self, String> {
        let mut dev: *mut ffi::Device = ptr::null_mut();
        let rc = unsafe { ffi::airspyhf_open(&mut dev) };
        if rc != 0 || dev.is_null() {
            return Err(format_hf_error(rc));
        }

        let mut count: u32 = 0;
        let rc = unsafe { ffi::airspyhf_get_samplerates(dev, &mut count, 0) };
        if rc != 0 || count == 0 {
            unsafe {
                ffi::airspyhf_close(dev);
            }
            return Err("Failed to read Airspy HF+ sample rates".into());
        }

        let mut rates = vec![0u32; count as usize];
        let rc = unsafe { ffi::airspyhf_get_samplerates(dev, rates.as_mut_ptr(), count) };
        if rc != 0 {
            unsafe {
                ffi::airspyhf_close(dev);
            }
            return Err("Failed to read Airspy HF+ sample rates".into());
        }

        Ok(Self {
            dev,
            supported_rates: rates,
            stream_ctx: None,
        })
    }

    pub fn supported_rates(&self) -> &[u32] {
        &self.supported_rates
    }

    pub fn configure(&self, freq_hz: u32, sample_rate: u32, gain_db: i32) -> Result<(), String> {
        let rc = unsafe { ffi::airspyhf_set_samplerate(self.dev, sample_rate) };
        if rc != 0 {
            return Err(format_hf_error(rc));
        }

        let rc = unsafe { ffi::airspyhf_set_freq(self.dev, freq_hz) };
        if rc != 0 {
            return Err(format_hf_error(rc));
        }

        let _ = unsafe { ffi::airspyhf_set_lib_dsp(self.dev, 1) };
        self.apply_gain(gain_db)
    }

    pub fn set_freq(&self, freq_hz: u32) -> Result<(), String> {
        let rc = unsafe { ffi::airspyhf_set_freq(self.dev, freq_hz) };
        if rc != 0 {
            Err(format_hf_error(rc))
        } else {
            Ok(())
        }
    }

    pub fn set_gain(&self, gain_db: i32) -> Result<(), String> {
        self.apply_gain(gain_db)
    }

    fn apply_gain(&self, gain_db: i32) -> Result<(), String> {
        if gain_db < 0 {
            let rc = unsafe { ffi::airspyhf_set_hf_agc(self.dev, 1) };
            if rc != 0 {
                return Err(format_hf_error(rc));
            }
        } else {
            let rc = unsafe { ffi::airspyhf_set_hf_agc(self.dev, 0) };
            if rc != 0 {
                return Err(format_hf_error(rc));
            }
            let att = (gain_db / 6).clamp(0, 48) as u8;
            let rc = unsafe { ffi::airspyhf_set_hf_att(self.dev, att) };
            if rc != 0 {
                return Err(format_hf_error(rc));
            }
        }
        Ok(())
    }

    pub fn start(&mut self, tx: Sender<Vec<u8>>) -> Result<(), String> {
        let ctx = Box::new(StreamCtx { tx });
        let ctx_ptr = Box::into_raw(ctx);
        let rc = unsafe {
            ffi::airspyhf_start(self.dev, sample_callback, ctx_ptr as *mut c_void)
        };
        if rc != 0 {
            unsafe {
                drop(Box::from_raw(ctx_ptr));
            }
            return Err(format_hf_error(rc));
        }
        self.stream_ctx = Some(ctx_ptr);
        Ok(())
    }

    pub fn stop(&mut self) {
        unsafe {
            ffi::airspyhf_stop(self.dev);
        }
        if let Some(ctx_ptr) = self.stream_ctx.take() {
            unsafe {
                drop(Box::from_raw(ctx_ptr));
            }
        }
    }
}

#[cfg(has_airspyhf)]
impl Drop for AirspyHf {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            ffi::airspyhf_close(self.dev);
        }
    }
}

#[cfg(has_airspyhf)]
fn format_hf_error(rc: c_int) -> String {
    match rc {
        0 => "success".into(),
        -1 => "Airspy HF+ I/O error (device busy or unplugged?)".into(),
        n => format!("Airspy HF+ error ({n})"),
    }
}

#[cfg(not(has_airspyhf))]
pub fn missing_lib_message() -> &'static str {
    "Airspy HF+ detected but libairspyhf is not available.\n\
     macOS: brew install airspyhf\n\
     Linux: sudo apt install libairspyhf-dev"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hf_usb_enumeration() {
        let n = usb_count();
        eprintln!("Airspy HF+ usb_count = {n}");
    }

    #[cfg(has_airspyhf)]
    #[test]
    fn hf_open_and_rates() {
        if usb_count() == 0 {
            eprintln!("skip: no HF+ connected");
            return;
        }
        let hf = AirspyHf::open_first().expect("open HF+");
        eprintln!("supported rates: {:?}", hf.supported_rates());
        assert!(!hf.supported_rates().is_empty());
    }
}
