use core::str;
use std::error::Error;
use std::fs::File;
use std::io::IsTerminal;
use std::io::Write;
use std::panic;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;
use crossbeam_channel::bounded;
use inferno::flamegraph;
use lightswitch::collector::{AggregatorCollector, Collector, NullCollector, StreamingCollector};
use lightswitch_metadata_provider::metadata_provider::GlobalMetadataProvider;
use nix::unistd::Uid;
use prost::Message;
use tracing::{error, info, Level};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::FmtSubscriber;

use lightswitch_capabilities::system_info::SystemInfo;
use lightswitch_metadata_provider::metadata_provider::ThreadSafeGlobalMetadataProvider;

use lightswitch::profile::symbolize_profile;
use lightswitch::profile::{fold_profile, to_pprof};
use lightswitch::profiler::{Profiler, ProfilerConfig};
use lightswitch::unwind_info::compact_unwind_info;
use lightswitch::unwind_info::CompactUnwindInfoBuilder;
use lightswitch_object::ObjectFile;

mod args;
mod validators;

use crate::args::CliArgs;
use crate::args::LoggingLevel;
use crate::args::ProfileFormat;
use crate::args::ProfileSender;
use crate::args::Symbolizer;

const DEFAULT_PPROF_INGEST_URL: &str = "http://localhost:4567/pprof/new";

/// Exit the main thread if any thread panics. We prefer this behaviour because pretty much every
/// thread is load bearing for the correct functioning.
fn panic_thread_hook() {
    let orig_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        orig_hook(panic_info);
        std::process::exit(1);
    }));
}

fn main() -> Result<(), Box<dyn Error>> {
    panic_thread_hook();

    let args = CliArgs::parse();

    if let Some(path) = args.show_unwind_info {
        show_unwind_info(&path);
        return Ok(());
    }

    let subscriber = FmtSubscriber::builder()
        .with_max_level(match args.logging {
            LoggingLevel::Trace => Level::TRACE,
            LoggingLevel::Debug => Level::DEBUG,
            LoggingLevel::Info => Level::INFO,
            LoggingLevel::Warn => Level::WARN,
            LoggingLevel::Error => Level::ERROR,
        })
        .with_span_events(FmtSpan::ENTER | FmtSpan::CLOSE)
        .with_ansi(std::io::stdout().is_terminal())
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    if let Some(path) = args.show_info {
        show_object_file_info(&path);
        return Ok(());
    }

    if !Uid::current().is_root() {
        error!("root permissions are required to run lightswitch");
        std::process::exit(1);
    }

    let system_info = SystemInfo::new();
    match system_info {
        Ok(system_info) => {
            info!("system_info = {:?}", system_info);
            if !system_info.has_minimal_requirements() {
                error!("Some start up requirements could not be met!");
                std::process::exit(1);
            }
        }
        Err(_) => {
            error!("Failed to detect system info!");
            std::process::exit(1)
        }
    }

    let metadata_provider: ThreadSafeGlobalMetadataProvider =
        Arc::new(Mutex::new(GlobalMetadataProvider::default()));

    let collector = Arc::new(Mutex::new(match args.sender {
        ProfileSender::None => Box::new(NullCollector::new()) as Box<dyn Collector + Send>,
        ProfileSender::LocalDisk => {
            Box::new(AggregatorCollector::new()) as Box<dyn Collector + Send>
        }
        ProfileSender::Remote => Box::new(StreamingCollector::new(
            args.symbolizer == Symbolizer::Local,
            args.server_url
                .as_ref()
                .map_or(DEFAULT_PPROF_INGEST_URL, |v| v),
            ProfilerConfig::default().session_duration,
            args.sample_freq,
            metadata_provider.clone(),
        )) as Box<dyn Collector + Send>,
    }));

    let profiler_config = ProfilerConfig {
        libbpf_debug: args.libbpf_debug,
        bpf_logging: args.bpf_logging,
        duration: args.duration,
        sample_freq: args.sample_freq,
        perf_buffer_bytes: args.perf_buffer_bytes,
        mapsize_info: args.mapsize_info,
        mapsize_stacks: args.mapsize_stacks,
        mapsize_aggregated_stacks: args.mapsize_aggregated_stacks,
        mapsize_rate_limits: args.mapsize_rate_limits,
        exclude_self: args.exclude_self,
        ..Default::default()
    };

    let (stop_signal_sender, stop_signal_receive) = bounded(1);

    ctrlc::set_handler(move || {
        info!("received Ctrl+C, stopping...");
        let _ = stop_signal_sender.send(());
    })
    .expect("Error setting Ctrl-C handler");

    let mut p: Profiler<'_> = Profiler::new(profiler_config, stop_signal_receive);
    p.profile_pids(args.pids);
    let profile_duration = p.run(collector.clone());

    let collector = collector.lock().unwrap();
    let (mut profile, procs, objs) = collector.finish();

    // If we need to send the profile to the backend there's nothing else to do.
    match args.sender {
        ProfileSender::Remote | ProfileSender::None => {
            return Ok(());
        }
        _ => {}
    }

    // Otherwise let's symbolize the profile and write it to disk.
    if args.symbolizer == Symbolizer::Local {
        profile = symbolize_profile(&profile, procs, objs);
    }

    let profile_path = args.profile_path.unwrap_or(PathBuf::from(""));

    match args.profile_format {
        ProfileFormat::FlameGraph => {
            let folded = fold_profile(profile);
            let mut options: flamegraph::Options<'_> = flamegraph::Options::default();
            let data = folded.as_bytes();
            let profile_name = args.profile_name.unwrap_or_else(|| "flame.svg".into());
            let profile_path = profile_path.join(profile_name);
            let f = File::create(&profile_path).unwrap();
            match flamegraph::from_reader(&mut options, data, f) {
                Ok(_) => {
                    eprintln!(
                        "Flamegraph profile successfully written to {}",
                        profile_path.to_string_lossy()
                    );
                }
                Err(e) => {
                    error!("Failed generate flamegraph: {:?}", e);
                }
            }
        }
        ProfileFormat::Pprof => {
            let mut buffer = Vec::new();
            let pprof_profile = to_pprof(
                profile,
                procs,
                objs,
                &metadata_provider,
                profile_duration,
                args.sample_freq,
            );
            pprof_profile.encode(&mut buffer).unwrap();
            let profile_name = args.profile_name.unwrap_or_else(|| "profile.pb".into());
            let profile_path = profile_path.join(profile_name);
            let mut pprof_file = File::create(&profile_path).unwrap();

            match pprof_file.write_all(&buffer) {
                Ok(_) => {
                    eprintln!(
                        "Pprof profile successfully written to {}",
                        profile_path.to_string_lossy()
                    );
                }
                Err(e) => {
                    error!("Failed generate pprof: {:?}", e);
                }
            }
        }
        ProfileFormat::None => {
            // Do nothing
        }
    }

    Ok(())
}

fn show_unwind_info(path: &str) {
    let unwind_info = compact_unwind_info(path).unwrap();
    for compact_row in unwind_info {
        let pc = compact_row.pc;
        let cfa_type = compact_row.cfa_type;
        let rbp_type = compact_row.rbp_type;
        let cfa_offset = compact_row.cfa_offset;
        let rbp_offset = compact_row.rbp_offset;
        println!(
            "pc: {:x} cfa_type: {:<2} rbp_type: {:<2} cfa_offset: {:<4} rbp_offset: {:<4}",
            pc, cfa_type, rbp_type, cfa_offset, rbp_offset
        );
    }
}

fn show_object_file_info(path: &str) {
    let objet_file = ObjectFile::new(&PathBuf::from(path)).unwrap();
    println!("build id {:?}", objet_file.build_id());
    let unwind_info: Result<CompactUnwindInfoBuilder<'_>, anyhow::Error> =
        CompactUnwindInfoBuilder::with_callback(path, |_| {});
    println!("unwind info {:?}", unwind_info.unwrap().process());
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_cmd::Command;
    use clap::Parser;
    use rstest::rstest;

    #[test]
    fn verify_cli() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert()
    }

    #[test]
    fn cli_help() {
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();

        cmd.arg("--help");
        cmd.assert().success();
        let actual = String::from_utf8(cmd.unwrap().stdout).unwrap();
        insta::assert_yaml_snapshot!(actual, @r#""Usage: lightswitch [OPTIONS]\n\nOptions:\n      --pids <PIDS>\n          Specific PIDs to profile\n\n      --tids <TIDS>\n          Specific TIDs to profile (these can be outside the PIDs selected above)\n\n      --show-unwind-info <PATH_TO_BINARY>\n          Show unwind info for given binary\n\n      --show-info <PATH_TO_BINARY>\n          Show build ID for given binary\n\n  -D, --duration <DURATION>\n          How long this agent will run in seconds\n          \n          [default: 18446744073709551615]\n\n      --libbpf-debug\n          Enable libbpf logs. This includes the BPF verifier output\n\n      --bpf-logging\n          Enable BPF programs logging\n\n      --logging <LOGGING>\n          Set lightswitch's logging level\n          \n          [default: info]\n          [possible values: trace, debug, info, warn, error]\n\n      --sample-freq <SAMPLE_FREQ_IN_HZ>\n          Per-CPU Sampling Frequency in Hz\n          \n          [default: 19]\n\n      --profile-format <PROFILE_FORMAT>\n          Output file for Flame Graph in SVG format\n          \n          [default: flame-graph]\n          [possible values: none, flame-graph, pprof]\n\n      --profile-path <PROFILE_PATH>\n          Path for the generated profile\n\n      --profile-name <PROFILE_NAME>\n          Name for the generated profile\n\n      --sender <SENDER>\n          Where to write the profile\n          \n          [default: local-disk]\n\n          Possible values:\n          - none:       Discard the profile. Used for kernel tests\n          - local-disk\n          - remote\n\n      --server-url <SERVER_URL>\n          \n\n      --perf-buffer-bytes <PERF_BUFFER_BYTES>\n          Size of each profiler perf buffer, in bytes (must be a power of 2)\n          \n          [default: 524288]\n\n      --mapsize-info\n          Print eBPF map sizes after creation\n\n      --mapsize-stacks <MAPSIZE_STACKS>\n          max number of individual stacks to capture before aggregation\n          \n          [default: 100000]\n\n      --mapsize-aggregated-stacks <MAPSIZE_AGGREGATED_STACKS>\n          max number of unique stacks after aggregation\n          \n          [default: 10000]\n\n      --mapsize-rate-limits <MAPSIZE_RATE_LIMITS>\n          max number of rate limit entries\n          \n          [default: 5000]\n\n      --exclude-self\n          Do not profile the profiler (myself)\n\n      --symbolizer <SYMBOLIZER>\n          [default: local]\n          [possible values: local, none]\n\n  -h, --help\n          Print help (see a summary with '-h')\n""#);
    }

    #[rstest]
    // The case tuples are: (string frequency to try, error string - if expected )
    #[case::prime_19("19", "")]
    #[case::non_prime_20(
        "20",
        "Sample frequency 20 is not prime - use 19 (before) or 23 (after) instead"
    )]
    #[case::prime_47("47", "")]
    #[case::non_prime_49(
        "49",
        "Sample frequency 49 is not prime - use 47 (before) or 53 (after) instead"
    )]
    #[case::prime_101("101", "")]
    #[case::prime_1009("1009", "")]
    #[case::non_prime_out_of_range1010("1010", "sample frequency not in allowed range")]
    #[case::prime_out_of_range_1013("1013", "sample frequency not in allowed range")]
    #[trace]
    fn sample_freq_successes(#[case] desired_freq: String, #[case] expected_msg: String) {
        let execname = env!("CARGO_PKG_NAME");
        let argname = "--sample-freq";
        let baseargs = vec![execname, argname];

        let mut myargs = baseargs.clone();
        myargs.push(desired_freq.as_str());
        let result = CliArgs::try_parse_from(myargs.iter());
        match result {
            Ok(config) => {
                assert_eq!(config.sample_freq, desired_freq.parse::<u64>().unwrap());
            }
            Err(err) => {
                let actual_message = err.to_string();
                assert!(actual_message.contains(expected_msg.as_str()));
            }
        }
    }
}