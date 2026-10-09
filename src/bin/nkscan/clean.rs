//! Infrared dust removal over two saved files, with no scanner attached.
//!
//! A scan saved without cleaning keeps its infrared file beside the color
//! one. This runs the same `clean_frame` a `--clean` scan runs, over those
//! two files, and writes the rebuilt color image.

use super::cli::{Clean, CleanQuality};
use anyhow::{Context, Result, bail};
use nkscan::{
    dust,
    protocol::{decode::Samples, image::Layout, model::Model},
    scan::{clean::clean_frame, pass::Pass},
};
use std::{fs::File, io::BufWriter, path::Path};
use tiff::{
    decoder::{Decoder, DecodingResult, Limits},
    encoder::{Rational, TiffEncoder, colortype::RGB16},
    tags::ResolutionUnit,
};

/// Read one 16-bit gray or chunky RGB image file
fn read_u16(path: &Path) -> Result<(Vec<u16>, u32, u32)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .with_context(|| format!("reading {}", path.display()))?
        .with_limits(Limits::unlimited());
    let (cols, rows) = decoder
        .dimensions()
        .with_context(|| format!("reading {}", path.display()))?;
    match decoder
        .read_image()
        .with_context(|| format!("reading {}", path.display()))?
    {
        DecodingResult::U16(values) => Ok((values, cols, rows)),
        other => bail!("{}: not 16-bit samples ({other:?})", path.display()),
    }
}

/// Split chunky RGB samples into three planes
fn deinterleave3(chunky: &[u16]) -> Vec<Vec<u16>> {
    let mut planes: Vec<Vec<u16>> = (0..3)
        .map(|_| Vec::with_capacity(chunky.len() / 3))
        .collect();
    for pixel in chunky.chunks_exact(3) {
        for (plane, &value) in planes.iter_mut().zip(pixel) {
            plane.push(value);
        }
    }
    planes
}

/// Write one 16-bit chunky RGB image file
fn write_rgb16(path: &Path, planes: &[Vec<u16>], cols: u32, rows: u32, dpi: u32) -> Result<()> {
    let file =
        BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let mut tiff = TiffEncoder::new(file)?;
    let mut image = tiff.new_image::<RGB16>(cols, rows)?;
    image.resolution(ResolutionUnit::Inch, Rational { n: dpi, d: 1 });
    let mut strip = Vec::new();
    let mut done = 0usize;
    let total = cols as usize * rows as usize;
    while image.next_strip_sample_count() > 0 {
        let count = image.next_strip_sample_count() as usize / 3;
        strip.clear();
        strip.reserve(count * 3);
        for pixel in done..(done + count).min(total) {
            strip.extend(planes.iter().map(|plane| plane[pixel]));
        }
        image.write_strip(&strip)?;
        done = (done + count).min(total);
    }
    image.finish()?;
    Ok(())
}

pub fn run(args: Clean) -> Result<()> {
    let unit = Model::from_product(&args.model).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown scanner model {:?}: use a Nikon product name such as \"LS-50 ED\"",
            args.model
        )
    })?;
    let quality = match args.quality {
        CleanQuality::Normal => dust::Quality::Normal,
        CleanQuality::Fine => dust::Quality::Fine,
    };

    let (chunky, cols, rows) = read_u16(&args.rgb)?;
    if chunky.len() != 3 * cols as usize * rows as usize {
        bail!("{}: not a 3-sample image", args.rgb.display());
    }
    let (ir, ir_cols, ir_rows) = read_u16(&args.ir)?;
    if (ir_cols, ir_rows) != (cols, rows) || ir.len() != cols as usize * rows as usize {
        bail!(
            "{}: not a 1-sample image the size of {}",
            args.ir.display(),
            args.rgb.display()
        );
    }

    let mut layout = Layout::single_line(cols, rows, vec![1, 2, 3]);
    layout.dpi = args.dpi;
    let pass = Pass {
        layout,
        cooperation: Vec::new(),
        complete: true,
        blocks: 1,
        rows: rows as usize,
        cols: cols as usize,
    };
    let mut samples = Samples {
        colors: deinterleave3(&chunky),
        ir: Some(ir),
    };
    let rebuilt = clean_frame(&mut samples, &pass, Some(unit), quality)?;
    write_rgb16(&args.out, &samples.colors, cols, rows, args.dpi)?;
    println!("rebuilt {rebuilt} pixels");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(rgb: &str, ir: &str, model: &str) -> Clean {
        Clean {
            rgb: Path::new(rgb).to_path_buf(),
            ir: Path::new(ir).to_path_buf(),
            model: model.to_string(),
            dpi: 4000,
            quality: CleanQuality::Normal,
            out: Path::new("out.tiff").to_path_buf(),
        }
    }

    #[test]
    fn unknown_scanner_model_is_an_error() {
        let result = run(args("rgb.tiff", "ir.tiff", "Flatbed 123"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown scanner model")
        );
    }

    #[test]
    fn missing_input_files_are_an_error() {
        let result = run(args("no-such-rgb.tiff", "no-such-ir.tiff", "LS-50 ED"));
        assert!(result.is_err());
    }
}
