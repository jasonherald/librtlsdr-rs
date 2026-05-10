//! Live hardware tests for the smol streaming variant.
//!
//! Mirror of `tests/live_streaming.rs`'s tokio Stream tests, against
//! `RtlSdrReader::stream_samples_smol`. Same gating
//! (`#![cfg(feature = "smol")]` + `#[ignore]`) — they require a real
//! RTL-SDR plugged in. Run with the dongle attached:
//!
//! ```text
//! cargo test --features smol --test live_streaming_smol -- --ignored
//! ```
//!
//! Closes the smol-coverage gap from audit issue #13. The smol path
//! has runtime-specific implementation details that differ from
//! tokio (`blocking::unblock` returns a `Task` we `.detach()`,
//! `async_channel` instead of `tokio::sync::mpsc`); without
//! parallel hardware coverage, drop-semantics or back-pressure
//! regressions in `blocking::unblock` would silently slip through.

#![cfg(feature = "smol")]
// Same rationale as `live_streaming.rs`: integration tests
// legitimately use `panic!` for assertion-failure messaging and
// reference identifiers without backticks in narrative doc
// comments; both lints fire on hot-path library code, not
// diagnostic test code.
#![allow(clippy::panic, clippy::doc_markdown)]

use std::time::Duration;

use librtlsdr_rs::{RtlSdrDevice, RtlSdrError, rusb};

/// Helper: open device 0 and configure for FM broadcast tuning.
/// Skips the test by returning `None` if no device is plugged
/// in — keeps `--ignored` runs informative without a hard panic
/// when the dongle is unplugged mid-suite.
///
/// Duplicated from `live_streaming.rs` rather than extracted to a
/// shared `tests/common/mod.rs` because the two test files are
/// compiled as separate test binaries and the shared module would
/// only save a handful of lines.
fn open_or_skip(test_name: &str) -> Option<RtlSdrDevice> {
    if librtlsdr_rs::get_device_count() == 0 {
        eprintln!("[{test_name}] no RTL-SDR plugged in; skipping");
        return None;
    }
    match RtlSdrDevice::open(0) {
        Ok(mut dev) => {
            // Stable, valid-everywhere config: FM broadcast in
            // most regions, 2.048 Msps.
            //
            // Config failures here are real bugs (the device
            // opened cleanly, so the configuration call should
            // succeed) — fail the test loudly rather than treating
            // them as "skip" via the pre-#13-round-2 `.ok()?`
            // pattern that masked them as missing-hardware.
            dev.set_sample_rate(2_048_000)
                .unwrap_or_else(|e| panic!("[{test_name}] set_sample_rate failed: {e}"));
            dev.set_center_freq(100_000_000)
                .unwrap_or_else(|e| panic!("[{test_name}] set_center_freq failed: {e}"));
            dev.reset_buffer()
                .unwrap_or_else(|e| panic!("[{test_name}] reset_buffer failed: {e}"));
            Some(dev)
        }
        Err(e) => {
            // Same Busy-vs-generic-open distinction as the tokio
            // sibling file — see its comment for the rationale.
            // Per audit issue #31.
            if matches!(e, RtlSdrError::Usb(rusb::Error::Busy)) {
                eprintln!(
                    "[{test_name}] device busy: {e}; skipping. \
                     If running these tests, pass `--test-threads=1` to serialize \
                     them (parallel access to USB interface 0 collides), or check \
                     for another rtl-sdr process holding the device."
                );
            } else {
                eprintln!("[{test_name}] open failed: {e}; skipping");
            }
            None
        }
    }
}

/// Smoke: smol stream yields real bytes.
///
/// Mirror of `tokio_stream_yields_bytes`. Validates the
/// `blocking::unblock` + `async_channel` + `Stream` impl
/// composition against real hardware.
#[test]
#[ignore = "needs real RTL-SDR hardware — run with --ignored"]
fn smol_stream_yields_bytes() {
    use futures_util::StreamExt;

    let Some(dev) = open_or_skip("smol_stream_yields_bytes") else {
        return;
    };

    let reader = dev.reader();
    let stream = reader
        .stream_samples_smol(0)
        .map_err(|boxed| boxed.0)
        .expect("stream_samples_smol should succeed on a free device");

    smol::block_on(async move {
        let mut stream = Box::pin(stream);
        for i in 0..3 {
            let item = stream
                .next()
                .await
                .unwrap_or_else(|| panic!("stream ended unexpectedly at buffer {i}"))
                .unwrap_or_else(|e| panic!("read error at buffer {i}: {e}"));
            assert!(
                !item.is_empty(),
                "buffer {i} was empty (expected ≥1 byte from a configured device)",
            );
        }
    });
}

/// Mirror of `parent_can_retune_during_stream`: pin the design-
/// pivot contract end-to-end for smol — parent retains
/// `&mut device` for control while the smol worker is reading.
#[test]
#[ignore = "needs real RTL-SDR hardware — run with --ignored"]
fn smol_parent_can_retune_during_stream() {
    use futures_util::StreamExt;

    let Some(mut dev) = open_or_skip("smol_parent_can_retune_during_stream") else {
        return;
    };

    let reader = dev.reader();
    let stream = reader
        .stream_samples_smol(0)
        .map_err(|boxed| boxed.0)
        .expect("stream_samples_smol should succeed on a free device");

    let mut stream = Box::pin(stream);

    // Drain one buffer at the initial freq.
    let _first = smol::block_on(stream.next())
        .expect("stream ended early")
        .expect("first read failed");

    // Retune the parent while the stream is live. Same shared-
    // handle pattern as the tokio variant — different USB
    // endpoints (control 0x00 vs bulk 0x81), no rusb-level
    // conflict; the busy-flag from #7 doesn't gate control
    // methods.
    dev.set_center_freq(99_000_000)
        .expect("retune during streaming should succeed");
    dev.set_tuner_gain(150)
        .expect("gain change during streaming should succeed");

    // Drain another buffer at the new freq — proves the stream
    // is still alive after the parent's control activity.
    let buf = smol::block_on(stream.next())
        .expect("stream ended after retune")
        .expect("post-retune read failed");
    assert!(!buf.is_empty(), "post-retune buffer was empty");
}

/// Mirror of `dropping_stream_stops_worker`: drop the smol stream
/// and confirm the worker exits without deadlocking the test.
///
/// `blocking::unblock` returns a `Task` we `.detach()` in the
/// stream constructor — the worker should observe the
/// `async_channel::Sender::is_closed()` between reads and exit.
/// Mid-read drops still wait for the in-flight bulk transfer to
/// return (~5 s worst case on a stalled device, documented on
/// `stream_samples_smol`).
#[test]
#[ignore = "needs real RTL-SDR hardware — run with --ignored"]
fn smol_dropping_stream_stops_worker() {
    use futures_util::StreamExt;

    let Some(dev) = open_or_skip("smol_dropping_stream_stops_worker") else {
        return;
    };

    let reader = dev.reader();
    let stream = reader
        .stream_samples_smol(0)
        .map_err(|boxed| boxed.0)
        .expect("stream_samples_smol should succeed on a free device");

    smol::block_on(async move {
        let mut stream = Box::pin(stream);

        // Drain one buffer.
        let _ = stream
            .next()
            .await
            .expect("stream ended early")
            .expect("read failed");

        // Drop the stream. The worker's
        // `tx.is_closed()` check between reads + the
        // `send_blocking` failure after each read cooperate to
        // exit the worker on the happy path.
        drop(stream);

        // Give the worker a moment to observe the drop.
        // We can't directly observe the worker exiting (the
        // detached Task handle was discarded inside
        // stream_samples_smol); the test confirms the smol
        // executor doesn't deadlock.
        smol::Timer::after(Duration::from_millis(500)).await;
    });
}
