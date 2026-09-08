//! An uploaded CSV/TSV: delimiter sniffing, quoting, the header row,
//! encodings that are not UTF-8, and rows of any width.

#![no_main]

use flash_store::import::parse_csv;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_csv(data);
});
