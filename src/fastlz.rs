// Copyright 2023 The Regents of the University of California
// Copyright 2024 Cornell University
// released under BSD 3-Clause License
// author: Kevin Laeufer <laeufer@cornell.edu>
//
// Simple Rust implementation of FastLZ: https://github.com/ariya/FastLZ
// Currently only reading is supported!

use crate::io::{ReadResult, invalid_data, read_bytes, read_u8};
use std::io::{Read, Seek, SeekFrom};

pub(crate) fn decompress(
    input: &mut (impl Read + Seek),
    input_len: usize,
    output_size_hint: usize,
) -> ReadResult<Vec<u8>> {
    let mut out = Vec::with_capacity(output_size_hint);

    let header = read_u8(input)?;
    let level = (header >> 5) + 1;
    // go back to header which is actually the first op code!
    input.seek(SeekFrom::Current(-1))?;

    match level {
        1 => decompress_level1(input, input_len, &mut out)?,
        2 => decompress_level2(input, input_len, &mut out)?,
        other => return Err(invalid_data(format!("invalid fastlz level {other}"))),
    };
    Ok(out)
}

fn decompress_level1(input: &mut impl Read, input_len: usize, out: &mut Vec<u8>) -> ReadResult<()> {
    let mut read_count: usize = 0;

    while read_count < input_len {
        let byte0 = read_u8(input)?;
        read_count += 1;
        // long or short match
        if byte0 >= 32 {
            let mut length = (byte0 >> 5) as usize + 2;
            let offset = 256 * ((byte0 & 0x1f) as usize);
            let start = match_start(out.len(), offset + 1)?;
            // long run (i.e. type == 7)
            if length == 7 + 2 {
                length += read_u8(input)? as usize;
                read_count += 1;
            }
            let adjustment = read_u8(input)? as usize; // offset adjustment
            read_count += 1;
            let start = start.checked_sub(adjustment).ok_or_else(match_error)?;
            copy_match(out, start, length);
        } else {
            literal_run(input, byte0, &mut read_count, out)?;
        }
    }
    Ok(())
}

const MAX_L2_DISTANCE: usize = 8191;

fn decompress_level2(input: &mut impl Read, input_len: usize, out: &mut Vec<u8>) -> ReadResult<()> {
    let mut read_count: usize = 0;
    let mut byte0 = read_u8(input)? & 0x1f; // remove header for first read
    read_count += 1;

    loop {
        // long or short match
        if byte0 >= 32 {
            let mut length = (byte0 >> 5) as usize + 2;
            let offset = 256 * ((byte0 & 0x1f) as usize);
            // long run (i.e. type == 7)
            if length == 7 + 2 {
                // lvl 2: read length until we get to a non 0xff byte
                loop {
                    let code = read_u8(input)?;
                    read_count += 1;
                    length += code as usize;
                    if code != 255 {
                        break;
                    }
                }
            }
            let offset_code = read_u8(input)?;
            read_count += 1;
            let start = if offset_code == 255 && byte0 & 0x1f == 31 {
                // lvl 2: match from 16-bit distance
                let lvl2_offset_high = (read_u8(input)? as usize) << 8;
                let lvl2_offset = lvl2_offset_high + read_u8(input)? as usize;
                read_count += 2;
                match_start(out.len(), lvl2_offset + MAX_L2_DISTANCE + 1)?
            } else {
                // offset adjustment
                match_start(out.len(), offset + 1)?
                    .checked_sub(offset_code as usize)
                    .ok_or_else(match_error)?
            };
            copy_match(out, start, length);
        } else {
            literal_run(input, byte0, &mut read_count, out)?;
        }

        // exit the loop
        if read_count >= input_len {
            break;
        }

        // load next instruction
        byte0 = read_u8(input)?;
        read_count += 1;
    }
    Ok(())
}

#[inline]
fn literal_run(
    input: &mut impl Read,
    byte0: u8,
    read_count: &mut usize,
    out: &mut Vec<u8>,
) -> ReadResult<()> {
    let run_length = (1 + byte0) as usize;
    let mut bytes = read_bytes(input, run_length)?;
    *read_count += run_length;
    out.append(&mut bytes);
    Ok(())
}

fn match_error() -> crate::ReaderError {
    invalid_data("a fastlz match reaches before the start of the output".into())
}

/// The start of a match that goes back `distance` bytes from the end of the output.
#[inline]
fn match_start(output_len: usize, distance: usize) -> ReadResult<usize> {
    output_len.checked_sub(distance).ok_or_else(match_error)
}

/// Copies `length` bytes from `start`. The source can overlap the new bytes. `start` must be
/// inside the output.
#[inline]
fn copy_match(out: &mut Vec<u8>, start: usize, length: usize) {
    for ii in start..start + length {
        out.push(out[ii]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    fn run(input: &[u8]) -> ReadResult<Vec<u8>> {
        decompress(&mut Cursor::new(input.to_vec()), input.len(), 16)
    }

    fn assert_invalid_data(result: ReadResult<Vec<u8>>) {
        match result {
            Err(crate::ReaderError::Io(e)) if e.kind() == ErrorKind::InvalidData => {}
            other => panic!("expected an InvalidData error, got {other:?}"),
        }
    }

    #[test]
    fn a_literal_run_and_a_match_are_decompressed() {
        // Level 1: a literal run of 1 byte, then a match of length 3 at distance 1.
        assert_eq!(run(&[0x00, b'a', 0x20, 0x00]).unwrap(), b"aaaa");
        // Level 2: the header bits 001 give the level. Same instructions.
        assert_eq!(run(&[0x20, b'a', 0x20, 0x00]).unwrap(), b"aaaa");
    }

    #[test]
    fn an_unknown_level_is_an_error() {
        // The header bits 010 would be level 3.
        assert_invalid_data(run(&[0x40, 0x00, b'a']));
    }

    #[test]
    fn a_match_before_the_start_of_the_output_is_an_error() {
        // Level 1: the match goes back 1 + 5 bytes, but there is 1 byte of output.
        assert_invalid_data(run(&[0x00, b'a', 0x20, 0x05]));
        // Level 2: the same.
        assert_invalid_data(run(&[0x20, b'a', 0x20, 0x05]));
        // Level 2: the offset 31 * 256 of a long distance is further back than the output.
        assert_invalid_data(run(&[0x20, b'a', 0xff, 0x00, 0xff, 0x00, 0x00]));
    }

    /// Level 2 input with 8192 bytes of literals `a`, followed by `tail`.
    fn level2_with_8192_literals(tail: &[u8]) -> Vec<u8> {
        // The first instruction is a literal run of 32 bytes. Its header bits give the level.
        let mut input = vec![0x3f];
        input.extend_from_slice(&[b'a'; 32]);
        for _ in 1..256 {
            input.push(0x1f);
            input.extend_from_slice(&[b'a'; 32]);
        }
        input.extend_from_slice(tail);
        input
    }

    #[test]
    fn a_long_distance_match_is_decompressed_or_an_error() {
        // The match has the length 9 and a 16-bit distance of 8192 + the two bytes after the
        // `0xff` marker. A distance of 8192 + 0 reaches the first byte.
        let input = level2_with_8192_literals(&[0xff, 0x00, 0xff, 0x00, 0x00]);
        assert_eq!(run(&input).unwrap(), vec![b'a'; 8192 + 9]);
        // A distance of 8192 + 65535 reaches before the first byte.
        let input = level2_with_8192_literals(&[0xff, 0x00, 0xff, 0xff, 0xff]);
        assert_invalid_data(run(&input));
    }
}
