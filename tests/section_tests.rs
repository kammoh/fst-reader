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
        // `read_signals` skips the sections that start after the header end time. It reports
        // the first section's frame at the section start time when the first time-table entry
        // is later than that.
        if start > end_time {
            continue;
        }
        if index == 0 && (times.is_empty() || times[0] > start) {
            section
                .for_each_frame_value(|h, v| out.push((start, h.get_index(), norm(v))))
                .unwrap();
        }
        // `read_signals(FstFilter::all())` stops at the first time after the header end time.
        let kept = times.iter().take_while(|&&t| t <= end_time).count();
        let Some(last_time_index) = kept.checked_sub(1) else {
            continue;
        };
        for idx in 0..section.max_handle() {
            section
                .for_each_change_until(
                    FstSignalHandle::from_index(idx),
                    last_time_index,
                    |ti, v| out.push((times[ti], idx, norm(v))),
                )
                .unwrap();
        }
    }
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

mod generated {
    use super::*;
    use fst_writer::{
        FstFileType, FstInfo, FstScopeType, FstSignalType, FstVarDirection, FstVarType, open_fst,
    };
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    struct Case {
        widths: Vec<u32>,
        initial: Vec<String>,
        /// (time delta > 0, changes as (signal index, value))
        steps: Vec<(u64, Vec<(usize, String)>)>,
        flush_before: Vec<usize>,
    }

    fn value(width: u32) -> impl Strategy<Value = String> {
        // mostly 2-state values, sometimes 4-state
        prop::collection::vec(
            prop_oneof![8 => Just(b'0'), 8 => Just(b'1'), 1 => Just(b'x'), 1 => Just(b'z')],
            width as usize,
        )
        .prop_map(|v| String::from_utf8(v).unwrap())
    }

    fn case() -> impl Strategy<Value = Case> {
        prop::collection::vec(
            prop_oneof![
                Just(1u32),
                Just(3),
                Just(8),
                Just(13),
                Just(64),
                Just(65),
                Just(130)
            ],
            1..6,
        )
        .prop_flat_map(|widths| {
            let initial: Vec<_> = widths.iter().map(|&w| value(w)).collect();
            let n = widths.len();
            let ws = widths.clone();
            let step = (
                1u64..5,
                prop::collection::btree_set(0..n, 0..=n).prop_flat_map(move |sigs| {
                    let ws = ws.clone();
                    sigs.into_iter()
                        .map(move |s| value(ws[s]).prop_map(move |v| (s, v)))
                        .collect::<Vec<_>>()
                }),
            );
            (
                Just(widths),
                initial,
                prop::collection::vec(step, 1..40),
                prop::collection::vec(1usize..40, 0..4),
            )
        })
        .prop_map(|(widths, initial, steps, flush_before)| Case {
            widths,
            initial,
            steps,
            flush_before,
        })
    }

    fn write(path: &Path, case: &Case) {
        let info = FstInfo {
            start_time: 0,
            timescale_exponent: -12,
            version: "section test".into(),
            date: "2026-10-08".into(),
            file_type: FstFileType::Verilog,
        };
        let mut header = open_fst(path, &info).unwrap();
        header.scope("top", "top", FstScopeType::Module).unwrap();
        let ids: Vec<_> = case
            .widths
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                header
                    .var(
                        format!("s{i}"),
                        FstSignalType::bit_vec(w),
                        FstVarType::Wire,
                        FstVarDirection::Implicit,
                        None,
                    )
                    .unwrap()
            })
            .collect();
        header.up_scope().unwrap();
        let mut body = header.finish().unwrap();
        for (i, v) in case.initial.iter().enumerate() {
            body.signal_change(ids[i], v.as_bytes()).unwrap();
        }
        let mut time = 0;
        for (k, (delta, changes)) in case.steps.iter().enumerate() {
            if case.flush_before.contains(&k) {
                body.flush().unwrap();
            }
            time += delta;
            body.time_change(time).unwrap();
            for (s, v) in changes {
                body.signal_change(ids[*s], v.as_bytes()).unwrap();
            }
        }
        body.finish().unwrap();
    }

    /// Returns true if the file at `path` has the time table that `case` describes.
    ///
    /// fst-writer 0.3.1 stores a zlib-compressed time table as if it were uncompressed when
    /// both have the same length. Such a file has a wrong time table, so the property test
    /// skips it. A test for a fixed case must assert this function, so that a change in
    /// fst-writer or in this guard cannot silently skip the case.
    fn time_table_matches(path: &Path, case: &Case) -> bool {
        let mut want = vec![0u64];
        let mut time = 0;
        for (delta, _) in &case.steps {
            time += delta;
            want.push(time);
        }
        let got =
            FstReader::open_and_read_time_table(BufReader::new(std::fs::File::open(path).unwrap()))
                .ok()
                .and_then(|r| r.get_time_table().map(|t| t.to_vec()));
        got.as_deref() == Some(&want[..])
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]
        #[test]
        fn sections_match_read_signals_on_generated_files(case in case()) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("case.fst");
            write(&path, &case);
            prop_assume!(time_table_matches(&path, &case));
            let expected = events_from_read_signals(&path).expect("read_signals failed");
            prop_assert_eq!(events_from_sections(&path), expected);
        }
    }

    #[test]
    fn generated_files_can_have_several_sections() {
        // Guard against a vacuous property test: flushes must create sections.
        let case = Case {
            widths: vec![4],
            initial: vec!["0000".into()],
            steps: vec![
                (10, vec![(0, "1111".into())]),
                (10, vec![(0, "0101".into())]),
                (10, vec![(0, "1x1z".into())]),
            ],
            flush_before: vec![1, 2],
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("three.fst");
        write(&path, &case);
        // The property test skips a file without this property, so this case must have it.
        assert!(time_table_matches(&path, &case));
        let reader = open(&path).unwrap();
        assert_eq!(reader.sections().len(), 3);
        assert_eq!(
            events_from_sections(&path),
            events_from_read_signals(&path).unwrap()
        );
    }

    #[test]
    fn for_each_change_reports_changes_after_the_header_end_time() {
        let step = |value: &str| (10, vec![(0, value.to_string())]);
        let case = Case {
            widths: vec![4],
            initial: vec!["0000".into()],
            steps: vec![step("0001"), step("0011"), step("0111")],
            flush_before: vec![],
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("end_time.fst");
        write(&path, &case);

        // Header block: type byte 0, section length (u64), start time (u64), end time (u64),
        // all big-endian. Set the end time to 20, before the last time point.
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[0], 0, "the first block is the header");
        assert_eq!(u64::from_be_bytes(bytes[17..25].try_into().unwrap()), 30);
        bytes[17..25].copy_from_slice(&20u64.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();

        let from_read_signals = events_from_read_signals(&path).unwrap();
        let times: Vec<u64> = from_read_signals.iter().map(|(time, _, _)| *time).collect();
        assert_eq!(
            times,
            vec![0, 10, 20],
            "read_signals drops the change at 30"
        );
        assert!(from_read_signals.iter().all(|(time, _, _)| *time <= 20));

        let mut reader = open(&path).unwrap();
        assert_eq!(reader.get_header().end_time, 20);
        let section = reader.read_section(0).unwrap();
        let mut times = Vec::new();
        section
            .for_each_change(FstSignalHandle::from_index(0), |ti, _| {
                times.push(section.time_table()[ti])
            })
            .unwrap();
        // The value at time 0 is in the frame, not in the change data.
        assert_eq!(times, vec![10, 20, 30], "for_each_change does not cut");

        // The cutoff at the last time that is not later than the end time gives the cut.
        let last_time_index = section
            .time_table()
            .iter()
            .take_while(|&&t| t <= 20)
            .count()
            - 1;
        let mut times = Vec::new();
        section
            .for_each_change_until(FstSignalHandle::from_index(0), last_time_index, |ti, _| {
                times.push(section.time_table()[ti])
            })
            .unwrap();
        assert_eq!(times, vec![10, 20], "for_each_change_until cuts");
    }
}
