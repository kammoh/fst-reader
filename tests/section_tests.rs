// Copyright 2026 Kamyar Mohajerani
// released under BSD 3-Clause License
//
// The section API must deliver exactly the same value changes as `read_signals`.

use fst_reader::*;
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// A value in a form that both APIs can produce and compare.
#[derive(Debug, Clone, PartialEq)]
enum Norm {
    Chars(Vec<u8>),
    Real(u64),
}

type Event = (u64, usize, Norm);

fn open(path: &Path) -> Option<FstReader<BufReader<std::fs::File>>> {
    FstReader::open(BufReader::new(std::fs::File::open(path).ok()?)).ok()
}

/// All changes from `read_signals`, in a canonical order.
fn events_from_read_signals(path: &Path) -> Option<Vec<Event>> {
    let mut reader = open(path)?;
    let mut out = Vec::new();
    reader
        .read_signals(&FstFilter::all(), |time, handle, value| -> Result<(), ()> {
            let norm = match value {
                FstSignalValue::String(s) => Norm::Chars(s.to_vec()),
                FstSignalValue::Real(r) => Norm::Real(r.to_bits()),
            };
            out.push((time, handle.get_index(), norm));
            Ok(())
        })
        .ok()?;
    out.sort_by_key(|(time, handle, _)| (*time, *handle)); // stable: keeps per-handle order
    Some(out)
}

fn norm(value: FstValue<'_>) -> Norm {
    match value {
        FstValue::Packed { width, bytes } => Norm::Chars(
            (0..width as usize)
                .map(|i| b'0' + ((bytes[i / 8] >> (7 - i % 8)) & 1))
                .collect(),
        ),
        FstValue::Chars(c) | FstValue::VarLen(c) => Norm::Chars(c.to_vec()),
        FstValue::Real(r) => Norm::Real(r.to_bits()),
    }
}

/// All changes from the section API, in the same canonical order.
fn events_from_sections(path: &Path) -> Vec<Event> {
    let mut reader = open(path).unwrap();
    let end_time = reader.get_header().end_time;
    let mut out = Vec::new();
    for index in 0..reader.sections().len() {
        let section = reader.read_section(index).unwrap();
        let start = section.info().start_time;
        let times = section.time_table().to_vec();
        // `read_signals` reports the first section's frame at the section start time
        // when the first time-table entry is later than that.
        if index == 0 && (times.is_empty() || times[0] > start) {
            section
                .for_each_frame_value(|h, v| out.push((start, h.get_index(), norm(v))))
                .unwrap();
        }
        for idx in 0..section.max_handle() {
            section
                .for_each_change(FstSignalHandle::from_index(idx), |ti, v| {
                    out.push((times[ti], idx, norm(v)))
                })
                .unwrap();
        }
    }
    // `read_signals(FstFilter::all())` stops after the header end time.
    out.retain(|(time, _, _)| *time <= end_time);
    out.sort_by_key(|(time, handle, _)| (*time, *handle));
    out
}

fn corpus_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "fst") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fsts").as_path(),
        &mut out,
    );
    out.sort();
    out
}

#[test]
fn sections_match_read_signals_on_corpus() {
    let mut compared = 0;
    for path in corpus_files() {
        // Files that `read_signals` cannot read are out of scope for this test.
        let Some(expected) = events_from_read_signals(&path) else {
            continue;
        };
        let actual = events_from_sections(&path);
        assert_eq!(actual, expected, "{}", path.display());
        compared += 1;
    }
    assert!(compared >= 20, "only {compared} corpus files compared");
}
