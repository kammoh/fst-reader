// Copyright 2026 Kamyar Mohajerani
// released under BSD 3-Clause License
//
// Incomplete FST files. A simulator that stops early leaves a file without a geometry block or
// without a hierarchy block. The hierarchy then comes from an external `.fst.hier` file.
//
// Each test builds its files at run time. It writes a complete file with `fst-writer` and then
// removes blocks, the same way as the files in `fsts/partial`.

use fst_reader::*;
use fst_writer as w;
use std::io::{BufRead, Cursor, Seek};
use std::path::Path;

/// Block type bytes of the FST format.
const BLOCK_HEADER: u8 = 0;
const BLOCK_GEOMETRY: u8 = 3;
const BLOCK_HIERARCHY: u8 = 4;

/// Writes a complete file with the scope `top`, a 1-bit wire `bit` (handle 0), and a string
/// variable `text` (handle 1) without value changes. A string variable has no fixed width. The
/// value of `bit` is 0 at time 0, and 1, 0, 1 at the times 10, 20, 30.
fn write_complete_file(path: &Path) {
    let info = w::FstInfo {
        start_time: 0,
        timescale_exponent: -12,
        version: "incomplete test".into(),
        date: "2026-10-08".into(),
        file_type: w::FstFileType::Verilog,
    };
    let mut header = w::open_fst(path, &info).unwrap();
    header.scope("top", "", w::FstScopeType::Module).unwrap();
    let bit = header
        .var(
            "bit",
            w::FstSignalType::bit_vec(1),
            w::FstVarType::Wire,
            w::FstVarDirection::Implicit,
            None,
        )
        .unwrap();
    // `bit_vec(0)` has no width. The geometry block stores this as a variable-length signal.
    header
        .var(
            "text",
            w::FstSignalType::bit_vec(0),
            w::FstVarType::GenericString,
            w::FstVarDirection::Implicit,
            None,
        )
        .unwrap();
    header.up_scope().unwrap();
    let mut body = header.finish().unwrap();
    body.signal_change(bit, b"0").unwrap();
    for (time, value) in [(10, b"1"), (20, b"0"), (30, b"1")] {
        body.time_change(time).unwrap();
        body.signal_change(bit, value).unwrap();
    }
    body.finish().unwrap();
}

/// The bytes of the complete file.
fn complete_file() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("complete.fst");
    write_complete_file(&path);
    std::fs::read(&path).unwrap()
}

/// The contents of an external `.fst.hier` file for the complete file: the hierarchy entries
/// without compression.
fn hierarchy_file() -> Vec<u8> {
    let mut out = vec![254, w::FstScopeType::Module as u8];
    out.extend_from_slice(b"top\0\0"); // scope name, empty component
    // variable: type, direction, name, length, alias (0 = not an alias)
    out.extend_from_slice(&[w::FstVarType::Wire as u8, 0]);
    out.extend_from_slice(b"bit\0");
    out.extend_from_slice(&[1, 0]);
    out.extend_from_slice(&[w::FstVarType::GenericString as u8, 0]);
    out.extend_from_slice(b"text\0");
    out.extend_from_slice(&[0, 0]);
    out.push(255); // up-scope
    out
}

/// Splits the bytes of a file into blocks. A block starts with a type byte. Then follows the
/// length of the rest of the block as a big-endian `u64` that includes its own 8 bytes.
fn blocks(bytes: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let length = u64::from_be_bytes(bytes[pos + 1..pos + 9].try_into().unwrap()) as usize;
        out.push((bytes[pos], &bytes[pos..pos + 1 + length]));
        pos += 1 + length;
    }
    out
}

/// The file without the blocks of the given types. It also sets the end time in the header to 0,
/// as a simulator that stops early leaves it.
fn without_blocks(bytes: &[u8], dropped: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tpe, block) in blocks(bytes) {
        if !dropped.contains(&tpe) {
            out.extend_from_slice(block);
        }
    }
    assert_eq!(out[0], BLOCK_HEADER, "the first block is the header");
    out[17..25].fill(0);
    out
}

fn open_complete(bytes: &[u8]) -> FstReader<Cursor<Vec<u8>>> {
    FstReader::open_and_read_time_table(Cursor::new(bytes.to_vec())).unwrap()
}

fn open_incomplete(bytes: &[u8]) -> FstReader<Cursor<Vec<u8>>> {
    FstReader::open_incomplete_and_read_time_table(
        Cursor::new(bytes.to_vec()),
        Cursor::new(hierarchy_file()),
    )
    .unwrap()
}

/// All values of `read_signals` as (time, handle index, value).
fn signal_events(
    reader: &mut FstReader<impl BufRead + Seek>,
    filter: &FstFilter,
) -> Vec<(u64, usize, String)> {
    let mut out = Vec::new();
    reader
        .read_signals(filter, |time, handle, value| {
            let value = match value {
                FstSignalValue::String(s) => String::from_utf8(s.to_vec()).unwrap(),
                FstSignalValue::Real(r) => format!("real {r}"),
            };
            out.push((time, handle.get_index(), value));
            Ok::<(), ()>(())
        })
        .unwrap();
    out
}

/// The frame values of the first section as (handle index, characters).
fn frame_values(reader: &mut FstReader<impl BufRead + Seek>) -> Vec<(usize, String)> {
    let section = reader.read_section(0).unwrap();
    let mut out = Vec::new();
    section
        .for_each_frame_value(|handle, value| match value {
            FstValue::Chars(c) => {
                out.push((handle.get_index(), String::from_utf8(c.to_vec()).unwrap()))
            }
            other => panic!("expected characters, got {other:?}"),
        })
        .unwrap();
    out
}

#[test]
fn the_test_file_has_a_variable_length_signal_and_a_usable_time_table() {
    let bytes = complete_file();
    let reader = open_complete(&bytes);
    // The writer stores the zlib-compressed time table as if it was uncompressed when both have
    // the same length. The tests below need the correct table.
    assert_eq!(reader.get_time_table().unwrap(), [0, 10, 20, 30]);
    assert_eq!(reader.sections().len(), 1);
    // The first time point is after the start of the section. The frame values count.
    assert_eq!(reader.sections()[0].start_time, 0);
}

/// A string variable has length 0 in the hierarchy. It must not become a real signal when the
/// geometry is rebuilt from the hierarchy.
#[test]
fn a_zero_width_string_variable_is_not_a_real_signal_in_a_rebuilt_geometry() {
    let bytes = complete_file();
    let mut complete = open_complete(&bytes);
    let expected_frame = frame_values(&mut complete);
    assert_eq!(expected_frame, vec![(0, "0".to_string())]);
    let expected_all = signal_events(&mut complete, &FstFilter::all());
    assert_eq!(expected_all.len(), 4, "the frame and three changes");

    // The geometry block and the hierarchy block are missing, like in `fsts/partial`.
    let incomplete = without_blocks(&bytes, &[BLOCK_GEOMETRY, BLOCK_HIERARCHY]);
    let mut reader = open_incomplete(&incomplete);
    assert_eq!(reader.get_time_table().unwrap(), [0, 10, 20, 30]);
    assert_eq!(reader.get_header().end_time, 30);

    // All signals: `read_signals` reads the frame of every signal.
    assert_eq!(signal_events(&mut reader, &FstFilter::all()), expected_all);
    // Only the 1-bit signal: the string variable is skipped in the frame by its width.
    let only_bit = FstFilter::filter_signals(vec![FstSignalHandle::from_index(0)]);
    assert_eq!(signal_events(&mut reader, &only_bit), expected_all);
    // The section API gives the same frame values.
    assert_eq!(frame_values(&mut reader), expected_frame);
}
