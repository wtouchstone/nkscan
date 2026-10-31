use clap::{Parser, Subcommand};
use nkscan::{
    protocol::{caps::film::FilmFormat, data::Rect},
    scan::profile,
};
use profile::Film;
use std::{path::PathBuf, str::FromStr, sync::LazyLock};
use tracing_subscriber::filter::LevelFilter;

/// The crate version, and the commit for a build that came off no release. CI
/// sets NKSCAN_BUILD for a preview build, and leaves it unset for a release
static VERSION: LazyLock<String> = LazyLock::new(|| match option_env!("NKSCAN_BUILD") {
    Some(build) if !build.is_empty() => format!("{} ({build})", env!("CARGO_PKG_VERSION")),
    _ => env!("CARGO_PKG_VERSION").to_string(),
});

fn parse_frame_offset(value: &str) -> Result<(usize, i32, i32), String> {
    let values = value
        .split(':')
        .map(str::parse)
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|_| "use FRAME:X:Y".to_string())?;
    match values.as_slice() {
        [frame, x, y] if *frame >= 1 => Ok((
            usize::try_from(*frame).map_err(|_| "frame is too large".to_string())?,
            i32::try_from(*x).map_err(|_| "X offset is too large".to_string())?,
            i32::try_from(*y).map_err(|_| "Y offset is too large".to_string())?,
        )),
        _ => Err("use FRAME:X:Y with a frame of 1 or greater".to_string()),
    }
}

fn parse_frame_rect(value: &str) -> Result<(usize, Rect), String> {
    let values = value
        .split(':')
        .map(str::parse)
        .collect::<Result<Vec<u64>, _>>()
        .map_err(|_| "use FRAME:TOP:LEFT:BOTTOM:RIGHT".to_string())?;
    match values.as_slice() {
        [frame, top, left, bottom, right] if *frame >= 1 && top < bottom && left < right => Ok((
            usize::try_from(*frame).map_err(|_| "frame is too large".to_string())?,
            Rect {
                top: u32::try_from(*top).map_err(|_| "frame top is too large".to_string())?,
                left: u32::try_from(*left).map_err(|_| "frame left is too large".to_string())?,
                bottom: u32::try_from(*bottom)
                    .map_err(|_| "frame bottom is too large".to_string())?,
                right: u32::try_from(*right).map_err(|_| "frame right is too large".to_string())?,
            },
        )),
        _ => Err("use FRAME:TOP:LEFT:BOTTOM:RIGHT with a non-empty rectangle".to_string()),
    }
}

/// One film base density: a finite density from 0 to 4
fn parse_base_density(value: &str) -> Result<f64, String> {
    let density: f64 = value
        .parse()
        .map_err(|_| format!("'{value}' is not a density"))?;
    if density.is_finite() && (0.0..=4.0).contains(&density) {
        Ok(density)
    } else {
        Err(format!("'{value}' is outside 0 to 4"))
    }
}

/// The version, plus the notices for what is compiled in alongside our own code
///
/// `-V` stays the bare version; this is what `--version` gives
static LONG_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{}\n\n{}", *VERSION, profile::NOTICE));

#[derive(Parser)]
#[command(version = VERSION.as_str(), about, long_version = LONG_VERSION.as_str())]
/// Scan film on a Nikon Coolscan
pub struct Cli {
    #[command(subcommand)]
    pub action: Action,

    /// Log verbosity: trace, debug, info, warn, error, or off.
    #[arg(long, global = true, default_value_t = LevelFilter::INFO, value_parser = parse_log_level)]
    pub log: LevelFilter,
}

#[derive(Subcommand)]
pub enum Action {
    /// List available scanners
    List,
    /// Perform a scan. Defaults to batch scanning with sensible defaults.
    Scan(Scan),
    /// Dump a scanner's INQUIRY pages. Reads only; nothing moves.
    Dump(Dump),
    /// Eject the loaded film or holder
    Eject(Eject),
    /// Measure the scanner's light source again (Nikon Scan's Calibrate)
    Calibrate(Calibrate),
    /// Clean a saved scan with its infrared file. No scanner needed.
    Clean(Clean),
}

/// Clean a saved 16-bit RGB scan with the infrared file saved beside it
#[derive(clap::Args)]
pub struct Clean {
    /// 16-bit RGB scan file, as the scanner delivered it
    pub rgb: PathBuf,
    /// 16-bit single-sample infrared file saved beside it
    pub ir: PathBuf,
    /// Scanner model, for example "LS-50 ED"
    #[arg(long)]
    pub model: String,
    /// Scan resolution, as the meter record reports it
    #[arg(long, default_value_t = 4000)]
    pub dpi: u32,
    /// Dust reconstruction quality
    #[arg(long, value_enum, default_value_t = CleanQuality::Normal)]
    pub quality: CleanQuality,
    /// Where to write the cleaned 16-bit RGB image
    pub out: PathBuf,
}

/// Eject the loaded film or holder
#[derive(clap::Args)]
pub struct Eject {
    /// The scanner to eject. Optional, will default to the first found.
    pub device: Option<String>,
}

/// Recalibrate the scanner's light source
#[derive(clap::Args)]
pub struct Calibrate {
    /// The scanner to calibrate. Optional, will default to the first found.
    pub device: Option<String>,
}

/// Which scanner to ask about itself
#[derive(clap::Args)]
pub struct Dump {
    /// The scanner to read. Optional, will default to the first found.
    pub device: Option<String>,
}

/// What to scan and how
#[derive(clap::Args)]
pub struct Scan {
    /// The scanner to connect to. Optional, will default to the first found.
    pub device: Option<String>,

    /// Where to write, as a path prefix. Each frame becomes <basename>_<n>.tiff, and its infrared mask <basename>_<n>_IR.tiff
    #[arg(long, default_value = "scan")]
    pub basename: PathBuf,

    /// Autoexpose per channel, taking the film's own cast off. The default for
    /// color negative, where that cast is the orange mask.
    #[arg(long, conflicts_with = "lock_wb")]
    pub unlock_wb: bool,

    /// Autoexpose the channels as one, keeping the film's cast and the factory
    /// balance. The default for slide, Kodachrome and black and white.
    #[arg(long)]
    pub lock_wb: bool,

    /// Autoexpose the first frame and reuse that exposure across all frames.
    #[arg(long)]
    pub lock_ae: bool,

    /// Keep each meter image and a text record of all exposure values
    #[arg(long)]
    pub meter: bool,

    /// Use Nikon Scan's matching color-negative meter sequence
    #[arg(long, requires = "meter", conflicts_with = "exposure")]
    pub nikon_meter: bool,

    /// On an LS-8000 color negative, scan at Nikon Scan's exposures. Without
    /// this, each channel is exposed to its own film base so the raw film base
    /// is neutral; Nikon's exposures are still written to the meter record.
    #[arg(long, conflicts_with = "exposure")]
    pub nikon_exposures: bool,

    /// Use these red, green, and blue exposure values instead of metering
    #[arg(long, num_args = 3, value_names = ["RED", "GREEN", "BLUE"])]
    pub exposure: Vec<u32>,

    /// Autofocus at this X Y point inside each frame. Values run from 0 at the
    /// top or left edge to 1 at the bottom or right edge.
    #[arg(
        long,
        num_args = 2,
        value_names = ["X", "Y"],
        value_parser = parse_focus_fraction,
        conflicts_with = "focus_position"
    )]
    pub focus: Vec<f32>,

    /// Move the lens to this absolute position and skip autofocus
    #[arg(long, conflicts_with = "focus")]
    pub focus_position: Option<u16>,

    /// Run autofocus again after metering, immediately before each full scan
    #[arg(long)]
    pub refocus_before_scan: bool,

    /// Resolution. Defaults to scanner maximum.
    #[arg(long)]
    pub dpi: Option<u16>,

    /// Number of samples. Defaults to 1.
    #[arg(long, default_value_t = 1)]
    pub samples: u8,

    /// Singleline CCD mode. Only supported on multiline CCD scanners.
    #[arg(long, conflicts_with = "superfine_pair")]
    pub superfine: bool,

    /// Capture the normal and Super Fine CCD modes from the same prepared
    /// frame window. The normal file keeps its usual name. The matching file
    /// uses <basename>_superfine_<n>.tiff.
    #[arg(
        long,
        conflicts_with_all = ["superfine", "ir", "clean"]
    )]
    pub superfine_pair: bool,

    /// Which frame(s) to scan, comma separated. Defaults to all detected.
    /// Naming any stops after one holder rather than batching.
    #[arg(long, value_delimiter = ',')]
    pub frames: Vec<usize>,

    /// Move every selected frame by X Y scanner address units without changing its size.
    #[arg(long, num_args = 2, value_names = ["X", "Y"], allow_hyphen_values = true)]
    pub frame_offset: Vec<i32>,

    /// Move one selected frame by X Y scanner address units. Repeat for more frames.
    #[arg(long = "frame-offset-for", value_name = "FRAME:X:Y", value_parser = parse_frame_offset)]
    pub frame_offsets: Vec<(usize, i32, i32)>,

    /// Add this many scanner address units around a negative full scan.
    /// Perforation-framed scanners already read the complete gate.
    #[arg(long, default_value_t = 0)]
    pub overscan: u32,

    /// Reuse a frame rectangle found by an earlier thumbnail pass. Repeat for
    /// more frames. This skips frame discovery for this run.
    #[arg(
        long = "frame-rect-for",
        value_name = "FRAME:TOP:LEFT:BOTTOM:RIGHT",
        value_parser = parse_frame_rect
    )]
    pub frame_rects: Vec<(usize, Rect)>,

    /// The film base density that the earlier thumbnail pass measured, for
    /// the meter. Upstream meters every frame against the base its own
    /// thumbnail measured; this gives a run that reuses that thumbnail the
    /// same base. Without it the meter uses Nikon Scan's default.
    #[arg(
        long,
        num_args = 3,
        value_names = ["RED", "GREEN", "BLUE"],
        value_parser = parse_base_density,
        requires = "frame_rects"
    )]
    pub base_density: Vec<f64>,

    /// Include the IR pass
    #[arg(long, conflicts_with = "clean")]
    pub ir: bool,

    /// Remove dust and scratches using the infrared channel.
    #[arg(long)]
    pub clean: bool,

    /// Dust reconstruction quality. Fine also rebuilds clean pixels.
    #[arg(long, value_enum, default_value_t = CleanQuality::Normal, requires = "clean")]
    pub clean_quality: CleanQuality,

    /// Don't eject at the end of the strip
    #[arg(long)]
    pub no_eject: bool,

    /// Keep the framing thumbnail as <basename>_<n>_thumbnail.tiff, on units that support this
    #[arg(long)]
    pub thumbnail: bool,

    /// Film format. One of: 135, half, IX240, 16, 645, 66, 67, 68, 69, or a custom frame length in mm (LS-8000/9000 only, up to 83 mm). Defaults to what the holder reports (if any).
    #[arg(long, value_parser = FilmFormat::from_str)]
    pub format: Option<FilmFormat>,

    /// Film type, which picks the color profile the scans are tagged with
    #[arg(long, value_enum, default_value_t = Film::Negative)]
    pub film: Film,
}

/// How nkscan applies its built-in infrared reconstruction.
#[derive(Debug, Copy, Clone, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum CleanQuality {
    #[default]
    Normal,
    Fine,
}

/// A log level flag, by name
pub fn parse_log_level(flag: &str) -> Result<LevelFilter, String> {
    flag.parse()
        .map_err(|_| format!("'{flag}' is not a valid log level (expected one of: trace, debug, info, warn, error, off)"))
}

/// A focus coordinate is a fraction of the detected frame size
fn parse_focus_fraction(flag: &str) -> Result<f32, String> {
    let value: f32 = flag
        .parse()
        .map_err(|_| format!("'{flag}' is not a number from 0 to 1"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!("'{flag}' is outside 0 to 1"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Action, Cli, parse_base_density, parse_focus_fraction, parse_frame_offset, parse_frame_rect,
    };
    use clap::Parser;
    use nkscan::protocol::{caps::film::FilmFormat, data::Rect};

    #[test]
    fn calibrate_is_its_own_command() {
        let cli = Cli::try_parse_from(["nkscan", "calibrate"]).unwrap();
        assert!(matches!(cli.action, Action::Calibrate(_)));
    }

    #[test]
    fn focus_fractions_include_both_edges() {
        assert_eq!(parse_focus_fraction("0"), Ok(0.0));
        assert_eq!(parse_focus_fraction("0.25"), Ok(0.25));
        assert_eq!(parse_focus_fraction("1"), Ok(1.0));
    }

    #[test]
    fn focus_fractions_reject_values_outside_the_frame() {
        assert!(parse_focus_fraction("-0.1").is_err());
        assert!(parse_focus_fraction("1.1").is_err());
        assert!(parse_focus_fraction("center").is_err());
    }

    #[test]
    fn frame_offsets_name_one_frame_and_two_axes() {
        assert_eq!(parse_frame_offset("3:-12:40"), Ok((3, -12, 40)));
        assert!(parse_frame_offset("0:1:2").is_err());
        assert!(parse_frame_offset("1:2").is_err());
    }

    #[test]
    fn overscan_is_a_nonnegative_address_margin() {
        let cli = Cli::try_parse_from(["nkscan", "scan", "--overscan", "157"]).unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!("scan action");
        };
        assert_eq!(scan.overscan, 157);
        assert!(Cli::try_parse_from(["nkscan", "scan", "--overscan", "-1"]).is_err());
    }

    #[test]
    fn frame_rectangles_name_one_previously_detected_frame() {
        assert_eq!(
            parse_frame_rect("2:100:20:300:220"),
            Ok((
                2,
                Rect {
                    top: 100,
                    left: 20,
                    bottom: 300,
                    right: 220,
                }
            ))
        );
        assert!(parse_frame_rect("0:100:20:300:220").is_err());
        assert!(parse_frame_rect("2:300:20:100:220").is_err());
    }

    #[test]
    fn a_complete_saved_table_can_be_combined_with_a_frame_selection() {
        let cli = Cli::try_parse_from([
            "nkscan",
            "scan",
            "--frames",
            "2",
            "--frame-rect-for",
            "1:10:0:110:40",
            "--frame-rect-for",
            "2:110:0:210:40",
        ])
        .unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!("scan action");
        };
        assert_eq!(scan.frames, vec![2]);
        assert_eq!(scan.frame_rects.len(), 2);
    }

    #[test]
    fn a_reused_thumbnail_carries_its_film_base() {
        let cli = Cli::try_parse_from([
            "nkscan",
            "scan",
            "--frame-rect-for",
            "1:10:0:110:40",
            "--base-density",
            "0.416",
            "0.773",
            "1.062",
        ])
        .unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!("scan action");
        };
        assert_eq!(scan.base_density, vec![0.416, 0.773, 1.062]);
        // A run that takes its own thumbnail measures its own base
        assert!(
            Cli::try_parse_from(["nkscan", "scan", "--base-density", "0.4", "0.7", "1.0"]).is_err()
        );
        assert!(parse_base_density("-0.1").is_err());
        assert!(parse_base_density("NaN").is_err());
        assert!(parse_base_density("inf").is_err());
    }

    #[test]
    fn comma_separated_frame_selection_keeps_all_requested_frames() {
        let cli = Cli::try_parse_from(["nkscan", "scan", "--format", "645", "--frames", "1,2,3,4"])
            .unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!("scan action");
        };
        assert_eq!(scan.format, Some(FilmFormat::F645));
        assert_eq!(scan.frames, vec![1, 2, 3, 4]);
    }

    #[test]
    fn built_in_cleaning_accepts_fine_quality() {
        let cli =
            Cli::try_parse_from(["nkscan", "scan", "--clean", "--clean-quality", "fine"]).unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!("scan action");
        };
        assert!(scan.clean);
        assert_eq!(scan.clean_quality, super::CleanQuality::Fine);
    }

    #[test]
    fn cleaning_and_retained_infrared_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["nkscan", "scan", "--clean", "--ir"]).is_err());
    }

    #[test]
    fn a_superfine_pair_keeps_raw_color_inputs_only() {
        let cli = Cli::try_parse_from(["nkscan", "scan", "--superfine-pair"]).unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!("scan action");
        };
        assert!(scan.superfine_pair);
        assert!(
            Cli::try_parse_from(["nkscan", "scan", "--superfine-pair", "--superfine"]).is_err()
        );
        assert!(Cli::try_parse_from(["nkscan", "scan", "--superfine-pair", "--ir"]).is_err());
        assert!(Cli::try_parse_from(["nkscan", "scan", "--superfine-pair", "--clean"]).is_err());
    }
}
