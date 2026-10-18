//! Non-shipping #2826 comparisons at the candidate's public streaming face.
//! A disagreement rejects substitution; it is not a passing replacement.

use miniz_oxide::deflate::core::{CompressorOxide, create_comp_flags_from_zip_params};
use miniz_oxide::deflate::stream::deflate;
use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZFlush};
use std::collections::BTreeMap;

fn present<T>(value: Option<T>) -> T {
    let mut values: Vec<T> = value.into_iter().collect();
    assert_eq!(
        values.len(),
        1,
        "missing or invalid qualification fixture field"
    );
    values.remove(0)
}

fn unhex(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    assert_eq!(text.len() % 2, 0);
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = present(std::str::from_utf8(pair).ok());
            present(u8::from_str_radix(text, 16).ok())
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    if bytes.is_empty() {
        return "-".into();
    }
    bytes
        .iter()
        .flat_map(|b| {
            [
                char::from(DIGITS[usize::from(b >> 4)]),
                char::from(DIGITS[usize::from(b & 15)]),
            ]
        })
        .collect()
}

fn compare(rows: &str) -> Vec<(String, bool)> {
    let mut inputs = BTreeMap::new();
    let mut observations = Vec::new();
    for line in rows.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        if fields[0] == "input" {
            inputs.insert(fields[1], unhex(fields[2]));
        } else if fields[0] == "deflate" && fields[2] != "31" {
            assert_eq!(fields.len(), 6);
            let input = &inputs[fields[1]];
            let level = present(fields[3].parse().ok());
            let strategy = present(fields[4].parse().ok());
            let window = present(fields[2].parse().ok());
            let mut codec =
                CompressorOxide::new(create_comp_flags_from_zip_params(level, window, strategy));
            let mut output = [0; 8192];
            let step = deflate(&mut codec, input, &mut output, MZFlush::Finish);
            assert_eq!(step.bytes_consumed, input.len());
            assert_eq!(step.status, Ok(miniz_oxide::MZStatus::StreamEnd));
            let bytes = &output[..step.bytes_written];
            let same = bytes == unhex(fields[5]);
            let key = fields[1..5].join(":");
            eprintln!(
                "qualification\tdeflate\t{key}\t{same}\t{}\t{}\t{}",
                step.bytes_consumed,
                step.bytes_written,
                hex(bytes)
            );
            observations.push((key, same));
        }
    }
    assert_eq!(observations.len(), 120);
    observations
}

#[test]
fn the_bounded_oracle_bytes_reject_unmodified_miniz() {
    let rows = include_str!("../../tests/data/oracle.tsv");
    assert!(rows.lines().any(|line| line.starts_with("deflate\t")));
    let observations = compare(rows);
    assert!(observations.iter().any(|(_, same)| !same));
}

#[test]
fn partial_flush_is_not_legacy_partial_flush() {
    let rows = include_str!("../../tests/data/oracle.tsv");
    let row = present(
        rows.lines()
            .find(|line| line.starts_with("flush\t-15\t1\t0\t")),
    );
    let fields: Vec<_> = row.split('\t').collect();
    let mut codec = CompressorOxide::new(create_comp_flags_from_zip_params(6, -15, 0));
    let mut output = [0; 8192];
    let step = deflate(&mut codec, &unhex(fields[4]), &mut output, MZFlush::Partial);
    assert_ne!(&output[..step.bytes_written], unhex(fields[5]));
    eprintln!(
        "qualification\tpartial\t{}\t{}",
        fields[5],
        hex(&output[..step.bytes_written])
    );
    assert!(MZFlush::new(5).is_err());
}

#[test]
fn incremental_inflate_records_bytes_consumption_status_and_errors() {
    let rows = include_str!("../../tests/data/oracle.tsv");
    let mut name = "";
    let mut codec = InflateState::new_boxed(DataFormat::Zlib);
    let mut total = 0;
    let mut count = 0;
    let mut mismatches = 0;
    let mut valid_output = Vec::new();
    for line in rows
        .lines()
        .filter(|line| line.starts_with("inflate-step\t"))
    {
        let fields: Vec<_> = line.split('\t').collect();
        if fields[1] != name {
            name = fields[1];
            codec.reset(DataFormat::Zlib);
            total = 0;
        }
        let mut output = [0; 8192];
        let step = inflate(&mut codec, &unhex(fields[3]), &mut output, MZFlush::Sync);
        total += step.bytes_consumed;
        if name == "valid" {
            valid_output.extend_from_slice(&output[..step.bytes_written]);
        }
        let status = step.status.map_or_else(|e| e as i32, |s| s as i32);
        let same = status.to_string() == fields[5]
            && total.to_string() == fields[6]
            && (fields[4] == "false" || output[..step.bytes_written] == unhex(fields[4]));
        mismatches += usize::from(!same);
        count += 1;
        eprintln!(
            "qualification\tinflate\t{name}:{}\t{same}\t{status}\t{total}\t{}\t{}",
            fields[2],
            step.bytes_written,
            hex(&output[..step.bytes_written])
        );
    }
    assert!(count > 20);
    // A valid stream must still decode. This does not qualify error progression.
    let valid: Vec<_> = rows
        .lines()
        .filter(|l| l.starts_with("inflate-step\tvalid\t"))
        .collect();
    assert!(!valid.is_empty());
    let text = present(rows.lines().find(|line| line.starts_with("input\ttext\t")));
    assert_eq!(valid_output, unhex(present(text.split('\t').nth(2))));
    eprintln!("qualification\tinflate-counts\t{count}\t{mismatches}");
}

#[test]
fn actual_candidate_storage_is_not_a_deferred_legacy_window() {
    eprintln!(
        "qualification\tminiz-layout\t{}\t{}\t{}\t{}",
        std::mem::size_of::<InflateState>(),
        std::mem::align_of::<InflateState>(),
        std::mem::size_of::<CompressorOxide>(),
        std::mem::align_of::<CompressorOxide>()
    );
    // The public state already contains its 32KiB history at initialization.
    assert!(std::mem::size_of::<InflateState>() > 32768);
}

use super::z;
use std::alloc::{Layout, alloc, dealloc};
use std::ffi::c_void;

#[derive(Default)]
struct Storage {
    live: BTreeMap<usize, Layout>,
    events: Vec<String>,
    attempts: usize,
    refuse: usize,
}

extern "C" fn allocate(opaque: *mut c_void, items: z::uInt, size: z::uInt) -> *mut c_void {
    // SAFETY: each test supplies its live Storage exclusively for the call;
    // the library invokes these callbacks synchronously and never runs PHP.
    let storage = unsafe { &mut *opaque.cast::<Storage>() };
    storage.attempts += 1;
    let bytes = items as usize * size as usize;
    if storage.attempts == storage.refuse {
        storage.events.push(format!("refuse:{bytes}"));
        return std::ptr::null_mut();
    }
    let layout = present(Layout::from_size_align(bytes, 16).ok());
    // SAFETY: positive zlib allocation sizes and valid, explicitly aligned layout.
    let address = unsafe { alloc(layout) };
    if !address.is_null() {
        storage.live.insert(address as usize, layout);
        storage
            .events
            .push(format!("allocate:{items}:{size}:{bytes}:16"));
    }
    address.cast()
}

extern "C" fn release(opaque: *mut c_void, address: *mut c_void) {
    // SAFETY: same synchronous, exclusively held test Storage as allocate.
    let storage = unsafe { &mut *opaque.cast::<Storage>() };
    let layout = present(storage.live.remove(&(address as usize)));
    storage.events.push(format!("release:{}", layout.size()));
    // SAFETY: address was allocated with this exact layout and removed once.
    unsafe { dealloc(address.cast(), layout) };
}

// zlib retains the stream's address, including when this function returns.
#[allow(clippy::unnecessary_box_returns)]
fn stream(storage: &mut Storage) -> Box<z::z_stream> {
    let mut stream = super::fresh(false);
    stream.zalloc = allocate;
    stream.zfree = release;
    stream.opaque = std::ptr::from_mut(storage).cast();
    stream
}

#[test]
fn legacy_inflate_defers_actual_window_storage_and_retains_it_on_reset() {
    for window in [8, 15] {
        let mut storage = Storage::default();
        let mut stream = stream(&mut storage);
        // SAFETY: boxed stream has a fixed address; callbacks and storage live
        // until end, and the header version/stream size follow the binding.
        let code = unsafe {
            z::inflateInit2_(
                &raw mut *stream,
                -window,
                super::header_version(),
                super::stream_size(),
            )
        };
        assert_eq!(code, z::Z_OK);
        assert_eq!(storage.live.len(), 1);
        eprintln!(
            "qualification\tlegacy-init\t{window}\t{}",
            storage.events.join(",")
        );
        let input = unhex(
            "2bc94855282ccd4cce56482aca2fcf5348cbaf50c82acdcd2d2856c82f4b2d5228c94855c849acaa5448c94fe72a19153d8aaa51d1a3a24745cf4a3400",
        );
        let mut output = [0; 7];
        stream.next_in = input.as_ptr().cast_mut();
        stream.avail_in = super::avail(input.len());
        stream.next_out = output.as_mut_ptr();
        stream.avail_out = super::avail(output.len());
        // SAFETY: input/output buffers remain live and have the stated lengths.
        let code = unsafe { z::inflate(&raw mut *stream, z::Z_SYNC_FLUSH) };
        assert_eq!(code, z::Z_OK);
        assert_eq!(storage.live.len(), 2);
        assert!(
            storage
                .live
                .values()
                .any(|layout| layout.size() == 1 << window)
        );
        let attempts = storage.attempts;
        // SAFETY: initialized stream and live callbacks, no outstanding call.
        assert_eq!(unsafe { z::inflateReset(&raw mut *stream) }, z::Z_OK);
        assert_eq!(storage.attempts, attempts);
        assert_eq!(storage.live.len(), 2);
        // SAFETY: initialized stream ended exactly once, storage still lives.
        assert_eq!(unsafe { z::inflateEnd(&raw mut *stream) }, z::Z_OK);
        assert!(storage.live.is_empty());
        eprintln!(
            "qualification\tlegacy-lifetime\t{window}\t{}",
            storage.events.join(",")
        );
    }
}

#[test]
fn legacy_deflate_partial_initialization_releases_real_storage() {
    for refuse in 0..=5 {
        let mut storage = Storage {
            refuse,
            ..Storage::default()
        };
        let mut stream = stream(&mut storage);
        // SAFETY: fixed-address stream, live synchronous storage callbacks,
        // compatible header and size; refused allocations return null.
        let code = unsafe {
            z::deflateInit2_(
                &raw mut *stream,
                6,
                z::Z_DEFLATED,
                -15,
                8,
                0,
                super::header_version(),
                super::stream_size(),
            )
        };
        if refuse == 0 {
            assert_eq!(code, z::Z_OK);
            assert_eq!(storage.live.len(), 5);
            // SAFETY: successful initialized stream ended once with live storage.
            assert_eq!(unsafe { z::deflateEnd(&raw mut *stream) }, z::Z_OK);
        } else {
            assert_eq!(code, z::Z_MEM_ERROR);
        }
        assert!(storage.live.is_empty());
        eprintln!(
            "qualification\tlegacy-deflate-failure\t{refuse}\t{code}\t{}",
            storage.events.join(",")
        );
    }
}

#[test]
fn bounded_raw_call_progress_is_recorded_separately_from_byte_equality() {
    let rows = include_str!("../../tests/data/oracle.tsv");
    let text = present(rows.lines().find(|line| line.starts_with("input\ttext\t")));
    let input = unhex(present(text.split('\t').nth(2)));
    let expected = present(
        rows.lines()
            .find(|line| line.starts_with("deflate\ttext\t-15\t6\t0\t")),
    );
    let expected = unhex(present(expected.split('\t').nth(5)));
    for capacity in [1, 7, 8192] {
        let mut storage = Storage::default();
        let mut stream = stream(&mut storage);
        // SAFETY: fixed stream address, live callbacks, matching header/size.
        let code = unsafe {
            z::deflateInit2_(
                &raw mut *stream,
                6,
                z::Z_DEFLATED,
                -15,
                8,
                0,
                super::header_version(),
                super::stream_size(),
            )
        };
        assert_eq!(code, z::Z_OK);
        let mut codec = CompressorOxide::new(create_comp_flags_from_zip_params(6, -15, 0));
        for backend in ["legacy", "miniz"] {
            let mut consumed = 0;
            let mut bytes = Vec::new();
            for call in 0..8192 {
                let mut output = vec![0; capacity];
                let (read, written, status) = if backend == "legacy" {
                    stream.next_in = input[consumed..].as_ptr().cast_mut();
                    stream.avail_in = super::avail(input.len() - consumed);
                    stream.next_out = output.as_mut_ptr();
                    stream.avail_out = super::avail(capacity);
                    // SAFETY: live input/output slices with exact available lengths.
                    let code = unsafe { z::deflate(&raw mut *stream, z::Z_FINISH) };
                    (
                        input.len() - consumed - stream.avail_in as usize,
                        capacity - stream.avail_out as usize,
                        code,
                    )
                } else {
                    let step =
                        deflate(&mut codec, &input[consumed..], &mut output, MZFlush::Finish);
                    (
                        step.bytes_consumed,
                        step.bytes_written,
                        step.status.map_or_else(|e| e as i32, |s| s as i32),
                    )
                };
                consumed += read;
                bytes.extend_from_slice(&output[..written]);
                eprintln!(
                    "qualification\traw-progress\t{backend}\t{capacity}\t{call}\t{read}\t{written}\t{status}\t{}",
                    hex(&output[..written])
                );
                if status == z::Z_STREAM_END {
                    break;
                }
                assert_eq!(status, z::Z_OK);
                assert!(read + written > 0);
                assert!(call < 8191, "bounded driver exhausted");
            }
            assert_eq!(consumed, input.len());
            if backend == "legacy" {
                assert_eq!(bytes, expected);
            } else {
                assert_ne!(bytes, expected);
            }
        }
        // SAFETY: initialized stream ended once with its storage still live.
        assert_eq!(unsafe { z::deflateEnd(&raw mut *stream) }, z::Z_OK);
        assert!(storage.live.is_empty());
    }
}

#[test]
fn mysql_source_bounded_output_rejects_expansion_without_growing() {
    let rows = include_str!("../../tests/data/oracle.tsv");
    for name in ["text", "random"] {
        let row = present(
            rows.lines()
                .find(|line| line.starts_with(&format!("input\t{name}\t"))),
        );
        let mut input = vec![0, 0, 0, 0]; // normal packet header belongs inside the compressed envelope
        input.extend_from_slice(&unhex(present(row.split('\t').nth(2))));
        let mut output = vec![0; input.len()];
        let mut length = input.len() as z::uLongf;
        // SAFETY: both buffers have their stated lengths and remain live;
        // compress uses and releases the library's own system storage.
        let code = unsafe {
            z::compress(
                output.as_mut_ptr(),
                &raw mut length,
                input.as_ptr(),
                input.len() as z::uLong,
            )
        };
        let expected = if name == "text" {
            z::Z_OK
        } else {
            z::Z_BUF_ERROR
        };
        assert_eq!(code, expected);
        eprintln!(
            "qualification\tmysql-bounded\t{name}\t{}\t{length}\t{code}\t{}",
            input.len(),
            hex(&output[..present(usize::try_from(length).ok())])
        );
    }
}

#[test]
fn zip_physical_entry_bytes_are_not_the_empty_entry_api_fast_path() {
    let rows = include_str!("../../tests/data/oracle.tsv");
    let mut count = 0;
    for line in rows.lines().filter(|line| line.starts_with("zip-entry\t")) {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(
            fields.len(),
            8,
            "physical bytes and API observation must be separate"
        );
        let bytes = unhex(fields[4]);
        assert_eq!(bytes.len(), present(fields[5].parse::<usize>().ok()));
        let input = present(
            rows.lines()
                .find(|line| line.starts_with(&format!("input\t{}\t", fields[1]))),
        );
        let decoded = present(miniz_oxide::inflate::decompress_to_vec(&bytes).ok());
        assert_eq!(decoded, unhex(present(input.split('\t').nth(2))));
        if fields[1] == "empty" {
            assert_eq!(bytes, [3, 0]);
            assert_eq!(fields[6], "string");
            assert_eq!(fields[7], "-");
        }
        count += 1;
    }
    assert_eq!(count, 12);
}
