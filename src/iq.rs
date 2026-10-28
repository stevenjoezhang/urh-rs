//! IQ sample container and file I/O, ported from URH's
//! `signalprocessing/IQArray.py` and the pure functions of `cythonext/util.pyx`.
//!
//! Semantics note: URH reads every integer format and normalizes it to `float32`
//! on load (see `IQArray.convert_to`); writes quantize back with scale 127/32767
//! (not 128/32768), exactly as the Python reference does. A same-format
//! read-write-read roundtrip is therefore slightly lossy in URH itself; see the
//! tests for the tolerance we assert.

use num_complex::Complex32;
use std::fs;
use std::io;

/// On-disk IQ format, determined by file extension (URH convention:
/// the number in the extension is bits per *sample pair*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqFormat {
    /// `.complex` — interleaved float32
    ComplexF32,
    /// `.complex16s` / `.cs8` — interleaved int8
    Complex16S,
    /// `.complex16u` / `.cu8` — interleaved uint8
    Complex16U,
    /// `.complex32s` / `.cs16` — interleaved int16
    Complex32S,
    /// `.complex32u` / `.cu16` — interleaved uint16
    Complex32U,
}

impl IqFormat {
    /// Infer the format from a filename's extension, defaulting to float32
    /// like `IQArray.from_file` does.
    pub fn from_filename(filename: &str) -> IqFormat {
        let name = filename.to_ascii_lowercase();
        if name.ends_with(".complex16u") || name.ends_with(".cu8") {
            IqFormat::Complex16U
        } else if name.ends_with(".complex16s") || name.ends_with(".cs8") {
            IqFormat::Complex16S
        } else if name.ends_with(".complex32u") || name.ends_with(".cu16") {
            IqFormat::Complex32U
        } else if name.ends_with(".complex32s") || name.ends_with(".cs16") {
            IqFormat::Complex32S
        } else {
            IqFormat::ComplexF32
        }
    }

    /// Bytes per complex sample (I + Q).
    pub fn bytes_per_sample(self) -> usize {
        match self {
            IqFormat::ComplexF32 => 8,
            IqFormat::Complex32S | IqFormat::Complex32U => 4,
            IqFormat::Complex16S | IqFormat::Complex16U => 2,
        }
    }
}

/// In-memory IQ array, always stored as normalized `complex64` samples.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IqArray {
    data: Vec<Complex32>,
}

impl IqArray {
    /// A zero-initialized array of `n` samples.
    pub fn zeros(n: usize) -> Self {
        IqArray {
            data: vec![Complex32::default(); n],
        }
    }

    /// Wrap existing complex samples.
    pub fn from_vec(data: Vec<Complex32>) -> Self {
        IqArray { data }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Complex32> {
        self.data.iter()
    }

    pub fn as_slice(&self) -> &[Complex32] {
        &self.data
    }

    pub fn as_mut_slice(&mut self) -> &mut [Complex32] {
        &mut self.data
    }

    pub fn into_vec(self) -> Vec<Complex32> {
        self.data
    }

    /// Read an IQ file, normalizing integer data to float32 as `IQArray.from_file`
    /// does. A trailing half sample (odd number of component values) is dropped,
    /// matching `IQArray.convert_array_to_iq`.
    pub fn from_file(filename: &str) -> io::Result<Self> {
        let bytes = fs::read(filename)?;
        Ok(Self::from_bytes(&bytes, IqFormat::from_filename(filename)))
    }

    /// Write to a file, inferring the format from the filename's extension.
    pub fn to_file(&self, filename: &str) -> io::Result<()> {
        self.to_file_with_format(filename, IqFormat::from_filename(filename))
    }

    /// Write to a file using an explicit format.
    pub fn to_file_with_format(&self, filename: &str, format: IqFormat) -> io::Result<()> {
        fs::write(filename, self.to_bytes(format))
    }

    /// Parse raw bytes in the given format into normalized samples.
    pub fn from_bytes(bytes: &[u8], format: IqFormat) -> Self {
        let data = match format {
            IqFormat::ComplexF32 => bytes
                .chunks_exact(8)
                .map(|c| {
                    Complex32::new(
                        f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                        f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                    )
                })
                .collect(),
            IqFormat::Complex16S => bytes
                .chunks_exact(2)
                .map(|c| {
                    Complex32::new(
                        i8::from_le_bytes([c[0]]) as f32 / 128.0,
                        i8::from_le_bytes([c[1]]) as f32 / 128.0,
                    )
                })
                .collect(),
            IqFormat::Complex16U => bytes
                .chunks_exact(2)
                .map(|c| {
                    // uint8 is first converted to int8 (value - 128), then divided by 128
                    Complex32::new(
                        (c[0] as i16 - 128) as f32 / 128.0,
                        (c[1] as i16 - 128) as f32 / 128.0,
                    )
                })
                .collect(),
            IqFormat::Complex32S => bytes
                .chunks_exact(4)
                .map(|c| {
                    Complex32::new(
                        i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0,
                        i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0,
                    )
                })
                .collect(),
            IqFormat::Complex32U => bytes
                .chunks_exact(4)
                .map(|c| {
                    Complex32::new(
                        (u16::from_le_bytes([c[0], c[1]]) as i32 - 32768) as f32 / 32768.0,
                        (u16::from_le_bytes([c[2], c[3]]) as i32 - 32768) as f32 / 32768.0,
                    )
                })
                .collect(),
        };
        IqArray { data }
    }

    /// Serialize to raw bytes in the given format, quantizing with the scales
    /// used by `IQArray.convert_to` (127 / 32767 for signed, offset by 1.0 for
    /// unsigned; `as` truncates toward zero like numpy's `.astype`).
    pub fn to_bytes(&self, format: IqFormat) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() * format.bytes_per_sample());
        for s in &self.data {
            match format {
                IqFormat::ComplexF32 => {
                    out.extend_from_slice(&s.re.to_le_bytes());
                    out.extend_from_slice(&s.im.to_le_bytes());
                }
                IqFormat::Complex16S => {
                    out.push((s.re * 127.0) as i8 as u8);
                    out.push((s.im * 127.0) as i8 as u8);
                }
                IqFormat::Complex16U => {
                    out.push(((s.re + 1.0) * 127.0) as u8);
                    out.push(((s.im + 1.0) * 127.0) as u8);
                }
                IqFormat::Complex32S => {
                    out.extend_from_slice(&((s.re * 32767.0) as i16).to_le_bytes());
                    out.extend_from_slice(&((s.im * 32767.0) as i16).to_le_bytes());
                }
                IqFormat::Complex32U => {
                    out.extend_from_slice(&(((s.re + 1.0) * 32767.0) as u16).to_le_bytes());
                    out.extend_from_slice(&(((s.im + 1.0) * 32767.0) as u16).to_le_bytes());
                }
            }
        }
        out
    }

    /// Magnitude per sample, ported from `util.pyx get_magnitudes` (f64 result).
    pub fn magnitudes(&self) -> Vec<f64> {
        get_magnitudes(&self.data)
    }
}

impl std::ops::Index<usize> for IqArray {
    type Output = Complex32;

    fn index(&self, index: usize) -> &Complex32 {
        &self.data[index]
    }
}

impl<'a> IntoIterator for &'a IqArray {
    type Item = &'a Complex32;
    type IntoIter = std::slice::Iter<'a, Complex32>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

/// Minimum and maximum of a real-valued array; `(0, 0)` for empty input,
/// as in `util.pyx minmax`.
pub fn minmax(arr: &[f32]) -> (f32, f32) {
    if arr.is_empty() {
        return (0.0, 0.0);
    }
    let mut minimum = arr[0];
    let mut maximum = arr[0];
    for &e in &arr[1..] {
        if e > maximum {
            maximum = e;
        }
        if e < minimum {
            minimum = e;
        }
    }
    (minimum, maximum)
}

/// `10 * log10(re^2 + im^2)` per sample, ported from `util.pyx arr2decibel`.
/// Zero-magnitude samples yield `-inf`, matching the reference.
pub fn arr2decibel(arr: &[Complex32]) -> Vec<f32> {
    arr.iter()
        .map(|c| 10.0 * (c.re * c.re + c.im * c.im).log10())
        .collect()
}

/// Euclidean magnitude per sample (f64), ported from `util.pyx get_magnitudes`.
pub fn get_magnitudes(arr: &[Complex32]) -> Vec<f64> {
    arr.iter()
        .map(|c| ((c.re as f64) * (c.re as f64) + (c.im as f64) * (c.im as f64)).sqrt())
        .collect()
}

/// Ported from `util.pyx bit_array_to_number` (MSB-first region, i.e. bits are
/// accumulated from index `end - 1` downwards). Returns 0 when `end < 1`.
pub fn bit_array_to_number(bits: &[u8], end: i64, start: i64) -> u64 {
    if end < 1 {
        return 0;
    }
    let mut result: u64 = 0;
    let mut acc: u64 = 1;
    for i in start..end {
        result += bits[(end - 1 - i + start) as usize] as u64 * acc;
        acc *= 2;
    }
    result
}

/// Ported from `util.pyx arr_to_number`: with `reverse == false` the first bit
/// is the most significant one; with `reverse == true` it is the least
/// significant. `start` skips a leading prefix.
pub fn arr_to_number(inpt: &[u8], reverse: bool, start: usize) -> u64 {
    let len_inpt = inpt.len();
    let mut result: u64 = 0;
    for i in start..len_inpt {
        if !reverse {
            if inpt[len_inpt - 1 - i + start] != 0 {
                result |= 1 << (i - start);
            }
        } else if inpt[i] != 0 {
            result |= 1 << (i - start);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex;

    const DATA_DIR: &str = "/Users/mimi/urh-ref/tests/data/";

    #[test]
    fn test_bit_array_to_number() {
        // MSB is the bit at end-1: bits[end-1] * 1 + bits[end-2] * 2 + ...
        assert_eq!(bit_array_to_number(&[0, 1, 1], 3, 0), 3);
        assert_eq!(bit_array_to_number(&[1, 1, 0, 0], 4, 0), 12);
        assert_eq!(bit_array_to_number(&[1, 0, 1, 1], 4, 0), 11);
        // sub-range start..end: i in 1..3 -> bits[2]*1 + bits[1]*2 = 1
        assert_eq!(bit_array_to_number(&[1, 0, 1, 1], 3, 1), 1);
        assert_eq!(bit_array_to_number(&[1, 1, 1, 0], 3, 0), 7);
        // end < 1 -> 0
        assert_eq!(bit_array_to_number(&[1, 1], 0, 0), 0);
    }

    #[test]
    fn test_arr_to_number() {
        // forward: first bit is MSB
        assert_eq!(arr_to_number(&[0, 1, 1], false, 0), 3);
        assert_eq!(arr_to_number(&[1, 1, 0, 0], false, 0), 12);
        assert_eq!(arr_to_number(&[1, 0, 1, 1], false, 0), 11);
        // reverse: first bit is LSB
        assert_eq!(arr_to_number(&[0, 1, 1], true, 0), 6);
        assert_eq!(arr_to_number(&[1, 1, 0, 0], true, 0), 3);
        // start prefix handling
        assert_eq!(arr_to_number(&[1, 1, 0, 0], false, 1), 4);
    }

    #[test]
    fn test_minmax() {
        assert_eq!(minmax(&[3.0, -1.0, 2.0]), (-1.0, 3.0));
        assert_eq!(minmax(&[5.5]), (5.5, 5.5));
        assert_eq!(minmax(&[]), (0.0, 0.0));
    }

    #[test]
    fn test_arr2decibel() {
        let arr = vec![Complex::new(1.0f32, 1.0f32), Complex::new(2.0, 0.0)];
        let db = arr2decibel(&arr);
        assert!((db[0] - 10.0 * (2.0f32).log10()).abs() < 1e-5);
        assert!((db[1] - 10.0 * (4.0f32).log10()).abs() < 1e-5);
        // 0 -> -inf, matching log10(0)
        assert!(arr2decibel(&[Complex::new(0.0, 0.0)])[0].is_infinite());
    }

    #[test]
    fn test_get_magnitudes() {
        let arr = vec![Complex::new(3.0f32, 4.0f32), Complex::new(0.0, 0.0)];
        let m = get_magnitudes(&arr);
        assert!((m[0] - 5.0).abs() < 1e-12);
        assert_eq!(m[1], 0.0);
    }

    #[test]
    fn test_format_detection() {
        assert_eq!(IqFormat::from_filename("a.complex"), IqFormat::ComplexF32);
        assert_eq!(
            IqFormat::from_filename("a.COMPLEX16S"),
            IqFormat::Complex16S
        );
        assert_eq!(IqFormat::from_filename("a.cs8"), IqFormat::Complex16S);
        assert_eq!(IqFormat::from_filename("a.cu8"), IqFormat::Complex16U);
        assert_eq!(IqFormat::from_filename("a.cs16"), IqFormat::Complex32S);
        assert_eq!(IqFormat::from_filename("a.cu16"), IqFormat::Complex32U);
        assert_eq!(IqFormat::from_filename("a.txt"), IqFormat::ComplexF32);
    }

    #[test]
    fn test_int_roundtrip_synthetic() {
        // int8: -128..127 maps to [-1, 127/128]
        let arr = IqArray::from_vec(vec![
            Complex::new(-1.0f32, 0.0),
            Complex::new(127.0 / 128.0, -127.0 / 128.0),
        ]);
        let bytes = arr.to_bytes(IqFormat::Complex16S);
        // int8 write: (v * 127) truncated toward zero, like numpy .astype
        assert_eq!(bytes, vec![0x81, 0x00, 0x7E, 0x82]);
        let back = IqArray::from_bytes(&bytes, IqFormat::Complex16S);
        for (a, b) in arr.iter().zip(back.iter()) {
            assert!((a - b).norm() < std::f32::consts::SQRT_2 / 126.0);
        }

        // uint8: 0..255 -> [-1, 127/128]
        let arr = IqArray::from_vec(vec![
            Complex::new(-1.0f32, -1.0),
            Complex::new(1.0, 0.0),
        ]);
        let bytes = arr.to_bytes(IqFormat::Complex16U);
        assert_eq!(bytes, vec![0x00, 0x00, 0xFE, 0x7F]);
        let back = IqArray::from_bytes(&bytes, IqFormat::Complex16U);
        for (a, b) in arr.iter().zip(back.iter()) {
            // uint8 loses up to one step on write (scale 127) and one on the
            // unsigned offset read (v/128 - 1), per component
            assert!((a - b).norm() < 2.0 * std::f32::consts::SQRT_2 / 127.0);
        }
    }

    #[test]
    fn test_trailing_half_sample_dropped() {
        // 5 int8 component values -> 2 samples, last byte ignored
        let arr = IqArray::from_bytes(&[1, 2, 3, 4, 5], IqFormat::Complex16S);
        assert_eq!(arr.len(), 2);
        // 20 bytes = 10 int16 components -> 5 samples
        let bytes: Vec<u8> = (0..10u16).flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(bytes.len(), 20);
        let arr = IqArray::from_bytes(&bytes, IqFormat::Complex32S);
        assert_eq!(arr.len(), 5);
        // 9 float32 bytes -> dropped entirely
        let arr = IqArray::from_bytes(&[0u8; 9], IqFormat::ComplexF32);
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn test_complex_f32_roundtrip() {
        let path = format!("{}ask.complex", DATA_DIR);
        let arr = IqArray::from_file(&path).unwrap();
        let file_size = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(arr.len(), file_size / 8);
        assert!(arr.magnitudes().iter().all(|m| m.is_finite()));
        assert!(arr.magnitudes().iter().any(|m| *m > 0.01));

        // f32 roundtrip is lossless
        let tmp = std::env::temp_dir().join("urh_rs_test_ask.complex");
        arr.to_file(tmp.to_str().unwrap()).unwrap();
        let back = IqArray::from_file(tmp.to_str().unwrap()).unwrap();
        assert_eq!(arr, back);
        std::fs::remove_file(tmp).unwrap();
    }

    #[test]
    fn test_complex16s_roundtrip() {
        for name in ["elektromaten.complex16s", "esaver.complex16s"] {
            let path = format!("{}{}", DATA_DIR, name);
            let arr = IqArray::from_file(&path).unwrap();
            let file_size = std::fs::metadata(&path).unwrap().len() as usize;
            // int8: 2 bytes per complex sample
            assert_eq!(arr.len(), file_size / 2);
            assert!(arr.len() > 0);
            assert!(arr.magnitudes().iter().any(|m| *m > 0.01));

            let tmp = std::env::temp_dir().join("urh_rs_test_16s.complex16s");
            arr.to_file(tmp.to_str().unwrap()).unwrap();
            let back = IqArray::from_file(tmp.to_str().unwrap()).unwrap();
            assert_eq!(back.len(), arr.len());
            // URH's int8 write uses scale 127 (not 128), so the roundtrip is
            // quantization-lossy by at most one LSB step per component.
            for (a, b) in arr.iter().zip(back.iter()) {
                assert!(
                    (a - b).norm() < std::f32::consts::SQRT_2 / 126.0,
                    "name={}",
                    name
                );
            }
            std::fs::remove_file(tmp).unwrap();
        }
    }

    #[test]
    fn test_complex32s_roundtrip() {
        let path = format!("{}homematic.complex32s", DATA_DIR);
        let arr = IqArray::from_file(&path).unwrap();
        let file_size = std::fs::metadata(&path).unwrap().len() as usize;
        // int16: 4 bytes per complex sample
        assert_eq!(arr.len(), file_size / 4);
        assert!(arr.len() > 0);
        assert!(arr.magnitudes().iter().any(|m| *m > 0.01));

        let tmp = std::env::temp_dir().join("urh_rs_test_32s.complex32s");
        arr.to_file(tmp.to_str().unwrap()).unwrap();
        let back = IqArray::from_file(tmp.to_str().unwrap()).unwrap();
        assert_eq!(back.len(), arr.len());
        // scale 32767 on write -> error below one int16 LSB step
        for (a, b) in arr.iter().zip(back.iter()) {
            assert!((a - b).norm() < 2.0 / 32767.0);
        }
        std::fs::remove_file(tmp).unwrap();
    }
}
