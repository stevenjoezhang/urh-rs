//! Integration tests for the v2 AWRE field label service
//! (docs/ipc-contract.md, "AWRE field labels").
//!
//! The service types live in a private module, so these tests drive the
//! public method with primitive arguments and `&Default::default()` (the
//! all-auto analysis parameterization, identical to the CLI `participants`
//! flow).

use urh_app::SignalRepository;

const ASK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/ask.complex");
const FSK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/fsk.complex");
const HOMEMATIC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/data/homematic.complex32s"
);
const ELEKTROMATEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/data/elektromaten.complex16s"
);

/// CLI `participants` golden: single-message files carry no labels.
#[test]
fn ask_and_fsk_have_no_labels() {
    let repo = SignalRepository::new();
    for path in [ASK, FSK] {
        let id = repo.open(path, None).unwrap().signal_id;
        let labels = repo.run_awre(&id, None, &Default::default()).unwrap();
        assert!(labels.is_empty(), "{path}: {labels:?}");
    }
}

/// CLI `participants` golden for homematic.complex32s: a 32-bit preamble
/// followed by a 32-bit synchronization field on both messages, sorted by
/// message then position.
#[test]
fn homematic_labels_match_cli() {
    let repo = SignalRepository::new();
    let id = repo.open(HOMEMATIC, None).unwrap().signal_id;
    let labels = repo.run_awre(&id, None, &Default::default()).unwrap();

    let expected = [
        (0usize, "preamble", 0usize, 32usize, 0usize),
        (0, "synchronization", 32, 64, 1),
        (1, "preamble", 0, 32, 0),
        (1, "synchronization", 32, 64, 1),
    ];
    assert_eq!(labels.len(), expected.len(), "{labels:?}");
    for (label, (message_index, name, bit_start, bit_end, color_index)) in
        labels.iter().zip(expected)
    {
        assert_eq!(label.message_index, message_index);
        assert_eq!(label.name, name);
        assert_eq!(label.bit_start, bit_start);
        assert_eq!(label.bit_end, bit_end, "bit_end must be exclusive");
        assert_eq!(label.color_index, color_index);
    }
}

/// The full-range restriction is equivalent to no restriction.
#[test]
fn homematic_full_range_equals_no_range() {
    let repo = SignalRepository::new();
    let id = repo.open(HOMEMATIC, None).unwrap().signal_id;
    let count = repo.open(HOMEMATIC, None).unwrap(); // sample count via meta
    let n = count.sample_count;
    let whole = repo
        .run_awre(&id, Some((0, n)), &Default::default())
        .unwrap();
    let bare = repo.run_awre(&id, None, &Default::default()).unwrap();
    assert_eq!(whole, bare);
}

/// The known FormatFinder panic on elektromaten (unequal message lengths in
/// the CRC check) surfaces as an `internal` error instead of aborting the
/// process.
#[test]
fn elektromaten_panic_is_contained() {
    let repo = SignalRepository::new();
    let id = repo.open(ELEKTROMATEN, None).unwrap().signal_id;
    let err = repo.run_awre(&id, None, &Default::default()).unwrap_err();
    assert_eq!(err.code, "internal", "{err}");
    assert!(err.message.contains("panicked"), "{err}");
}

#[test]
fn errors_propagate() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;

    let err = repo
        .run_awre("sig-99", None, &Default::default())
        .unwrap_err();
    assert_eq!(err.code, "unknown-signal");

    let err = repo
        .run_awre(&id, Some((10, 5)), &Default::default())
        .unwrap_err();
    assert_eq!(err.code, "invalid-range", "{err}");

    let err = repo
        .run_awre(&id, Some((0, 13711)), &Default::default())
        .unwrap_err();
    assert_eq!(err.code, "invalid-range", "{err}");
}
