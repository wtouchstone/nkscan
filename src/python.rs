//! Python bindings, gated behind the `python` feature
//!
//! A thin skin over [`session`](crate::session) and [`scan::frame`](crate::scan::frame):
//! converts arguments, hands the decoded planes to numpy without copying
//! them, and releases the interpreter for however long a call blocks on the
//! scanner.

use crate::{
    device::{self, Device as RustDevice},
    dust,
    error::Error,
    protocol::{
        caps::{Capabilities as RustCapabilities, film::FilmFormat, set_window::ScanMode},
        data::{Op, Rect},
        decode::Samples,
        image::Layout,
        model::Model,
        window::Channel,
    },
    scan::{
        autoexpose::{Exposures, unit_autoexposure_kind},
        clean::clean_frame,
        focus::{Focus, Focused},
        frame::{self, Phase},
        framing::{self, Framing},
        meter::Metering,
        pass::{Pass, Progress},
        profile::{self, Film},
        thumbnail,
        window::{MAX_SAMPLES, Recipe},
    },
    session::{Session as RustSession, block_on},
    transport::Transport,
};
use numpy::{IntoPyArray, PyArray2, PyArray3, PyArrayMethods, PyReadonlyArray2, PyReadonlyArray3};
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
};
use pyo3_stub_gen::{create_exception, define_stub_info_gatherer, derive::*};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs::OpenOptions,
    io,
    ops::ControlFlow,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
use tracing_subscriber::{
    EnvFilter, Registry,
    filter::Filtered,
    fmt::{
        self,
        format::{DefaultFields, Format},
        writer::BoxMakeWriter,
    },
    prelude::*,
    reload,
};

// ----- errors -----

create_exception!(
    nkscan,
    ScannerError,
    PyRuntimeError,
    "Base for every error this crate raises"
);
create_exception!(nkscan, TransientError, ScannerError, "Worth retrying");
create_exception!(
    nkscan,
    TransportError,
    TransientError,
    "The link to the scanner failed"
);
create_exception!(
    nkscan,
    DeviceBusy,
    TransientError,
    "Something else has the scanner"
);
create_exception!(nkscan, DeviceNotFound, ScannerError, "No such scanner");
create_exception!(
    nkscan,
    MediaError,
    ScannerError,
    "Something a person has to go fix"
);
create_exception!(
    nkscan,
    UnsupportedError,
    ScannerError,
    "This unit or adapter cannot do that. Carries `.op` and `.reason`"
);
create_exception!(
    nkscan,
    ScanCancelled,
    ScannerError,
    "A progress callback returned False"
);

impl From<Error> for PyErr {
    fn from(error: Error) -> Self {
        match error {
            Error::Transport(e) => TransportError::new_err(e.to_string()),
            Error::Busy(c) => DeviceBusy::new_err(c.to_string()),
            Error::Media(i) => MediaError::new_err(i.to_string()),
            Error::NotFound => DeviceNotFound::new_err("no such scanner"),
            Error::Unsupported { op, reason } => {
                let err = UnsupportedError::new_err(format!("{op}: {reason}"));
                Python::attach(|py| {
                    let _ = err.value(py).setattr("op", op);
                    let _ = err.value(py).setattr("reason", &reason);
                });
                err
            }
            Error::Cancelled => ScanCancelled::new_err("scan cancelled"),
            Error::IncompleteScan { blocks } => {
                ScannerError::new_err(Error::IncompleteScan { blocks }.to_string())
            }
            Error::Device(fault) => ScannerError::new_err(fault.to_string()),
        }
    }
}

fn closed() -> PyErr {
    ScannerError::new_err("session is closed")
}

// ----- device discovery -----

/// A scanner this library found, and can open
#[gen_stub_pyclass]
#[pyclass(name = "Device", frozen, module = "nkscan")]
pub struct PyDevice(RustDevice);

#[gen_stub_pymethods]
#[pymethods]
impl PyDevice {
    /// Where it is
    #[getter]
    fn location(&self) -> String {
        self.0.attach.to_string()
    }

    /// What to show a person
    #[getter]
    fn name(&self) -> String {
        self.0.name()
    }

    fn __repr__(&self) -> String {
        format!("Device({:?})", self.location())
    }
}

/// Every scanner this library thinks it can drive
#[gen_stub_pyfunction]
#[pyfunction]
fn list_devices() -> Vec<PyDevice> {
    block_on(device::list()).into_iter().map(PyDevice).collect()
}

// ----- logging -----

/// The log layer that `init_logging` installs, with its filter
type LogLayer =
    Filtered<fmt::Layer<Registry, DefaultFields, Format, BoxMakeWriter>, EnvFilter, Registry>;

/// The handle that lets a later `init_logging` change the level and the destination
static LOGGING: OnceLock<reload::Handle<LogLayer, Registry>> = OnceLock::new();

/// Send the crate's `tracing` diagnostics to stderr, or append them to `path`.
///
/// The extension installs no subscriber on its own, so without calling this the
/// `debug!`/`trace!`/`warn!` calls throughout `session`, `scan`, `transport`, and
/// `protocol` go nowhere when `nkscan` is used as a library. `level` sets the
/// default (`"info"` if omitted, `"off"` to stop); `RUST_LOG` always overrides it
/// and can target individual modules the way it does for the CLI, e.g.
/// `nkscan::cdb=trace`. A later call changes the level and the destination.
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (level=None, path=None))]
fn init_logging(level: Option<&str>, path: Option<PathBuf>) -> PyResult<()> {
    let level = level.unwrap_or("info");
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::try_new(format!("{level},nusb=warn"))
            .map_err(|e| PyValueError::new_err(format!("log level '{level}': {e}")))?,
    };
    let writer = match path {
        Some(path) => BoxMakeWriter::new(Mutex::new(
            OpenOptions::new().create(true).append(true).open(path)?,
        )),
        None => BoxMakeWriter::new(io::stderr),
    };
    let handle = LOGGING.get_or_init(|| {
        let layer = fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(BoxMakeWriter::new(io::sink))
            .with_filter(EnvFilter::new("off"));
        let (layer, handle) = reload::Layer::new(layer);
        let _ = tracing_subscriber::registry().with(layer).try_init();
        handle
    });
    handle
        .modify(|layer| {
            *layer.filter_mut() = filter;
            *layer.inner_mut().writer_mut() = writer;
        })
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

// ----- nkcolor: saved-scan cleaning -----

/// Remove infrared-detected defects from a saved RGB image.
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (rgb, infrared, model, dpi=4000, quality="normal"))]
fn clean_rgb_ir(
    py: Python<'_>,
    rgb: PyReadonlyArray3<'_, u16>,
    infrared: PyReadonlyArray2<'_, u16>,
    model: &str,
    dpi: u32,
    quality: &str,
) -> PyResult<(Py<PyArray3<u16>>, usize)> {
    let rgb = rgb.as_array();
    let infrared = infrared.as_array();
    let shape = rgb.shape();
    if shape.len() != 3 || shape[2] != 3 {
        return Err(PyValueError::new_err(
            "rgb must be an H by W by 3 uint16 array",
        ));
    }
    let rows = shape[0];
    let cols = shape[1];
    if infrared.shape() != [rows, cols] {
        return Err(PyValueError::new_err(
            "infrared must match the RGB height and width",
        ));
    }
    if dpi == 0 {
        return Err(PyValueError::new_err("dpi must be greater than zero"));
    }
    let unit = Model::from_product(model)
        .ok_or_else(|| PyValueError::new_err(format!("unknown scanner model {model:?}")))?;
    let quality = match quality.to_ascii_lowercase().as_str() {
        "normal" => dust::Quality::Normal,
        "fine" => dust::Quality::Fine,
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown dust quality {other:?}"
            )));
        }
    };

    let count = rows
        .checked_mul(cols)
        .ok_or_else(|| PyValueError::new_err("the image dimensions are too large"))?;
    let mut colors: Vec<Vec<u16>> = (0..3).map(|_| Vec::with_capacity(count)).collect();
    let mut infrared_plane = Vec::with_capacity(count);
    for row in 0..rows {
        for col in 0..cols {
            colors[0].push(rgb[[row, col, 0]]);
            colors[1].push(rgb[[row, col, 1]]);
            colors[2].push(rgb[[row, col, 2]]);
            infrared_plane.push(infrared[[row, col]]);
        }
    }

    let (chunky, rebuilt) = py.detach(move || -> Result<(Vec<u16>, usize), Error> {
        let mut layout = Layout::single_line(cols as u32, rows as u32, vec![1, 2, 3]);
        layout.dpi = dpi;
        let pass = Pass {
            layout,
            cooperation: Vec::new(),
            complete: true,
            blocks: 1,
            rows,
            cols,
        };
        let mut samples = Samples {
            colors,
            ir: Some(infrared_plane),
        };
        let rebuilt = clean_frame(&mut samples, &pass, Some(unit), quality)?;
        let mut chunky = Vec::with_capacity(count * 3);
        for index in 0..count {
            chunky.push(samples.colors[0][index]);
            chunky.push(samples.colors[1][index]);
            chunky.push(samples.colors[2][index]);
        }
        Ok((chunky, rebuilt))
    })?;
    let output = chunky.into_pyarray(py).reshape([rows, cols, 3])?.unbind();
    Ok((output, rebuilt))
}

// ----- capabilities -----

/// What the scanner says it can do, projected down to what a caller needs to
/// place a scan. Grows as bindings turn up a real need for more of it
#[gen_stub_pyclass]
#[pyclass(name = "Capabilities", frozen, get_all, module = "nkscan")]
pub struct PyCapabilities {
    vendor: String,
    product: String,
    revision: String,
    model: Option<String>,
    x_dpi_range: (u16, u16),
    y_dpi_range: (u16, u16),
    optical_dpi: u16,
    /// Full scanner-address width across the sensor bar
    x_boundary: u32,
    /// Full scanner-address length along the film transport
    y_boundary: u32,

    // What follows is for a caller deciding which controls to offer. Every
    // field is read off the unit's own pages, so a control gated on one is
    // gated on what this scanner will actually accept
    /// Frames the loaded holder can hold, 0 where the unit publishes no table
    max_frames: u8,
    /// The thumbnail pass's resolution range, empty where there is no thumbnail
    thumbnail_dpi: (u16, u16),
    /// Focus positions the unit accepts
    focus_range: (u16, u16),
    /// nkcolor: exposure values the unit accepts, in the scanner's native units
    exposure_range: (u32, u32),
    /// Most readings of one line a pass may ask for. 1 where the unit reads a
    /// line one time, so a multi-sample control can be hidden
    max_samples: u8,
    /// How this unit finds frames: "published", "thumbnail", "perforation" or "address"
    framing: String,
    /// Whether a thumbnail pass happens at all, and so whether a caller can
    /// offer to keep it
    thumbnail: bool,
    /// The film format the loaded holder or adapter sets, as `discover_frames` takes it.
    /// `None` where it sets no format, or where the framing does not use a format
    film_format: Option<String>,
    /// The film formats to offer for the loaded holder or adapter. Empty where it sets the
    /// format, or where the framing does not use a format
    film_formats: Vec<String>,
    /// Whether the CCD can read its lines at once. False means a "superfine"
    /// control has nothing to switch
    multi_line: bool,
    /// Whether the unit can give the medium back on its own
    eject: bool,
    /// Whether the unit offers Nikon Scan's Calibrate, its shading measurement
    calibrate: bool,
    /// Whether the unit focuses itself
    autofocus: bool,
    /// Whether the unit runs an AE pass itself. False on every unit seen, an
    /// LS-9000 included, so metering is host-side and the white-balance
    /// controls are what decide it
    hardware_metering: bool,
    /// The reading modes this unit offers, by name
    interleavings: Vec<String>,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyCapabilities {
    /// Whether this film type is metered with its channels held together
    ///
    /// The default a `lock_white_balance` control should start from. It follows
    /// the film rather than the scanner, so it is the same answer everywhere -
    /// it lives here because this is where a caller already looks
    #[staticmethod]
    fn locks_white_balance(film: &str) -> PyResult<bool> {
        Ok(Metering::locks_white_balance(film_from_name(film)?))
    }
}

/// Parse a film name. See `Film`'s `FromStr` for the names
fn film_from_name(film: &str) -> PyResult<Film> {
    film.parse().map_err(PyRuntimeError::new_err)
}

/// Parse a format name, as `discover_frames` takes it
fn format_from_name(format: Option<&str>) -> PyResult<Option<FilmFormat>> {
    format
        .map(str::parse)
        .transpose()
        .map_err(pyo3::exceptions::PyValueError::new_err)
}

/// The metering `scan_frame` and `meter_frame` run, from their arguments
fn metering_from(
    film: Option<&str>,
    format: Option<&str>,
    base_density: Option<(f64, f64, f64)>,
    lock_white_balance: Option<bool>,
) -> PyResult<Metering> {
    let mut metering = Metering::new(film.map(film_from_name).transpose()?.unwrap_or_default());
    metering.format = format_from_name(format)?;
    metering.base_density = base_density.map(|(r, g, b)| [r, g, b]);
    if let Some(lock) = lock_white_balance {
        metering.lock_white_balance = lock;
    }
    Ok(metering)
}

// ----- focus -----

/// Parse the `focus` argument of `scan_frame` and `focus_frame`
///
/// - `None` or "auto": the unit focuses on the center of the frame.
/// - `(x, y)`: the unit focuses on this point. Each value is a fraction of the
///   frame size.
/// - An int: the lens moves to this position. The position must be in
///   `Capabilities.focus_range`.
/// - "hold": the lens does not move.
fn focus_from(focus: Option<&Bound<'_, PyAny>>) -> PyResult<Focus> {
    let Some(focus) = focus else {
        return Ok(Focus::default());
    };
    if let Ok(position) = focus.extract::<u16>() {
        return Ok(Focus::At(position));
    }
    if let Ok(at) = focus.extract::<(f32, f32)>() {
        return Ok(Focus::Auto { at, color: None });
    }
    match focus.extract::<String>() {
        Ok(name) if name.eq_ignore_ascii_case("auto") => Ok(Focus::default()),
        Ok(name) if name.eq_ignore_ascii_case("hold") => Ok(Focus::Hold),
        _ => Err(PyValueError::new_err(format!(
            "focus is \"auto\", \"hold\", a position, or an (x, y) point, not {focus}"
        ))),
    }
}

/// The name of a focus result, as `ScanResult.focused` gives it
fn focused_name(focused: Focused) -> String {
    match focused {
        Focused::Yes => "focused",
        Focused::NotReached => "not_reached",
        Focused::Skipped => "skipped",
    }
    .to_string()
}

impl From<&RustCapabilities> for PyCapabilities {
    fn from(caps: &RustCapabilities) -> Self {
        let framing = framing::Framing::choose(caps);
        // Only the framings that measure the frames need the format
        let measured = matches!(framing, Framing::Thumbnail | Framing::Perforation);
        Self {
            vendor: caps.identity.vendor.clone(),
            product: caps.identity.product.clone(),
            revision: caps.identity.revision.clone(),
            model: caps.identity.model().map(|m| m.name().to_string()),
            x_dpi_range: (
                caps.address.x_axis.dpi_range.start,
                caps.address.x_axis.dpi_range.last,
            ),
            y_dpi_range: (
                caps.address.y_axis.dpi_range.start,
                caps.address.y_axis.dpi_range.last,
            ),
            optical_dpi: caps.address.x_axis.optical_dpi,
            x_boundary: caps.address.x_axis.boundary,
            y_boundary: caps.address.y_axis.boundary,

            max_frames: caps.address.max_frames,
            thumbnail_dpi: (
                caps.address.thumbnail_resolution.start,
                caps.address.thumbnail_resolution.last,
            ),
            focus_range: (
                caps.address.focus_range.start,
                caps.address.focus_range.last,
            ),
            exposure_range: (
                caps.set_window.exposure.start,
                caps.set_window.exposure.last,
            ),
            // 2-10 byte 43: a unit that does not offer the mode reads a line
            // once, and refuses a window that asks for more
            max_samples: match caps.set_window.mode.contains(ScanMode::MULTI_READING) {
                true => MAX_SAMPLES,
                false => 1,
            },
            framing: match framing {
                Framing::Published => "published",
                Framing::Thumbnail => "thumbnail",
                Framing::Perforation => "perforation",
                Framing::Address => "address",
            }
            .to_string(),
            thumbnail: framing.takes_thumbnail(caps),
            film_format: measured
                .then(|| FilmFormat::resolve(None, caps).ok())
                .flatten()
                .map(|f| f.to_string()),
            film_formats: match measured {
                true => FilmFormat::choices(caps)
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                false => Vec::new(),
            },
            multi_line: caps.reads_lines_at_once(),
            eject: caps.features.execute.supports(Op::Unload),
            calibrate: caps.features.execute.supports(Op::SetupShading),
            autofocus: caps.features.execute.supports(Op::AutoFocus),
            hardware_metering: unit_autoexposure_kind(
                caps.identity.model(),
                caps.set_window.kind,
                false,
            )
            .is_some(),
            interleavings: caps
                .set_window
                .interleaving
                .iter_names()
                .map(|(n, _)| n.to_ascii_lowercase())
                .collect(),
        }
    }
}

// ----- a finished scan -----

/// What one frame's scan produced
#[gen_stub_pyclass]
#[pyclass(name = "ScanResult", frozen, get_all, module = "nkscan")]
pub struct PyScanResult {
    /// One array per captured channel, keyed by name ("red", "green", "blue", ...)
    colors: HashMap<String, Py<PyArray2<u16>>>,
    /// The infrared plane, where the recipe asked for it
    ir: Option<Py<PyArray2<u16>>>,
    dpi: u32,
    rows: usize,
    cols: usize,
    /// What the frame was exposed at, keyed the same way as `colors`
    exposures: HashMap<String, u32>,
    /// The scanner's start-up exposure values used to seed host metering
    meter_startup_exposures: Option<HashMap<String, u32>>,
    /// Every decoded meter pass and the exposure that produced it
    meter_passes: Vec<Py<PyMeterPass>>,
    /// Pixels dust removal rebuilt, where asked for
    cleaned: Option<usize>,
    /// True if all blocks of the pass arrived. If this is false, the planes
    /// contain data only for the blocks in `blocks`
    complete: bool,
    /// The number of blocks of the pass that arrived
    blocks: usize,
    /// The focus result:
    ///
    /// - "focused": the unit reached focus, or the lens moved to the position.
    /// - "not_reached": autofocus did not reach focus. The scan continued at
    ///   the last lens position.
    /// - "skipped": `focus` was "hold", so the lens did not move.
    focused: String,
    /// The lens position during the pass. `None` if the unit does not report it
    focus_position: Option<u16>,
}

/// One decoded image used to decide a frame's exposure
#[gen_stub_pyclass]
#[pyclass(name = "MeterPass", frozen, get_all, module = "nkscan")]
pub struct PyMeterPass {
    colors: HashMap<String, Py<PyArray2<u16>>>,
    ir: Option<Py<PyArray2<u16>>>,
    dpi: u32,
    rows: usize,
    cols: usize,
    /// The exposure values that produced these pixels
    exposures: HashMap<String, u32>,
    /// The rectangle metering measured, inclusive: first line, first pixel,
    /// last line, last pixel, in Nikon Scan's builder orientation (lines along
    /// the feed, pixels along the sensor from the far end)
    analyzer_rect: (usize, usize, usize, usize),
}

/// nkcolor: one meter pass as a `MeterPass`
fn meter_pass_to_python(
    py: Python<'_>,
    meter: crate::scan::autoexpose::MeterPass,
) -> PyResult<Py<PyMeterPass>> {
    let ids: Vec<u8> = meter.pass.layout.colors().collect();
    let (rows, cols) = (meter.pass.rows, meter.pass.cols);
    let colors = colors_to_numpy(py, &ids, meter.samples.colors, rows, cols);
    let ir = meter
        .samples
        .ir
        .map(|plane| plane_to_numpy(py, plane, rows, cols));
    let exposures = meter
        .exposures
        .iter()
        .map(|(c, e)| (channel_name(c.id()), e))
        .collect();
    let [line0, pixel0, line1, pixel1] = meter.analyzer_rect;
    Py::new(
        py,
        PyMeterPass {
            colors,
            ir,
            dpi: meter.pass.layout.dpi,
            rows,
            cols,
            exposures,
            analyzer_rect: (line0, pixel0, line1, pixel1),
        },
    )
}

/// nkcolor: the index of the thumbnail column whose feed address is nearest `address`
fn nearest_column(columns: &[u32], address: u32) -> usize {
    let after = columns.partition_point(|&c| c < address);
    match (after.checked_sub(1), columns.get(after)) {
        (Some(before), Some(&next)) if address - columns[before] <= next - address => before,
        (_, Some(_)) => after,
        (Some(before), None) => before,
        (None, None) => 0,
    }
}

fn channel_name(id: u8) -> String {
    format!("{:?}", Channel::from(id)).to_lowercase()
}

/// One plane, zero-copy, as a 2D numpy array
fn plane_to_numpy(py: Python<'_>, plane: Vec<u16>, rows: usize, cols: usize) -> Py<PyArray2<u16>> {
    plane
        .into_pyarray(py)
        .reshape([rows, cols])
        .expect("plane is rows * cols long")
        .unbind()
}

/// Zero-copy numpy arrays for `samples`, keyed by channel name, using `ids` (in `samples`'
/// plane order) to name them
fn colors_to_numpy(
    py: Python<'_>,
    ids: &[u8],
    samples: Vec<Vec<u16>>,
    rows: usize,
    cols: usize,
) -> HashMap<String, Py<PyArray2<u16>>> {
    ids.iter()
        .zip(samples)
        .map(|(&id, plane)| (channel_name(id), plane_to_numpy(py, plane, rows, cols)))
        .collect()
}

/// What discovery found
#[gen_stub_pyclass]
#[pyclass(name = "Discovery", frozen, get_all, module = "nkscan")]
pub struct PyDiscovery {
    frames: Vec<(u32, u32, u32, u32)>,
    /// One array per channel, keyed the way `ScanResult.colors` is;
    /// `None` where discovery takes no thumbnail pass, see `Capabilities.thumbnail`.
    /// On a unit that counts perforations, this is the resampled thumbnail
    thumbnail: Option<HashMap<String, Py<PyArray2<u16>>>>,
    /// Feed addresses one column of `thumbnail` spans
    ///
    /// Use this to put a rectangle drawn on the thumbnail onto the film, so the
    /// rectangle previewed is the rectangle scanned. Do not compute it as
    /// `optical_dpi / thumbnail_dpi`: the film does not keep to the thumbnail
    /// resolution the unit reports, and the error accumulates along the strip.
    /// `None` without a thumbnail, and on a unit that counts perforations
    addresses_per_column: Option<f64>,
    /// The feed address of each column of `thumbnail`. The addresses never decrease
    ///
    /// The columns of a frame start at the first column at or after its top. They stop before
    /// the first column at or after its bottom. `None` without a thumbnail, or where no
    /// thumbnail line has a perforation record
    column_addresses: Option<Vec<u32>>,
    /// The X address of each row of `thumbnail`. The addresses decrease
    ///
    /// Use this with `column_addresses` to find a frame on the thumbnail. On a
    /// holder with more than one strip the thumbnail shows all strips, and the
    /// strips are at different X addresses. `None` without a thumbnail
    row_addresses: Option<Vec<u32>>,
    /// The quality of the fallback frame fit; `None` where Nikon Scan's search found the frames
    contrast: Option<f32>,
    /// True if all blocks of the thumbnail pass arrived. `None` if there is no
    /// thumbnail
    thumbnail_complete: Option<bool>,
    /// The number of blocks of the thumbnail pass that arrived. `None` if there
    /// is no thumbnail
    thumbnail_blocks: Option<usize>,
    /// The film base density off the thumbnail, for `scan_frame`. `None` without a
    /// thumbnail, and on the LS-8000 and LS-9000, where `scan_frame` uses the default
    base_density: Option<(f64, f64, f64)>,
    /// nkcolor: the scanner-address rectangle covered by `thumbnail`
    thumbnail_bounds: Option<(u32, u32, u32, u32)>,
    /// nkcolor: each frame's first and last thumbnail column and its feed
    /// extent, where the unit positions the film by its perforation table.
    /// Read off `column_addresses`
    thumbnail_frames: Option<Vec<(usize, usize, u32)>>,
}

// ----- a session -----

/// An open, exclusive hold of a scanner
#[gen_stub_pyclass]
#[pyclass(name = "Session", module = "nkscan")]
pub struct PySession(Mutex<Option<RustSession>>);

impl PySession {
    fn with<T>(&self, f: impl FnOnce(&mut RustSession) -> Result<T, Error>) -> PyResult<T> {
        let mut guard = self.0.lock().expect("not poisoned");
        let session = guard.as_mut().ok_or_else(closed)?;
        Ok(f(session)?)
    }
}

fn open_session(transport: Box<dyn Transport>) -> Result<PySession, Error> {
    let session = block_on(RustSession::open(transport))?;
    Ok(PySession(Mutex::new(Some(session))))
}

#[gen_stub_pymethods]
#[pymethods]
impl PySession {
    /// Start a session against whatever `list_devices` reports at this `location`
    #[new]
    fn new(py: Python<'_>, location: &str) -> PyResult<Self> {
        let location = location.to_string();
        py.detach(move || {
            let (_, transport) =
                block_on(device::Selector::Location(location).open()).map_err(|e| match e {
                    device::SelectError::Open(e) => PyErr::from(e),
                    e => DeviceNotFound::new_err(e.to_string()),
                })?;
            open_session(transport).map_err(PyErr::from)
        })
    }

    /// Start a session against `device`
    #[staticmethod]
    fn open(py: Python<'_>, device: &PyDevice) -> PyResult<Self> {
        let dev = device.0.clone();
        py.detach(move || open_session(block_on(dev.open())?))
            .map_err(PyErr::from)
    }

    /// What the scanner says it can do
    #[getter]
    fn capabilities(&self) -> PyResult<PyCapabilities> {
        self.with(|s| Ok(PyCapabilities::from(s.capabilities())))
    }

    /// Whether a holder is loaded
    fn media_loaded(&self, py: Python<'_>) -> PyResult<bool> {
        py.detach(|| self.with(|s| block_on(s.media_loaded())))
    }

    /// Put the unit in the state a scan expects
    fn stage(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.with(|s| block_on(s.stage())))
    }

    /// Run the scanner's shading measurement, Nikon Scan's Calibrate
    ///
    /// The scanner measures its light source again, which corrects streaks
    /// along the film travel. It can take about two minutes. The film is
    /// staged again afterwards. Returns false when the scanner does not offer
    /// it; `Capabilities.calibrate` says so in advance.
    fn calibrate_shading(&self, py: Python<'_>) -> PyResult<bool> {
        py.detach(|| {
            self.with(|session| {
                let calibrated = block_on(session.calibrate_shading())?;
                if calibrated {
                    block_on(session.stage())?;
                }
                Ok(calibrated)
            })
        })
    }

    /// Give back whatever is loaded, answering whether the unit did anything
    fn eject(&self, py: Python<'_>) -> PyResult<bool> {
        py.detach(|| self.with(|s| block_on(s.eject())))
    }

    /// Take in whatever the adapter has waiting, answering whether anything came
    fn load(&self, py: Python<'_>) -> PyResult<bool> {
        py.detach(|| self.with(|s| block_on(s.load())))
    }

    /// Find every frame on whatever is loaded
    ///
    /// `format` is one of "135", "half", "IX240", "16", "645", "66", "67",
    /// "68", "69", or a custom frame length in mm as a string (e.g. "56").
    /// Only asked for by the two of four discovery mechanisms that need a
    /// thumbnail pass to find frames, and even there only where the loaded
    /// holder does not fix or narrow it by itself, so it can usually be left
    /// `None`. `film` ("positive", "slide", "negative", "kodachrome" or
    /// "mono", default positive) sets the thumbnail exposure.
    /// `Discovery.thumbnail`, where the mechanism took one, is what a caller
    /// wanting to nudge `Discovery.frames` by hand shows the operator: a
    /// rectangle handed to `scan_frame` needs no match in it, so a nudged one
    /// works the same as a detected one, just slower if the stage has to home
    /// first to reach it
    #[pyo3(signature = (format=None, film=None, progress=None))]
    fn discover_frames(
        &self,
        py: Python<'_>,
        format: Option<&str>,
        film: Option<&str>,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<PyDiscovery> {
        let format = format_from_name(format)?;
        let film = film.map(film_from_name).transpose()?.unwrap_or_default();

        let (
            frames,
            thumbnail,
            ids,
            samples,
            shape,
            pitch,
            columns,
            row_addresses,
            contrast,
            arrived,
            base_density,
            thumbnail_bounds,
            thumbnail_frames,
        ) = py.detach(move || {
            self.with(|session| {
                let mut samples = Samples::default();
                let discovery = block_on(framing::discover_with(
                    session,
                    format,
                    film,
                    &mut samples,
                    |p| report(&progress, "discover", 0, p),
                ))?;
                let frames: Vec<_> = discovery
                    .frames
                    .into_iter()
                    .map(|r| (r.top, r.left, r.bottom, r.right))
                    .collect();
                let pitch = discovery.line_pitch.map(|p| p.addresses_per_column());
                let columns = discovery.column_addresses;
                let row_addresses = discovery.row_addresses;
                let contrast = discovery.contrast;
                let base_density = discovery.base_density.map(|[r, g, b]| (r, g, b));
                // nkcolor: the thumbnail columns each frame spans
                let thumbnail_frames = match (&columns, Framing::choose(session.capabilities())) {
                    (Some(columns), Framing::Perforation) => Some(
                        frames
                            .iter()
                            .map(|&(top, _left, bottom, _right)| {
                                (
                                    nearest_column(columns, top),
                                    nearest_column(columns, bottom),
                                    bottom.saturating_sub(top),
                                )
                            })
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                };
                match discovery.thumbnail {
                    Some(pass) => {
                        // nkcolor: the scanner-address rectangle the thumbnail covers
                        let thumbnail_bounds = thumbnail::windows(session.capabilities())?
                            .first()
                            .map(|window| {
                                let (left, top) = window.origin;
                                let (width, height) = window.size;
                                (top, left, top + height, left + width)
                            });
                        let ids: Vec<u8> = pass.layout.colors().collect();
                        let shape = (pass.rows, pass.cols);
                        let arrived = Some((pass.complete, pass.blocks));
                        Ok((
                            frames,
                            true,
                            ids,
                            samples.colors,
                            shape,
                            pitch,
                            columns,
                            row_addresses,
                            contrast,
                            arrived,
                            base_density,
                            thumbnail_bounds,
                            thumbnail_frames,
                        ))
                    }
                    None => Ok((
                        frames,
                        false,
                        Vec::new(),
                        Vec::new(),
                        (0, 0),
                        pitch,
                        columns,
                        row_addresses,
                        contrast,
                        None,
                        base_density,
                        None,
                        None,
                    )),
                }
            })
        })?;

        let thumbnail = thumbnail.then(|| {
            let (rows, cols) = shape;
            Python::attach(|py| colors_to_numpy(py, &ids, samples, rows, cols))
        });
        Ok(PyDiscovery {
            frames,
            thumbnail,
            addresses_per_column: pitch,
            column_addresses: columns,
            row_addresses,
            contrast,
            thumbnail_complete: arrived.map(|(complete, _)| complete),
            thumbnail_blocks: arrived.map(|(_, blocks)| blocks),
            base_density,
            thumbnail_bounds,
            thumbnail_frames,
        })
    }

    /// Focus, meter, take the pass over `frame`, and optionally clean it
    ///
    /// `frame` is `(top, left, bottom, right)`, one of `discover_frames`'s, or one
    /// of them moved or cropped. The pass is that rectangle at both ends. On a unit
    /// that positions the film by its own frame table, the rectangle is put in that
    /// table first, so a moved or cropped one reaches the film it asks for.
    /// `exposures`, keyed the way `ScanResult.exposures` is, reuses an exposure
    /// already decided rather than metering this frame fresh. `focus` is the
    /// same as for `focus_frame`. Use "hold" to scan at the focus that
    /// `focus_frame` set.
    ///
    /// `film` and `format` are as for `discover_frames`, and `base_density` is
    /// `Discovery.base_density`. `lock_white_balance` defaults to
    /// `Capabilities.locks_white_balance(film)`. `clean` uses infrared
    /// internally and cannot be combined with retained `infrared` output.
    /// nkcolor: `nikon_negative_meter` meters as a color negative with the rule
    /// a Nikon Scan capture verifies on this unit, and refuses other units.
    /// Every host meter image comes back in `meter_passes`
    #[pyo3(signature = (
        frame,
        dpi=None,
        samples=1,
        superfine=false,
        infrared=false,
        clean=false,
        clean_quality="normal",
        film=None,
        format=None,
        base_density=None,
        lock_white_balance=None,
        nikon_negative_meter=false,
        exposures=None,
        focus=None,
        progress=None,
        overscan=0,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn scan_frame(
        &self,
        py: Python<'_>,
        frame: (u32, u32, u32, u32),
        dpi: Option<u16>,
        samples: u8,
        superfine: bool,
        infrared: bool,
        clean: bool,
        clean_quality: &str,
        film: Option<&str>,
        format: Option<&str>,
        base_density: Option<(f64, f64, f64)>,
        lock_white_balance: Option<bool>,
        nikon_negative_meter: bool,
        exposures: Option<HashMap<String, u32>>,
        #[gen_stub(override_type(type_repr = "typing.Optional[typing.Union[builtins.int, tuple[builtins.float, builtins.float], typing.Literal['auto', 'hold']]]", imports = ("builtins", "typing")))]
        focus: Option<Bound<'_, PyAny>>,
        progress: Option<Py<PyAny>>,
        overscan: u32,
    ) -> PyResult<PyScanResult> {
        if infrared && clean {
            return Err(PyValueError::new_err(
                "infrared retention and dust removal are mutually exclusive",
            ));
        }
        let clean_quality = match clean_quality {
            "normal" => crate::dust::Quality::Normal,
            "fine" => crate::dust::Quality::Fine,
            value => {
                return Err(PyValueError::new_err(format!(
                    "unknown clean quality: {value}"
                )));
            }
        };
        let focus = focus_from(focus.as_ref())?;
        let frame = rect(frame);
        let metering = metering_from(film, format, base_density, lock_white_balance)?;
        let locked = exposures.map(|by_name| {
            let mut e = Exposures::default();
            for (name, value) in by_name {
                e.set(channel_from_name(&name), value);
            }
            e
        });

        let (scanned, buf) = py.detach(move || {
            self.with(|session| {
                let recipe = Recipe::new(
                    session.capabilities(),
                    dpi,
                    samples,
                    superfine,
                    infrared || clean,
                );
                recipe.supported(session.capabilities())?;

                let mut buf = Samples::default();
                let options = frame::Options {
                    refocus_before_scan: false,
                    overscan,
                    exposures: locked.as_ref(),
                    metering,
                    nikon_negative_meter,
                    clean,
                    focus,
                    clean_quality,
                };
                let scanned = block_on(frame::scan_frame_with(
                    session,
                    &recipe,
                    frame,
                    options,
                    &mut buf,
                    |phase, p| match phase {
                        Phase::Meter(pass) => report(&progress, "meter", pass, p),
                        Phase::Scan => report(&progress, "scan", 0, p),
                    },
                ))?;
                Ok((scanned, buf))
            })
        })?;

        let ids: Vec<u8> = scanned.pass.layout.colors().collect();
        let (rows, cols) = (scanned.pass.rows, scanned.pass.cols);
        let colors = colors_to_numpy(py, &ids, buf.colors, rows, cols);
        let ir = buf.ir.map(|plane| plane_to_numpy(py, plane, rows, cols));
        let exposures = scanned
            .exposures
            .iter()
            .map(|(c, e)| (channel_name(c.id()), e))
            .collect();
        // nkcolor: the meter evidence
        let (meter_startup_exposures, meter_passes) = match scanned.metering {
            Some(metering) => {
                let startup = metering.startup_exposures.map(|values| {
                    values
                        .iter()
                        .map(|(c, e)| (channel_name(c.id()), e))
                        .collect()
                });
                let passes = metering
                    .passes
                    .into_iter()
                    .map(|meter| meter_pass_to_python(py, meter))
                    .collect::<PyResult<Vec<_>>>()?;
                (startup, passes)
            }
            None => (None, Vec::new()),
        };

        Ok(PyScanResult {
            colors,
            ir,
            dpi: scanned.pass.layout.dpi,
            rows,
            cols,
            exposures,
            meter_startup_exposures,
            meter_passes,
            cleaned: scanned.cleaned,
            complete: scanned.pass.complete,
            blocks: scanned.pass.blocks,
            focused: focused_name(scanned.focused),
            focus_position: scanned.focus_position,
        })
    }

    /// Meter `frame` and answer the exposures `scan_frame` would scan it at
    ///
    /// The metering `scan_frame` runs when it is not handed `exposures`, without
    /// the pass after it. Handing the result to `scan_frame` exposes any frame
    /// the way this one would be. `infrared` meters for a scan that takes the
    /// infrared plane or cleans, so the result carries that channel too.
    /// `film`, `format`, `base_density` and `lock_white_balance` are
    /// `scan_frame`'s
    #[pyo3(signature = (
        frame,
        infrared=false,
        film=None,
        format=None,
        base_density=None,
        lock_white_balance=None,
        progress=None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn meter_frame(
        &self,
        py: Python<'_>,
        frame: (u32, u32, u32, u32),
        infrared: bool,
        film: Option<&str>,
        format: Option<&str>,
        base_density: Option<(f64, f64, f64)>,
        lock_white_balance: Option<bool>,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<HashMap<String, u32>> {
        let frame = rect(frame);
        let metering = metering_from(film, format, base_density, lock_white_balance)?;

        py.detach(move || {
            self.with(|session| {
                // Metering takes its own resolution and reading mode from the
                // recipe's `metering`, so only the channels matter here
                let recipe = Recipe::new(session.capabilities(), None, 1, false, infrared);
                let exposures = block_on(frame::meter_frame_with(
                    session,
                    &recipe,
                    frame,
                    &metering,
                    |pass, p| report(&progress, "meter", pass, p),
                ))?;
                Ok(exposures
                    .iter()
                    .map(|(c, e)| (channel_name(c.id()), e))
                    .collect())
            })
        })
    }

    /// Focus on `frame` without metering or scanning it
    ///
    /// Returns the focus result and the lens position, with the same values as
    /// `ScanResult.focused` and `ScanResult.focus_position`.
    ///
    /// `focus` is one of:
    ///
    /// - `None` or "auto": the unit focuses on the center of the frame.
    /// - `(x, y)`: the unit focuses on this point. Each value is a fraction of
    ///   the frame size.
    /// - An int: the lens moves to this position. The position must be in
    ///   `Capabilities.focus_range`.
    /// - "hold": the lens does not move.
    ///
    /// To scan at this focus, call `scan_frame` with `focus="hold"`
    #[pyo3(signature = (frame, focus=None))]
    fn focus_frame(
        &self,
        py: Python<'_>,
        frame: (u32, u32, u32, u32),
        #[gen_stub(override_type(type_repr = "typing.Optional[typing.Union[builtins.int, tuple[builtins.float, builtins.float], typing.Literal['auto', 'hold']]]", imports = ("builtins", "typing")))]
        focus: Option<Bound<'_, PyAny>>,
    ) -> PyResult<(String, Option<u16>)> {
        let focus = focus_from(focus.as_ref())?;
        let frame = rect(frame);
        let (focused, position) = py.detach(move || {
            self.with(|session| block_on(frame::focus_frame(session, frame, focus)))
        })?;
        Ok((focused_name(focused), position))
    }

    /// Autofocus on the point `(x, y)`, in frame addresses
    ///
    /// The point must be in one of the unit's frames. `color` selects the
    /// channel to focus on. Not all units can focus on one channel. If the unit
    /// does not reach focus, this raises `ScannerError`. `focus_frame` does not
    /// raise in that case
    #[pyo3(signature = (x, y, color=None))]
    fn autofocus(&self, py: Python<'_>, x: u32, y: u32, color: Option<u8>) -> PyResult<()> {
        py.detach(|| self.with(|s| block_on(s.autofocus(x, y, color))))
    }

    /// Move the lens to `position`. The position must be in
    /// `Capabilities.focus_range`
    fn focus_to(&self, py: Python<'_>, position: u16) -> PyResult<()> {
        py.detach(|| self.with(|s| block_on(s.focus_to(position))))
    }

    /// The current lens position, in the units that `focus_to` uses
    fn focus_position(&self, py: Python<'_>) -> PyResult<u16> {
        py.detach(|| self.with(|s| block_on(s.focus_position())))
    }

    /// Nikon's ICC profile for this unit and `film`, as the bytes of the .icc
    /// file
    ///
    /// `film` is "positive", "slide", "negative", "kodachrome" or "mono".
    /// Returns `None` if Nikon Scan has no profile for this unit and film
    fn nikon_profile(&self, film: &str) -> PyResult<Option<Cow<'static, [u8]>>> {
        let film = film_from_name(film)?;
        self.with(|s| Ok(profile::nikon(&s.capabilities().identity, film).map(Cow::Borrowed)))
    }

    /// Drop the hold on the scanner. A closed session refuses every other method
    fn close(&self) {
        *self.0.lock().expect("not poisoned") = None;
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, pyo3::types::PyTuple>) {
        self.close();
    }
}

/// Report progress on `on`, if there is one, throttled to roughly 10 updates a
/// second, and translate a `False` return into a cancel
fn report(on: &Option<Py<PyAny>>, phase: &str, pass: usize, p: Progress) -> ControlFlow<()> {
    let Some(on) = on else {
        return ControlFlow::Continue(());
    };
    Python::attach(|py| {
        let Ok(result) = on.call1(py, (phase, pass, p.bytes, p.total)) else {
            return ControlFlow::Continue(());
        };
        match result.extract::<bool>(py) {
            Ok(false) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    })
}

/// Convert a frame argument, `(top, left, bottom, right)`, to a `Rect`
fn rect((top, left, bottom, right): (u32, u32, u32, u32)) -> Rect {
    Rect {
        top,
        left,
        bottom,
        right,
    }
}

/// The channel a `scan_frame`/`ScanResult` name refers to
fn channel_from_name(name: &str) -> Channel {
    match name.to_lowercase().as_str() {
        "red" => Channel::Red,
        "green" => Channel::Green,
        "blue" => Channel::Blue,
        "infrared" => Channel::Infrared,
        "neutralgray" => Channel::NeutralGray,
        _ => Channel::Default,
    }
}

// ----- the module -----

#[pymodule]
#[pyo3(name = "nkscan")]
fn nkscan_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDevice>()?;
    m.add_class::<PyCapabilities>()?;
    m.add_class::<PySession>()?;
    m.add_class::<PyScanResult>()?;
    m.add_class::<PyMeterPass>()?;
    m.add_class::<PyDiscovery>()?;
    m.add_function(wrap_pyfunction!(list_devices, m)?)?;
    m.add_function(wrap_pyfunction!(init_logging, m)?)?;
    m.add_function(wrap_pyfunction!(clean_rgb_ir, m)?)?;

    let py = m.py();
    m.add("ScannerError", py.get_type::<ScannerError>())?;
    m.add("TransientError", py.get_type::<TransientError>())?;
    m.add("TransportError", py.get_type::<TransportError>())?;
    m.add("DeviceBusy", py.get_type::<DeviceBusy>())?;
    m.add("DeviceNotFound", py.get_type::<DeviceNotFound>())?;
    m.add("MediaError", py.get_type::<MediaError>())?;
    m.add("UnsupportedError", py.get_type::<UnsupportedError>())?;
    m.add("ScanCancelled", py.get_type::<ScanCancelled>())?;
    Ok(())
}

define_stub_info_gatherer!(stub_info);
