use fst_reader::{FstFilter, FstReader};
use std::io::Cursor;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

const CASES: [(&str, u64); 3] = [
    ("fsts/verilator/new_attributes_pull_24.fst", 0x85f3_9d17),
    ("fsts/icarus/test1.vcd.fst", 0x1a2b_3c4d),
    ("fsts/ghdl/oscar/ghdl.fst", 0x9e37_79b9),
];
const ROUNDS: usize = if cfg!(debug_assertions) { 10 } else { 100 };

fn rounds() -> usize {
    std::env::var("FST_CORRUPT_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(ROUNDS)
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

fn exercise(bytes: Vec<u8>) {
    if let Ok(mut reader) = FstReader::open(Cursor::new(bytes.clone())) {
        let _ = reader.read_hierarchy(|_| {});
        let _ = reader.read_signals(&FstFilter::all(), |_, _, _| Ok::<_, ()>(()));
    }

    if let Ok(mut reader) = FstReader::open_and_read_time_table(Cursor::new(bytes)) {
        let sections = reader.sections();
        for index in 0..sections.len() {
            if let Ok(section) = reader.read_section(index) {
                let _ = section.for_each_frame_value(|_, _| {});
                for handle in 0..section.max_handle() {
                    let _ = section.for_each_change(
                        fst_reader::FstSignalHandle::from_index(handle),
                        |_, _| {},
                    );
                }
            }
        }
    }
}

#[test]
fn byte_flips_do_not_panic() {
    let rounds = rounds();
    let mut failures = Vec::new();
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        for (relative_path, seed) in CASES {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path);
            let original = std::fs::read(&path).expect("read corpus file");
            let mut rng = seed;
            for mutation_index in 0..rounds {
                let mut bytes = original.clone();
                let count = (next(&mut rng) % 3 + 1) as usize;
                let mut changes = Vec::new();
                for _ in 0..count {
                    let random = next(&mut rng) as usize;
                    let offset = if next(&mut rng) & 1 == 0 {
                        if random & 1 == 0 {
                            random % bytes.len().min(512)
                        } else {
                            bytes.len() - bytes.len().min(8 * 1024)
                                + random % bytes.len().min(8 * 1024)
                        }
                    } else {
                        random % bytes.len()
                    };
                    let old = bytes[offset];
                    let mask = (next(&mut rng) as u8) | 1;
                    bytes[offset] ^= mask;
                    changes.push((offset, old, bytes[offset]));
                }
                let truncated_to = if mutation_index % 10 == 9 {
                    let length = (next(&mut rng) as usize) % bytes.len();
                    bytes.truncate(length);
                    Some(length)
                } else {
                    None
                };
                if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| exercise(bytes))) {
                    failures.push(format!(
                        "{} mutation {mutation_index} flips {changes:?}, truncation {truncated_to:?}: {}",
                        path.display(), panic_message(payload.as_ref())
                    ));
                }
            }
        }
    }));
    panic::set_hook(previous_hook);
    if let Err(payload) = result {
        failures.push(panic_message(payload.as_ref()));
    }
    assert!(
        failures.is_empty(),
        "corruption failures:\n{}",
        failures.join("\n")
    );
}
