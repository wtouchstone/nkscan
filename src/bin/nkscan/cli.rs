use clap::{Parser, Subcommand};
use nkscan::{protocol::caps::film::FilmFormat, scan::profile};
use profile::Film;
use std::{path::PathBuf, str::FromStr, sync::LazyLock};
use tracing_subscriber::filter::LevelFilter;

/// The crate version, and the commit for a build that came off no release. CI
/// sets NKSCAN_BUILD for a preview build, and leaves it unset for a release
static VERSION: LazyLock<String> = LazyLock::new(|| match option_env!("NKSCAN_BUILD") {
    Some(build) if !build.is_empty() => format!("{} ({build})", env!("CARGO_PKG_VERSION")),
    _ => env!("CARGO_PKG_VERSION").to_string(),
});

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
    #[arg(long)]
    pub superfine: bool,

    /// Which frame(s) to scan, comma separated. Defaults to all detected.
    /// Naming any stops after one holder rather than batching.
    #[arg(long, value_delimiter = ',')]
    pub frames: Vec<usize>,

    /// Add this many scanner address units around a negative full scan.
    /// Perforation-framed scanners already read the complete gate.
    #[arg(long, default_value_t = 0)]
    pub overscan: u32,

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
    use super::{Action, Cli, parse_focus_fraction};
    use clap::Parser;

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
    fn overscan_is_a_nonnegative_address_margin() {
        let cli = Cli::try_parse_from(["nkscan", "scan", "--overscan", "157"]).unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!("scan action");
        };
        assert_eq!(scan.overscan, 157);
        assert!(Cli::try_parse_from(["nkscan", "scan", "--overscan", "-1"]).is_err());
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
}
