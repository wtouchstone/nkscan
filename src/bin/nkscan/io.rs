//! Writing a finished pass out as 16-bit TIFF
//!
//! One file for the color planes and, where the pass carried infrared, one
//! beside it for that. Infrared is not a color and a fourth sample would be
//! read as alpha, so it does not share a file.
//!
//! Uncompressed: the samples are film grain, which deflates poorly, and the
//! write is hundreds of megabytes either way.

use crate::mono::{self, Luminance};
use anyhow::{Context, Result, bail};
use nkscan::{
    protocol::{
        data::Setup,
        decode::{Block, Samples, filled_columns},
        window::{Channel, Window},
    },
    scan::{
        autoexpose::{Exposures, MeteringEvidence},
        pass::{Pass, corrects_rows},
    },
};
use std::{
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
};
use tiff::{
    encoder::{
        Rational, TiffEncoder,
        colortype::{ColorType, Gray16, RGB16},
    },
    tags::{ResolutionUnit, Tag},
};
use tracing::{debug, warn};

/// What a frame's files are named, one-indexed, infrared beside the color
pub fn paths(basename: &Path, n: usize) -> (PathBuf, PathBuf) {
    let stem = basename.display();
    (
        PathBuf::from(format!("{stem}_{n}.tiff")),
        PathBuf::from(format!("{stem}_{n}_IR.tiff")),
    )
}

/// What a strip's thumbnail is named, numbered by the strip's first frame
pub fn thumbnail_path(basename: &Path, n: usize) -> PathBuf {
    PathBuf::from(format!("{}_{n}_thumbnail.tiff", basename.display()))
}

/// The first frame number whose files are all free
///
/// A batch appends rather than overwrites, so a second strip through the same
/// basename carries on where the first stopped. The thumbnail counts: a strip
/// with no frames on it still takes a number, and taking it again would
/// overwrite the thumbnail that says why it had none
pub fn next_free(basename: &Path) -> usize {
    (1..)
        .find(|&n| {
            let (color, ir) = paths(basename, n);
            !color.exists() && !ir.exists() && !thumbnail_path(basename, n).exists()
        })
        .expect("the range is unbounded")
}

/// Write one frame, returning the files it made
///
/// `icc` is embedded as-is where it is given. Without one the file says nothing
/// about its color, which is the truth: the samples are linear and this unit
/// has no characterization of its own.
///
/// `mono` writes one gray plane rather than three color ones. The pass is a
/// color one either way, since the unit scans RGB whatever the film is, so the
/// three become one the way Nikon's driver does it: through the monochrome
/// profile to XYZ, keeping Y. That is what `icc` is for on a mono write, and
/// the file is tagged with a gray space instead. Without a profile to convert
/// through there is nothing to weight the channels by, so it keeps green.
///
/// `infrared` says whether to keep the mask. Cleaning takes the IR pass for
/// itself, so a pass can carry infrared the operator never asked to see.
pub fn write_frame(
    basename: &Path,
    n: usize,
    samples: &Samples,
    pass: &Pass,
    icc: Option<&[u8]>,
    mono: bool,
    infrared: bool,
) -> Result<Vec<PathBuf>> {
    let plane = |channel: Channel| plane_of(pass, channel);
    let color = color_planes(pass);

    let (color_path, ir_path) = paths(basename, n);
    let mut written = Vec::new();
    let planes: Vec<&[u16]> = samples.colors.iter().map(Vec::as_slice).collect();

    // Three channels and a profile to weigh them by is the only way to make
    // luminance. Anything less falls back to the channel 2-11-3 calls the
    // default, which is green
    let luminance = match (mono, color.len(), icc) {
        (true, 3, Some(icc)) => Some(Luminance::from_profile(icc)?),
        (true, _, _) => {
            warn!("no monochrome profile for this unit, so the gray is the green channel alone");
            None
        }
        (false, _, _) => None,
    };
    let fallback = mono
        .then(|| plane(Channel::Green).or_else(|| plane(Channel::Default)))
        .flatten();

    match (&luminance, fallback, color.len()) {
        (Some(luminance), _, _) => {
            let gray = mono::gray_profile()?;
            let source_planes = [color[0], color[1], color[2]];
            write_planes::<Gray16>(
                &color_path,
                &planes,
                pass,
                Source::Luminance {
                    planes: source_planes,
                    luminance,
                },
                Some(&gray),
            )?
        }
        (None, Some(gray), _) => {
            write_planes::<Gray16>(&color_path, &planes, pass, Source::Planes(&[gray]), None)?
        }
        (None, None, 3) => {
            write_planes::<RGB16>(&color_path, &planes, pass, Source::Planes(&color), icc)?
        }
        (None, None, 1) => {
            write_planes::<Gray16>(&color_path, &planes, pass, Source::Planes(&color), icc)?
        }
        (None, None, n) => bail!("{n} color planes is not a TIFF this writes"),
    }
    written.push(color_path);

    if let Some(ir) = samples.ir.as_ref().filter(|_| infrared) {
        // The mask measures obstructions rather than color, so no profile
        write_planes::<Gray16>(&ir_path, &[ir], pass, Source::Planes(&[0]), None)?;
        written.push(ir_path);
    }

    Ok(written)
}

/// Write where each pixel of frame `n` came from on the CCD, beside its image
///
/// nkcolor's banding correction keys on this, so one calibration holds for any
/// window on any roll. Column `x` is at `(first_block_line + x) mod (ccd_rows *
/// lines_per_row)` in its block and came from CCD row `that / lines_per_row`.
/// Row `y` sits at X address `address_first + y * address_step`: the bar reads
/// out opposite to increasing X, so the first row has the highest address.
/// `origin_x` is the window's X origin, `have_curves` whether the session holds
/// the unit's row curves.
pub fn write_ccd_record(
    basename: &Path,
    n: usize,
    pass: &Pass,
    origin_x: u32,
    have_curves: bool,
) -> Result<PathBuf> {
    let block = Block::of(&pass.layout);
    let pitch = pass.layout.pitch.max(1);
    let rows = pass.rows as u32;
    let mut record = String::from("format=nkscan-ccd-v1\n");
    record.push_str(&format!("dpi={}\n", pass.layout.dpi));
    record.push_str(&format!("rows={rows}\n"));
    record.push_str(&format!("ccd_rows={}\n", block.ccd_lines));
    record.push_str(&format!("lines_per_row={}\n", block.gap));
    record.push_str("first_block_line=0\n");
    record.push_str(&format!(
        "row_curves={}\n",
        match corrects_rows(&pass.layout, have_curves) {
            true => "applied",
            false => "none",
        }
    ));
    record.push_str(&format!(
        "address_first={}\n",
        origin_x + rows.saturating_sub(1) * pitch
    ));
    record.push_str(&format!("address_step=-{pitch}\n"));
    let path = PathBuf::from(format!("{}_{n}_ccd.txt", basename.display()));
    fs::write(&path, record).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Write every image and exposure value retained from one frame's metering
#[allow(clippy::too_many_arguments)]
pub fn write_meter_evidence(
    basename: &Path,
    frame: usize,
    evidence: &MeteringEvidence,
    selected: &Exposures,
    full_windows: &[Window],
    setup_before: &[(Channel, Vec<u8>)],
    setup_after: &[(Channel, Vec<u8>)],
    nikon_negative: bool,
) -> Result<Vec<PathBuf>> {
    let meter_basename = PathBuf::from(format!("{}_{}_meter", basename.display(), frame));
    let mut written = Vec::new();
    let mut record = String::from("format=nkscan-meter-v2\n");
    record.push_str(if nikon_negative {
        "method=nikon-negative\n"
    } else {
        "method=generic\n"
    });

    if let Some(startup) = &evidence.startup_exposures {
        record.push_str(&format!("startup={}\n", exposure_text(startup)));
    }

    for (index, meter) in evidence.passes.iter().enumerate() {
        let number = index + 1;
        written.extend(write_frame(
            &meter_basename,
            number,
            &meter.samples,
            &meter.pass,
            None,
            false,
            true,
        )?);
        record.push_str(&format!(
            "pass={number} dpi={} rows={} cols={} bits={} channels={:?} complete={} exposures={}\n",
            meter.pass.layout.dpi,
            meter.pass.rows,
            meter.pass.cols,
            meter.pass.layout.bits_per_sample,
            meter.pass.layout.channels,
            meter.pass.complete,
            exposure_text(&meter.exposures),
        ));
        // The measured rectangle of this pass: first line, first pixel, last
        // line, last pixel, in Nikon Scan's builder orientation
        let [line0, pixel0, line1, pixel1] = meter.analyzer_rect;
        record.push_str(&format!(
            "analyzer_rect={number} {line0},{pixel0},{line1},{pixel1}\n"
        ));
        let path = PathBuf::from(format!(
            "{}_{}_meter_pass{}_windows.bin",
            basename.display(),
            frame,
            number
        ));
        write_windows(&path, &meter.windows)?;
        written.push(path);
    }
    record.push_str(&format!("selected={}\n", exposure_text(selected)));
    // nkcolor: Nikon Scan's exposures, where the full scan used others
    if let Some(nikon) = &evidence.nikon_exposures {
        record.push_str(&format!("nikon_final={}\n", exposure_text(nikon)));
    }

    let windows_path = PathBuf::from(format!("{}_{}_scan_windows.bin", basename.display(), frame));
    write_windows(&windows_path, full_windows)?;
    written.push(windows_path);

    write_setup_records(
        basename,
        frame,
        "before",
        setup_before,
        &mut record,
        &mut written,
    )?;
    write_setup_records(
        basename,
        frame,
        "after",
        setup_after,
        &mut record,
        &mut written,
    )?;

    let record_path = PathBuf::from(format!("{}_{}_meter.txt", basename.display(), frame));
    fs::write(&record_path, record)
        .with_context(|| format!("writing {}", record_path.display()))?;
    written.push(record_path);
    Ok(written)
}

fn write_windows(path: &Path, windows: &[Window]) -> Result<()> {
    let mut bytes = Vec::with_capacity(12 + windows.len() * nkscan::protocol::window::LENGTH);
    bytes.extend_from_slice(b"NWND");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&(windows.len() as u32).to_le_bytes());
    for window in windows {
        bytes.extend_from_slice(&window.to_bytes());
    }
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn write_setup_records(
    basename: &Path,
    frame: usize,
    stage: &str,
    records: &[(Channel, Vec<u8>)],
    report: &mut String,
    written: &mut Vec<PathBuf>,
) -> Result<()> {
    for (channel, bytes) in records {
        let name = channel_name(*channel);
        let path = PathBuf::from(format!(
            "{}_{}_setup_{}_{}.bin",
            basename.display(),
            frame,
            stage,
            name
        ));
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);

        match Setup::from_bytes(bytes) {
            Some(setup) => record_setup(report, stage, &name, &setup),
            None => report.push_str(&format!(
                "setup_{stage}_{name}=bytes:{} parse:error\n",
                bytes.len()
            )),
        }
    }
    Ok(())
}

fn record_setup(report: &mut String, stage: &str, channel: &str, setup: &Setup) {
    report.push_str(&format!(
        "setup_{stage}_{channel}=format:{} base_level:{} base_exposure:{} base_white_balance:{} images:{}\n",
        setup.format,
        setup.base_level,
        setup.base_exposure,
        setup.base_white_balance,
        setup.images.len(),
    ));
    for image in &setup.images {
        report.push_str(&format!(
            "setup_{stage}_{channel}_image={}:exposure:{}:white_balance:{}:min:{}:max:{}\n",
            image.index, image.exposure, image.white_balance, image.min, image.max
        ));
    }
}

fn exposure_text(exposures: &Exposures) -> String {
    exposures
        .iter()
        .map(|(channel, exposure)| format!("{}:{exposure}", channel_name(channel)))
        .collect::<Vec<_>>()
        .join(",")
}

fn channel_name(channel: Channel) -> String {
    format!("{channel:?}").to_lowercase()
}

/// Where `channel` sits in the color buffer, which carries only the color
/// channels of `pass.layout.channels`, in the same relative order
fn plane_of(pass: &Pass, channel: Channel) -> Option<usize> {
    pass.layout
        .colors()
        .position(|id| Channel::from(id) == channel)
}

/// R, G, B whatever order the stream interleaves them in
///
/// A unit that scans one channel calls it the default rather than green
fn color_planes(pass: &Pass) -> Vec<usize> {
    let color: Vec<usize> = [Channel::Red, Channel::Green, Channel::Blue]
        .into_iter()
        .filter_map(|c| plane_of(pass, c))
        .collect();
    match color.is_empty() {
        true => plane_of(pass, Channel::Default).into_iter().collect(),
        false => color,
    }
}

/// Write a strip's framing thumbnail, returning the file it made
///
/// This is the pass frame detection reads, kept as it arrived: no profile and
/// no monochrome conversion, since what it is for is measuring the detector
/// against, not looking at. The samples are stretched to full scale here
/// rather than in the caller's buffer, which detection still has to read
pub fn write_thumbnail(
    basename: &Path,
    n: usize,
    samples: &Samples,
    pass: &Pass,
) -> Result<PathBuf> {
    let color = color_planes(pass);

    let mut samples = samples.clone();
    samples.to_full_scale(pass.layout.bits_per_sample);
    let planes: Vec<&[u16]> = samples.colors.iter().map(Vec::as_slice).collect();

    let path = thumbnail_path(basename, n);
    match color.len() {
        3 => write_planes::<RGB16>(&path, &planes, pass, Source::Planes(&color), None)?,
        1 => write_planes::<Gray16>(&path, &planes, pass, Source::Planes(&color), None)?,
        n => bail!("{n} color planes is not a thumbnail this writes"),
    }
    Ok(path)
}

/// Where a file's samples come from
enum Source<'a> {
    /// These channels of the pass, as they are
    Planes(&'a [usize]),
    /// One value weighed out of three channels, which is what a gray file of a
    /// color pass is
    Luminance {
        planes: [usize; 3],
        luminance: &'a Luminance,
    },
}

impl Source<'_> {
    /// Samples this puts in the file for each pixel
    fn per_pixel(&self) -> usize {
        match self {
            Source::Planes(planes) => planes.len(),
            Source::Luminance { .. } => 1,
        }
    }
}

/// Write `source` to one file, a strip at a time
///
/// `planes` is one slice per channel, never interleaved; a TIFF holds only
/// what is going in this file, so each strip is gathered rather than copied
fn write_planes<C>(
    path: &Path,
    planes: &[&[u16]],
    pass: &Pass,
    source: Source<'_>,
    icc: Option<&[u8]>,
) -> Result<()>
where
    C: ColorType<Inner = u16>,
{
    let file =
        BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let mut tiff = TiffEncoder::new(file)?;
    // Only what arrived. The tail of a short pass is padding, not film
    let (cols, stride) = (filled_columns(planes, pass.rows, pass.cols), pass.cols);
    let mut image = tiff.new_image::<C>(cols as u32, pass.rows as u32)?;

    // 2-10 resolves both axes to the same pitch for a scan, so one number
    image.resolution(
        ResolutionUnit::Inch,
        Rational {
            n: pass.layout.dpi,
            d: 1,
        },
    );
    if let Some(icc) = icc {
        image.encoder().write_tag(Tag::IccProfile, icc)?;
    }

    // Already full scale: `to_full_scale` stretched the buffer after the pass
    let at = |pixel: usize, plane: usize| planes[plane][(pixel / cols) * stride + pixel % cols];

    let per_pixel = source.per_pixel();
    let mut strip = Vec::new();
    let mut done = 0usize;
    while image.next_strip_sample_count() > 0 {
        let count = image.next_strip_sample_count() as usize;
        let pixels = count / per_pixel;
        let first = done / per_pixel;

        strip.clear();
        strip.reserve(count);
        for pixel in first..first + pixels {
            match &source {
                Source::Planes(planes) => strip.extend(planes.iter().map(|&p| at(pixel, p))),
                Source::Luminance { planes, luminance } => strip.push(luminance.of([
                    at(pixel, planes[0]),
                    at(pixel, planes[1]),
                    at(pixel, planes[2]),
                ])),
            }
        }
        image.write_strip(&strip)?;
        done += count;
    }
    image.finish()?;

    debug!(path = %path.display(), samples = per_pixel, "wrote");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nkscan::protocol::image::Layout;
    use tiff::decoder::{Decoder, DecodingResult};

    /// The unit pads a pass that runs out of film rather than truncating it,
    /// so the pass reports complete and its tail is zeros. Writing that out
    /// would put a black band on the end of every file
    #[test]
    fn a_padded_pass_writes_only_the_columns_that_hold_film() {
        let (rows, promised, filled) = (4usize, 8usize, 5usize);
        let layout = Layout::single_line(rows as u32, promised as u32, vec![1, 2, 3]);
        // Each sample says which column it came from, so the gather is checked
        // and not just the width
        let plane: Vec<u16> = (0..rows * promised)
            .map(|i| match i % promised < filled {
                true => (i % promised) as u16 + 1,
                false => 0,
            })
            .collect();
        let samples = Samples {
            colors: vec![plane; 3],
            ir: None,
        };
        let pass = Pass {
            layout,
            cooperation: Vec::new(),
            complete: true,
            blocks: promised,
            rows,
            cols: promised,
        };

        let dir = std::env::temp_dir().join(format!("nkscan-io-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory to write into");
        let path = write_thumbnail(&dir.join("t"), 0, &samples, &pass).expect("a thumbnail");

        let mut decoder = Decoder::new(std::io::BufReader::new(
            File::open(&path).expect("the file it just wrote"),
        ))
        .expect("a TIFF");
        assert_eq!(
            decoder.dimensions().expect("dimensions"),
            (filled as u32, rows as u32)
        );

        let DecodingResult::U16(read) = decoder.read_image().expect("samples") else {
            panic!("not 16-bit samples")
        };
        // Every row is columns 1..=filled and none of the tail. The samples
        // are already 16 bits, so `to_full_scale` leaves them alone
        let want: Vec<u16> = (0..rows)
            .flat_map(|_| (1..=filled as u16).flat_map(|x| [x; 3]))
            .collect();
        assert_eq!(read, want);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The CCD record says which row made each column and where each row sat
    /// on the CCD, so nkcolor can key a correction on the scanner and not on
    /// the window
    #[test]
    fn a_ccd_record_places_every_column_and_row_on_the_ccd() {
        use nkscan::protocol::caps::set_window::ColorInterleaving;
        let (rows, cols) = (5usize, 72usize);
        let mut layout = Layout::single_line(rows as u32, cols as u32, vec![1, 2, 3]);
        layout.interleaving = ColorInterleaving::MULTILINE_SIMULTANEOUS;
        layout.ccd_lines = 3;
        layout.registration_gap = 12;
        layout.pitch = 2;
        layout.dpi = 2000;
        let pass = Pass {
            layout,
            cooperation: Vec::new(),
            complete: true,
            blocks: 2,
            rows,
            cols,
        };
        let dir = std::env::temp_dir().join(format!("nkscan-ccd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory to write into");
        let path = write_ccd_record(&dir.join("scan"), 3, &pass, 518, true).unwrap();
        assert_eq!(path, dir.join("scan_3_ccd.txt"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "format=nkscan-ccd-v1\ndpi=2000\nrows=5\nccd_rows=3\nlines_per_row=12\n\
             first_block_line=0\nrow_curves=applied\naddress_first=526\naddress_step=-2\n"
        );

        // A single-line pass has one row and nothing to remap
        let mut single = pass.clone();
        single.layout.interleaving = ColorInterleaving::LINE_WITHOUT_DISTANCE;
        let text = std::fs::read_to_string(
            write_ccd_record(&dir.join("fine"), 3, &single, 518, true).unwrap(),
        )
        .unwrap();
        assert!(text.contains("ccd_rows=1\nlines_per_row=1\n"), "{text}");
        assert!(text.contains("row_curves=none\n"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// nkcolor: the meter record names the method, the start-up and chosen
    /// exposures, each pass, and the rectangle each pass measured
    #[test]
    fn a_meter_record_keeps_each_pass_and_its_measured_rectangle() {
        use nkscan::scan::autoexpose::{MeterPass, MeteringEvidence};
        let (rows, cols) = (3usize, 4usize);
        let layout = Layout::single_line(rows as u32, cols as u32, vec![1, 2, 3]);
        let samples = Samples {
            colors: vec![vec![1000u16; rows * cols]; 3],
            ir: None,
        };
        let pass = Pass {
            layout,
            cooperation: Vec::new(),
            complete: true,
            blocks: cols,
            rows,
            cols,
        };
        let mut exposures = Exposures::default();
        for (channel, value) in [
            (Channel::Red, 10),
            (Channel::Green, 20),
            (Channel::Blue, 30),
        ] {
            exposures.set(channel, value);
        }
        let evidence = MeteringEvidence {
            startup_exposures: Some(exposures.clone()),
            passes: vec![MeterPass {
                samples,
                windows: Vec::new(),
                pass,
                exposures: exposures.clone(),
                analyzer_rect: [1, 0, 2, 3],
            }],
            nikon_exposures: None,
        };

        let dir = std::env::temp_dir().join(format!("nkscan-meter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory to write into");
        let written = write_meter_evidence(
            &dir.join("scan"),
            1,
            &evidence,
            &exposures,
            &[],
            &[],
            &[],
            true,
        )
        .expect("the evidence");
        let record = written
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "txt"))
            .expect("a meter record");
        let text = std::fs::read_to_string(record).expect("the record");
        std::fs::remove_dir_all(&dir).ok();

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "format=nkscan-meter-v2");
        assert_eq!(lines[1], "method=nikon-negative");
        assert_eq!(lines[2], "startup=red:10,green:20,blue:30");
        assert!(lines[3].starts_with("pass=1 dpi="), "{text}");
        assert!(
            lines[3].ends_with("exposures=red:10,green:20,blue:30"),
            "{text}"
        );
        assert_eq!(lines[4], "analyzer_rect=1 1,0,2,3");
        assert!(
            lines.contains(&"selected=red:10,green:20,blue:30"),
            "{text}"
        );
        // No Nikon exposures in the evidence, so no line for them
        assert!(!text.contains("nikon_final="), "{text}");
    }

    /// nkcolor: the meter record keeps Nikon Scan's exposures beside the ones
    /// the full scan used
    #[test]
    fn a_meter_record_keeps_nikons_exposures_beside_the_scanned_ones() {
        use nkscan::scan::autoexpose::MeteringEvidence;
        let triple = |r, g, b| {
            let mut e = Exposures::default();
            e.set(Channel::Red, r);
            e.set(Channel::Green, g);
            e.set(Channel::Blue, b);
            e
        };
        let evidence = MeteringEvidence {
            startup_exposures: Some(triple(10, 20, 30)),
            passes: Vec::new(),
            nikon_exposures: Some(triple(637570, 2504657, 3196974)),
        };
        let dir = std::env::temp_dir().join(format!("nkscan-nikon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory to write into");
        let written = write_meter_evidence(
            &dir.join("scan"),
            1,
            &evidence,
            &triple(816217, 2504657, 5652043),
            &[],
            &[],
            &[],
            true,
        )
        .expect("the evidence");
        let record = written
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "txt"))
            .expect("a meter record");
        let text = std::fs::read_to_string(record).expect("the record");
        std::fs::remove_dir_all(&dir).ok();

        let lines: Vec<&str> = text.lines().collect();
        let selected = lines
            .iter()
            .position(|l| *l == "selected=red:816217,green:2504657,blue:5652043")
            .expect("the scanned exposures");
        assert_eq!(
            lines[selected + 1],
            "nikon_final=red:637570,green:2504657,blue:3196974"
        );
    }
}
