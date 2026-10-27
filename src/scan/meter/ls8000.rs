//! nkcolor: the LS-8000 form of `CNegaAlgorithm::Process`, from LS8000.md3
//!
//! [`super::negative`] ports LS5000.md3 and LS9000.md3. Direct execution of
//! LS8000.md3 on the 2026-09-10 Nikon Scan capture showed that the LS-8000
//! differs in these points:
//!
//! - It has its own film curve, the 1,024-byte characteristic resource
//!   ([`curve`]).
//! - A neutral ratio is valid between 0.125 and 8, and both ratios of a channel
//!   must be valid.
//! - The balanced shadow end counts its tail over every pixel of the crop.
//! - The highlight weight is the smallest channel weight off the shifted curve.
//! - `AutoFilmMode` weights by the least dense channel.
//!
//! With the LS-9000 curve the LS-5000 port gives the wrong green and blue
//! exposures for that capture. This port gives Nikon's three exposures and all
//! 12 analyzer limits.

use super::negative::{Crop, Curve, Inputs};
use crate::protocol::decode::Image;

/// LS8000.md3's characteristic resource: 32 log exposures, then 32 densities per channel, as f64
const CHARACTERISTIC: &[u8; 1024] = include_bytes!("ls8000-characteristic.bin");

/// The fraction of the population in a range's shadow tail
const TAIL: f64 = 0.0042;

/// After this fraction of a channel's pixels is rejected, every pixel counts
const REJECT_LIMIT: f64 = 0.9;

/// The LS-8000 color negative curve
pub(crate) fn curve() -> Curve {
    let mut v = CHARACTERISTIC
        .chunks_exact(8)
        .map(|b| f64::from_le_bytes(b.try_into().expect("eight bytes")));
    let mut next = || std::array::from_fn(|_| v.next().expect("128 values"));
    Curve {
        log_exposure: next(),
        density: [next(), next(), next()],
    }
}

/// The 12 histogram limits of `Tanten`, as samples
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ranges {
    pub low: [u16; 3],
    pub high: [u16; 3],
    pub balanced_low: [u16; 3],
    pub balanced_high: [u16; 3],
}

/// The scan exposures, or `None` if the crop is empty
pub(crate) fn exposures(image: &Image, crop: Crop, i: &Inputs) -> Option<[u32; 3]> {
    Some(final_exposures(ranges(image, crop, i)?, i))
}

/// The lowest sample whose count reaches `threshold`, and the highest present
///
/// The counts are 16 bits wide and wrap, as Nikon's are
fn histogram_range(samples: impl Iterator<Item = u16>, threshold: usize) -> (u16, u16) {
    let mut histogram = vec![0u16; 1 << 16];
    for sample in samples {
        let n = &mut histogram[usize::from(sample)];
        *n = n.wrapping_add(1);
    }
    let mut cumulative = 0usize;
    let mut low = 0u16;
    for (value, count) in histogram.iter().enumerate() {
        cumulative += usize::from(*count);
        if cumulative >= threshold {
            low = value as u16;
            break;
        }
    }
    let high = histogram.iter().rposition(|&count| count != 0).unwrap_or(0) as u16;
    (low, high)
}

/// `Tanten` on the LS-8000: the ends of each channel in `crop`, for all pixels and the neutral ones
pub(crate) fn ranges(image: &Image, crop: Crop, i: &Inputs) -> Option<Ranges> {
    if image.colors.len() != 3 {
        return None;
    }
    let widen = |v: u16| super::widen(v, image.bits);
    let mut selected = Vec::with_capacity((crop.x.1 - crop.x.0 + 1) * (crop.y.1 - crop.y.0 + 1));
    for y in crop.y.0..=crop.y.1 {
        for x in crop.x.0..=crop.x.1 {
            let at = (image.rows - 1 - x) * image.stride + y;
            selected.push([0, 1, 2].map(|c| widen(image.colors[c][at])));
        }
    }
    let total = selected.len();
    if total == 0 {
        return None;
    }
    let threshold = (total as f64 * TAIL) as usize;

    let mut low = [0u16; 3];
    let mut high = [0u16; 3];
    for c in 0..3 {
        (low[c], high[c]) = histogram_range(selected.iter().map(|p| p[c]), threshold);
    }

    let scale: [f64; 3] = std::array::from_fn(|c| f64::from(i.base[c]) / f64::from(i.prescan[c]));
    let d = i.base_density;
    let mask = [
        10f64.powf(d[1] - d[0]),
        10f64.powf(d[2] - d[0]),
        10f64.powf(d[2] - d[1]),
    ];
    let mut accepted = [Vec::new(), Vec::new(), Vec::new()];
    let mut rejected = [0usize; 3];
    for pixel in selected {
        let v: [f64; 3] = std::array::from_fn(|c| f64::from(pixel[c]) * scale[c]);
        let ratios = [
            v[0] / (mask[0] * v[1]),
            v[0] / (mask[1] * v[2]),
            v[1] / (mask[2] * v[2]),
        ];
        let valid = ratios.map(|q| q > 0.125 && q < 8.0);
        for (c, (a, b)) in [(0, 1), (0, 2), (1, 2)].into_iter().enumerate() {
            let good = valid[a] && valid[b];
            if !good {
                rejected[c] += 1;
            }
            if good || rejected[c] as f64 > REJECT_LIMIT * total as f64 {
                accepted[c].push(pixel[c]);
            }
        }
    }
    let mut balanced_low = [0u16; 3];
    let mut balanced_high = [0u16; 3];
    for c in 0..3 {
        (balanced_low[c], balanced_high[c]) =
            histogram_range(accepted[c].iter().copied(), threshold);
    }
    Some(Ranges {
        low,
        high,
        balanced_low,
        balanced_high,
    })
}

/// `GetComplementedValue`: linear interpolation of `query` from `x` to `y`
fn interpolate(x: &[f64; 32], y: &[f64; 32], query: f64) -> f64 {
    let index = x.partition_point(|value| *value <= query);
    if index == 0 {
        return y[0];
    }
    if index >= 32 {
        return y[31];
    }
    let (x0, x1) = (x[index - 1], x[index]);
    let (y0, y1) = (y[index - 1], y[index]);
    if x1 == x0 {
        y0
    } else {
        y0 + (query - x0) * (y1 - y0) / (x1 - x0)
    }
}

/// `AutoFilmMode` on the LS-8000: the shifts of the red and blue curves
fn balance_offsets(low: [f64; 3], high: [f64; 3], low_y: [f64; 3], high_y: [f64; 3]) -> (f64, f64) {
    let green_span = low[1] - high[1];
    let red_pair = 10f64.powf(-(high[0] - high[1]).abs());
    let blue_pair = 10f64.powf(-(high[2] - high[1]).abs());
    let red_shape = 3f64.powf(-((low[0] - high[0]) - green_span).abs());
    let blue_shape = 3f64.powf(-(green_span - (low[2] - high[2])).abs());
    let weight = 1f64.min(10f64.powf(low.into_iter().fold(f64::INFINITY, f64::min) - 0.8));
    let red_mix = ((1.0 - red_pair) + red_shape * red_pair) * weight;
    let blue_mix = ((1.0 - blue_pair) + blue_shape * blue_pair) * weight;
    (
        (low_y[0] - low_y[1]) * red_mix + (high_y[0] - high_y[1]) * (1.0 - red_mix) * red_pair,
        (low_y[1] - low_y[2]) * blue_mix + (high_y[1] - high_y[2]) * (1.0 - blue_mix) * blue_pair,
    )
}

/// The rest of `Process` on the LS-8000: the film curve placement and the three exposures
pub(crate) fn final_exposures(ranges: Ranges, i: &Inputs) -> [u32; 3] {
    let master = i.curve.log_exposure;
    let mut control_x = i.curve.density;
    let base = i.base.map(f64::from);
    let scale: [f64; 3] = std::array::from_fn(|c| base[c] / f64::from(i.prescan[c]));
    let code_density = |code: f64| (65535.0 / code.max(1.0)).log10();
    let density = |codes: [u16; 3]| -> [f64; 3] {
        std::array::from_fn(|c| code_density(f64::from(codes[c]) * scale[c]))
    };
    let low_density = density(ranges.low);
    let high_density = density(ranges.high);
    let balanced_low_density = density(ranges.balanced_low);
    let balanced_high_density = density(ranges.balanced_high);
    let densities: [f64; 3] =
        std::array::from_fn(|c| i.base_density[c].min(balanced_high_density[c]));
    for c in 0..3 {
        let shift = densities[c] - control_x[c][0];
        for value in &mut control_x[c] {
            *value += shift;
        }
    }
    let on_master = |c: usize, x: f64| interpolate(&control_x[c], &master, x);
    let balanced_low_y: [f64; 3] = std::array::from_fn(|c| on_master(c, balanced_low_density[c]));
    let balanced_high_y: [f64; 3] = std::array::from_fn(|c| on_master(c, balanced_high_density[c]));
    let anchor_y: [f64; 3] = std::array::from_fn(|c| on_master(c, densities[c] + 0.15));
    let (red_shift, blue_shift) = balance_offsets(
        std::array::from_fn(|c| balanced_low_y[c] - anchor_y[c]),
        std::array::from_fn(|c| balanced_high_y[c] - anchor_y[c]),
        balanced_low_y,
        balanced_high_y,
    );
    let control_y = [
        master.map(|value| value - red_shift),
        master,
        master.map(|value| value + blue_shift),
    ];
    let on_shifted = |c: usize, x: f64| interpolate(&control_x[c], &control_y[c], x);
    let base_y: [f64; 3] = std::array::from_fn(|c| on_shifted(c, densities[c] + 0.15));
    let low_difference: [f64; 3] =
        std::array::from_fn(|c| on_shifted(c, low_density[c]) - base_y[c]);
    let high_difference: [f64; 3] =
        std::array::from_fn(|c| on_shifted(c, high_density[c]) - base_y[c]);
    let lower_span = high_difference.into_iter().fold(f64::INFINITY, f64::min);
    let upper_span = low_difference.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let mut top = (0..3)
        .map(|c| on_shifted(c, low_density[c].min(control_x[c][31])))
        .fold(f64::NEG_INFINITY, f64::max);
    let mut bottom = (0..3)
        .map(|c| on_shifted(c, high_density[c].max(control_x[c][0])))
        .fold(f64::INFINITY, f64::min);
    let factor = high_difference
        .map(|value| 1f64.min(10f64.powf(-4.0 * (value + 0.23))))
        .into_iter()
        .fold(f64::INFINITY, f64::min);
    let width = top - bottom;
    if width < 0.6 {
        let previous_top = top;
        bottom -= (1.0 - factor) * (0.6 - width);
        top = (bottom + 0.6).max(previous_top + 0.05);
    } else {
        top += 0.05;
    }
    let width = top - bottom;
    let candidate = 1.9f64.min(1.9 * 0.4f64.powf(lower_span + 0.23));
    let contrast = if width < 1.9 {
        candidate.max(width)
    } else {
        1.9
    };
    // Nikon moves the shadow end too, but only the highlight end sets the exposures
    if upper_span < 1.2 {
        top += (1.2 - upper_span) * (width / contrast);
    }
    let _ = top;
    std::array::from_fn(|c| {
        let aligned = interpolate(&control_y[c], &control_x[c], bottom);
        (base[c] * 10f64.powf(aligned)).trunc() as u32
    })
}
