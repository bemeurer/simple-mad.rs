/*!
This crate provides an interface to libmad, the MPEG audio decoding library.

To begin, create a `Decoder` from a byte-oriented source using `Decoder::decode`
or `Decoder::decode_interval`. Fetch results using `get_frame` or the `Iterator`
interface. MP3 files often begin or end with metadata, which will cause libmad
to produce errors. It is safe to ignore these errors until libmad reaches the
start of the audio data or the end of the file.

# Examples
```no_run
#![allow(unused_variables)]
use simplemad::{Decoder, Frame};
use std::fs::File;
use std::path::Path;

let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
let file = File::open(&path).unwrap();
let file_b = File::open(&path).unwrap();
let decoder = Decoder::decode(file).unwrap();

for decoding_result in decoder {
    match decoding_result {
        Err(e) => println!("Error: {:?}", e),
        Ok(frame) => {
            println!("Frame sample rate: {}", frame.sample_rate);
            println!("First audio sample (left channel): {:?}", frame.samples[0][0]);
            println!("First audio sample (right channel): {:?}", frame.samples[1][0]);
        },
    }
}

// Decode the interval from 1s to 2s (to the nearest frame),
let partial_decoder = Decoder::decode_interval(file_b, 1_000_f64, 2_000_f64);
let frames: Vec<Frame> = partial_decoder.unwrap()
                                        .filter_map(|r| match r {
                                            Ok(f) => Some(f),
                                            Err(_) => None})
                                        .collect();
```
*/

#![crate_name = "simplemad"]

extern crate simplemad_sys;

use std::io;
use std::io::Read;
use std::default::Default;
use std::cmp::{min, max};
use simplemad_sys::*;

/// A decoded frame
#[derive(Clone, Debug)]
pub struct Frame {
    /// Number of samples per second
    pub sample_rate: u32,
    /// Stream bit rate
    pub bit_rate: u32,
    /// Audio layer (I, II or III)
    pub layer: MadLayer,
    /// Single Channel, Dual Channel, Joint Stereo or Stereo
    pub mode: MadMode,
    /// Samples are organized into a vector of channels. For
    /// stereo, the left channel is channel 0.
    pub samples: Vec<Vec<MadFixed32>>,
    /// The duration of the frame in milliseconds
    pub duration: f32,
    /// The position in milliseconds at the start of the frame
    pub position: f64,
}

/// An interface for the decoding operation
///
/// Create a decoder using `decode` or `decode_interval`. Fetch
/// results with `get_frame` or the `Iterator` interface.
pub struct Decoder<R> where R: io::Read {
    reader: R,
    buffer: Box<[u8; 32_768]>,
    headers_only: bool,
    stream: MadStream,
    synth: MadSynth,
    frame: MadFrame,
    start_ms: Option<f64>,
    end_ms: Option<f64>,
    position_ms: f64,
}

impl<R> Decoder<R> where R: io::Read {
    fn new(reader: R,
           start_ms: Option<f64>,
           end_ms: Option<f64>,
           headers_only: bool) -> Result<Decoder<R>, SimplemadError> {
        let mut new_decoder =
            Decoder {
                reader: reader,
                buffer: Box::new([0u8; 32_768]),
                headers_only: headers_only,
                stream: Default::default(),
                synth: Default::default(),
                frame: Default::default(),
                start_ms: start_ms,
                end_ms: end_ms,
                position_ms: 0.0,
            };

        let bytes_read = try!(new_decoder.reader.read(&mut *new_decoder.buffer));

        unsafe {
            mad_stream_init(&mut new_decoder.stream);
            mad_frame_init(&mut new_decoder.frame);
            mad_synth_init(&mut new_decoder.synth);
            mad_stream_buffer(&mut new_decoder.stream,
                              new_decoder.buffer.as_ptr(),
                              bytes_read as c_ulong);
        }

        Ok(new_decoder)
    }

    /// Decode a file in full
    pub fn decode(reader: R) -> Result<Decoder<R>, SimplemadError> {
        Decoder::new(reader, None, None, false)
    }

    /// Decode only the header information of each frame
    pub fn decode_headers(reader: R) -> Result<Decoder<R>, SimplemadError> {
        Decoder::new(reader, None, None, true)
    }

    /// Decode part of a file from `start_time` to `end_time`, measured in milliseconds
    pub fn decode_interval(reader: R, start_time: f64, end_time: f64)
            -> Result<Decoder<R>, SimplemadError> {
        Decoder::new(reader, Some(start_time), Some(end_time), false)
    }

    /// Get the next decoding result, either a `Frame` or a `SimplemadError`
    pub fn get_frame(&mut self) -> Result<Frame, SimplemadError> {
        match self.start_ms {
            Some(t) if self.position_ms < t => { return self.seek_to_start() },
            _ => {},
        }

        match self.end_ms {
            Some(t) if self.position_ms > t => { return Err(SimplemadError::EOF) },
            _ => {},
        }

        let decoding_result =
            if self.headers_only {
                self.decode_header_only()
            } else {
                self.decode_frame()
            };

        match decoding_result {
            Ok(frame) => {
                self.position_ms += frame_duration(&self.frame);
                Ok(frame)
            },
            Err(SimplemadError::Mad(MadError::BufLen)) => {
                // Refill buffer and try again
                self.stream.error = MadError::None;
                match self.refill_buffer() {
                    Err(e) => Err(SimplemadError::Read(e)),
                    Ok(0) => Err(SimplemadError::EOF),
                    Ok(_) => self.get_frame(),
                }
            },
            Err(SimplemadError::Mad(e)) => {
                if error_is_recoverable(&e) {
                    self.stream.error = MadError::None;
                }
                Err(SimplemadError::Mad(e))
            },
            Err(e) => Err(e),
        }
    }

    fn seek_to_start(&mut self) -> Result<Frame, SimplemadError> {
        match self.start_ms {
            None => {},
            Some(start_time) => {
                while self.position_ms < start_time {
                    match self.decode_header_only() {
                        Ok(frame) => { self.position_ms += frame.duration as f64 },
                        Err(SimplemadError::Mad(MadError::BufLen)) => {
                            match self.refill_buffer() {
                                Ok(0) => { return Err(SimplemadError::EOF) },
                                Err(e) => { return Err(SimplemadError::Read(e)) },
                                Ok(_) => { },
                            }
                        },
                        Err(e) => return Err(e),
                    }
                }
            },
        }

        self.get_frame()
    }

    fn decode_header_only(&mut self) -> Result<Frame, SimplemadError> {
        unsafe {
            mad_header_decode(&mut self.frame.header, &mut self.stream);
        }

        let error = self.stream.error.clone();

        if error == MadError::None {
            let frame =
                Frame {sample_rate: self.frame.header.sample_rate as u32,
                       mode: self.frame.header.mode.clone(),
                       layer: self.frame.header.layer.clone(),
                       bit_rate: self.frame.header.bit_rate as u32,
                       samples: Vec::new(),
                       duration: frame_duration(&self.frame) as f32,
                       position: self.position_ms};
            Ok(frame)
        } else if error_is_recoverable(&error) {
            self.stream.error = MadError::None;
            Err(SimplemadError::Mad(error))
        } else {
            Err(SimplemadError::Mad(error))
        }
    }

    fn decode_frame(&mut self) -> Result<Frame, SimplemadError> {
        unsafe {
            mad_frame_decode(&mut self.frame, &mut self.stream);
        }

        if self.stream.error != MadError::None {
            return Err(SimplemadError::Mad(self.stream.error.clone()));
        }

        unsafe {
            mad_synth_frame(&mut self.synth, &mut self.frame);
        }

        if self.stream.error != MadError::None {
            return Err(SimplemadError::Mad(self.stream.error.clone()));
        }

        let pcm = &self.synth.pcm;
        let mut samples: Vec<Vec<MadFixed32>> = Vec::new();

        for channel_idx in 0..pcm.channels as usize {
            let mut channel = Vec::with_capacity(pcm.length as usize);
            for sample_idx in 0..pcm.length as usize {
                channel.push(
                    MadFixed32::from(pcm.samples[channel_idx][sample_idx])
                );
            }
            samples.push(channel);
        }

        let frame =
            Frame {sample_rate: pcm.sample_rate as u32,
                   duration: frame_duration(&self.frame) as f32,
                   mode: self.frame.header.mode.clone(),
                   layer: self.frame.header.layer.clone(),
                   bit_rate: self.frame.header.bit_rate as u32,
                   position: self.position_ms,
                   samples: samples};
        Ok(frame)
    }

    fn refill_buffer(&mut self) -> Result<usize, io::Error> {
        let buffer_len = self.buffer.len();
        let next_frame_position =
            (self.stream.next_frame - self.stream.buffer) as usize;
        let unused_byte_count =
            buffer_len - min(next_frame_position, buffer_len);

        // Shift unused data to front of buffer
        for idx in 0 .. unused_byte_count {
            self.buffer[idx] = self.buffer[idx + next_frame_position];
        }

        // Refill rest of buffer
        let mut free_region_start = unused_byte_count;
        while free_region_start != buffer_len {
            let slice = &mut self.buffer[free_region_start..buffer_len];
            match self.reader.read(slice) {
                Err(e) => return Err(e),
                Ok(0) => break,
                Ok(n) => free_region_start += n,
            }
        }

        unsafe {
            mad_stream_buffer(&mut self.stream,
                              self.buffer.as_ptr(),
                              free_region_start as c_ulong);
        }

        // Suppress BufLen error since buffer was refilled
        if self.stream.error == MadError::BufLen {
            self.stream.error = MadError::None;
        }

        let bytes_read = free_region_start - unused_byte_count;
        Ok(bytes_read)
    }
}

impl<R> Iterator for Decoder<R> where R: io::Read {
    type Item = Result<Frame, SimplemadError>;
    fn next(&mut self) -> Option<Result<Frame, SimplemadError>> {
        if !error_is_recoverable(&self.stream.error) {
            return None;
        }

        match self.get_frame() {
            Ok(f) => Some(Ok(f)),
            Err(SimplemadError::EOF) => None,
            Err(e) => {
                Some(Err(e))
            }
        }
    }
}

impl<R> Drop for Decoder<R> where R: io::Read {
    fn drop(&mut self) {
        unsafe {
            mad_stream_finish(&mut self.stream);
            mad_frame_finish(&mut self.frame);
            // mad_synth_finish is present in the libmad docs
            // but is defined as nothing in the library
            // mad_synth_finish(&mut self.synth);
        }
    }
}

#[derive(Debug)]
/// An error encountered during the decoding process
pub enum SimplemadError {
    /// An `io::Error` generated by the `Reader`
    Read(io::Error),
    /// A `MadError` generated by libmad
    Mad(MadError),
    /// The `Reader` has stopped producing data
    EOF,
}

impl From<MadError> for SimplemadError {
    fn from(err: MadError) -> SimplemadError {
        SimplemadError::Mad(err)
    }
}

impl From<io::Error> for SimplemadError {
    fn from(err: io::Error) -> SimplemadError {
        SimplemadError::Read(err)
    }
}

fn error_is_recoverable(err: &MadError) -> bool {
    err == &MadError::None || (err.clone() as u16) & 0xff00 != 0
}

fn frame_duration(frame: &MadFrame) -> f64 {
    let duration = &frame.header.duration;
    (duration.seconds as f64) * 1000.0 + (duration.fraction as f64) / 352800.0
}

#[derive(Clone, Copy, Default, Debug)]
#[repr(C)]
/// libmad's native fixed-point sample format
///
/// A 32-bit value comprised of a sign bit,
/// three whole number bits and 28 fractional
/// bits.
pub struct MadFixed32 {
    value: i32,
}

impl MadFixed32 {
    /// Construct a new MadFixed32 from a value in libmad's fixed-point format
    pub fn new(v: i32) -> MadFixed32 {
        MadFixed32 {
            value: v,
        }
    }

    /// Get the raw fixed-point representation
    pub fn to_raw(&self) -> i32 {
        self.value
    }

    /// Convert to i16
    pub fn to_i16(&self) -> i16 {
        let frac_bits = 28;
        let unity_value = 0x1000_0000;

        let rounded_value = self.value + (1 << (frac_bits - 16));

        let clipped_value =
            max(-unity_value, min(rounded_value, unity_value - 1));

        let quantized_value = clipped_value >> (frac_bits + 1 - 16);

        quantized_value as i16
    }

    /// Convert to i32
    pub fn to_i32(&self) -> i32 {
        // clip only
        if self.value > i32::max_value() / 8 {
            i32::max_value()
        } else if self.value < i32::min_value() / 8 {
            i32::min_value()
        } else {
            self.value * 8
        }
    }

    /// Convert to f32
    pub fn to_f32(&self) -> f32 {
        // The big number is 2^28, as 28 is the fractional bit count)
        f32::max(-1.0, f32::min(1.0, (self.value as f32) / 268435456.0))
    }

    /// Convert to f64
    pub fn to_f64(&self) -> f64 {
        // The big number is 2^28, as 28 is the fractional bit count)
        f64::max(-1.0, f64::min(1.0, (self.value as f64) / 268435456.0))
    }
}

impl From<i32> for MadFixed32 {
    fn from(v: i32) -> MadFixed32 {
        MadFixed32 {value: v / 8}
    }
}

impl From<f32> for MadFixed32 {
    fn from(v: f32) -> MadFixed32 {
        MadFixed32 {
            // The big number is 2^28, as
            // 28 is the fractional bit count)
            value: (v * 268435456.0) as i32,
        }
    }
}

impl From<f64> for MadFixed32 {
    fn from(v: f64) -> MadFixed32 {
        MadFixed32 {
            // The big number is 2^28, as
            // 28 is the fractional bit count)
            value: (v * 268435456.0) as i32,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use simplemad_sys::*;
    use std::io::BufReader;
    use std::fs::File;
    use std::path::Path;

    #[test]
    fn test_find_duration() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let bufreader = BufReader::new(file);
        let decoder = Decoder::decode_headers(bufreader).unwrap();

        let duration =
            decoder.filter_map(|r| match r {
                       Ok(f) => Some(f.duration),
                       Err(_) => None})
                   .fold(0.0, |acc, dur| acc + (dur as f64));

        assert!(f64::abs(duration - 5041.0) < 1.0);
    }

    #[test]
    fn test_decode_headers() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let bufreader = BufReader::new(file);
        let decoder = Decoder::decode_headers(bufreader).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.mode, MadMode::Stereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.bit_rate, 128000);
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.samples.len(), 0);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[test]
    fn test_bufreader() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let bufreader = BufReader::new(file);
        let decoder = Decoder::decode(bufreader).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.mode, MadMode::Stereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.bit_rate, 128000);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[test]
    fn test_decode_interval() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode_interval(file, 3000.0, 4000.0).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 39);
    }

    #[test]
    fn test_interval_beyond_eof() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let mut decoder = Decoder::decode_interval(file, 60000.0, 65000.0).unwrap();

        assert!(decoder.next().is_none());
    }

    #[test]
    fn test_decode_empty_interval() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode_interval(file, 2000.0, 2000.0).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 0);
    }

    #[test]
    fn test_decode_overlong_interval() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode_interval(file, 3000.0, 45000.0).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 77);
    }

    #[test]
    fn constant_stereo_128() {
        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.mode, MadMode::Stereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.bit_rate, 128000);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[test]
    fn constant_joint_stereo_128() {
        let path = Path::new("sample_mp3s/constant_joint_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.mode, MadMode::JointStereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.bit_rate, 128000);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 950);
    }

    #[test]
    fn average_stereo_128() {
        let path = Path::new("sample_mp3s/average_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.mode, MadMode::Stereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[test]
    fn constant_stereo_320() {
        let path = Path::new("sample_mp3s/constant_stereo_320.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.mode, MadMode::Stereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.bit_rate, 320000);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[test]
    fn variable_joint_stereo() {
        let path = Path::new("sample_mp3s/variable_joint_stereo.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1 }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.mode, MadMode::JointStereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[test]
    fn variable_stereo() {
        let path = Path::new("sample_mp3s/variable_stereo.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1 }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 1152);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[test]
    fn constant_stereo_16() {
        let path = Path::new("sample_mp3s/constant_stereo_16.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 24000);
                    assert_eq!(f.mode, MadMode::Stereo);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.bit_rate, 16000);
                    assert_eq!(f.samples.len(), 2);
                    assert_eq!(f.samples[0].len(), 576);
                }
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 210);
    }

    #[test]
    fn constant_single_channel_128() {
        let path = Path::new("sample_mp3s/constant_single_channel_128.mp3");
        let file = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();
        let mut frame_count = 0;
        let mut error_count = 0;

        for item in decoder {
            match item {
                Err(_) => {
                    if frame_count > 0 { error_count += 1; }
                },
                Ok(f) => {
                    frame_count += 1;
                    assert_eq!(f.sample_rate, 44100);
                    assert_eq!(f.mode, MadMode::SingleChannel);
                    assert_eq!(f.layer, MadLayer::LayerIII);
                    assert_eq!(f.bit_rate, 128000);
                    assert_eq!(f.samples.len(), 1);
                    assert_eq!(f.samples[0].len(), 1152);
                },
            }
        }
        assert_eq!(error_count, 0);
        assert_eq!(frame_count, 193);
    }

    #[allow(unused_variables)]
    #[test]
    fn test_readme_md() {
        use std::fs::File;
        use std::path::Path;

        let path = Path::new("sample_mp3s/constant_stereo_128.mp3");
        let file = File::open(&path).unwrap();
        let file2 = File::open(&path).unwrap();
        let decoder = Decoder::decode(file).unwrap();

        for decoding_result in decoder {
            match decoding_result {
                Err(e) => println!("Error: {:?}", e),
                Ok(frame) => {
                    println!("Frame sample rate: {}", frame.sample_rate);
                    println!("First audio sample (left channel): {:?}", frame.samples[0][0]);
                    println!("First audio sample (right channel): {:?}", frame.samples[1][0]);
                },
            }
        }
        let partial_decoder = Decoder::decode_interval(file2, 30_000_f64, 60_000_f64).unwrap();
    }
}
