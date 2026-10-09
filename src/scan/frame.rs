//! Scanning one frame: focus, meter, take the pass, and optionally clean it

use crate::{
    dust,
    error::Error,
    protocol::{data::Rect, decode::Samples, window::Window},
    scan::{
        autoexpose::{Exposures, MeteringEvidence},
        clean::clean_frame,
        focus::{Focus, Focused},
        framing,
        meter::{self, Metering},
        pass::{Pass, Progress},
        window::Recipe,
    },
    session::Session,
};
use std::{ops::ControlFlow, time::Duration};
use tracing::*;

/// Long enough for a full-resolution pass over the largest frame
const SCAN_TIMEOUT: Duration = Duration::from_secs(1800);

/// Which pass `scan_frame_with`'s progress belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Metering, and which pass of it, counting from one
    Meter(usize),
    /// The scan pass itself
    Scan,
}

/// What to do beyond taking the pass
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// Reuse an exposure already decided; `None` meters this frame fresh
    pub exposures: Option<&'a Exposures>,
    /// How to meter the frame; used only if `exposures` is `None`
    pub metering: Metering,
    /// Run dust removal over the result in place
    pub clean: bool,
    /// How to focus before the pass
    pub focus: Focus,
    /// nkcolor: autofocus again after metering, just before the full pass
    pub refocus_before_scan: bool,
    /// nkcolor: scanner-address units added around the full pass on a unit
    /// that addresses the film with the window. Focus and metering use `frame`
    pub overscan: u32,
    /// nkcolor: meter as a color negative with the rule a Nikon Scan capture
    /// verifies on this unit, whatever `metering.film` says. Refused on a unit
    /// [`meter::nikon_negative_supported`] does not name
    pub nikon_negative_meter: bool,
    /// nkcolor: normal or fine dust reconstruction
    pub clean_quality: dust::Quality,
}

/// What one frame's scan produced
pub struct Scanned {
    /// The pass over the rectangle that was asked for, which is that rectangle
    /// at both ends
    pub pass: Pass,
    /// What the frame was exposed at
    pub exposures: Exposures,
    /// Pixels dust removal rebuilt, where asked for
    pub cleaned: Option<usize>,
    /// The focus result
    pub focused: Focused,
    /// The lens position during the pass. `None` if the unit does not report it
    pub focus_position: Option<u16>,
    /// nkcolor: the SET WINDOW descriptors of the full pass
    pub windows: Vec<Window>,
    /// nkcolor: every meter image and its exposure. `None` when the caller
    /// supplied the exposures
    pub metering: Option<MeteringEvidence>,
}

/// Focus, meter, and take the pass over `frame`
pub async fn scan_frame(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    options: Options<'_>,
    samples: &mut Samples,
) -> Result<Scanned, Error> {
    scan_frame_with(session, recipe, frame, options, samples, |_, _| {
        ControlFlow::Continue(())
    })
    .await
}

/// The same as [`scan_frame`], with `on` told which [`Phase`] is running and able to cancel it by returning `Break`
pub async fn scan_frame_with(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    options: Options<'_>,
    samples: &mut Samples,
    mut on: impl FnMut(Phase, Progress) -> ControlFlow<()>,
) -> Result<Scanned, Error> {
    let mut metering = options.metering;
    if options.nikon_negative_meter {
        let caps = session.capabilities();
        if !meter::nikon_negative_supported(caps.identity.model()) {
            return Err(Error::Unsupported {
                op: "Nikon negative metering",
                reason: format!(
                    "no Nikon Scan capture verifies the negative meter on {}",
                    caps.identity.product
                ),
            });
        }
        metering.film = crate::scan::profile::Film::Negative;
        metering.lock_white_balance = false;
    }
    let scan_rect = framing::overscan_rect(session.capabilities(), frame, options.overscan)?;

    let (pass, exposures, metering, windows, focused, focus_position) =
        at_frame(session, frame, async |session| {
            let (focused, focus_position) = focus_on(session, frame, options.focus).await?;

            let (exposures, evidence) = match options.exposures {
                Some(locked) => (locked.clone(), None),
                None => {
                    let metered = session
                        .autoexpose_frame_evidence_with(frame, recipe, &metering, |pass, p| {
                            on(Phase::Meter(pass), p)
                        })
                        .await?;
                    (metered.exposures, Some(metered.evidence))
                }
            };

            if options.refocus_before_scan && matches!(options.focus, Focus::Auto { .. }) {
                focus_on(session, frame, options.focus).await?;
            }

            let mut windows = recipe.windows(session.capabilities(), scan_rect)?;
            exposures.apply(&mut windows);

            let pass = session
                .scan_pass_with(&windows, SCAN_TIMEOUT, samples, |p| on(Phase::Scan, p))
                .await?;
            Ok((pass, exposures, evidence, windows, focused, focus_position))
        })
        .await?;

    // nkcolor publishes nothing from a pass that ended early: the unused part
    // of the sample buffer is padding, not film
    if !pass.complete {
        return Err(Error::IncompleteScan {
            blocks: pass.blocks,
        });
    }
    samples.to_full_scale(pass.layout.bits_per_sample);

    let cleaned = options
        .clean
        .then(|| {
            clean_frame(
                samples,
                &pass,
                session.capabilities().identity.model(),
                options.clean_quality,
            )
        })
        .transpose()?;

    Ok(Scanned {
        pass,
        exposures,
        cleaned,
        focused,
        focus_position,
        windows,
        metering,
    })
}

/// Meter `frame` without taking its pass
///
/// The exposures are the ones [`scan_frame_with`] meters for itself, so handing
/// them to a later scan through [`Options::exposures`] exposes that frame the
/// way this one would have been. `on` is told which metering pass is running,
/// counting from one, and can cancel it by returning `Break`
pub async fn meter_frame_with(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    metering: &Metering,
    on: impl FnMut(usize, Progress) -> ControlFlow<()>,
) -> Result<Exposures, Error> {
    at_frame(session, frame, async |session| {
        session
            .autoexpose_frame_with(frame, recipe, metering, on)
            .await
    })
    .await
}

/// Focus on `frame` without metering or scanning it
///
/// Returns the focus result and the lens position. The position is `None` if
/// the unit does not report it. To scan at this focus, give the scan
/// [`Focus::Hold`]
pub async fn focus_frame(
    session: &mut Session,
    frame: Rect,
    focus: Focus,
) -> Result<(Focused, Option<u16>), Error> {
    at_frame(session, frame, async |session| {
        focus_on(session, frame, focus).await
    })
    .await
}

/// Focus on `frame`, then read the lens position. The frame must already be
/// in the unit's frame table
async fn focus_on(
    session: &mut Session,
    frame: Rect,
    focus: Focus,
) -> Result<(Focused, Option<u16>), Error> {
    let focused = crate::scan::focus::focus_in_table(session, frame, focus).await?;
    let position = session.focus_position().await.ok();
    if let Some(position) = position {
        info!(position, "focused at");
    }
    Ok((focused, position))
}

/// Run `f` with `frame` reachable, and leave the unit's frame table as measured
async fn at_frame<T>(
    session: &mut Session,
    frame: Rect,
    f: impl AsyncFnOnce(&mut Session) -> Result<T, Error>,
) -> Result<T, Error> {
    // Where the unit positions the film by its own frame table, the window's Y
    // selects a table entry and is an offset into that frame, not an address.
    // The rectangle must be in the table before the window can reach it
    let registered = framing::register(session, frame).await?;

    let taken = f(session).await;

    // Put the measured table back, whether or not `f` got that far, so the
    // next rectangle of this session starts from it. Written and not
    // registered: `register` starts from the measured table, finds the entry it
    // wants already there, and leaves this pass's table in the unit
    if registered
        && let Some(measured) = session.frames_type2().cloned()
        && let Err(e) = session.set_boundaries_type2_for_pass(&measured).await
    {
        warn!(
            %e,
            "could not put the measured frame table back, so a later rectangle can position against this one"
        );
    }

    taken
}
