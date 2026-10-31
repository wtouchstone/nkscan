//! How the exposures get decided, and the exposure terms themselves

use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            set_window::{AnalogControl, ScanKind, ScanMode},
        },
        decode::Samples,
        model::Model,
        window::{Channel, Flags, Window},
    },
    scan::{
        meter::{Metering, prescan_divisor},
        pass::Pass,
    },
};
use std::collections::BTreeMap;

/// Per-channel exposure terms, window descriptor bytes 46-49
///
/// One descriptor per channel and one exposure per descriptor, in units of
/// 10 ns and bounded by `SetWindowFunction` bytes 16-24. Metering answers one
/// of these; a batch that meters only its first frame carries it to the rest,
/// and a caller that knows what it wants writes one by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exposures(BTreeMap<Channel, u32>);

impl Exposures {
    /// The exposures a set of descriptors is carrying
    pub fn read(windows: &[Window]) -> Self {
        Self(windows.iter().map(|w| (w.channel(), w.exposure)).collect())
    }

    /// What this channel will be exposed for, if anything says
    pub fn get(&self, channel: Channel) -> Option<u32> {
        self.0.get(&channel).copied()
    }

    /// Expose one channel for `exposure`
    ///
    /// Unchecked here: the unit's range is `SetWindowFunction`'s, and
    /// [`Window::validate`] is what answers a value outside it
    pub fn set(&mut self, channel: Channel, exposure: u32) {
        self.0.insert(channel, exposure);
    }

    /// Put these into `windows`, leaving a channel we have nothing for alone
    pub fn apply(&self, windows: &mut [Window]) {
        for w in windows {
            if let Some(&exposure) = self.0.get(&w.channel()) {
                w.exposure = exposure;
            }
        }
    }

    /// Every channel and its exposure, in channel order
    pub fn iter(&self) -> impl Iterator<Item = (Channel, u32)> + '_ {
        self.0.iter().map(|(&c, &e)| (c, e))
    }
}

/// nkcolor: one image used to decide a frame's exposure
#[derive(Debug, Clone)]
pub struct MeterPass {
    /// The decoded meter pixels
    pub samples: Samples,
    /// The exact SET WINDOW descriptors that produced the pixels
    pub windows: Vec<Window>,
    /// Their shape and channel order
    pub pass: Pass,
    /// The exposure that produced these pixels
    pub exposures: Exposures,
    /// The rectangle metering measured in these pixels, inclusive, as
    /// `[first line, first pixel, last line, last pixel]`. Lines run along the
    /// feed. Pixels run along the sensor, counted from the far end, which is
    /// the orientation of Nikon Scan's builder image
    pub analyzer_rect: [usize; 4],
}

/// nkcolor: the evidence retained from one exposure decision
#[derive(Debug, Clone, Default)]
pub struct MeteringEvidence {
    /// The scanner's start-up channel balance, where host metering used it
    pub startup_exposures: Option<Exposures>,
    /// Every host meter pass, in capture order
    pub passes: Vec<MeterPass>,
    /// The full-scan exposures Nikon Scan's rule gives. They differ from the
    /// selected exposures only where the fork scans at other exposures: on an
    /// LS-8000 color negative each channel is exposed to its own film base
    pub nikon_exposures: Option<Exposures>,
}

/// nkcolor: an exposure decision and the evidence that produced it
#[derive(Debug, Clone, Default)]
pub struct Metered {
    /// The exposures selected for the full scan
    pub exposures: Exposures,
    pub evidence: MeteringEvidence,
}

/// How the exposures get decided
///
/// `SetWindowFunction` byte 4 says whether the unit will meter for itself. If
/// neither AE bit is set, we do it. There is no host-cooperation bit for this in
/// `Features` the way there is for autofocus, so the missing scan kind is the
/// only signal.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AutoExposure {
    /// The unit meters itself. This is a scanning kind, so it goes in the
    /// window descriptor
    Unit(ScanKind),
    /// We take an ordinary pass and work the exposures out from it
    Host(Metering),
}

impl AutoExposure {
    /// Determine the kind of AE to use
    ///
    /// The two unit-side branches are unexercised: neither an LS-50 nor an
    /// LS-9000 sets [`ScanKind::AE`] or `AE_WB`, so every unit seen so far
    /// lands on [`Host`](Self::Host)
    pub(crate) fn choose(caps: &Capabilities, metering: &Metering) -> Result<Self, Error> {
        if let Some(kind) = unit_autoexposure_kind(
            caps.identity.model(),
            caps.set_window.kind,
            metering.lock_white_balance,
        ) {
            return Ok(Self::Unit(kind));
        }

        // We meter by moving the exposure in the descriptor, so the unit has to
        // offer that as an analog control. `SetWindowFunction` byte 14
        let aic = caps.set_window.aic;
        if !aic.intersects(AnalogControl::EXPOSURE_VALUE | AnalogControl::EXPOSURE_TIME) {
            return Err(Error::Unsupported {
                op: "exposure",
                reason: format!(
                    "this unit runs no AE pass and offers no exposure control, only {aic:?}"
                ),
            });
        }

        Ok(Self::Host(*metering))
    }
}

/// Select a scanner-side exposure pass only when the model implements it.
///
/// The LS-8000 reports bits 5 and 6 in D1h byte 4. Its interface document calls
/// those bits reserved. A complete Nikon Scan trace uses ordinary image passes
/// and host-side exposure decisions. Treating the bits as AE loses every meter
/// image and returns one stale exposure set for all frames.
pub(crate) fn unit_autoexposure_kind(
    model: Option<Model>,
    kinds: ScanKind,
    lock_white_balance: bool,
) -> Option<ScanKind> {
    if matches!(model, Some(Model::Ls8000)) {
        return None;
    }
    if lock_white_balance && kinds.contains(ScanKind::AE_WB) {
        return Some(ScanKind::AE_WB);
    }
    if !lock_white_balance && kinds.contains(ScanKind::AE) {
        return Some(ScanKind::AE);
    }
    None
}

/// The same windows, shrunk to something quick to take and read
///
/// Lowest resolution the unit offers, high speed if it has it, and no
/// multisampling. Anything past that only costs time, and multisampling or
/// multi-line reading would make the pass ask us for post-processing first.
pub(crate) fn prescan_windows(caps: &Capabilities, windows: &[Window]) -> Vec<Window> {
    let dpi = caps.address.x_axis.dpi_range.start;
    let fast = caps.set_window.mode.contains(ScanMode::HIGH_SPEED);

    windows
        .iter()
        .map(|w| {
            let mut w = w.clone();
            // An LS-8000/9000 prescan halves Y and has no averaging (byte 41 is 01h)
            // An LS-50/5000 prescan is 285 dpi with averaging (byte 41 is 80h). The
            // LS-40 prescan is 290 dpi (LS4000.md3 `10034dc8`)
            if caps.identity.is_mf_scanner() {
                w.resolution = (dpi, dpi / 2);
                w.flags.remove(Flags::AVERAGING);
            } else {
                let dpi = caps.address.x_axis.optical_dpi / prescan_divisor(caps);
                w.resolution = (dpi, dpi);
            }
            if fast {
                w.scanning_mode = ScanMode::HIGH_SPEED;
            }
            // Nikon Scan meters off a single reading, and averaging costs a
            // pass what it saves nothing on. Both bytes have to say so
            w.multiple_reading = 0;
            w.scanning_mode.remove(ScanMode::MULTI_READING);
            w
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::window::LENGTH;

    fn window(id: u8, exposure: u32) -> Window {
        let mut w = Window::try_from(&[0u8; LENGTH][..]).expect("a zeroed descriptor");
        w.id = id;
        w.exposure = exposure;
        w
    }

    /// Carrying a metered exposure to the next frame is read then apply
    #[test]
    fn exposures_go_back_where_they_came_from() {
        let metered = [window(1, 50842), window(2, 60000), window(3, 71125)];
        let exposures = Exposures::read(&metered);

        let mut next = [window(1, 0), window(2, 0), window(3, 0)];
        exposures.apply(&mut next);
        assert_eq!(
            next.map(|w| w.exposure),
            metered.map(|w: Window| w.exposure)
        );
    }

    /// Infrared is not metered, so a set that scans it keeps what it came with
    #[test]
    fn a_channel_nothing_was_measured_for_is_left_alone() {
        let exposures = Exposures::read(&[window(1, 50842)]);
        let mut windows = [window(1, 0), window(Channel::Infrared.id(), 93004)];
        exposures.apply(&mut windows);
        assert_eq!(windows[0].exposure, 50842);
        assert_eq!(windows[1].exposure, 93004);
    }

    #[test]
    fn ls8000_reserved_kind_bits_do_not_select_scanner_side_metering() {
        let reported = ScanKind::IMAGE
            | ScanKind::THUMBNAIL
            | ScanKind::SETUP_1
            | ScanKind::HISTOGRAM
            | ScanKind::AE
            | ScanKind::AE_WB;

        assert_eq!(
            unit_autoexposure_kind(Some(Model::Ls8000), reported, false),
            None
        );
        assert_eq!(
            unit_autoexposure_kind(Some(Model::Ls8000), reported, true),
            None
        );
    }

    #[test]
    fn a_real_autoexposure_capability_remains_available_to_other_models() {
        let reported = ScanKind::IMAGE | ScanKind::AE | ScanKind::AE_WB;

        assert_eq!(
            unit_autoexposure_kind(None, reported, false),
            Some(ScanKind::AE)
        );
        assert_eq!(
            unit_autoexposure_kind(None, reported, true),
            Some(ScanKind::AE_WB)
        );
    }

    /// A term set by hand is what gets applied
    #[test]
    fn an_exposure_can_be_written_by_hand() {
        let mut exposures = Exposures::default();
        exposures.set(Channel::Green, 60000);
        assert_eq!(exposures.get(Channel::Green), Some(60000));

        let mut windows = [window(Channel::Green.id(), 0)];
        exposures.apply(&mut windows);
        assert_eq!(windows[0].exposure, 60000);
    }
}
