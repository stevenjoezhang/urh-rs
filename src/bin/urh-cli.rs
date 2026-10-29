//! `urh-cli` — offline IQ file analysis CLI for the urh crate.
//!
//! Unlike the original URH CLI (which targets live device sniffing), this tool
//! analyzes recorded IQ files:
//!
//!   urh-cli info <file> [--sample-rate N]
//!   urh-cli analyze <file> [--sample-rate N] [--modulation ASK|FSK|PSK|GFSK|OQPSK]
//!   urh-cli participants <file> [--sample-rate N] [--modulation X]
//!
//! Output format is stable and diff-friendly (key=value blocks plus one line
//! per message) so it can be used as the baseline for comparison tests.

use std::process::ExitCode;

use urh::autointerpret;
use urh::iq::{arr2decibel, IqArray, IqFormat};
use urh::modulation::afp_demod;
use urh::protocol::ProtocolAnalyzer;
use urh::Modulation;

const DEFAULT_SAMPLE_RATE: f64 = 1_000_000.0;
const USAGE: &str = "\
urh-cli — offline IQ file analysis (urh)

USAGE:
  urh-cli info <file> [--sample-rate N]
  urh-cli analyze <file> [--sample-rate N] [--modulation ASK|FSK|PSK|GFSK|OQPSK]
  urh-cli participants <file> [--sample-rate N] [--modulation X]

COMMANDS:
  info         Print sample count, duration, magnitude/dB range of an IQ file.
  analyze      Auto-interpret the signal (or use --modulation), demodulate it
               and print the detected parameters plus all messages.
  participants Like analyze, then run AWRE (FormatFinder) and print the
               inferred message types and field labels.

OPTIONS:
  --sample-rate N     Sample rate in Hz used for duration output (default 1e6).
  --modulation X      Skip modulation detection and use X (default: auto).
";

struct CommonArgs {
    file: String,
    sample_rate: f64,
    modulation: Option<Modulation>,
    /// true when the user explicitly asked for OOK (ASK + OOK segment merge,
    /// like `AutoInterpretation.estimate(modulation="OOK")` in the reference)
    is_ook: bool,
}

fn parse_modulation(s: &str) -> Option<(Modulation, bool)> {
    match s.to_ascii_uppercase().as_str() {
        "ASK" => Some((Modulation::Ask, false)),
        "OOK" => Some((Modulation::Ask, true)), // OOK is ASK with pause=0
        "FSK" => Some((Modulation::Fsk, false)),
        "PSK" => Some((Modulation::Psk, false)),
        "GFSK" => Some((Modulation::Gfsk, false)),
        "OQPSK" => Some((Modulation::Oqpsk, false)),
        _ => None,
    }
}

fn parse_common(args: &[String]) -> Result<CommonArgs, String> {
    let mut file: Option<String> = None;
    let mut sample_rate = DEFAULT_SAMPLE_RATE;
    let mut modulation: Option<Modulation> = None;
    let mut ook = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sample-rate" => {
                i += 1;
                let v = args.get(i).ok_or("--sample-rate needs a value")?;
                let mult = match v.chars().last().unwrap().to_ascii_uppercase() {
                    'K' => 1e3,
                    'M' => 1e6,
                    'G' => 1e9,
                    _ => 1.0,
                };
                let num = if mult == 1.0 {
                    v.to_string()
                } else {
                    v[..v.len() - 1].to_string()
                };
                sample_rate = num
                    .parse::<f64>()
                    .map_err(|_| format!("invalid sample rate: {v}"))?
                    * mult;
                if sample_rate <= 0.0 {
                    return Err("sample rate must be positive".to_string());
                }
            }
            "--modulation" => {
                i += 1;
                let v = args.get(i).ok_or("--modulation needs a value")?;
                let (m, is_ook) = parse_modulation(v).ok_or_else(|| {
                    format!("unknown modulation: {v} (use ASK|OOK|FSK|PSK|GFSK|OQPSK)")
                })?;
                modulation = Some(m);
                ook = is_ook;
            }
            other => {
                if other.starts_with("--") {
                    return Err(format!("unknown option: {other}"));
                }
                if file.replace(other.to_string()).is_some() {
                    return Err(format!("unexpected extra argument: {other}"));
                }
            }
        }
        i += 1;
    }

    Ok(CommonArgs {
        file: file.ok_or("missing input file")?,
        sample_rate,
        modulation,
        is_ook: ook,
    })
}

fn load_iq(path: &str) -> Result<IqArray, String> {
    let format = IqFormat::from_filename(path);
    IqArray::from_file(path).map_err(|e| format!("cannot read {path}: {e}")).inspect(|arr| {
        if arr.is_empty() {
            // keep going; empty signals produce zero messages like the reference
        }
        let _ = format;
    })
}

fn cmd_info(args: &[String]) -> Result<(), String> {
    let common = parse_common(args)?;
    let iq = load_iq(&common.file)?;
    let n = iq.len();
    let mags = iq.magnitudes();
    let (mut mag_min, mut mag_max) = (f64::INFINITY, f64::NEG_INFINITY);
    for &m in &mags {
        mag_min = mag_min.min(m);
        mag_max = mag_max.max(m);
    }
    if n == 0 {
        mag_min = 0.0;
        mag_max = 0.0;
    }
    let dbs = arr2decibel(iq.as_slice());
    let (mut db_min, mut db_max) = (f32::INFINITY, f32::NEG_INFINITY);
    for &d in &dbs {
        db_min = db_min.min(d);
        db_max = db_max.max(d);
    }
    if n == 0 {
        db_min = 0.0;
        db_max = 0.0;
    }
    let duration = n as f64 / common.sample_rate;

    println!("file: {}", common.file);
    println!("samples: {n}");
    println!("sample_rate: {}", common.sample_rate);
    println!("duration_s: {duration:.6}");
    println!("magnitude_min: {mag_min:.6}");
    println!("magnitude_max: {mag_max:.6}");
    println!("db_min: {db_min:.3}");
    println!("db_max: {db_max:.3}");
    Ok(())
}

/// Result of the (auto or fixed-modulation) interpretation step.
struct Analysis {
    modulation: Modulation,
    bit_length: usize,
    center: f32,
    noise_threshold: f32,
    tolerance: u32,
}

/// Interpretation with a user-supplied modulation.
///
/// Mirrors `AutoInterpretation.estimate(..., modulation=X)`: everything except
/// the modulation detection runs the same way (segmentation, demodulation,
/// center/plateau estimation per message segment).
fn estimate_with_modulation(
    iq: &[urh::Complex32],
    modulation: Modulation,
    ook: bool,
) -> Option<Analysis> {
    use autointerpret::*;

    let magnitudes: Vec<f32> = iq.iter().map(|c| c.norm()).collect();
    let noise = detect_noise_level(&magnitudes);
    let mut message_indices = segment_messages_from_magnitudes(&magnitudes, noise);

    if ook {
        message_indices = merge_message_segments_for_ook(&message_indices);
    }

    let data = afp_demod(iq, noise, modulation, 2, 0.1);

    let mut centers: Vec<f64> = Vec::new();
    let mut bit_lengths: Vec<usize> = Vec::new();
    let mut tolerances: Vec<u64> = Vec::new();

    for &(start, end) in &message_indices {
        let msg_rect_data = &data[start..end];
        let center = detect_center(msg_rect_data, None)?;
        let plateau_lengths = get_plateau_lengths(msg_rect_data, center, 25.0);
        let tolerance = match estimate_tolerance_from_plateau_lengths(&plateau_lengths) {
            Some(t) => {
                tolerances.push(t);
                t
            }
            None => 0,
        };
        let merged_lengths = merge_plateau_lengths(&plateau_lengths, Some(tolerance));
        if merged_lengths.len() < 2 {
            continue;
        }
        let bit_length = get_bit_length_from_plateau_lengths(&merged_lengths);
        if bit_length > (tolerance + 1) as usize {
            centers.push(center as f64);
            bit_lengths.push(bit_length);
        }
    }

    if bit_lengths.is_empty() {
        return None;
    }
    let center = if modulation == Modulation::Ask {
        min_without_outliers(&centers, 2.0)?
    } else {
        centers.iter().sum::<f64>() / centers.len() as f64
    };
    let bit_length = get_most_frequent_value(&bit_lengths)?;
    let tolerance = if tolerances.is_empty() {
        std::cmp::max(1, (0.05 * bit_length as f64) as u32)
    } else {
        let mut sorted: Vec<f64> = tolerances.iter().map(|&t| t as f64).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mid = sorted.len() / 2;
        let val = if sorted.len() % 2 == 1 {
            sorted[mid]
        } else {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        };
        val as u32
    };

    Some(Analysis {
        modulation,
        bit_length,
        center: center as f32,
        noise_threshold: noise,
        tolerance,
    })
}

fn run_analysis(common: &CommonArgs) -> Result<(Analysis, ProtocolAnalyzer), String> {
    let iq = load_iq(&common.file)?;
    let samples = iq.as_slice().to_vec();

    let analysis = match common.modulation {
        Some(m) => estimate_with_modulation(&samples, m, common.is_ook).ok_or_else(|| {
            format!(
                "could not interpret {} with modulation {:?}",
                common.file, m
            )
        })?,
        None => {
            let est = autointerpret::auto_detect(&samples)
                .ok_or_else(|| format!("could not auto-interpret {}", common.file))?;
            Analysis {
                modulation: est.modulation,
                bit_length: est.bit_length,
                center: est.center,
                noise_threshold: est.noise_threshold,
                tolerance: est.tolerance,
            }
        }
    };

    // qad: quadrature demodulated signal (Signal.quad_demod in the reference)
    let qad = afp_demod(
        &samples,
        analysis.noise_threshold,
        analysis.modulation,
        2, // modulation order (bits_per_symbol = 1)
        0.1,
    );

    let mut pa = ProtocolAnalyzer::new();
    pa.analyze_signal(
        &qad,
        analysis.center,
        analysis.tolerance as u16,
        analysis.modulation,
        analysis.bit_length as u32,
        1,     // bits per symbol
        0.1,   // center spacing
        8,     // pause threshold (Signal default)
    );
    Ok((analysis, pa))
}

fn print_analysis_header(common: &CommonArgs, a: &Analysis) {
    println!("file: {}", common.file);
    println!("modulation: {}", a.modulation.as_str());
    println!("bit_length: {}", a.bit_length);
    println!("center: {:.6}", a.center);
    println!("noise_threshold: {:.6}", a.noise_threshold);
    println!("tolerance: {}", a.tolerance);
}

fn print_messages(pa: &ProtocolAnalyzer) {
    println!("messages: {}", pa.messages.len());
    println!("index\tbits\tpause\thex");
    for (i, msg) in pa.messages.iter().enumerate() {
        println!("{}\t{}\t{}\t{}", i, msg.len(), msg.pause, msg.plain_hex_str());
    }
}

fn cmd_analyze(args: &[String]) -> Result<(), String> {
    let common = parse_common(args)?;
    let (analysis, pa) = run_analysis(&common)?;
    print_analysis_header(&common, &analysis);
    print_messages(&pa);
    Ok(())
}

fn cmd_participants(args: &[String]) -> Result<(), String> {
    use urh::awre::FormatFinder;

    let common = parse_common(args)?;
    let (analysis, pa) = run_analysis(&common)?;
    print_analysis_header(&common, &analysis);
    print_messages(&pa);

    println!("participant_labels_begin");
    let mut ff = FormatFinder::new(pa.messages.clone());
    ff.perform_analysis();
    for mt in ff.message_types() {
        println!("message_type: {}", mt.message_type.name);
        println!("message_indices: {:?}", mt.message_indices);
        for label in &mt.message_type.labels {
            println!(
                "label: {} [{}..{})",
                label.name, label.start, label.end
            );
        }
    }
    println!("participant_labels_end");
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let result = match cmd {
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        "info" => cmd_info(rest),
        "analyze" => cmd_analyze(rest),
        "participants" => cmd_participants(rest),
        other => Err(format!("unknown command: {other}\n\n{USAGE}")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
