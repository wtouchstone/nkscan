//! Host metering, a port of Nikon Scan's `CNkIPPrescan::Execute`
//!
//! LS5000.md3 Mac `1009eb50`, LS9000.md3 `10044d90`. Each number is Nikon's,
//! from the decompiled function that the item names.

pub(crate) mod ls8000;
pub(crate) mod negative;

use crate::{
    error::Error,
    protocol::{
        caps::{Capabilities, film::FilmFormat},
        decode::Image,
        model::Model,
        window::{Channel, Window},
    },
    scan::profile::Film,
};
use negative::{COLOR_NEGATIVE, Crop, Inputs, MONOCHROME_NEGATIVE};
use tracing::*;

/// How to meter a frame
#[derive(Debug, Clone, Copy)]
pub struct Metering {
    /// The percentile that is a channel's brightest sample; 1.0 is Nikon Scan's
    pub percentile: f32,
    /// Move the visible channels by one factor, so they keep the unit's balance
    pub lock_white_balance: bool,
    /// The film type, which sets the prescan exposure and how the prescan is read
    pub film: Film,
    /// The frame format, which sets the measured crop; `None` measures the whole frame
    pub format: Option<FilmFormat>,
    /// The film base density per channel off the thumbnail; `None` uses Nikon Scan's defaults
    pub base_density: Option<[f64; 3]>,
}

impl Default for Metering {
    fn default() -> Self {
        Self::new(Film::default())
    }
}

/// What one prescan says to do next
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Prescan again at these exposures
    Again(Vec<u32>),
    /// Scan at these
    Done(Vec<u32>),
}

/// Nikon Scan's infrared target, 85% of full scale (Mac `100a005c`)
pub const INFRARED_TARGET: f32 = 0.85;

/// Nikon Scan's maximum exposure, whatever the unit allows
const MAX_EXPOSURE: u32 = 0x3ff_ffff;

/// Nikon Scan's film base density if the thumbnail gives none (`ReadBaseDensity`, Mac `100a1970`)
pub(crate) const DEFAULT_BASE_DENSITY: [f64; 3] = [0.316, 0.737, 0.886];

impl Metering {
    /// Metering for `film`, with the white balance lock from [`Self::locks_white_balance`]
    pub fn new(film: Film) -> Self {
        Self {
            percentile: 1.0,
            lock_white_balance: Self::locks_white_balance(film),
            film,
            format: None,
            base_density: None,
        }
    }

    /// Whether Nikon Scan meters the channels of `film` together; a negative is metered per channel
    pub fn locks_white_balance(film: Film) -> bool {
        !film.is_negative()
    }

    /// The prescan exposures from the start-up `base` (`CNkIPPreviewScan::SetDataInfo`, Mac `1009c320`)
    pub(crate) fn seed(&self, windows: &[Window], base: &[u32]) -> Vec<u32> {
        let factors = match self.film {
            Film::Negative => [1.65959, 3.98107, 5.37032],
            Film::MonochromeNegative => [1.86209, 2.0893, 2.0893],
            Film::Positive => [1.20009, 1.22458, 1.27356],
            Film::Kodachrome => [0.995, 1.0, 1.01],
        };
        windows
            .iter()
            .zip(base)
            .map(|(w, &b)| (f64::from(b) * color(w).map_or(0.9, |c| factors[c])) as u32)
            .collect()
    }

    /// The number of the last prescan, which gets no correction (`SetDataInfo` `+0x48`, Mac `1009e250`)
    fn passes(&self) -> usize {
        if self.film.is_negative() { 3 } else { 2 }
    }

    /// The next step after prescan `pass` (from 1); `base` is the start-up exposures in `windows` order
    pub(crate) fn step(
        &self,
        caps: &Capabilities,
        image: &Image,
        windows: &[Window],
        base: &[u32],
        pass: usize,
    ) -> Result<Step, Error> {
        let carried = image.colors.len() + usize::from(!image.ir.is_empty());
        if carried != windows.len() || windows.len() != base.len() {
            return Err(Error::Unsupported {
                op: "metering",
                reason: format!(
                    "the pass carried {carried} channels for {} windows",
                    windows.len()
                ),
            });
        }
        let crop = self.crop(caps, &windows[0], image);
        let current: Vec<u32> = windows.iter().map(|w| w.exposure).collect();
        let colors: Vec<usize> = (0..windows.len())
            .filter(|&i| color(&windows[i]).is_some())
            .collect();
        let measured: Vec<u16> = (0..colors.len())
            .map(|plane| high(image, image.colors[plane], crop, self.percentile))
            .collect();
        debug!(?crop, ?measured, pass, "metering levels");

        // Positives read as one channel unless asked not to
        let levels: Vec<u16> = match (self.film.is_negative(), self.lock_white_balance) {
            (false, true) => vec![measured.iter().copied().max().unwrap_or(0); measured.len()],
            _ => measured,
        };

        let mut next = current.clone();
        // Infrared is metered alone, on the whole frame
        if let Some(i) = (0..windows.len()).find(|&i| color(&windows[i]).is_none()) {
            let ir = high(image, image.ir, Crop::whole(image), self.percentile);
            if ir > 0 {
                next[i] = scale(
                    current[i],
                    65535.0 * f64::from(INFRARED_TARGET) / f64::from(ir),
                );
            }
        }

        // Below half scale, go to 90% and prescan again; if clipped, restart (`100a0068`, `100a0b4c`)
        if pass < self.passes() {
            let mut again = false;
            for (&i, &level) in colors.iter().zip(&levels) {
                let c = color(&windows[i]).expect("a color window");
                if level <= 0x7fff {
                    if current[i] != MAX_EXPOSURE {
                        next[i] = scale(current[i], 65535.0 / f64::from(level.max(1)) * 0.9);
                        again = true;
                    }
                } else if level == 0xffff {
                    next[i] = scale(base[i], self.clipped(c, pass));
                    again = true;
                }
            }
            if again {
                return Ok(Step::Again(self.limit(caps, next)));
            }
        }

        if self.film.is_negative() {
            let channel = |c: usize| colors.iter().position(|&i| color(&windows[i]) == Some(c));
            let (Some(r), Some(g), Some(b)) = (channel(0), channel(1), channel(2)) else {
                // A negative needs all three colors; fewer are read as a positive
                return Ok(Step::Done(self.positive(caps, &colors, &levels, next)));
            };
            let pick = |v: &[u32]| [v[colors[r]], v[colors[g]], v[colors[b]]];
            let planes = Image {
                colors: vec![image.colors[r], image.colors[g], image.colors[b]],
                ..*image
            };
            // nkcolor: the LS-8000 has its own curve and its own form of the analyzer
            let ls8000 =
                self.film == Film::Negative && caps.identity.model() == Some(Model::Ls8000);
            let ls8000_curve = ls8000.then(ls8000::curve);
            let inputs = Inputs {
                base: pick(base),
                prescan: pick(&current),
                base_density: self.base_density.unwrap_or(DEFAULT_BASE_DENSITY),
                curve: match (self.film, &ls8000_curve) {
                    (Film::MonochromeNegative, _) => &MONOCHROME_NEGATIVE,
                    (_, Some(curve)) => curve,
                    _ => &COLOR_NEGATIVE,
                },
                percentile: self.percentile,
            };
            let metered = if ls8000 {
                ls8000::exposures(&planes, crop, &inputs)
            } else {
                negative::exposures(&planes, crop, &inputs)
            };
            let Some(mut metered) = metered else {
                warn!("the prescan gave the film curve nothing to place; scanning as prescanned");
                return Ok(Step::Done(self.limit(caps, next)));
            };
            debug!(?metered, "off the film curve");
            if self.lock_white_balance {
                // The smallest factor of the three, so that no channel clips
                let k = (0..3)
                    .map(|c| f64::from(metered[c]) / f64::from(inputs.base[c]))
                    .fold(f64::INFINITY, f64::min);
                metered = std::array::from_fn(|c| scale(inputs.base[c], k));
            }
            for (c, &plane) in [r, g, b].iter().enumerate() {
                next[colors[plane]] = metered[c];
            }
            return Ok(Step::Done(self.limit(caps, next)));
        }
        Ok(Step::Done(self.positive(caps, &colors, &levels, next)))
    }

    /// A positive's exposures, with the brightest sample at full scale (Mac `100a04a4`)
    fn positive(
        &self,
        caps: &Capabilities,
        colors: &[usize],
        levels: &[u16],
        mut next: Vec<u32>,
    ) -> Vec<u32> {
        for (&i, &level) in colors.iter().zip(levels) {
            if level > 0 {
                next[i] = scale(next[i], 65535.0 / f64::from(level));
            }
        }
        self.limit(caps, next)
    }

    /// The restart factor on the base for clipped channel `c` (Win `100ced78`, Mac `100a018c`)
    fn clipped(&self, c: usize, pass: usize) -> f64 {
        const COLOR: [[f64; 4]; 3] = [
            [1.65959, 1.15942, 0.9, 0.9],
            [3.98107, 2.84605, 1.4264, 0.9],
            [5.37032, 3.19332, 1.60045, 0.9],
        ];
        const MONOCHROME: [[f64; 3]; 3] = [
            [1.86209, 0.94242, 0.9],
            [2.0893, 1.04713, 0.9],
            [2.0893, 1.04713, 0.9],
        ];
        match self.film {
            Film::Negative => COLOR[c][pass.min(3)],
            Film::MonochromeNegative => MONOCHROME[c][pass.min(2)],
            Film::Positive => 0.98,
            Film::Kodachrome => [0.8955, 0.9, 0.909][c],
        }
    }

    /// The measured crop in prescan pixels (LS5000.md3 Mac `1009e250`, LS4000.md3 `10034f6a`,
    /// LS9000.md3 `1003a780`)
    fn crop(&self, caps: &Capabilities, window: &Window, image: &Image) -> Crop {
        let ls40 = caps.address.x_axis.optical_dpi != 4000;
        let n = u32::from(prescan_divisor(caps));
        let device: Option<[u32; 4]> = match adapter_crop(caps, self.film) {
            Some(crop) => crop.map(|c| c.map(|v| v * n)),
            None => match self.format {
                Some(FilmFormat::F135) if ls40 => Some([36, 249, 51, 381].map(|v| v * n)),
                Some(FilmFormat::F135) => Some([36, 245, 50, 375].map(|v| v * n)),
                Some(FilmFormat::IX240) if ls40 => Some([0x1d, 0xb6, 0x22, 0x121].map(|v| v * n)),
                Some(FilmFormat::IX240) => Some([0x1c, 0xb2, 0x21, 0x11b].map(|v| v * n)),
                Some(FilmFormat::F645) => Some([547, 8417, 675, 6021]),
                Some(FilmFormat::F66) => Some([547, 8417, 675, 8289]),
                Some(FilmFormat::F67) => Some([547, 8417, 675, 10341]),
                Some(FilmFormat::F68) => Some([547, 8417, 675, 11529]),
                Some(FilmFormat::F69) => Some([547, 8417, 675, 12501]),
                _ => None,
            },
        };
        let Some([x0, x1, y0, y1]) = device else {
            return Crop::whole(image);
        };
        let (pixels, lines) = (image.rows - 1, image.cols - 1);
        let reduce = |optical: u16, dpi: u16| u32::from(optical / dpi.max(1)).max(1);
        let rx = reduce(caps.address.x_axis.optical_dpi, window.resolution.0);
        let ry = reduce(caps.address.y_axis.optical_dpi, window.resolution.1);
        let at = |v: u32, r: u32, last: usize| ((v / r) as usize).min(last);
        Crop {
            x: (at(x0, rx, pixels), at(x1, rx, pixels)),
            y: (at(y0, ry, lines), at(y1, ry, lines)),
        }
    }

    /// Clamp to the exposures the unit accepts (`SetWindowFunction` bytes 16-24)
    fn limit(&self, caps: &Capabilities, exposures: Vec<u32>) -> Vec<u32> {
        let limit = &caps.set_window.exposure;
        exposures
            .into_iter()
            .map(|e| e.clamp(limit.start, limit.last.min(MAX_EXPOSURE)))
            .collect()
    }
}

/// nkcolor: whether a captured Nikon Scan session verifies the color negative meter on `model`
///
/// The LS-50 and LS-5000 share LS5000.md3's rule, which [`negative`] ports. The
/// LS-8000 uses [`ls8000`]. Each was checked against a Nikon builder record:
/// the three final exposures match to the count. `scan --nikon-meter` and
/// `scan_frame(nikon_negative_meter=True)` refuse every other unit.
pub fn nikon_negative_supported(model: Option<Model>) -> bool {
    matches!(model, Some(Model::Ls50 | Model::Ls5000 | Model::Ls8000))
}

/// The 35mm crop of Nikon Scan in prescan pixels, from the adapter, the holder and the film.
/// `Some(None)` is the full prescan. `None` is an adapter that is not known (LS5000.md3
/// `1003c1f0`, LS4000.md3 `10034bd0`)
fn adapter_crop(caps: &Capabilities, film: Film) -> Option<Option<[u32; 4]>> {
    if caps.identity.is_mf_scanner() {
        return None;
    }
    let ls40 = caps.address.x_axis.optical_dpi != 4000;
    let fh3 = matches!(caps.holder.as_deref(), Some("FH-3" | "FH3"));
    let crop = match (caps.adapter.as_deref()?, film.is_negative(), ls40) {
        ("240", true, false) => [0x1c, 0xb2, 0x21, 0x11b],
        ("240", true, true) => [0x1d, 0xb6, 0x22, 0x121],
        ("Mount" | "Feeder" | "Strip" | "6Strip" | "36Strip", true, false) => [36, 245, 50, 375],
        ("Mount" | "Feeder" | "Strip" | "6Strip" | "36Strip", true, true) => [36, 249, 51, 381],
        ("Strip" | "6Strip" | "36Strip", false, false) => [0, 0x119, 0x13, 0x196],
        ("Strip" | "6Strip" | "36Strip", false, true) => [0, 0x11e, 0x13, 0x19d],
        ("Mount", false, false) if fh3 => [0, 0x119, 9, 0x192],
        ("Mount", false, true) if fh3 => [0, 0x11e, 0xa, 0x199],
        ("Mount" | "Feeder" | "240", false, _) => return Some(None),
        _ => return None,
    };
    Some(Some(crop))
}

/// The optical resolution divided by the 35mm prescan resolution. This is 14 on a 4000 dpi
/// unit and 10 on the 2900 dpi LS-40 (LS4000.md3 `10034dc8`)
pub(crate) fn prescan_divisor(caps: &Capabilities) -> u16 {
    if caps.address.x_axis.optical_dpi == 4000 {
        14
    } else {
        10
    }
}

/// Which of red, green and blue a window carries
pub(crate) fn color(w: &Window) -> Option<usize> {
    match w.channel() {
        Channel::Red => Some(0),
        Channel::Green => Some(1),
        Channel::Blue => Some(2),
        _ => None,
    }
}

/// `exposure` times `factor`, truncated as Nikon Scan does, up to [`MAX_EXPOSURE`]
pub(crate) fn scale(exposure: u32, factor: f64) -> u32 {
    (f64::from(exposure) * factor).min(f64::from(MAX_EXPOSURE)) as u32
}

/// A sample of `bits` valid bits on Nikon Scan's 16-bit scale
pub(crate) fn widen(v: u16, bits: u8) -> u16 {
    let full = f64::from(ceiling(bits));
    (f64::from(v) * 65535.0 / full).round() as u16
}

/// The `percentile` sample of `plane` in `crop` on the 16-bit scale; `crop.x` counts rows from the bottom
fn high(image: &Image, plane: &[u16], crop: Crop, percentile: f32) -> u16 {
    let mut counts = vec![0u32; 1 << 16];
    for y in crop.y.0..=crop.y.1 {
        for x in crop.x.0..=crop.x.1 {
            let v = plane[(image.rows - 1 - x) * image.stride + y];
            counts[usize::from(widen(v, image.bits))] += 1;
        }
    }
    let total: u32 = counts.iter().sum();
    let above = ((1.0 - f64::from(percentile.clamp(0.0, 1.0))) * f64::from(total)) as u32;
    let mut seen = 0;
    counts
        .iter()
        .rposition(|&n| {
            seen += n;
            seen > above
        })
        .unwrap_or(0) as u16
}

/// Full scale for a sample of `bits` valid bits
pub(crate) fn ceiling(bits: u8) -> u16 {
    match bits {
        0 | 16.. => u16::MAX,
        b => (1u16 << b) - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::window::LENGTH,
        scan::{boundary::fixtures::read_tiff, thumbnail, window::tests::caps},
    };
    use std::path::PathBuf;

    fn fixture(path: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(path)
    }

    /// A prescan from an LS-50 Nikon Scan capture, 281 14-bit samples a line, as planes
    fn prescan(name: &str, channels: usize) -> Vec<Vec<u16>> {
        let raw: Vec<u16> = std::fs::read(fixture(&format!("meter/{name}.bin")))
            .expect("prescan")
            .chunks(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let pixels = 281;
        let lines = raw.len() / (pixels * channels);
        (0..channels)
            .map(|c| {
                let mut plane = vec![0; pixels * lines];
                for l in 0..lines {
                    for x in 0..pixels {
                        plane[(pixels - 1 - x) * lines + l] = raw[(l * channels + c) * pixels + x];
                    }
                }
                plane
            })
            .collect()
    }

    fn image(planes: &[Vec<u16>]) -> Image<'_> {
        let rows = 281;
        let cols = planes[0].len() / rows;
        Image {
            colors: planes[..3].iter().map(Vec::as_slice).collect(),
            ir: planes.get(3).map_or(&[], Vec::as_slice),
            rows,
            cols,
            stride: cols,
            bits: 14,
        }
    }

    /// Windows 1, 2, 3 (and 9) at the prescan's resolution and these exposures
    fn windows(exposures: &[u32]) -> Vec<Window> {
        [1, 2, 3, 9]
            .into_iter()
            .zip(exposures)
            .map(|(id, &exposure)| {
                let mut w = Window::try_from(&[0u8; LENGTH][..]).unwrap();
                w.id = id;
                w.exposure = exposure;
                w.resolution = (285, 285);
                w
            })
            .collect()
    }

    fn unit() -> Capabilities {
        let mut caps = caps();
        caps.set_window.exposure = std::range::RangeInclusive {
            start: 1,
            last: MAX_EXPOSURE,
        };
        caps
    }

    /// The film base density of a fixture thumbnail at Nikon Scan's exposures
    fn base_density(name: &str, base: [u32; 3], taken: [u32; 3]) -> [f64; 3] {
        let (planes, rows, cols) = read_tiff(&fixture(&format!("framing/{name}/thumbnail.tiff")));
        let image = Image {
            colors: planes.iter().map(Vec::as_slice).collect(),
            ir: &[],
            rows,
            cols,
            stride: cols,
            bits: 14,
        };
        let ratio = std::array::from_fn(|c| f64::from(base[c]) / f64::from(taken[c]));
        thumbnail::base_density(&image, ratio).expect("a base density")
    }

    /// A 35mm unit selects the crop from its adapter, its holder and the film (LS5000.md3 `1003c1f0`)
    #[test]
    fn a_35mm_unit_crops_by_adapter_holder_and_film() {
        let planes = vec![vec![0u16; 281 * 425]; 3];
        let image = Image {
            colors: planes.iter().map(Vec::as_slice).collect(),
            ir: &[],
            rows: 281,
            cols: 425,
            stride: 425,
            bits: 14,
        };
        let w = windows(&[1])[0].clone();
        let crop = |adapter: &str, holder: Option<&str>, film| {
            let mut caps = unit();
            caps.identity.product = "LS-50 ED".into();
            caps.adapter = Some(adapter.into());
            caps.holder = holder.map(Into::into);
            let c = Metering::new(film).crop(&caps, &w, &image);
            (c.x, c.y)
        };
        let whole = ((0, 280), (0, 424));
        assert_eq!(crop("6Strip", None, Film::Negative), ((36, 245), (50, 375)));
        assert_eq!(crop("Mount", None, Film::Negative), ((36, 245), (50, 375)));
        assert_eq!(crop("6Strip", None, Film::Positive), ((0, 280), (19, 406)));
        assert_eq!(
            crop("Mount", Some("FH-3"), Film::Positive),
            ((0, 280), (9, 402))
        );
        assert_eq!(crop("Mount", Some("FH-G1"), Film::Positive), whole);
        assert_eq!(crop("Feeder", None, Film::Kodachrome), whole);
        assert_eq!(crop("240", None, Film::Positive), whole);
    }

    /// The LS-40 has its own 135 crop (LS4000.md3 `10035021`)
    #[test]
    fn the_ls40_has_its_own_135_crop() {
        let planes = vec![vec![0u16; 400 * 600]; 3];
        let image = Image {
            colors: planes.iter().map(Vec::as_slice).collect(),
            ir: &[],
            rows: 400,
            cols: 600,
            stride: 600,
            bits: 14,
        };
        let metering = Metering {
            format: Some(FilmFormat::F135),
            ..Metering::new(Film::Positive)
        };
        let mut w = windows(&[1])[0].clone();
        w.resolution = (400, 400);
        let four = metering.crop(&unit(), &w, &image);
        assert_eq!((four.x, four.y), ((50, 343), (70, 525)));

        let mut ls40 = unit();
        ls40.address.x_axis.optical_dpi = 2900;
        ls40.address.y_axis.optical_dpi = 2900;
        w.resolution = (290, 290);
        let crop = metering.crop(&ls40, &w, &image);
        assert_eq!((crop.x, crop.y), ((36, 249), (51, 381)));
    }

    /// `coolscan_v_frame_test` gets Nikon Scan's exposures to the count
    #[test]
    fn a_color_negative_is_exposed_as_nikon_exposes_it() {
        let base = [48132, 51833, 35369];
        let density = base_density("coolscan_v_frame_test", base, [59250, 147413, 135781]);
        assert!((density[0] - 0.4172).abs() < 1e-4, "{density:?}");
        let metering = Metering {
            format: Some(FilmFormat::F135),
            base_density: Some(density),
            ..Metering::new(Film::Negative)
        };
        let seeded = metering.seed(&windows(&base), &base);
        assert_eq!(seeded, [79879, 206350, 189942]);

        let planes = prescan("color_negative", 4);
        let w = windows(&[79879, 206350, 189942, 302968]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[48132, 51833, 35369, 336631],
                1,
            )
            .unwrap()
        else {
            panic!("one prescan was enough for Nikon Scan");
        };
        assert_eq!(got[..3], [109319, 313660, 368912]);
    }

    /// `coolscan_v_bw`: the first prescan is too dark, so each channel goes to 90%, then the curve
    #[test]
    fn a_black_and_white_negative_is_brought_up_then_read_off_its_curve() {
        // Inferred from Nikon Scan's prescan exposures; the capture holds no 8Ch read
        let base = [47596, 51545, 35160, 334764];
        let density = base_density(
            "coolscan_v_bw",
            base[..3].try_into().unwrap(),
            [61923, 78760, 49610],
        );
        let metering = Metering {
            format: Some(FilmFormat::F135),
            base_density: Some(density),
            ..Metering::new(Film::MonochromeNegative)
        };

        let planes = prescan("mono_negative_1", 4);
        let w = windows(&[88629, 107692, 73459, 301288]);
        let step = metering
            .step(&unit(), &image(&planes), &w, &base, 1)
            .unwrap();
        let Step::Again(next) = step else {
            panic!("Nikon Scan prescanned again, this said {step:?}");
        };
        assert_eq!(next[..3], [296863, 321107, 185595]);

        // Nikon Scan's code, emulated on these inputs; its capture sent 176472, 318763, 220587
        let metering = Metering {
            base_density: Some([0.4468, 0.737, 0.886]),
            ..metering
        };
        let planes = prescan("mono_negative_2", 3);
        let w = windows(&[296863, 321107, 185595]);
        let step = metering
            .step(&unit(), &image(&planes), &w, &base[..3], 2)
            .unwrap();
        assert_eq!(step, Step::Done(vec![176308, 319607, 220587]));
    }

    /// `coolscan_v_slide`: one factor puts the brightest sample at full scale
    #[test]
    fn a_slide_is_exposed_as_nikon_exposes_it() {
        let metering = Metering {
            format: Some(FilmFormat::F135),
            ..Metering::new(Film::Positive)
        };
        let planes = prescan("slide", 4);
        let w = windows(&[57120, 63120, 44778, 301288]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[47597, 51544, 35160, 334764],
                1,
            )
            .unwrap()
        else {
            panic!("one prescan was enough for Nikon Scan");
        };
        assert_eq!(got[..3], [76667, 84720, 60101]);
        // Nikon Scan's was 334093: its infrared reads a max this does not
        assert!(got[3].abs_diff(334093) < 10, "{got:?}");
    }

    /// Unlocked, a slide's channels each put their own brightest at full scale
    #[test]
    fn an_unlocked_slide_is_metered_per_channel() {
        let metering = Metering {
            format: Some(FilmFormat::F135),
            lock_white_balance: false,
            ..Metering::new(Film::Positive)
        };
        let planes = prescan("slide", 4);
        let w = windows(&[57120, 63120, 44778, 301288]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[47597, 51544, 35160, 334764],
                1,
            )
            .unwrap()
        else {
            panic!("every channel reads");
        };
        // Red is the brightest, so it is where it was locked; the others go further
        assert_eq!(got[0], 76667);
        assert!(got[1] > 84720 && got[2] > 60101, "{got:?}");
    }

    /// A locked negative keeps the start-up balance, with the smallest factor
    #[test]
    fn a_locked_negative_keeps_the_start_up_balance() {
        let base = [48132, 51833, 35369];
        let metering = Metering {
            format: Some(FilmFormat::F135),
            lock_white_balance: true,
            ..Metering::new(Film::Negative)
        };
        let planes = prescan("color_negative", 4);
        let w = windows(&[79879, 206350, 189942, 302968]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[48132, 51833, 35369, 336631],
                1,
            )
            .unwrap()
        else {
            panic!("one prescan");
        };
        let k: Vec<f64> = (0..3)
            .map(|c| f64::from(got[c]) / f64::from(base[c]))
            .collect();
        assert!(k.windows(2).all(|p| (p[0] - p[1]).abs() < 1e-4), "{k:?}");
    }

    /// nkcolor: a Nikon builder record (`NBLD`): the meter image as planes, its width and height
    fn builder_record(record: &[u8], bytes_per_pixel: usize) -> (Vec<Vec<u16>>, usize, usize) {
        let read = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        assert_eq!(&record[..4], b"NBLD");
        let descriptor_length = read(record, 12) as usize;
        let descriptor = &record[16..16 + descriptor_length];
        let at = 16 + descriptor_length;
        let pixels = &record[at + 4..at + 4 + read(record, at) as usize];
        let (width, height) = (read(descriptor, 0) as usize, read(descriptor, 4) as usize);
        let mut planes = vec![vec![0u16; width * height]; 3];
        for line in 0..height {
            for x in 0..width {
                let source = (line * width + x) * bytes_per_pixel;
                for (c, plane) in planes.iter_mut().enumerate() {
                    let o = source + c * 2;
                    plane[(width - 1 - x) * height + line] =
                        u16::from_le_bytes([pixels[o], pixels[o + 1]]);
                }
            }
        }
        (planes, width, height)
    }

    fn record_image(planes: &[Vec<u16>], width: usize, height: usize) -> Image<'_> {
        Image {
            colors: planes.iter().map(Vec::as_slice).collect(),
            ir: &[],
            rows: width,
            cols: height,
            stride: height,
            bits: 16,
        }
    }

    fn ls8000() -> Capabilities {
        let mut caps = unit();
        caps.identity.product = "LS-8000 ED".into();
        caps
    }

    /// nkcolor: only the units a Nikon builder record verifies take `--nikon-meter`
    #[test]
    fn the_nikon_negative_meter_is_verified_per_unit() {
        assert!(nikon_negative_supported(Some(Model::Ls50)));
        assert!(nikon_negative_supported(Some(Model::Ls5000)));
        assert!(nikon_negative_supported(Some(Model::Ls8000)));
        assert!(!nikon_negative_supported(Some(Model::Ls9000)));
        assert!(!nikon_negative_supported(None));
    }

    /// nkcolor: the LS-5000 builder record of 2026-09-02 gets Nikon Scan's three exposures
    #[test]
    fn an_ls5000_builder_record_is_exposed_as_nikon_exposes_it() {
        let (planes, width, height) = builder_record(
            include_bytes!("../../../tests/data/nikon-builder-20260902.bin"),
            8,
        );
        assert_eq!((width, height), (281, 413));
        let base = [59536, 43137, 31992];
        let metering = Metering {
            format: Some(FilmFormat::F135),
            ..Metering::new(Film::Negative)
        };
        // Nikon Scan's first meter exposures, off the start-up values
        let seeded = metering.seed(&windows(&base), &base);
        assert_eq!(seeded, [98805, 171731, 171807]);
        let mut caps = unit();
        caps.identity.product = "LS-5000 ED".into();
        let step = metering
            .step(
                &caps,
                &record_image(&planes, width, height),
                &windows(&seeded),
                &base,
                1,
            )
            .unwrap();
        assert_eq!(step, Step::Done(vec![100454, 197670, 244673]));
    }

    /// The LS-8000 meter windows of the builder record: 666 by 333 dpi
    fn ls8000_windows(exposures: &[u32]) -> Vec<Window> {
        let mut w = windows(exposures);
        for w in &mut w {
            w.resolution = (666, 333);
        }
        w
    }

    /// nkcolor: the LS-8000 builder record of 2026-09-10 gets Nikon Scan's 12 limits
    #[test]
    fn the_ls8000_analyzer_matches_the_builder_record() {
        let (planes, width, height) = builder_record(
            include_bytes!("../../../tests/data/nikon-builder-ls8000-20260910.bin"),
            6,
        );
        assert_eq!((width, height), (1494, 558));
        let image = record_image(&planes, width, height);
        let curve = ls8000::curve();
        let inputs = negative::Inputs {
            base: [275778, 339801, 282416],
            prescan: [457678, 3298679, 9845379],
            base_density: DEFAULT_BASE_DENSITY,
            curve: &curve,
            percentile: 1.0,
        };
        // The 645 crop at 666 by 333 dpi is the analysis rectangle Nikon Scan used
        let metering = Metering {
            format: Some(FilmFormat::F645),
            ..Metering::new(Film::Negative)
        };
        let crop = metering.crop(&ls8000(), &ls8000_windows(&[1])[0], &image);
        assert_eq!((crop.x, crop.y), ((91, 1402), (56, 501)));
        let ranges = ls8000::ranges(&image, crop, &inputs).unwrap();
        assert_eq!(ranges.low, [2282, 4294, 6966]);
        assert_eq!(ranges.high, [34122, 57586, 56450]);
        assert_eq!(ranges.balanced_low, [2282, 4294, 6966]);
        assert_eq!(ranges.balanced_high, [34122, 57586, 56450]);
    }

    /// nkcolor: the LS-8000 builder record gets Nikon Scan's three exposures, and the LS-9000
    /// curve does not give them
    #[test]
    fn an_ls8000_builder_record_is_exposed_as_nikon_exposes_it() {
        let (planes, width, height) = builder_record(
            include_bytes!("../../../tests/data/nikon-builder-ls8000-20260910.bin"),
            6,
        );
        let image = record_image(&planes, width, height);
        let base = [275778, 339801, 282416];
        let metered = [457678, 3298679, 9845379];
        let metering = Metering {
            format: Some(FilmFormat::F645),
            ..Metering::new(Film::Negative)
        };
        let step = metering
            .step(&ls8000(), &image, &ls8000_windows(&metered), &base, 3)
            .unwrap();
        assert_eq!(step, Step::Done(vec![879020, 3701152, 4671874]));

        let mut ls9000 = unit();
        ls9000.identity.product = "LS-9000 ED".into();
        let step = metering
            .step(&ls9000, &image, &ls8000_windows(&metered), &base, 3)
            .unwrap();
        assert_ne!(step, Step::Done(vec![879020, 3701152, 4671874]));
    }

    /// nkcolor: an underexposed first LS-8000 meter image is metered again before the analyzer
    #[test]
    fn a_low_first_ls8000_pass_is_metered_again() {
        let (mut planes, width, height) = builder_record(
            include_bytes!("../../../tests/data/nikon-builder-ls8000-20260910.bin"),
            6,
        );
        let base = [275778u32, 339801, 282416];
        let metering = Metering {
            format: Some(FilmFormat::F645),
            ..Metering::new(Film::Negative)
        };
        let first = metering.seed(&windows(&base), &base);
        // The same film at the first exposures instead of the settled ones
        let settled = [457678.0, 3298679.0, 9845379.0];
        for (c, plane) in planes.iter_mut().enumerate() {
            for v in plane.iter_mut() {
                *v = (f64::from(*v) * f64::from(first[c]) / settled[c])
                    .round()
                    .clamp(0.0, 65535.0) as u16;
            }
        }
        let step = metering
            .step(
                &ls8000(),
                &record_image(&planes, width, height),
                &ls8000_windows(&first),
                &base,
                1,
            )
            .unwrap();
        let Step::Again(next) = step else {
            panic!("a dark first pass is metered again, this said {step:?}");
        };
        assert_eq!(next[0], first[0]);
        assert!(
            next[1] > first[1] && next[2] > first[2],
            "{next:?} {first:?}"
        );
    }
}
