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
    RCV_STR, ReadResult, ReaderError, invalid_data, multi_bit_digital_signal_to_chars, read_bytes,
    read_f64, read_packed_signal_value_bytes, read_signal_locs, read_time_table, read_u8, read_u64,
    read_variant_u32, read_variant_u64, read_zlib_compressed_bytes,
};
use crate::types::{DataSectionInfo, FloatingPointEndian, SignalInfo, ValueChangePackType};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// Start and end time of one value-change section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FstSectionInfo {
    /// The start time of the section, as stored in the section header.
    pub start_time: u64,
    /// The end time of the section, as stored in the section header.
    pub end_time: u64,
}

/// A signal value as stored in the file, without expansion to ASCII.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FstValue<'a> {
    /// A 2-state bit vector of `width` bits in `width.div_ceil(8)` bytes. The most significant
    /// bit is bit 7 of `bytes[0]`. The unused low bits of the last byte are always zero.
    Packed { width: u32, bytes: &'a [u8] },
    /// The state characters of a bit vector as stored in the file, one per bit, most significant
    /// bit first. Used when the stored value is not packed 2-state, and for all frame values.
    Chars(&'a [u8]),
    /// A variable-length string value.
    VarLen(&'a [u8]),
    /// A real value.
    Real(f64),
}

const PACKED_ZERO: [u8; 1] = [0x00];
const PACKED_ONE: [u8; 1] = [0x80];

/// One value-change section, read into memory. The value-change data stays compressed until
/// [`FstSection::for_each_change`] decodes one signal. The frame stays compressed until
/// [`FstSection::for_each_frame_value`] reads it.
pub struct FstSection {
    info: FstSectionInfo,
    time_table: Vec<u64>,
    /// The frame as stored in the file, usually zlib compressed.
    frame: Vec<u8>,
    /// The length of the frame after decompression, as declared in the file.
    frame_uncompressed_len: u64,
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

// The section can be shared between threads. A caller decodes different signals in parallel.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FstSection>();
};

fn unexpected_eof() -> ReaderError {
    ReaderError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "unexpected eof",
    ))
}

fn time_index_out_of_range(time_index: usize, len: usize) -> ReaderError {
    invalid_data(format!(
        "time index {time_index} is outside the time table of {len} entries"
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
    /// The start and end time of this section.
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
    ///
    /// This method decompresses the frame. It returns an error if the frame is damaged. The
    /// other methods do not need the frame, so they do not fail because of a damaged frame.
    pub fn for_each_frame_value(
        &self,
        mut f: impl FnMut(FstSignalHandle, FstValue<'_>),
    ) -> ReadResult<()> {
        let frame = read_zlib_compressed_bytes(
            &mut Cursor::new(self.frame.as_slice()),
            self.frame_uncompressed_len,
            self.frame.len() as u64,
            true,
        )?;
        let mut rest: &[u8] = &frame;
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
    /// Returns an error if the stored bytes are inconsistent or damaged. For example, a time
    /// index may be outside [`FstSection::time_table`], a value may be cut short, a handle may
    /// have no signal information, or the compressed data may be corrupt.
    pub fn for_each_change(
        &self,
        handle: FstSignalHandle,
        mut f: impl FnMut(usize, FstValue<'_>),
    ) -> ReadResult<()> {
        let idx = handle.get_index();
        let Some((offset, len)) = self.locs.get(idx).copied().flatten() else {
            return Ok(());
        };
        let &(width, is_real) = self.signals.get(idx).ok_or_else(|| {
            invalid_data(format!(
                "signal handle {idx} has changes but no signal information ({} signals)",
                self.signals.len()
            ))
        })?;
        let start = usize::try_from(offset).map_err(|_| unexpected_eof())?;
        let mut input = Cursor::new(self.data.get(start..).ok_or_else(unexpected_eof)?);
        let bytes = read_packed_signal_value_bytes(&mut input, len, self.pack)?;
        let mut masked: Vec<u8> = Vec::new();
        let mut chars: Vec<u8> = Vec::new();
        let mut rest: &[u8] = &bytes;
        let one_bit_chars: &'static [u8; 8] = &RCV_STR;
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
                        FstValue::Chars(&one_bit_chars[code..code + 1])
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
    // `read_signals` reads the frame of the first section only, and the caller of this function
    // may not need it at all. Keep the stored bytes and decompress them on demand.
    if frame_compressed > section_length {
        return Err(invalid_data(format!(
            "the frame has {frame_compressed} bytes, but the section has {section_length}"
        )));
    }
    let frame = read_bytes(input, frame_compressed as usize)?;

    // value-change data follows the frame
    let (max_handle, _) = read_variant_u64(input)?;
    // Every handle needs signal information. This check also keeps a corrupt `max_handle` from
    // causing a huge allocation.
    if max_handle > signals.len() as u64 {
        return Err(invalid_data(format!(
            "the section has {max_handle} signal handles, but the file has {} signals",
            signals.len()
        )));
    }
    let vc_start = input.stream_position()?;
    let pack = ValueChangePackType::from_u8(read_u8(input)?);
    // the chain length is right in front of the time section
    let chain_len_offset = section
        .file_offset
        .checked_add(section_length)
        .and_then(|section_end| section_end.checked_sub(time_section_length))
        .and_then(|offset| offset.checked_sub(8))
        .filter(|&offset| offset >= vc_start)
        .ok_or_else(|| invalid_data("the value-change data ends before it starts".into()))?;
    // `read_signal_locs` subtracts without a check, so reject a corrupt chain length here.
    input.seek(SeekFrom::Start(chain_len_offset))?;
    let chain_len = read_u64(input)?;
    if chain_len > chain_len_offset - vc_start {
        return Err(invalid_data(format!(
            "the chain length {chain_len} reaches before the value-change data"
        )));
    }
    let offsets = read_signal_locs(input, chain_len_offset, section.kind, max_handle, vc_start)?;
    input.seek(SeekFrom::Start(vc_start))?;
    let data = read_bytes(input, (chain_len_offset - vc_start) as usize)?;

    let mut locs = vec![None; max_handle as usize];
    for entry in offsets.iter() {
        let loc = locs.get_mut(entry.signal_idx).ok_or_else(|| {
            invalid_data(format!(
                "signal index {} is not below the handle count {max_handle}",
                entry.signal_idx
            ))
        })?;
        *loc = Some((entry.offset, entry.len));
    }
    Ok(FstSection {
        info: FstSectionInfo {
            start_time,
            end_time,
        },
        time_table,
        frame,
        frame_uncompressed_len: frame_uncompressed,
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
    use crate::FstReader;
    use crate::io::read_variant_u64;
    use crate::types::{BlockType, DataSectionKind};
    use std::io::ErrorKind;

    /// A section with one signal of `width` bits (0 means variable length) and `time_points`
    /// time table entries. The signal has one uncompressed chunk, which holds `change_bytes`.
    fn section(width: u32, is_real: bool, time_points: usize, change_bytes: &[u8]) -> FstSection {
        // chunk: varint 0 (= stored uncompressed), then the raw change bytes
        let mut chunk = vec![0x00];
        chunk.extend_from_slice(change_bytes);
        let mut data = vec![b'4']; // pack type byte at vc_start
        data.extend_from_slice(&chunk);
        FstSection {
            info: FstSectionInfo {
                start_time: 0,
                end_time: 0,
            },
            time_table: (0..time_points as u64).map(|t| t * 10).collect(),
            frame: vec![b'0'; width as usize],
            frame_uncompressed_len: width as u64,
            pack: ValueChangePackType::Lz4,
            data,
            locs: vec![Some((1, chunk.len() as u32))],
            signals: vec![(width, is_real)],
            float_endian: FloatingPointEndian::Little,
        }
    }

    /// A section with one 2-state signal of `width` bits, one time point, and one change at
    /// time index 0 with the raw value bytes `value`.
    fn section_with_one_change(width: u32, value: &[u8]) -> FstSection {
        // vli = (time delta 0 << 1) | 2-state
        let mut change_bytes = vec![0x00];
        change_bytes.extend_from_slice(value);
        section(width, false, 1, &change_bytes)
    }

    /// A value with owned bytes. A test can keep it after the callback returns.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Packed(u32, Vec<u8>),
        Chars(Vec<u8>),
        VarLen(Vec<u8>),
        Real(f64),
    }

    /// All changes of handle 0 as `(time index, value)`.
    fn changes(section: &FstSection) -> Vec<(usize, Seen)> {
        let mut seen = Vec::new();
        section
            .for_each_change(FstSignalHandle::from_index(0), |ti, v| {
                let owned = match v {
                    FstValue::Packed { width, bytes } => Seen::Packed(width, bytes.to_vec()),
                    FstValue::Chars(c) => Seen::Chars(c.to_vec()),
                    FstValue::VarLen(c) => Seen::VarLen(c.to_vec()),
                    FstValue::Real(r) => Seen::Real(r),
                };
                seen.push((ti, owned));
            })
            .unwrap();
        seen
    }

    fn assert_invalid_data(err: ReaderError) {
        assert!(
            matches!(&err, ReaderError::Io(e) if e.kind() == ErrorKind::InvalidData),
            "expected an InvalidData error, got {err:?}"
        );
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
                vec![(0, Seen::Packed(width, expected))],
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
        assert_invalid_data(err);
    }

    #[test]
    fn one_bit_2_state_values_are_packed() {
        // 1-bit vli = (time delta << 2) | (value << 1), with bit 0 clear for 2-state.
        // value 0 at time index 0, value 1 at time index 3.
        let section = section(1, false, 4, &[0x00, (3 << 2) | (1 << 1)]);
        assert_eq!(
            changes(&section),
            vec![
                (0, Seen::Packed(1, vec![0x00])),
                (3, Seen::Packed(1, vec![0x80])),
            ]
        );
    }

    #[test]
    fn one_bit_4_and_9_state_codes_are_chars() {
        // 1-bit vli = (time delta << 4) | (code << 1) | 1. Codes 0 to 7 stand for `xzhuwl-?`.
        // The first change is at time index 0. Each later change is one time index after the
        // previous one.
        let mut change_bytes = Vec::new();
        for code in 0u8..8 {
            let delta = u8::from(code != 0);
            change_bytes.push((delta << 4) | (code << 1) | 1);
        }
        let section = section(1, false, 8, &change_bytes);
        let expected: Vec<(usize, Seen)> = b"xzhuwl-?"
            .iter()
            .enumerate()
            .map(|(i, &c)| (i, Seen::Chars(vec![c])))
            .collect();
        assert_eq!(changes(&section), expected);
    }

    #[test]
    fn variable_length_values_are_byte_strings() {
        // vli = time delta << 1, then a varint length, then the bytes.
        let mut change_bytes = vec![0x00, 5];
        change_bytes.extend_from_slice(b"hello");
        // an empty string two time indices later
        change_bytes.extend_from_slice(&[2 << 1, 0]);
        let section = section(0, false, 3, &change_bytes);
        assert_eq!(
            changes(&section),
            vec![
                (0, Seen::VarLen(b"hello".to_vec())),
                (2, Seen::VarLen(Vec::new())),
            ]
        );
    }

    #[test]
    fn multi_bit_values_with_chars_are_chars() {
        // vli = (time delta << 1) | 1, then one character per bit.
        let mut change_bytes = vec![(2 << 1) | 1];
        change_bytes.extend_from_slice(b"x01z");
        let section = section(4, false, 3, &change_bytes);
        assert_eq!(changes(&section), vec![(2, Seen::Chars(b"x01z".to_vec()))]);
    }

    #[test]
    fn real_values_stored_as_f64_bytes_are_decoded() {
        for endian in [FloatingPointEndian::Little, FloatingPointEndian::Big] {
            // vli = (time delta << 1) | 1, then the 8 raw bytes of the f64.
            let mut change_bytes = vec![(1 << 1) | 1];
            match endian {
                FloatingPointEndian::Little => {
                    change_bytes.extend_from_slice(&1.5f64.to_le_bytes())
                }
                FloatingPointEndian::Big => change_bytes.extend_from_slice(&1.5f64.to_be_bytes()),
            }
            let mut section = section(8, true, 2, &change_bytes);
            section.float_endian = endian;
            assert_eq!(changes(&section), vec![(1, Seen::Real(1.5))]);
        }
    }

    #[test]
    fn real_values_stored_as_packed_bits_mirror_upstream() {
        // vli = time delta << 1, then ceil(8 / 8) = 1 packed byte. Upstream `read_signals`
        // expands the packed byte to 8 ASCII characters (`0` or `1`) and reads an f64 from
        // those characters. This is a quirk, not a meaningful value. The section API must give
        // the same f64 as `read_signals`, so this test computes the expectation the same way.
        let raw = 0b1011_0010u8;
        let chars: [u8; 8] = std::array::from_fn(|i| b'0' + ((raw >> (7 - i)) & 1));
        assert_eq!(&chars, b"10110010");
        for endian in [FloatingPointEndian::Little, FloatingPointEndian::Big] {
            let expected = match endian {
                FloatingPointEndian::Little => f64::from_le_bytes(chars),
                FloatingPointEndian::Big => f64::from_be_bytes(chars),
            };
            assert!(expected.is_finite());
            let mut section = section(8, true, 3, &[2 << 1, raw]);
            section.float_endian = endian;
            assert_eq!(changes(&section), vec![(2, Seen::Real(expected))]);
        }
    }

    #[test]
    fn a_handle_without_signal_info_is_an_error() {
        // `locs` has two handles, but `signals` has only one. This cannot come from
        // `read_section`, which checks it. `for_each_change` must still return an error.
        let mut section = section_with_one_change(4, &[0b1010_0000]);
        section.locs.push(section.locs[0]);
        assert_eq!(section.max_handle(), 2);
        assert_eq!(changes(&section).len(), 1, "handle 0 is fine");
        let mut calls = 0;
        let err = section
            .for_each_change(FstSignalHandle::from_index(1), |_, _| calls += 1)
            .unwrap_err();
        assert_eq!(calls, 0);
        assert_invalid_data(err);
    }

    /// The bytes of a small valid FST file with one value-change section and one 4-bit signal.
    fn small_fst_file() -> Vec<u8> {
        use fst_writer::{
            FstFileType, FstInfo, FstScopeType, FstSignalType, FstVarDirection, FstVarType,
            open_fst,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.fst");
        let info = FstInfo {
            start_time: 0,
            timescale_exponent: -12,
            version: "section unit test".into(),
            date: "2026-10-08".into(),
            file_type: FstFileType::Verilog,
        };
        let mut header = open_fst(&path, &info).unwrap();
        header.scope("top", "top", FstScopeType::Module).unwrap();
        let id = header
            .var(
                "s",
                FstSignalType::bit_vec(4),
                FstVarType::Wire,
                FstVarDirection::Implicit,
                None,
            )
            .unwrap();
        header.up_scope().unwrap();
        let mut body = header.finish().unwrap();
        body.signal_change(id, b"0000").unwrap();
        for (time, value) in [(10, b"0001"), (20, b"0011"), (30, b"0111")] {
            body.time_change(time).unwrap();
            body.signal_change(id, value).unwrap();
        }
        body.finish().unwrap();
        std::fs::read(&path).unwrap()
    }

    /// Finds the first value-change section in the bytes of an FST file. Returns its info and
    /// the file offset of its chain length field.
    ///
    /// The tests below patch bytes of a real file. This is simpler than crafting a file by hand.
    fn locate_data_section(bytes: &[u8]) -> (DataSectionInfo, u64) {
        let be_u64 = |pos: u64| {
            let pos = pos as usize;
            u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap())
        };
        let mut block_start = 0u64;
        while (block_start as usize) < bytes.len() {
            let block_type = BlockType::try_from(bytes[block_start as usize]).unwrap();
            // `file_offset` points to the section length, after the block type byte.
            let file_offset = block_start + 1;
            let section_length = be_u64(file_offset);
            if let Some(kind) = DataSectionKind::from_block_type(&block_type) {
                let section_end = file_offset + section_length;
                // The last 24 bytes of the section describe the time table.
                let time_compressed = be_u64(section_end - 16);
                let time_section_length = time_compressed + 24;
                let info = DataSectionInfo {
                    file_offset,
                    start_time: be_u64(file_offset + 8),
                    end_time: be_u64(file_offset + 16),
                    kind,
                    mem_required_for_traversal: be_u64(file_offset + 24),
                };
                return (info, section_end - time_section_length - 8);
            }
            block_start = file_offset + section_length;
        }
        panic!("no value-change section in the file");
    }

    #[test]
    fn a_chain_length_before_the_value_change_data_is_an_error() {
        let bytes = small_fst_file();
        let (_, chain_len_offset) = locate_data_section(&bytes);
        let at = chain_len_offset as usize;
        FstReader::open(std::io::Cursor::new(bytes.clone()))
            .unwrap()
            .read_section(0)
            .map(|_| ())
            .expect("the unchanged file is valid");

        // The chain starts `chain length` bytes before the chain length field. Each value puts
        // the start of the chain before the start of the value-change data.
        for chain_length in [chain_len_offset - 1, chain_len_offset, u64::MAX] {
            let mut corrupt = bytes.clone();
            corrupt[at..at + 8].copy_from_slice(&chain_length.to_be_bytes());
            let mut reader = FstReader::open(std::io::Cursor::new(corrupt)).unwrap();
            let err = reader
                .read_section(0)
                .err()
                .unwrap_or_else(|| panic!("chain length {chain_length} must be an error"));
            assert_invalid_data(err);
        }
    }

    /// Replaces the only byte of the chain of `small_fst_file` and reads the section.
    fn read_section_with_chain_byte(chain_byte: u8) -> ReadResult<FstSection> {
        let mut bytes = small_fst_file();
        let (_, chain_len_offset) = locate_data_section(&bytes);
        let at = chain_len_offset as usize;
        let chain_len = u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());
        // One signal with data: its offset delta is one byte.
        assert_eq!(chain_len, 1);
        bytes[at - 1] = chain_byte;
        FstReader::open(std::io::Cursor::new(bytes))
            .unwrap()
            .read_section(0)
    }

    #[test]
    fn a_chain_alias_outside_the_signals_is_an_error() {
        // The byte 0x7d is the alias of signal 1, but there is only signal 0.
        assert_invalid_data(
            read_section_with_chain_byte(0x7d)
                .err()
                .expect("the alias target is outside the table"),
        );
    }

    #[test]
    fn a_chain_alias_of_a_signal_without_an_offset_is_an_error() {
        // The byte 0x7f is the alias of signal 0, which is this alias itself.
        assert_invalid_data(
            read_section_with_chain_byte(0x7f)
                .err()
                .expect("the alias target has no offset"),
        );
    }

    #[test]
    fn the_original_chain_byte_is_valid() {
        // 0x03 is an offset delta of 1, which the writer produces.
        read_section_with_chain_byte(0x03)
            .map(|_| ())
            .expect("the unchanged file is valid");
    }

    /// The file with another frame in the first value-change section. `stored` is written as
    /// the frame bytes. `uncompressed` and `compressed` are the lengths declared in front of
    /// them. The section length is adjusted. The rest of the file stays the same.
    fn replace_frame(bytes: &[u8], stored: &[u8], uncompressed: u64, compressed: u64) -> Vec<u8> {
        let (info, _) = locate_data_section(bytes);
        let section_start = info.file_offset as usize;
        // the section header has 4 u64 fields; the frame follows
        let frame_start = section_start + 4 * 8;
        let mut cursor = &bytes[frame_start..];
        let (_, _) = read_variant_u64(&mut cursor).unwrap();
        let (old_compressed, _) = read_variant_u64(&mut cursor).unwrap();
        let (max_handle, _) = read_variant_u64(&mut cursor).unwrap();
        let old_frame_end = bytes.len() - cursor.len() + old_compressed as usize;

        let mut frame = Vec::new();
        for value in [uncompressed, compressed, max_handle] {
            crate::io::write_variant_u64(&mut frame, value).unwrap();
        }
        frame.extend_from_slice(stored);

        let old_section_length =
            u64::from_be_bytes(bytes[section_start..section_start + 8].try_into().unwrap());
        let section_length =
            old_section_length + frame.len() as u64 - (old_frame_end - frame_start) as u64;
        let mut out = bytes[..section_start].to_vec();
        out.extend_from_slice(&section_length.to_be_bytes());
        out.extend_from_slice(&bytes[section_start + 8..frame_start]);
        out.extend_from_slice(&frame);
        out.extend_from_slice(&bytes[old_frame_end..]);
        out
    }

    /// The frame values as (handle index, characters).
    fn frame_values(section: &FstSection) -> ReadResult<Vec<(usize, Vec<u8>)>> {
        let mut seen = Vec::new();
        section.for_each_frame_value(|h, v| match v {
            FstValue::Chars(c) => seen.push((h.get_index(), c.to_vec())),
            other => panic!("expected chars, got {other:?}"),
        })?;
        Ok(seen)
    }

    fn open_section(bytes: Vec<u8>) -> ReadResult<FstSection> {
        FstReader::open(std::io::Cursor::new(bytes))
            .unwrap()
            .read_section(0)
    }

    #[test]
    fn a_zlib_frame_is_decompressed_when_the_frame_values_are_read() {
        let original = small_fst_file();
        let expected_changes = changes(&open_section(original.clone()).unwrap());
        let stored = miniz_oxide::deflate::compress_to_vec_zlib(b"0000", 3);
        assert_ne!(stored.len(), 4, "equal lengths would mean stored bytes");
        let file = replace_frame(&original, &stored, 4, stored.len() as u64);
        let section = open_section(file).unwrap();
        assert_eq!(frame_values(&section).unwrap(), vec![(0, b"0000".to_vec())]);
        assert_eq!(changes(&section), expected_changes);
    }

    #[test]
    fn a_stored_frame_is_returned_as_it_is() {
        let original = small_fst_file();
        let section = open_section(replace_frame(&original, b"01xz", 4, 4)).unwrap();
        assert_eq!(frame_values(&section).unwrap(), vec![(0, b"01xz".to_vec())]);
    }

    #[test]
    fn a_frame_with_a_wrong_declared_length_is_an_error_only_when_it_is_read() {
        let original = small_fst_file();
        let expected_changes = changes(&open_section(original.clone()).unwrap());
        // A valid zlib stream of 4 bytes, but the declared length is 5.
        let stored = miniz_oxide::deflate::compress_to_vec_zlib(b"0000", 3);
        let file = replace_frame(&original, &stored, 5, stored.len() as u64);
        let section = open_section(file).expect("the frame is not needed to read the section");
        assert_invalid_data(frame_values(&section).unwrap_err());
        // The value changes do not depend on the frame.
        assert_eq!(changes(&section), expected_changes);
    }

    #[test]
    fn a_corrupt_frame_is_an_error_only_when_it_is_read() {
        let original = small_fst_file();
        let expected_changes = changes(&open_section(original.clone()).unwrap());
        // The lengths differ, so the bytes must be a zlib stream, but they are not.
        let file = replace_frame(&original, b"not zlib", 4, 8);
        let section = open_section(file).expect("the frame is not needed to read the section");
        assert!(frame_values(&section).is_err());
        assert_eq!(changes(&section), expected_changes);

        // A zlib header followed by damaged data.
        let file = replace_frame(&original, &[0x78, 0x9c, 0xff, 0xff, 0xff, 0xff], 4, 6);
        let section = open_section(file).expect("the frame is not needed to read the section");
        assert!(frame_values(&section).is_err());
        assert_eq!(changes(&section), expected_changes);
    }

    #[test]
    fn a_frame_longer_than_its_section_is_an_error() {
        let original = small_fst_file();
        let file = replace_frame(&original, b"0000", 4, u64::MAX / 2);
        assert_invalid_data(open_section(file).err().expect("the frame is too long"));
    }

    #[test]
    fn more_handles_than_signals_is_an_error() {
        // `read_section` is called with the internal section info and no signal info, so the
        // section has one handle (`max_handle`) but 0 signals.
        let bytes = small_fst_file();
        let (info, _) = locate_data_section(&bytes);
        let mut input = std::io::Cursor::new(bytes);
        let read = |input: &mut std::io::Cursor<Vec<u8>>, signals: &[SignalInfo]| {
            read_section(input, &info, signals, FloatingPointEndian::Little)
        };
        let one_signal = [SignalInfo::from_file_format(4)];
        read(&mut input, &one_signal)
            .map(|_| ())
            .expect("one handle, one signal");
        let err = read(&mut input, &[]).err().expect("one handle, no signal");
        assert_invalid_data(err);
    }
}
