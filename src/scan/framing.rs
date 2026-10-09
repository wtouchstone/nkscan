//! Where the frames are, and how this unit expects us to find out
//!
//! Four mechanisms, picked from what the unit and the loaded adapter advertise.

use super::{
    boundary,
    pass::{Pass, Progress},
    profile::Film,
    thumbnail,
};
use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            address::{AddressingKind, CoordinateBase},
            film::FilmFormat,
            other::DataTypes,
        },
        data::{Boundary, FrameTable, Op, Rect},
        decode::{Image, Samples},
        window::Channel,
    },
    session::Session,
};
use std::ops::ControlFlow;
use tracing::*;

/// How a scan comes to know where each frame sits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `Frames` already carries every frame. A masked adapter knows its own geometry
    Published,
    /// `Frames` carries rectangles with no length.
    /// 2-11-6: after a thumbnail of strip film the host works the boundaries out and sends them as `DataType::Boundary`.
    /// `thumbnail::frames` is what works them out
    Thumbnail,
    /// No rectangles at all. 135 film seeks by counting perforations: read `DataType::Perforation` and write `DataType::Boundary2` back
    Perforation,
    /// No mechanism is offered, so the address page's own geometry is what says
    /// where the frames are
    Address,
}

impl Framing {
    /// Pick the mechanism this unit and adapter use
    pub fn choose(caps: &Capabilities) -> Self {
        let rects = caps
            .address
            .coordinate_base
            .contains(CoordinateBase::FRAME_RECTS);

        if rects {
            // Lengths present means there is nothing left to measure
            if caps.frames.as_ref().is_some_and(|f| f.measured()) {
                return Self::Published;
            }
            if thumbnail::available(caps) {
                return Self::Thumbnail;
            }
            return Self::Address;
        }

        // Perforations are the other way a frame gets found, and only the families that read DataType::Perforation can do it
        if caps
            .features
            .data_types
            .contains(DataTypes::PERFORATION_READ)
        {
            return Self::Perforation;
        }
        Self::Address
    }

    /// Whether discovery takes a thumbnail pass
    ///
    /// With published frames the pass is only a preview, taken where Nikon Scan takes one
    pub fn takes_thumbnail(self, caps: &Capabilities) -> bool {
        match self {
            Self::Thumbnail | Self::Perforation => true,
            Self::Published => {
                thumbnail::available(caps)
                    && thumbnail::exposure_factors(caps, Film::default()).is_some()
            }
            Self::Address => false,
        }
    }
}

/// Refuse a frame the stage cannot step to
///
/// Past `Address`'s Y boundary the stage target comes out behind the home stop,
/// and the mechanism grinds there until a power cycle. A length of zero is not a
/// frame and would tile nothing
pub(crate) fn reachable(caps: &Capabilities, extent: u32) -> Result<(), Error> {
    if extent == 0 {
        return Err(Error::Unsupported {
            op: "frame table",
            reason: "a frame of length 0 is not a frame".into(),
        });
    }
    let limit = caps.address.y_axis.boundary;
    match extent > limit {
        true => Err(Error::Unsupported {
            op: "frame table",
            reason: format!(
                "a frame of {extent} is past the {limit} boundary and would stall the stage"
            ),
        }),
        false => Ok(()),
    }
}

/// The frame table a masked holder publishes, 2-11-6
///
/// Both the stage and autofocus resolve against the `88h` table, and until the
/// host writes one the unit answers with a single whole-sensor rect that makes
/// every frame-kind SET WINDOW drive the holder out and back. A masked holder
/// publishes its frames with lengths in C8h, and this emits one rect per image
/// so the host can write them back as the `88h` table.
///
/// Strip holders publish no lengths, and this returns empty: the host has not
/// measured yet, and the table they need comes from `thumbnail::frames` after a
/// whole-strip pass. A rect longer than the Y boundary stalls the stage, so a
/// placeholder is not something we can invent here
pub fn table(caps: &Capabilities) -> Result<Boundary, Error> {
    let Some(published) = caps.frames.as_ref() else {
        return Ok(Boundary::default());
    };
    let mut frames = Vec::new();

    for image in &published.images {
        // A published length is the adapter's own geometry. Without one, the
        // host owes a measurement and the table comes from `thumbnail::frames`
        let Some(extent) = image.length else {
            continue;
        };
        // A scan window has to be whole readout blocks and has to sit inside
        // the frame this table gives, so the table carries the rounding
        let extent = super::window::whole_blocks(caps, extent);
        reachable(caps, extent)?;
        frames.push(Rect {
            top: image.top,
            left: image.left,
            bottom: image.top + extent,
            right: image.left + image.width,
        });
    }
    Ok(Boundary { frames })
}

/// How many frames the addressable Y range holds, at one boundary each
///
/// More than one only where the window address is a position on the medium
/// rather than the gate, 2-2-2-2 byte 17 bit 0
pub fn frames_on_medium(caps: &Capabilities) -> u32 {
    let y = &caps.address.y_axis;
    let span = y.address_range.last.saturating_sub(y.address_range.start) + 1;
    match y.boundary {
        0 => 1,
        boundary => (span / boundary).max(1),
    }
}

/// Whether the unit takes in the next medium itself
///
/// A slide feeder addresses its gate, and LOAD takes the next slide from the
/// stack. A cartridge addresses the film, 2-2-2-3 byte 17 bit 0, and LOAD takes
/// in the same roll again. Do not use the frame count for this: after the
/// eject, an IA-20 can show a range of one frame
pub fn self_feeding(caps: &Capabilities) -> bool {
    caps.features.execute.supports(Op::Load)
        && !caps
            .address
            .addressing_kind
            .contains(AddressingKind::ADDR_MEDIUM)
}

/// Whether the unit has LOAD but keeps the same film behind the gate, as the
/// IA-20 does
pub fn cartridge(caps: &Capabilities) -> bool {
    caps.features.execute.supports(Op::Load) && !self_feeding(caps)
}

/// The frames a [`Framing::Address`] unit never describes on its own: one rect
/// per frame the Y range holds, the whole opening wide
pub fn frames(caps: &Capabilities) -> Result<Boundary, Error> {
    let (x, y) = (&caps.address.x_axis, &caps.address.y_axis);
    let extent = super::window::reachable_blocks(caps, y.boundary);
    reachable(caps, extent)?;

    let count = frames_on_medium(caps);
    debug!(
        count,
        pitch = y.boundary,
        extent,
        "framed from the address page"
    );
    let frames = (0..count)
        .map(|n| {
            let top = y.address_range.start + n * y.boundary;
            Rect {
                top,
                left: x.address_range.start,
                bottom: top + extent,
                right: x.address_range.start + x.boundary,
            }
        })
        .collect();
    Ok(Boundary { frames })
}

/// Put `frame` in the unit's frame table before a pass over it
///
/// Only for a unit that positions the film by that table. Such a unit reads a
/// rectangle that is not in the table as an offset into the frame that holds
/// its top, and sends black data past the end of that frame. The unit accepts
/// the table only as a whole, so this starts from the table the session knows,
/// or from the unit's table if the session knows none. A top already in the
/// table is left alone.
///
/// Answers whether a table was written. The caller must put the measured table
/// back, because [`BoundaryType2::register`](crate::protocol::data::BoundaryType2::register)
/// replaces an entry and does not add one
pub async fn register(session: &mut Session, frame: Rect) -> Result<bool, Error> {
    if Framing::choose(session.capabilities()) != Framing::Perforation {
        return Ok(false);
    }
    let mut table = match session.frames_type2() {
        Some(table) => table.clone(),
        // Nothing measured this strip, so ask the unit what it is holding
        None => match session.boundaries_type2().await {
            Ok(table) => table,
            Err(e) => {
                debug!(%e, "no frame table to register against");
                return Ok(false);
            }
        },
    };
    if table.frames.is_empty() || table.frames.iter().any(|f| f.top == frame.top) {
        return Ok(false);
    }

    // Use a record of the pass: a calculated record needs a length that only the pass measures
    let perfs = session.read_perforations().await?;
    let optical = session.capabilities().address.y_axis.optical_dpi;
    let entry = boundary::perforation::record_at(&perfs, optical, frame.top).ok_or_else(|| {
        Error::Unsupported {
            op: "frame registration",
            reason: format!(
                "no perforation reading of the last pass is at or before {}",
                frame.top
            ),
        }
    })?;
    table.register(entry);
    debug!(top = frame.top, ?entry, "registered a frame off the table");
    session.set_boundaries_type2_for_pass(&table).await?;
    Ok(true)
}

/// The discovered frame table, however that happened.
/// If this required a thumbnail pass, this holds that thumbnail.
pub struct Discovery {
    pub table: FrameTable,
    pub frames: Vec<Rect>,
    pub thumbnail: Option<Pass>,
    /// The feed addresses of one [`thumbnail`](Self::thumbnail) column; `None` if the columns are not evenly spaced
    pub line_pitch: Option<thumbnail::LinePitch>,
    /// The feed address of each [`thumbnail`](Self::thumbnail) column. The addresses never decrease.
    /// `None` without a thumbnail, or where no thumbnail line has a perforation record
    pub column_addresses: Option<Vec<u32>>,
    /// The X address of each [`thumbnail`](Self::thumbnail) row. The addresses decrease.
    /// `None` without a thumbnail
    pub row_addresses: Option<Vec<u32>>,
    /// The [`contrast`](crate::scan::strip::Strip::contrast) of a `strip::find` fit that found frames
    pub contrast: Option<f32>,
    /// The film base density off the thumbnail, for [`crate::scan::meter::Metering::base_density`].
    /// `None` without a thumbnail, and on a unit with holders
    pub base_density: Option<[f64; 3]>,
}

/// The thumbnail's film base density at the unit's start-up exposures
///
/// `None` on a unit with holders. LS9000.md3 has no base density sampler: its
/// metering (`1003e040`) reads the value from the color capabilities 102h to
/// 104h and uses the default when it is out of range. LS8000.md3 has a sampler
/// (`100844b0`), but we use the newer module on the LS-8000 too.
async fn base_density(session: &mut Session, image: &Image<'_>, film: Film) -> Option<[f64; 3]> {
    if session.capabilities().address.holder_id.is_some() {
        return None;
    }
    let factors = thumbnail::exposure_factors(session.capabilities(), film).unwrap_or([1.0; 3]);
    let mut ratio = [1.0; 3];
    for (c, channel) in [Channel::Red, Channel::Green, Channel::Blue]
        .into_iter()
        .enumerate()
    {
        let base = session.white_balance(channel).await.ok()?;
        let thumbnail = super::meter::scale(base, factors[c]);
        ratio[c] = f64::from(base) / f64::from(thumbnail.max(1));
    }
    thumbnail::base_density(image, ratio)
}

/// Find every frame on whatever is loaded, driving whatever pass the chosen mechanism needs
pub async fn discover(
    session: &mut Session,
    format: Option<FilmFormat>,
    film: Film,
    samples: &mut Samples,
) -> Result<Discovery, Error> {
    discover_with(
        session,
        format,
        film,
        samples,
        |_| ControlFlow::Continue(()),
    )
    .await
}

/// The same as [`discover`], letting `on` cancel a thumbnail pass by returning `Break`
///
/// `format` is only needed by [`Framing::Thumbnail`] and [`Framing::Perforation`], and even
/// there only where [`FilmFormat::resolve`] cannot work it out from the loaded holder.
/// `film` sets the thumbnail exposure, see [`thumbnail::exposure_factors`].
/// `samples` is scratch, left holding the thumbnail where [`Discovery::thumbnail`] is `Some`
pub async fn discover_with(
    session: &mut Session,
    format: Option<FilmFormat>,
    film: Film,
    samples: &mut Samples,
    on: impl FnMut(Progress) -> ControlFlow<()>,
) -> Result<Discovery, Error> {
    // Only the two mechanisms below need it, but both need it before they move
    let need_format = |session: &Session| FilmFormat::resolve(format, session.capabilities());

    let mechanism = Framing::choose(session.capabilities());
    debug!(?mechanism, "frame discovery");

    match mechanism {
        Framing::Published => {
            let boundary = table(session.capabilities())?;
            let found = boundary.frames.clone();
            info!(frames = found.len(), "published frames");
            let (thumbnail, line_pitch, column_addresses, row_addresses, base_density) =
                if mechanism.takes_thumbnail(session.capabilities()) {
                    let pass = session.scan_thumbnail_with(samples, film, on).await?;
                    let base_density = base_density(session, &pass.image(samples)?, film).await;
                    let caps = session.capabilities();
                    let pitch = thumbnail::LinePitch::of(&pass);
                    let columns = pitch.column_addresses(caps, pass.cols);
                    let rows = thumbnail::row_addresses(caps, &pass);
                    (
                        Some(pass),
                        Some(pitch),
                        Some(columns),
                        Some(rows),
                        base_density,
                    )
                } else {
                    (None, None, None, None, None)
                };
            Ok(Discovery {
                table: FrameTable::Boundary(boundary),
                frames: found,
                thumbnail,
                line_pitch,
                column_addresses,
                row_addresses,
                contrast: None,
                base_density,
            })
        }
        Framing::Thumbnail => {
            let format = need_format(session)?;
            let pass = session.scan_thumbnail_with(samples, film, on).await?;
            debug!(
                rows = pass.rows,
                cols = pass.cols,
                complete = pass.complete,
                "thumbnail"
            );
            let base_density = base_density(session, &pass.image(samples)?, film).await;
            let (measured, pitch, contrast) =
                thumbnail::frames(session.capabilities(), &pass, samples, format, film)?;
            // Nothing measured is nothing to tell the unit, and it refuses an
            // empty record. The caller gets the pass either way, which is the
            // only evidence of why the strip measured empty
            if !measured.frames.is_empty() {
                session.set_boundaries(&measured).await?;
            }
            let found = measured.frames.clone();
            info!(frames = found.len(), "detected frames");
            Ok(Discovery {
                table: FrameTable::Boundary(measured),
                frames: found,
                column_addresses: Some(pitch.column_addresses(session.capabilities(), pass.cols)),
                row_addresses: Some(thumbnail::row_addresses(session.capabilities(), &pass)),
                thumbnail: Some(pass),
                line_pitch: Some(pitch),
                contrast,
                base_density,
            })
        }
        Framing::Address => {
            let boundary = frames(session.capabilities())?;
            let found = boundary.frames.clone();
            Ok(Discovery {
                table: FrameTable::Boundary(boundary),
                frames: found,
                thumbnail: None,
                line_pitch: None,
                column_addresses: None,
                row_addresses: None,
                contrast: None,
                base_density: None,
            })
        }
        Framing::Perforation => {
            let format = need_format(session)?;
            // Discard whatever a previous strip left behind
            let _ = session.read_perforations().await?;
            let _ = session.read_boundaries_type2().await;

            let mut pass = session.scan_thumbnail_with(samples, film, on).await?;
            debug!(
                rows = pass.rows,
                cols = pass.cols,
                complete = pass.complete,
                "thumbnail"
            );
            let perfs = session.read_perforations().await?;
            // Measure before the resample
            let base_density = base_density(session, &pass.image(samples)?, film).await;
            let caps = session.capabilities();
            let optical = caps.address.y_axis.optical_dpi;
            let length = super::window::reachable_blocks(caps, format.height_dots(optical));
            reachable(caps, length)?;
            info!(?format, length, "frame length");

            // Nikon Scan's search, for the number of frames the unit holds
            let line_pitch = pass.layout.line_pitch;
            let held = usize::from(caps.address.loaded_frames);
            let starts = boundary::perforation::frames(
                &pass.image(samples)?,
                &perfs,
                optical,
                line_pitch,
                film,
                held,
            )
            .unwrap_or_default();
            let kept = boundary::perforation::resample(&perfs, optical, line_pitch);
            let measured = boundary::perforation::table(
                &perfs,
                optical,
                &kept,
                starts.iter().map(|&s| s as i64),
            );
            // Return the resampled thumbnail, where a frame's line is its column
            let column_addresses = (!kept.is_empty()).then(|| {
                pass.keep_columns(samples, &kept);
                boundary::perforation::addresses(&perfs, optical, &kept)
            });
            if !measured.frames.is_empty() {
                session.set_boundaries_type2(&measured).await?;
            }

            let caps = session.capabilities();
            let (x_start, x_boundary) = (
                caps.address.x_axis.address_range.start,
                caps.address.x_axis.boundary,
            );
            let found = measured
                .frames
                .iter()
                .map(|f| f.rect(x_start, x_boundary, length))
                .collect::<Vec<_>>();
            info!(frames = found.len(), "detected frames");
            Ok(Discovery {
                table: FrameTable::BoundaryType2(measured),
                frames: found,
                row_addresses: Some(thumbnail::row_addresses(caps, &pass)),
                thumbnail: Some(pass),
                // The resampled columns are not evenly spaced
                line_pitch: None,
                column_addresses,
                contrast: None,
                base_density,
            })
        }
    }
}

/// Expand an addressed frame for a full scan while keeping it inside the
/// scanner's address range and maximum window size.
///
/// A perforation-framed 135 frame is already the complete physical gate, so
/// overscan is refused there.
pub fn overscan_rect(caps: &Capabilities, frame: Rect, margin: u32) -> Result<Rect, Error> {
    if margin == 0 {
        return Ok(frame);
    }
    if Framing::choose(caps) == Framing::Perforation {
        return Err(Error::Unsupported {
            op: "frame overscan",
            reason: "a perforation-framed frame is already the complete physical gate".into(),
        });
    }

    let expand = |start: u32, end: u32, axis: &crate::protocol::caps::address::Axis| {
        let extent = end.saturating_sub(start);
        // Address reports its last coordinate as inclusive. Rect uses a
        // half-open end. Convert the axis once before comparing their extents.
        // A full-width LS-8000 frame is 518..9482 over an axis of 518..=9481.
        let range_end = axis.address_range.last.saturating_add(1);
        let range_extent = range_end.saturating_sub(axis.address_range.start);
        let maximum = if axis.boundary == 0 {
            range_extent
        } else {
            axis.boundary.min(range_extent)
        };
        let target = extent
            .saturating_add(margin.saturating_mul(2))
            .min(maximum.max(extent));
        let extra = target.saturating_sub(extent);
        let preferred = start.saturating_sub(extra / 2);
        let minimum_start = axis.address_range.start.max(end.saturating_sub(target));
        let maximum_start = start.min(range_end.saturating_sub(target));
        let expanded_start = preferred.clamp(minimum_start, maximum_start);
        (expanded_start, expanded_start.saturating_add(target))
    };

    let (top, bottom) = expand(frame.top, frame.bottom, &caps.address.y_axis);
    let (left, right) = expand(frame.left, frame.right, &caps.address.x_axis);
    Ok(Rect {
        top,
        left,
        bottom,
        right,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::caps::other::DataTypes, scan::window::tests::caps};

    #[test]
    fn overscan_expands_an_addressed_frame_without_changing_its_center() {
        let plain = caps();
        let frame = Rect {
            top: 3000,
            left: 1000,
            bottom: 7000,
            right: 5000,
        };

        assert_eq!(
            overscan_rect(&plain, frame, 200).expect("overscan"),
            Rect {
                top: 2800,
                left: 800,
                bottom: 7200,
                right: 5200,
            }
        );
        assert_eq!(overscan_rect(&plain, frame, 0).expect("no overscan"), frame);
    }

    #[test]
    fn overscan_stays_inside_the_address_range() {
        let plain = caps();
        let frame = Rect {
            top: plain.address.y_axis.address_range.start,
            left: plain.address.x_axis.address_range.start,
            bottom: plain.address.y_axis.address_range.start + 4000,
            right: plain.address.x_axis.address_range.start + 4000,
        };
        let expanded = overscan_rect(&plain, frame, 200).expect("overscan");

        assert_eq!(expanded.top, plain.address.y_axis.address_range.start);
        assert_eq!(expanded.left, plain.address.x_axis.address_range.start);
        assert!(expanded.bottom >= frame.bottom);
        assert!(expanded.right >= frame.right);
        assert!(expanded.bottom - expanded.top <= plain.address.y_axis.boundary);
        assert!(expanded.right - expanded.left <= plain.address.x_axis.boundary);
    }

    /// The LS-8000 holder reports an inclusive last X coordinate. Its frame
    /// rectangle uses the same physical edge as a half-open end. Treating both
    /// values as half-open made the overscan start bounds cross and panicked.
    #[test]
    fn overscan_accepts_a_full_width_ls8000_frame() {
        let mut plain = caps();
        plain.address.x_axis.address_range = (518..=9481).into();
        plain.address.x_axis.boundary = 8964;
        let frame = Rect {
            top: 3000,
            left: 518,
            bottom: 7000,
            right: 9482,
        };

        assert_eq!(
            overscan_rect(&plain, frame, 200).expect("overscan"),
            Rect {
                top: 2800,
                left: 518,
                bottom: 7200,
                right: 9482,
            }
        );
    }

    #[test]
    fn perforation_scanners_reject_a_second_overscan_rule() {
        let mut perforated = caps();
        perforated.features.data_types |= DataTypes::PERFORATION_READ;
        let frame = Rect {
            top: 2236,
            left: 518,
            bottom: 2236 + 5669,
            right: 518 + 3946,
        };

        let error = overscan_rect(&perforated, frame, 100).expect_err("locked gate path");
        assert!(format!("{error}").contains("complete physical gate"));
    }

    /// Published frames take a thumbnail as a preview where Nikon Scan takes one
    #[test]
    fn published_frames_take_a_thumbnail_where_nikon_takes_one() {
        let mut caps = caps();
        caps.set_window.kind |= crate::protocol::caps::set_window::ScanKind::THUMBNAIL;
        caps.address.thumbnail_resolution = (83u16..=83u16).into();
        // FH-835S on an LS-9000
        caps.address.holder_id = Some(0x15);
        assert!(Framing::Published.takes_thumbnail(&caps));
        // An LS-50 mount adapter, where Nikon Scan takes none
        caps.address.holder_id = None;
        caps.adapter = Some("Mount".into());
        assert!(!Framing::Published.takes_thumbnail(&caps));
        assert!(!Framing::Address.takes_thumbnail(&caps));
    }

    /// Byte 17 and the LOAD bit for each adapter, 2-2-2-3 and 2-2-2-5. Only
    /// the slide feeder takes the next medium from a stack. An IA-20 with a
    /// range of one frame is not a feeder
    #[test]
    fn only_a_slide_feeder_feeds_itself() {
        use crate::protocol::caps::other::ExecuteOps;
        let mut caps = caps();
        let adapter = |caps: &mut Capabilities, kind: u8, ops: &[Op]| {
            caps.address.addressing_kind = AddressingKind::from_bits_truncate(kind);
            caps.features.execute = ExecuteOps::of(ops);
        };

        // SF-210
        adapter(&mut caps, 0x32, &[Op::Unload, Op::Load]);
        assert!(self_feeding(&caps));
        assert!(!cartridge(&caps));

        // IA-20, with a range of one frame
        adapter(&mut caps, 0x35, &[Op::Unload, Op::Load]);
        assert_eq!(frames_on_medium(&caps), 1);
        assert!(!self_feeding(&caps));
        assert!(cartridge(&caps));

        // SA-21/SA-30, then MA-21
        adapter(&mut caps, 0x31, &[Op::Unload]);
        assert!(!self_feeding(&caps) && !cartridge(&caps));
        adapter(&mut caps, 0x22, &[]);
        assert!(!self_feeding(&caps) && !cartridge(&caps));
    }

    /// A cartridge addresses the film itself, so its range runs the length of
    /// the roll at one frame per boundary. These are an IA-20's
    #[test]
    fn a_medium_longer_than_the_gate_is_framed_along_its_range() {
        let mut caps = caps();
        assert_eq!(frames(&caps).expect("frames").frames.len(), 1);

        caps.address.y_axis.address_range = (0..=111324).into();
        caps.address.y_axis.boundary = 4453;
        caps.address.line_gap = 0;

        let found = frames(&caps).expect("frames").frames;
        assert_eq!(found.len(), 25);
        assert_eq!(found[0].bottom - found[0].top, 4453);
        assert_eq!(found[24].top, 24 * 4453);
    }
}
