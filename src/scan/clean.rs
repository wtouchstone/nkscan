//! Running IR dust removal over a finished pass

use crate::{
    dust,
    error::Error,
    protocol::{decode::Samples, model::Model, window::Channel},
    scan::pass::Pass,
};
use tracing::*;

/// About how many pixels calibration wants to measure over
const PRESCAN_PIXELS: usize = 2_500_000;

/// Select the dust profile for the named scanner family.
fn profile_for(model: Option<Model>) -> Result<dust::Model, Error> {
    match model {
        Some(Model::Ls8000 | Model::Ls9000) => Ok(dust::Model::Ls9000),
        Some(Model::Ls5000) => Ok(dust::Model::Ls5000),
        Some(Model::Ls50) => Ok(dust::Model::Ls50),
        Some(model) => Err(Error::Unsupported {
            op: "clean",
            reason: format!("no verified dust profile for {}", model.name()),
        }),
        None => Err(Error::Unsupported {
            op: "clean",
            reason: "no verified dust profile for an unrecognized scanner".into(),
        }),
    }
}

/// Every `step`th pixel of a plane, which is all calibration needs
fn decimate(plane: &[u16], cols: usize, step: usize) -> Vec<u16> {
    let rows = plane.len() / cols;
    let mut out = Vec::with_capacity((rows / step) * (cols / step));
    for y in (0..rows - rows % step).step_by(step) {
        for x in (0..cols - cols % step).step_by(step) {
            out.push(plane[y * cols + x]);
        }
    }
    out
}

/// Run dust removal over a finished pass, in place, returning how many pixels it rebuilt
pub fn clean_frame(
    samples: &mut Samples,
    pass: &Pass,
    model: Option<Model>,
    quality: dust::Quality,
) -> Result<usize, Error> {
    // The buffer holds the pass's color channels in the stream's own order
    let ids: Vec<u8> = pass.layout.colors().collect();
    let at = |want: Channel| ids.iter().position(|&id| Channel::from(id) == want);
    let (Some(r), Some(g), Some(b)) = (at(Channel::Red), at(Channel::Green), at(Channel::Blue))
    else {
        return Err(Error::Unsupported {
            op: "clean",
            reason: format!("needs a red, green and blue plane, this pass has {ids:?}"),
        });
    };

    let model = profile_for(model)?;
    let opts = dust::Options {
        model,
        quality,
        dpi: pass.layout.dpi,
    };

    let Some(ir) = samples.ir.as_deref() else {
        return Err(Error::Unsupported {
            op: "clean",
            reason: "needs the infrared pass".into(),
        });
    };

    // Red and infrared only: that is all calibration reads
    let step = ((pass.rows * pass.cols) / PRESCAN_PIXELS).isqrt().max(1);
    let small_red = decimate(&samples.colors[r], pass.cols, step);
    let small_ir = decimate(ir, pass.cols, step);
    let cal = dust::calibrate(&dust::Prescan {
        red: &small_red,
        ir: &small_ir,
        rows: pass.rows / step,
        cols: pass.cols / step,
    })
    .or_else(|| {
        // A frame with little clear film can have none left after decimation while the full pass still has plenty
        warn!("no clear film in the decimated prescan, calibrating off the whole frame");
        dust::calibrate(&dust::Prescan {
            red: &samples.colors[r],
            ir,
            rows: pass.rows,
            cols: pass.cols,
        })
    })
    .ok_or_else(|| Error::Unsupported {
        op: "clean",
        reason: "no clear film in this frame to calibrate against".into(),
    })?;
    debug!(?cal, step, "ICE calibration");

    let [pr, pg, pb] = samples
        .colors
        .get_disjoint_mut([r, g, b])
        .expect("three distinct color planes");
    Ok(dust::clean(
        [pr, pg, pb],
        ir,
        &cal,
        pass.rows,
        pass.cols,
        &opts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls8000_uses_the_medium_format_dust_profile() {
        assert_eq!(
            profile_for(Some(Model::Ls9000)).unwrap(),
            dust::Model::Ls9000
        );
        assert_eq!(
            profile_for(Some(Model::Ls8000)).unwrap(),
            dust::Model::Ls9000
        );
    }
}
