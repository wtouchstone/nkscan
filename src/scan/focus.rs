//! Deciding where to focus

use crate::{
    error::Error,
    protocol::data::{Boundary, Rect},
    scan::framing::Framing,
    session::Session,
};
use tracing::*;

/// How focusing went
///
/// Not reaching focus is a recovered error, sense key 01h, so the command
/// finished and the lens is wherever it ended up. Worth knowing about, not
/// worth refusing to scan over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focused {
    /// The unit reached focus, or was driven to a position
    Yes,
    /// Autofocus ran and did not converge. It focuses on grain rather than on
    /// the picture, so there is no such thing as a subject too smooth for it
    NotReached,
    /// Nothing was asked of it
    Skipped,
}

/// What to do about focus before a scan
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    /// Let the unit focus on a point of the window, given as a fraction of its
    /// size. `(0.5, 0.5)` is the middle
    ///
    /// `color` needs the unit to offer `Op::ColorAutoFocus`. `None` uses
    /// `Op::AutoFocus` and lets it choose the channel.
    Auto { at: (f32, f32), color: Option<u8> },
    /// Drive the lens to an absolute position, bounded by `Address` bytes 76-79
    At(u16),
    /// Leave the focus wherever it is
    Hold,
}

impl Default for Focus {
    /// The middle of the window. Nikon Scan focuses there and nowhere else,
    /// and grain is what the unit measures, so the picture does not matter
    fn default() -> Self {
        Self::Auto {
            at: (0.5, 0.5),
            color: None,
        }
    }
}

/// The point of `frame` that autofocus is sent to, as `Session::focus_frame` works it out
fn focus_point(frame: Rect, at: (f32, f32)) -> (u32, u32) {
    let along = |origin: u32, end: u32, fraction: f32| {
        origin.saturating_add((end.saturating_sub(origin) as f32 * fraction.clamp(0.0, 1.0)) as u32)
    };
    (
        along(frame.left, frame.right, at.0),
        along(frame.top, frame.bottom, at.1),
    )
}

/// The table with `frame` in place of the entries it overlaps, or else the
/// nearest one, so the table never grows and no two entries overlap. `None` when `frame` already
/// holds the focus point's entry or there is no table to change
fn table_with(table: &Boundary, frame: Rect, point: (u32, u32)) -> Option<Boundary> {
    if table.frames.is_empty() || table.at(point.0, point.1).is_some() {
        return None;
    }
    let overlaps = |f: &Rect| f.top < frame.bottom && frame.top < f.bottom;
    let mut frames: Vec<Rect> = table
        .frames
        .iter()
        .copied()
        .filter(|f| !overlaps(f))
        .collect();
    if frames.len() == table.frames.len() {
        let nearest = (0..frames.len())
            .min_by_key(|&i| frames[i].top.abs_diff(frame.top))
            .expect("a non-empty table");
        frames.remove(nearest);
    }
    frames.push(frame);
    frames.sort_by_key(|f| f.top);
    Some(Boundary { frames })
}

/// nkcolor: on a unit the host sends frame rectangles to (LS-8000 strip
/// holders), autofocus resolves its point against that table and answers a
/// point in no entry with "out of focus" at once, moving nothing. A frame the
/// operator moved past its detected place is such a point. Put the moved frame
/// in the table for the autofocus, then put the table back. Other units take
/// the rectangle as it is; upstream's `frame::at_frame` registers frames on
/// perforation-framed units.
pub(crate) async fn focus_in_table(
    session: &mut Session,
    frame: Rect,
    focus: Focus,
) -> Result<Focused, Error> {
    if Framing::choose(session.capabilities()) != Framing::Thumbnail {
        return session.focus_frame(frame, focus).await;
    }
    let Focus::Auto { at, .. } = focus else {
        return session.focus_frame(frame, focus).await;
    };
    let table = match session.frames().cloned() {
        Some(table) => table,
        None => session.boundaries().await?,
    };
    let Some(moved) = table_with(&table, frame, focus_point(frame, at)) else {
        return session.focus_frame(frame, focus).await;
    };
    info!(
        top = frame.top,
        bottom = frame.bottom,
        "NKSCAN_FOCUS_TABLE the focus point is in no frame, so the moved frame is in the table for this autofocus"
    );
    session.set_boundaries(&moved).await?;
    let outcome = session.focus_frame(frame, focus).await;
    if let Err(e) = session.set_boundaries(&table).await {
        warn!(
            %e,
            "could not put the frame table back after focusing, so a later rectangle may position against the moved one"
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(top: u32, bottom: u32) -> Rect {
        Rect {
            top,
            left: 500,
            bottom,
            right: 9000,
        }
    }

    #[test]
    fn a_frame_moved_past_its_entry_takes_the_place_of_the_entries_it_overlaps() {
        // Reported: LS-8000, 645 strip, a 37 mm offset; autofocus answered
        // out of focus at once because the point was in no table entry
        let table = Boundary {
            frames: vec![rect(1000, 9800), rect(10200, 19000), rect(19400, 28200)],
        };
        // Moved so its middle falls in the gap between the first two entries
        let moved = rect(5600, 14400);
        let point = focus_point(moved, (0.5, 0.5));
        assert_eq!(table.at(point.0, point.1), None);
        let changed = table_with(&table, moved, point).expect("a changed table");
        // Both entries it overlaps give way, so no two entries overlap
        assert_eq!(changed.frames, vec![moved, rect(19400, 28200)]);
        assert_eq!(changed.at(point.0, point.1), Some(moved));
    }

    #[test]
    fn a_frame_past_every_entry_replaces_the_nearest_one() {
        let table = Boundary {
            frames: vec![rect(1000, 9800), rect(10200, 19000)],
        };
        let moved = rect(30000, 38800);
        let point = focus_point(moved, (0.5, 0.5));
        let changed = table_with(&table, moved, point).expect("a changed table");
        assert_eq!(changed.frames, vec![rect(1000, 9800), moved]);
    }

    #[test]
    fn a_focus_point_already_in_an_entry_leaves_the_table_alone() {
        let table = Boundary {
            frames: vec![rect(1000, 9800)],
        };
        let inside = rect(1500, 9000);
        assert_eq!(
            table_with(&table, inside, focus_point(inside, (0.5, 0.5))),
            None
        );
    }

    #[test]
    fn the_default_is_the_middle_of_the_window() {
        assert_eq!(
            Focus::default(),
            Focus::Auto {
                at: (0.5, 0.5),
                color: None
            }
        );
    }
}
