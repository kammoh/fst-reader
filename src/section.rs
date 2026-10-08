// Copyright 2026 Kamyar Mohajerani
// released under BSD 3-Clause License
//
//! Section-level access to value-change data.
//!
//! [`crate::FstReader::read_signals`] merges the changes of all signals into one stream in time
//! order and expands every value to ASCII. This module gives access to one value-change section
//! at a time and to the changes of each signal separately. Different signals of a section can
//! be decoded on different threads. 2-state values stay packed.

use crate::FstSignalHandle;
use crate::io::{
    ReadResult, ReaderError, multi_bit_digital_signal_to_chars, read_bytes, read_f64,
    read_packed_signal_value_bytes, read_signal_locs, read_time_table, read_u8, read_u64,
    read_variant_u32, read_variant_u64, read_zlib_compressed_bytes,
};
use crate::types::{DataSectionInfo, FloatingPointEndian, SignalInfo, ValueChangePackType};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// Start and end time of one value-change section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FstSectionInfo {
    pub start_time: u64,
    pub end_time: u64,
}

/// A signal value as stored in the file, without expansion to ASCII.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FstValue<'a> {
    /// A 2-state bit vector of `width` bits in `width.div_ceil(8)` bytes. The most significant
    /// bit is bit 7 of `bytes[0]`. The unused low bits of the last byte are always zero.
    Packed { width: u32, bytes: &'a [u8] },
    /// A bit vector with one ASCII state character per bit, most significant bit first.
    /// Used for values with a bit that is not `0` or `1`, and for all frame values.
    Chars(&'a [u8]),
    /// A variable-length string value.
    VarLen(&'a [u8]),
    /// A real value.
    Real(f64),
}

const PACKED_ZERO: [u8; 1] = [0x00];
const PACKED_ONE: [u8; 1] = [0x80];
/// Same order as the 1-bit 4/9-state codes in `io::one_bit_signal_value_to_char`.
static ONE_BIT_CHARS: [u8; 8] = *b"xzhuwl-?";

/// One value-change section, read into memory. The value-change data stays compressed until
/// [`FstSection::for_each_change`] decodes one signal.
pub struct FstSection {
    info: FstSectionInfo,
    time_table: Vec<u64>,
    frame: Vec<u8>,
    pack: ValueChangePackType,
    /// Value-change data from the pack-type byte (`vc_start`) to the chain length field.
    data: Vec<u8>,
    /// Per handle index: data offset relative to `vc_start` and length, or `None` if the signal
    /// has no changes in this section.
    locs: Vec<Option<(u64, u32)>>,
    /// Per handle index: bit width (0 means variable length) and whether the signal is real.
    signals: Vec<(u32, bool)>,
    float_endian: FloatingPointEndian,
}

fn unexpected_eof() -> ReaderError {
    ReaderError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "unexpected eof",
    ))
}

fn time_index_out_of_range(time_index: usize, len: usize) -> ReaderError {
    ReaderError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("time index {time_index} is outside the time table of {len} entries"),
    ))
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> ReadResult<&'a [u8]> {
    if input.len() < n {
        return Err(unexpected_eof());
    }
    let (head, tail) = input.split_at(n);
    *input = tail;
    Ok(head)
}

impl FstSection {
    pub fn info(&self) -> FstSectionInfo {
        self.info
    }

    /// The times of this section. Time indices in [`FstSection::for_each_change`] point here.
    pub fn time_table(&self) -> &[u64] {
        &self.time_table
    }

    /// The number of signal handles. Valid handle indices are `0..max_handle()`.
    pub fn max_handle(&self) -> usize {
        self.locs.len()
    }

    /// Calls `f` with the value of every signal at the start of the section.
    /// Variable-length signals have no frame value and are skipped.
    pub fn for_each_frame_value(
        &self,
        mut f: impl FnMut(FstSignalHandle, FstValue<'_>),
    ) -> ReadResult<()> {
        let mut rest: &[u8] = &self.frame;
        for (idx, &(width, is_real)) in self.signals.iter().enumerate() {
            let handle = FstSignalHandle::from_index(idx);
            if width == 0 {
                continue;
            }
            if is_real {
                f(
                    handle,
                    FstValue::Real(read_f64(&mut rest, self.float_endian)?),
                );
            } else {
                f(handle, FstValue::Chars(take(&mut rest, width as usize)?));
            }
        }
        Ok(())
    }

    /// Decodes the changes of one signal and calls `f(time_index, value)` for each change, in
    /// time order. Does nothing if the signal has no changes in this section.
    ///
    /// This method reports every change in the section, including changes after the end time in
    /// the file header. [`crate::FstReader::read_signals`] drops those changes. A caller that
    /// needs the same result as `read_signals` must apply the same cut.
    ///
    /// Returns an error if the data is corrupt, for example if a time index is not in
    /// [`FstSection::time_table`].
    pub fn for_each_change(
        &self,
        handle: FstSignalHandle,
        mut f: impl FnMut(usize, FstValue<'_>),
    ) -> ReadResult<()> {
        let idx = handle.get_index();
        let Some((offset, len)) = self.locs.get(idx).copied().flatten() else {
            return Ok(());
        };
        let start = usize::try_from(offset).map_err(|_| unexpected_eof())?;
        let mut input = Cursor::new(self.data.get(start..).ok_or_else(unexpected_eof)?);
        let bytes = read_packed_signal_value_bytes(&mut input, len, self.pack)?;
        let (width, is_real) = self.signals[idx];
        let mut masked: Vec<u8> = Vec::new();
        let mut chars: Vec<u8> = Vec::new();
        let mut rest: &[u8] = &bytes;
        let mut time_index = 0usize;
        while !rest.is_empty() {
            let (vli, _) = read_variant_u32(&mut rest)?;
            // The time delta is in the upper bits of `vli`. 1-bit signals use fewer lower bits.
            let delta = match width {
                1 => vli >> (2u32 << (vli & 1)),
                _ => vli >> 1,
            };
            time_index = time_index.saturating_add(delta as usize);
            if time_index >= self.time_table.len() {
                return Err(time_index_out_of_range(time_index, self.time_table.len()));
            }
            match width {
                1 => {
                    let value = if vli & 1 == 0 {
                        let bytes: &[u8] = if (vli >> 1) & 1 == 1 {
                            &PACKED_ONE
                        } else {
                            &PACKED_ZERO
                        };
                        FstValue::Packed { width: 1, bytes }
                    } else {
                        let code = ((vli >> 1) & 7) as usize;
                        FstValue::Chars(&ONE_BIT_CHARS[code..code + 1])
                    };
                    f(time_index, value);
                }
                0 => {
                    let (n, _) = read_variant_u32(&mut rest)?;
                    f(time_index, FstValue::VarLen(take(&mut rest, n as usize)?));
                }
                width => {
                    let w = width as usize;
                    if vli & 1 == 0 {
                        let raw = take(&mut rest, w.div_ceil(8))?;
                        if is_real {
                            // Same as `read_signals` on upstream main ("packed real").
                            multi_bit_digital_signal_to_chars(raw, w, &mut chars);
                            let mut value: &[u8] = &chars;
                            f(
                                time_index,
                                FstValue::Real(read_f64(&mut value, self.float_endian)?),
                            );
                        } else {
                            let unused = (8 - w % 8) % 8;
                            let pad_mask = ((1u16 << unused) - 1) as u8;
                            let bytes = if raw[raw.len() - 1] & pad_mask != 0 {
                                masked.clear();
                                masked.extend_from_slice(raw);
                                *masked.last_mut().unwrap() &= !pad_mask;
                                &masked[..]
                            } else {
                                raw
                            };
                            f(time_index, FstValue::Packed { width, bytes });
                        }
                    } else {
                        let raw = take(&mut rest, w)?;
                        if is_real {
                            let mut value = raw;
                            f(
                                time_index,
                                FstValue::Real(read_f64(&mut value, self.float_endian)?),
                            );
                        } else {
                            f(time_index, FstValue::Chars(raw));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Reads one value-change section. The layout follows `DataReader::read` and
/// `DataReader::read_value_changes` in `reader.rs`.
pub(crate) fn read_section(
    input: &mut (impl Read + Seek),
    section: &DataSectionInfo,
    signals: &[SignalInfo],
    float_endian: FloatingPointEndian,
) -> ReadResult<FstSection> {
    input.seek(SeekFrom::Start(section.file_offset))?;
    let section_length = read_u64(input)?;
    let start_time = read_u64(input)?;
    let end_time = read_u64(input)?;
    let (time_section_length, time_table) =
        read_time_table(input, section.file_offset, section_length)?;

    // frame: section header (4 x u64), then lengths, then zlib data
    input.seek(SeekFrom::Start(section.file_offset + 4 * 8))?;
    let (frame_uncompressed, _) = read_variant_u64(input)?;
    let (frame_compressed, _) = read_variant_u64(input)?;
    let (_frame_max_handle, _) = read_variant_u64(input)?;
    let frame = read_zlib_compressed_bytes(input, frame_uncompressed, frame_compressed, true)?;

    // value-change data follows the frame
    let (max_handle, _) = read_variant_u64(input)?;
    let vc_start = input.stream_position()?;
    let pack = ValueChangePackType::from_u8(read_u8(input)?);
    let chain_len_offset = section.file_offset + section_length - time_section_length - 8;
    let offsets = read_signal_locs(input, chain_len_offset, section.kind, max_handle, vc_start)?;
    input.seek(SeekFrom::Start(vc_start))?;
    let data = read_bytes(input, (chain_len_offset - vc_start) as usize)?;

    let mut locs = vec![None; max_handle as usize];
    for entry in offsets.iter() {
        locs[entry.signal_idx] = Some((entry.offset, entry.len));
    }
    Ok(FstSection {
        info: FstSectionInfo {
            start_time,
            end_time,
        },
        time_table,
        frame,
        pack,
        data,
        locs,
        signals: signals.iter().map(|s| (s.len(), s.is_real())).collect(),
        float_endian,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section with one 2-state signal of `width` bits, one time point, and one uncompressed
    /// change with the raw value bytes `value`.
    fn section_with_one_change(width: u32, value: &[u8]) -> FstSection {
        // chunk: varint 0 (= stored uncompressed), vli = (time delta 0 << 1) | 2-state, value
        let mut chunk = vec![0x00, 0x00];
        chunk.extend_from_slice(value);
        let mut data = vec![b'4']; // pack type byte at vc_start
        data.extend_from_slice(&chunk);
        FstSection {
            info: FstSectionInfo {
                start_time: 0,
                end_time: 0,
            },
            time_table: vec![0],
            frame: vec![b'0'; width as usize],
            pack: ValueChangePackType::Lz4,
            data,
            locs: vec![Some((1, chunk.len() as u32))],
            signals: vec![(width, false)],
            float_endian: FloatingPointEndian::Little,
        }
    }

    fn changes(section: &FstSection) -> Vec<(usize, u32, Vec<u8>)> {
        let mut seen = Vec::new();
        section
            .for_each_change(FstSignalHandle::from_index(0), |ti, v| match v {
                FstValue::Packed { width, bytes } => seen.push((ti, width, bytes.to_vec())),
                other => panic!("unexpected value {other:?}"),
            })
            .unwrap();
        seen
    }

    #[test]
    fn packed_value_padding_bits_are_cleared() {
        // Garbage only in the unused low bits of the last byte.
        let cases: Vec<(u32, Vec<u8>, Vec<u8>)> = vec![
            (4, vec![0b1010_1111], vec![0b1010_0000]),
            (7, vec![0b1010_1011], vec![0b1010_1010]),
            (9, vec![0xA5, 0b1111_1111], vec![0xA5, 0b1000_0000]),
            (
                65,
                [vec![0xFF; 8], vec![0b1111_1111]].concat(),
                [vec![0xFF; 8], vec![0b1000_0000]].concat(),
            ),
            // No unused bits: nothing to clear.
            (8, vec![0xFF], vec![0xFF]),
            (16, vec![0x12, 0x34], vec![0x12, 0x34]),
        ];
        for (width, raw, expected) in cases {
            let section = section_with_one_change(width, &raw);
            assert_eq!(
                changes(&section),
                vec![(0, width, expected)],
                "width {width}"
            );
        }
    }

    #[test]
    fn frame_values_are_chars() {
        let section = section_with_one_change(4, &[0]);
        let mut seen = Vec::new();
        section
            .for_each_frame_value(|h, v| match v {
                FstValue::Chars(c) => seen.push((h.get_index(), c.to_vec())),
                other => panic!("expected chars, got {other:?}"),
            })
            .unwrap();
        assert_eq!(seen, vec![(0, b"0000".to_vec())]);
    }

    #[test]
    fn a_time_index_past_the_time_table_is_an_error() {
        // vli = (time delta 1 << 1) | 2-state points to index 1, but the table has one entry.
        let mut section = section_with_one_change(4, &[0b1010_0000]);
        section.data[2] = 0x02;
        let mut calls = 0;
        let err = section
            .for_each_change(FstSignalHandle::from_index(0), |_, _| calls += 1)
            .unwrap_err();
        assert_eq!(calls, 0);
        assert!(matches!(
            err,
            ReaderError::Io(e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }
}
