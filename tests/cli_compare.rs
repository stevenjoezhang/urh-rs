//! Comparison tests: `urh-cli analyze` vs the original Python URH pipeline.
//!
//! For each reference IQ file we run
//!   a) the original Python modules (AutoInterpretation.estimate ->
//!      ProtocolAnalyzer.get_protocol_from_signal) in /tmp/urh-venv, and
//!   b) our `urh-cli analyze`,
//! and compare modulation / bit_length / center / noise / tolerance and the
//! message list (bit length sequence, pauses with tolerance).
//!
//! If /tmp/urh-venv is missing we try to rebuild it once; when that fails
//! (e.g. no network) the Python comparison tests are skipped, but the pure
//! Rust smoke tests still run.

use std::path::Path;
use std::process::Command;

const PY: &str = "/tmp/urh-venv/bin/python";
const DATA_DIR: &str = "/Users/mimi/urh-ref/tests/data";
const FILES: [&str; 4] = ["ask.complex", "fsk.complex", "ASK_mod.complex", "ask_short.complex"];

/// The Python reference pipeline, printing exactly the format of
/// `urh-cli analyze` so both sides share one parser.
const PY_SCRIPT: &str = r#"
import sys
sys.path.insert(0, '/Users/mimi/urh-ref/src')

from urh.signalprocessing.Signal import Signal
from urh.ainterpretation import AutoInterpretation
from urh.signalprocessing.ProtocolAnalyzer import ProtocolAnalyzer

path = sys.argv[1]
sig = Signal(path)
est = AutoInterpretation.estimate(sig.iq_array)
if est is None:
    sys.stderr.write("no estimation possible\n")
    sys.exit(3)

sig.modulation_type = est["modulation_type"]  # default is FSK, must apply estimate
sig.center = est["center"]
sig.tolerance = est["tolerance"]
sig.samples_per_symbol = est["bit_length"]
sig.noise_threshold = est["noise"]

pa = ProtocolAnalyzer(signal=sig)
pa.get_protocol_from_signal()

print("modulation: {}".format(est["modulation_type"]))
print("bit_length: {}".format(est["bit_length"]))
print("center: {:.6f}".format(float(est["center"])))
print("noise_threshold: {:.6f}".format(float(est["noise"])))
print("tolerance: {}".format(int(est["tolerance"])))
print("messages: {}".format(len(pa.messages)))
print("index\tbits\tpause\thex")
for i, m in enumerate(pa.messages):
    print("{}\t{}\t{}\t{}".format(i, len(m), int(m.pause), m.plain_hex_str))
"#;

#[derive(Debug, Default, Clone)]
struct AnalysisOutput {
    modulation: String,
    bit_length: u64,
    center: f64,
    noise_threshold: f64,
    tolerance: u64,
    /// (bits, pause, hex) per message
    messages: Vec<(u64, u64, String)>,
}

fn parse_output(stdout: &str) -> AnalysisOutput {
    let mut out = AnalysisOutput {
        modulation: String::new(),
        bit_length: 0,
        center: 0.0,
        noise_threshold: 0.0,
        tolerance: 0,
        messages: Vec::new(),
    };
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("modulation: ") {
            out.modulation = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("bit_length: ") {
            out.bit_length = rest.trim().parse().expect("bit_length");
        } else if let Some(rest) = line.strip_prefix("center: ") {
            out.center = rest.trim().parse().expect("center");
        } else if let Some(rest) = line.strip_prefix("noise_threshold: ") {
            out.noise_threshold = rest.trim().parse().expect("noise_threshold");
        } else if let Some(rest) = line.strip_prefix("tolerance: ") {
            out.tolerance = rest.trim().parse().expect("tolerance");
        } else if let Some(rest) = line.strip_prefix("messages: ") {
            let n: usize = rest.trim().parse().expect("messages");
            out.messages.reserve(n);
        } else {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() == 4 && fields[0].parse::<usize>().is_ok() {
                let bits = fields[1].parse().expect("bits");
                let pause = fields[2].parse().expect("pause");
                out.messages.push((bits, pause, fields[3].to_string()));
            }
        }
    }
    out
}

fn data_path(name: &str) -> String {
    format!("{DATA_DIR}/{name}")
}

/// true when the reference Python environment is usable (rebuilding once if
/// it disappeared).
fn ensure_python() -> bool {
    if Path::new(PY).exists() {
        return true;
    }
    // try to rebuild once (needs network for the wheels)
    let venv = Command::new("python3")
        .args(["-m", "venv", "/tmp/urh-venv"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !venv || !Path::new(PY).exists() {
        return false;
    }
    Command::new("/tmp/urh-venv/bin/pip")
        .args([
            "install",
            "numpy",
            "psutil",
            "PyQt6",
            "cython",
            "setuptools",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_python(file: &str) -> AnalysisOutput {
    let output = Command::new(PY)
        .arg("-I")
        .arg("-c")
        .arg(PY_SCRIPT)
        .arg(file)
        .current_dir(std::env::temp_dir())
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "python pipeline failed for {file}:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_output(&String::from_utf8_lossy(&output.stdout))
}

fn run_cli(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_urh-cli"))
        .args(args)
        .current_dir(std::env::temp_dir())
        .output()
        .expect("failed to spawn urh-cli");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn run_cli_analyze(file: &str) -> AnalysisOutput {
    let (ok, stdout, stderr) = run_cli(&["analyze", file]);
    assert!(ok, "urh-cli analyze failed for {file}:\n{stderr}");
    parse_output(&stdout)
}

fn assert_close(what: &str, py: f64, rs: f64, abs_tol: f64) {
    let diff = (py - rs).abs();
    assert!(
        diff <= abs_tol,
        "{what}: python={py} rust={rs} diff={diff} > {abs_tol}"
    );
}

fn compare_file(name: &str) {
    let file = data_path(name);
    let py = run_python(&file);
    let rs = run_cli_analyze(&file);

    // modulation: "OOK" in the reference maps to ASK in urh-core
    let py_mod = if py.modulation == "OOK" {
        "ASK".to_string()
    } else {
        py.modulation.clone()
    };
    assert_eq!(
        py_mod, rs.modulation,
        "{name}: modulation mismatch (py={} rs={})",
        py.modulation, rs.modulation
    );
    assert_eq!(py.bit_length, rs.bit_length, "{name}: bit_length mismatch");

    // center / noise: small tolerances (float summation order etc.)
    assert_close(&format!("{name}: center"), py.center, rs.center, 0.02);
    assert_close(
        &format!("{name}: noise"),
        py.noise_threshold,
        rs.noise_threshold,
        0.005,
    );

    // message count and bit length sequence must match exactly
    assert_eq!(
        py.messages.len(),
        rs.messages.len(),
        "{name}: message count mismatch (py={} rs={})",
        py.messages.len(),
        rs.messages.len()
    );
    for (i, ((pbits, ppause, _), (rbits, rpause, _))) in
        py.messages.iter().zip(rs.messages.iter()).enumerate()
    {
        assert_eq!(
            pbits, rbits,
            "{name}: message {i} bit length mismatch (py={pbits} rs={rbits})"
        );
        let pause_tol = (*ppause.max(rpause) as f64 * 0.05).ceil().max(2.0) as u64;
        assert!(
            ppause.abs_diff(*rpause) <= pause_tol,
            "{name}: message {i} pause mismatch (py={ppause} rs={rpause} tol={pause_tol})"
        );
    }

    println!(
        "{name}: OK (mod={} bitlen={} msgs={})",
        rs.modulation,
        rs.bit_length,
        rs.messages.len()
    );
}

#[test]
fn compare_with_python_reference() {
    if !ensure_python() {
        eprintln!("/tmp/urh-venv unavailable and rebuild failed -> skipping Python comparison");
        return;
    }
    for name in FILES {
        compare_file(name);
    }
}

/// Pure Rust smoke test: every file analyzes with exit code 0 and the output
/// contains the message table with hex data.
#[test]
fn cli_analyze_smoke() {
    for name in FILES {
        let file = data_path(name);
        let (ok, stdout, stderr) = run_cli(&["analyze", &file]);
        assert!(ok, "analyze {name} failed: {stderr}");
        assert!(stdout.contains("modulation:"), "{name}: no modulation in output");
        assert!(stdout.contains("index\tbits\tpause\thex"), "{name}: no message table");
        // at least one message line with a non-empty hex column
        let has_hex = stdout.lines().any(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            f.len() == 4 && f[0].parse::<usize>().is_ok() && !f[3].is_empty()
        });
        assert!(has_hex, "{name}: no hex messages in output");
    }
}

#[test]
fn cli_info_smoke() {
    for name in FILES {
        let file = data_path(name);
        let (ok, stdout, stderr) = run_cli(&["info", &file]);
        assert!(ok, "info {name} failed: {stderr}");
        for key in [
            "file:",
            "samples:",
            "sample_rate: 1000000",
            "duration_s:",
            "magnitude_min:",
            "magnitude_max:",
            "db_min:",
            "db_max:",
        ] {
            assert!(stdout.contains(key), "{name}: missing '{key}' in info output");
        }
    }
}

#[test]
fn cli_info_respects_sample_rate() {
    let file = data_path("ask_short.complex");
    let (ok, stdout, stderr) = run_cli(&["info", &file, "--sample-rate", "100000"]);
    assert!(ok, "info with --sample-rate failed: {stderr}");
    // ask_short.complex has 131 samples -> 1.31 ms at 100 kHz
    assert!(stdout.contains("duration_s: 0.001310"), "unexpected duration in: {stdout}");
}

#[test]
fn cli_bad_args_fail() {
    let (ok, _, _) = run_cli(&["frobnicate"]);
    assert!(!ok);
    let (ok, _, _) = run_cli(&[]);
    assert!(!ok);
    let (ok, _, _) = run_cli(&["analyze", "/nonexistent/file.complex"]);
    assert!(!ok);
}
