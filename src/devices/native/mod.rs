//! Direct vendor-library bindings for URH's native device backends.
//!
//! Port of `urh.dev.native.*` (the Cython layer in `src/urh/dev/native/lib/`
//! plus the per-device wrappers `HackRF.py`, `RTLSDR.py`, ...).
//!
//! Every backend loads its vendor shared library at **runtime** via
//! [`libloading`], so this crate compiles and runs on machines without any
//! SDR SDK installed; [`DeviceError::LibraryUnavailable`] is returned when a
//! library is missing. The only exception is [`soundcard`], which is built on
//! the `cpal` crate and statically linked.
//!
//! - [`hackrf`]: libhackrf (also covers the rad1o, which uses the same
//!   library — see `Rad1o.py`)
//! - [`rtlsdr`]: librtlsdr
//! - [`airspy`]: libairspy
//! - [`bladerf`]: libbladeRF
//! - [`limesdr`]: libLimeSuite
//! - [`plutosdr`]: libiio
//! - [`usrp`]: libuhd
//! - [`sdrplay`]: libmirsdrapi-rsp
//! - [`soundcard`]: cpal (no dlopen)

pub mod airspy;
pub mod bladerf;
pub mod hackrf;
pub mod limesdr;
pub mod plutosdr;
pub mod rtlsdr;
pub mod sdrplay;
pub mod soundcard;
pub mod usrp;

use crate::devices::{Device, DeviceError};

/// All native backend names accepted by [`open_by_name`] (lowercase,
/// as used by URH's `BackendHandler` device-name matching).
pub const BACKENDS: &[&str] = &[
    "hackrf",
    "rad1o",
    "rtlsdr",
    "airspy",
    "bladerf",
    "limesdr",
    "plutosdr",
    "usrp",
    "sdrplay",
];

/// Static registry entry describing one native backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeBackend {
    /// canonical lowercase name (also matches "rad1o" for the hackrf lib)
    pub name: &'static str,
    /// shared-library file names tried (in order) when dlopening
    pub libraries: &'static [&'static str],
}

/// The static backend registry — simplified port of the device-discovery
/// part of `BackendHandler` (no extension scanning, native backends only).
pub fn registry() -> Vec<NativeBackend> {
    vec![
        NativeBackend {
            name: "hackrf",
            libraries: &[
                "libhackrf.0.dylib",
                "libhackrf.dylib",
                "libhackrf.so.0",
                "libhackrf.so",
                "hackrf.dll",
                "hackrf.0.dll",
            ],
        },
        NativeBackend {
            name: "rad1o",
            libraries: &[
                "libhackrf.0.dylib",
                "libhackrf.dylib",
                "libhackrf.so.0",
                "libhackrf.so",
                "hackrf.dll",
                "hackrf.0.dll",
            ],
        },
        NativeBackend {
            name: "rtlsdr",
            libraries: &[
                "librtlsdr.0.dylib",
                "librtlsdr.dylib",
                "librtlsdr.so.0",
                "librtlsdr.so",
                "rtlsdr.dll",
            ],
        },
        NativeBackend {
            name: "airspy",
            libraries: &[
                "libairspy.0.dylib",
                "libairspy.dylib",
                "libairspy.so.0",
                "libairspy.so",
                "airspy.dll",
            ],
        },
        NativeBackend {
            name: "bladerf",
            libraries: &[
                "libbladeRF.2.dylib",
                "libbladeRF.dylib",
                "libbladeRF.so.2",
                "libbladeRF.so",
                "bladeRF.dll",
            ],
        },
        NativeBackend {
            name: "limesdr",
            libraries: &[
                "libLimeSuite.23.11-1.dylib",
                "libLimeSuite.dylib",
                "libLimeSuite.so",
                "LimeSuite.dll",
            ],
        },
        NativeBackend {
            name: "plutosdr",
            libraries: &[
                "libiio.0.dylib",
                "libiio.dylib",
                "libiio.so.0",
                "libiio.so",
                "libiio.dll",
                "iio.dll",
            ],
        },
        NativeBackend {
            name: "usrp",
            libraries: &[
                "libuhd.410.dylib",
                "libuhd.dylib",
                "libuhd.so.40.0.0",
                "libuhd.so",
                "uhd.dll",
            ],
        },
        NativeBackend {
            name: "sdrplay",
            libraries: &[
                "libmirsdrapi-rsp.3.dylib",
                "libmirsdrapi-rsp.dylib",
                "libmirsdrapi-rsp.so",
                "mirsdrapi-rsp.dll",
            ],
        },
    ]
}

/// Check whether the vendor library for `name` can be loaded right now.
pub fn library_available(name: &str) -> bool {
    let names: &[&str] = match name {
        "hackrf" | "rad1o" => &[
            "libhackrf.0.dylib",
            "libhackrf.dylib",
            "libhackrf.so.0",
            "libhackrf.so",
            "hackrf.dll",
        ],
        "rtlsdr" => &[
            "librtlsdr.0.dylib",
            "librtlsdr.dylib",
            "librtlsdr.so.0",
            "librtlsdr.so",
            "rtlsdr.dll",
        ],
        _ => match registry().iter().find(|b| b.name == name) {
            Some(b) => b.libraries,
            None => return false,
        },
    };
    names.iter().any(|n| {
        // SAFETY: loading a shared library by name only runs its
        // constructors; we immediately drop it and never use any symbol.
        unsafe { libloading::Library::new(n).is_ok() }
    })
}

/// Open a native device by backend name (simplified `BackendHandler`
/// factory). `identifier` is the USB serial / URI where the backend supports
/// multi-device selection.
///
/// Returns [`DeviceError::Unsupported`] for unknown names and
/// [`DeviceError::LibraryUnavailable`] when the vendor library is missing.
/// The sound card backend is not a [`Device`] and must be opened through
/// [`soundcard::SoundCardDevice`] directly.
pub fn open_by_name(name: &str, identifier: Option<&str>) -> Result<Box<dyn Device>, DeviceError> {
    let id = identifier.filter(|s| !s.is_empty());
    match name.to_lowercase().as_str() {
        "hackrf" | "rad1o" => Ok(Box::new(hackrf::HackRfDevice::open(id)?)),
        "rtlsdr" => Ok(Box::new(rtlsdr::RtlSdrDevice::open(id)?)),
        "airspy" => Ok(Box::new(airspy::AirSpyDevice::open(id)?)),
        "bladerf" => Ok(Box::new(bladerf::BladeRfDevice::open(id)?)),
        "limesdr" => Ok(Box::new(limesdr::LimeSdrDevice::open(id)?)),
        "plutosdr" => Ok(Box::new(plutosdr::PlutoSdrDevice::open(id)?)),
        "usrp" => Ok(Box::new(usrp::UsrpDevice::open(id)?)),
        "sdrplay" => Ok(Box::new(sdrplay::SdrPlayDevice::open(id)?)),
        _ => Err(DeviceError::Unsupported("unknown native backend")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_all_nine_backends() {
        let reg = registry();
        assert_eq!(reg.len(), 9);
        for expected in BACKENDS {
            assert!(reg.iter().any(|b| b.name == *expected), "missing {expected}");
        }
    }

    #[test]
    fn registry_names_are_unique_and_lowercase() {
        let reg = registry();
        for (i, b) in reg.iter().enumerate() {
            assert!(b.libraries.iter().all(|l| !l.is_empty()));
            assert!(b.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
            assert!(!reg[..i].iter().any(|o| o.name == b.name), "duplicate {}", b.name);
        }
    }

    #[test]
    fn library_names_have_platform_shape() {
        // every entry carries at least one unix-style and one windows-style name
        for b in registry() {
            assert!(
                b.libraries.iter().any(|l| l.ends_with(".dylib") || l.ends_with(".so") || l.contains(".so.")),
                "{} has no unix library name",
                b.name
            );
            assert!(b.libraries.iter().any(|l| l.ends_with(".dll")), "{} has no dll name", b.name);
        }
    }

    #[test]
    fn open_by_name_unknown_backend_is_unsupported() {
        assert!(matches!(
            open_by_name("does-not-exist", None),
            Err(DeviceError::Unsupported(_))
        ));
        assert!(matches!(
            open_by_name("soundcard", None),
            Err(DeviceError::Unsupported(_))
        ));
    }

    #[test]
    fn open_by_name_missing_library_is_not_a_panic() {
        // On a machine without the SDKs this must return LibraryUnavailable
        // (or succeed if the lib happens to be installed) — never panic.
        for name in BACKENDS {
            if !library_available(name) {
                match open_by_name(name, None) {
                    Err(DeviceError::LibraryUnavailable(_)) => {}
                    Err(DeviceError::OpenFailed(_)) => {} // lib present, no hardware
                    Err(_) => panic!("{name} returned an unexpected error"),
                    Ok(_) => {} // library actually installed and device opened
                }
            }
        }
    }
}
