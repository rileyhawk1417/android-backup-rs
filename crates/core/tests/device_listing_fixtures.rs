//! E03.1 acceptance: the device-listing parser against real-world adb output.
//!
//! The fixtures in `tests/fixtures/devices/` are kept as files rather than inline
//! strings on purpose — they are the sample corpus that grows every time a real
//! phone surprises us (`adb devices -l` output differs between adb versions,
//! transports and operating systems), and a reviewer can diff them.
//!
//! The table below is exhaustive: [`every_fixture_is_covered`] fails when a
//! fixture exists that the table does not describe, so adding a sample forces
//! someone to say what it should parse to.

// A test that cannot fail loudly is a test nobody trusts.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use android_backup_core::adb::parse::{parse_devices, DeviceSnapshot};

/// Expected `(serial, state token, model)`. The state is the token adb prints,
/// not a `RawState`: `Unrecognized` carries a `String`, so a table of raw states
/// cannot be a `'static` slice.
type ExpectedDevice = (&'static str, &'static str, Option<&'static str>);

/// What a fixture is expected to parse to.
struct Expected {
    file: &'static str,
    devices: &'static [ExpectedDevice],
}

fn table() -> Vec<Expected> {
    vec![
        Expected {
            file: "plain.txt",
            devices: &[("ABC123", "device", None)],
        },
        Expected {
            file: "verbose.txt",
            devices: &[("R58M12ABCDE", "device", Some("SM_A536B"))],
        },
        Expected {
            file: "unauthorized.txt",
            devices: &[("ABC123", "unauthorized", None)],
        },
        Expected {
            file: "offline.txt",
            devices: &[("ABC123", "offline", None)],
        },
        Expected {
            file: "mixed_emulator_and_physical.txt",
            devices: &[
                ("emulator-5554", "device", Some("sdk_gphone64_x86_64")),
                ("R58M12ABCDE", "device", Some("SM_A536B")),
            ],
        },
        // A wifi-adb serial contains a colon; splitting on whitespace would
        // truncate it to an IP address and lose the port.
        Expected {
            file: "wifi_adb.txt",
            devices: &[("192.168.1.20:5555", "device", Some("Pixel_5"))],
        },
        Expected {
            file: "crlf.txt",
            devices: &[("ABC123", "device", None), ("DF94", "unauthorized", None)],
        },
        Expected {
            file: "trailing_whitespace.txt",
            devices: &[("ABC123", "device", None)],
        },
        Expected {
            file: "daemon_chatter.txt",
            devices: &[("ABC123", "device", Some("SM_A536B"))],
        },
        Expected {
            file: "unknown_state.txt",
            devices: &[("ABC123", "sideload", None)],
        },
        Expected {
            file: "empty.txt",
            devices: &[],
        },
    ]
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("devices")
}

fn read(file: &str) -> String {
    let path = fixtures_dir().join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// Byte-for-byte read, for fixtures whose line endings must not be normalized by
/// an editor or by `read_to_string`'s newline handling.
fn read_bytes(file: &str) -> Vec<u8> {
    let path = fixtures_dir().join(file);
    std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

#[test]
fn every_fixture_parses_to_what_the_table_says() {
    for case in table() {
        let output = read(case.file);
        let devices = parse_devices(&output)
            .unwrap_or_else(|err| panic!("{} must not be a parse error: {err}", case.file));
        let actual: Vec<(&str, &str, Option<&str>)> = devices
            .iter()
            .map(|d| (d.serial.as_str(), d.state.as_str(), d.model.as_deref()))
            .collect();
        let expected: Vec<(&str, &str, Option<&str>)> = case
            .devices
            .iter()
            .map(|(serial, state, model)| (*serial, *state, *model))
            .collect();
        assert_eq!(actual, expected, "fixture {}", case.file);
    }
}

#[test]
fn every_fixture_is_covered() {
    let described: Vec<&str> = table().iter().map(|case| case.file).collect();
    let mut on_disk: Vec<String> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures directory")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".txt"))
        .collect();
    on_disk.sort();
    let mut described_sorted = described.clone();
    described_sorted.sort();
    assert_eq!(
        on_disk, described_sorted,
        "a fixture on disk is not in the expectation table (or vice versa)"
    );
    assert!(
        on_disk.len() >= 6,
        "the wave doc asks for at least 6 real samples, found {}",
        on_disk.len()
    );
}

#[test]
fn no_fixture_is_a_parse_error() {
    // `parse_devices` is lenient by contract: a future adb inventing a state, a
    // half-written line or daemon chatter must degrade into "we parsed what we
    // could", never into an error screen.
    for case in table() {
        assert!(
            parse_devices(&read(case.file)).is_ok(),
            "{} must parse",
            case.file
        );
    }
}

#[test]
fn the_crlf_fixture_actually_contains_carriage_returns() {
    // Guard against a tool "fixing" the line endings, which would silently turn
    // the CRLF regression test into a copy of `plain.txt`.
    let bytes = read_bytes("crlf.txt");
    assert!(
        bytes.windows(2).any(|pair| pair == b"\r\n"),
        "crlf.txt lost its CRLF line endings"
    );
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\r' {
            assert_eq!(
                bytes.get(index + 1),
                Some(&b'\n'),
                "lone CR at byte {index}: every CR must terminate a CRLF"
            );
        }
    }
}

/// The wave doc's "random spacing still parses" requirement, as a generated
/// matrix rather than a property-testing dependency.
#[test]
fn arbitrary_spacing_around_entries_still_parses() {
    let separators = ["\t", "\t\t", " ", "  ", " \t "];
    let trailers = ["", " ", "  ", "\t"];
    let states = [
        ("device", "device"),
        ("unauthorized", "unauthorized"),
        ("offline", "offline"),
    ];
    let mut checked = 0;
    for separator in separators {
        for trailer in trailers {
            for (token, expected) in states {
                for serial in ["ABC123", "192.168.1.20:5555", "emulator-5554"] {
                    let output =
                        format!("List of devices attached\n{serial}{separator}{token}{trailer}\n");
                    let devices = parse_devices(&output).unwrap();
                    assert_eq!(devices.len(), 1, "{output:?}");
                    assert_eq!(devices[0].serial, serial, "{output:?}");
                    assert_eq!(devices[0].state.as_str(), expected, "{output:?}");
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 5 * 4 * 3 * 3);
}

#[test]
fn the_header_is_not_assumed_to_be_the_first_line() {
    let noisy = "* daemon not running; starting now at tcp:5037\n\
                 adb server version (41) doesn't match this client (39)\n\
                 * daemon started successfully\n\
                 List of devices attached\n\
                 ABC123\tdevice\n";
    let devices = parse_devices(noisy).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].serial, "ABC123");
}

#[test]
fn the_verbose_fixture_keeps_every_field_it_reports() {
    let devices = parse_devices(&read("verbose.txt")).unwrap();
    let device: &DeviceSnapshot = &devices[0];
    assert_eq!(device.serial, "R58M12ABCDE");
    assert_eq!(device.model.as_deref(), Some("SM_A536B"));
    assert_eq!(device.product.as_deref(), Some("a53x"));
    assert_eq!(device.transport_id.as_deref(), Some("3"));
}

/// A sanity check on the corpus itself: the fixtures must collectively exercise
/// every raw state the parser knows about, or the table above is not really a
/// corpus.
#[test]
fn the_corpus_covers_every_known_state() {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for case in table() {
        for device in parse_devices(&read(case.file)).unwrap() {
            *seen.entry(device.state.as_str().to_string()).or_default() += 1;
        }
    }
    for state in ["device", "unauthorized", "offline", "sideload"] {
        assert!(seen.contains_key(state), "no fixture reports {state:?}");
    }
}
