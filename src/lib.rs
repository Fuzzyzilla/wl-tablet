//! # Cross-platform [Tablet](tablet), [Pad](pad), and [Stylus](tool) API 🐙✨
//!
//! A platform abstraction across stylus APIs, aiming to provide the *union* of platform feature sets so that no
//! expressiveness is lost in translation.
//!
//! This crate requires low-level access to the windowing server, which is provided by many windowing abstractions
//! such as `eframe` and `winit` through the [`raw_window_handle`](https://crates.io/crates/raw-window-handle) crate.
//!
//! To get started, create a [`Builder`].
//!
//! ## Supported platforms
//! See the [`Backend`] enum and [`README.md`](https://github.com/Fuzzyzilla/octotablet/blob/master/README.md)
//!
//! ## Supported Hardware
//! Aims to support a wide range of devices, as long as they use standard platform APIs (ie, no tablet-specific APIs will be implemented).
//!
//! During development, tested on:
//! * *Wacom Cintiq 16* \[DTK-1660\]
//! * *Wacom Intuos (S)* \[CTL-4100\]
//! * *Wacom Intuos Pro small* \[PTH-451\]
//! * *Wacom Pro Pen 2*
//! * *Wacom Pro Pen 2k*
//! * *XP-Pen Deco-01*
//!
//! ## Quirks
//! Graphics tablets, drivers, and system compositors have no shortage of quirks. Some platforms try to correct for that,
//! some dont. While API-level quirks are smoothed out by this crate, per-device quirk correction is beyond the scope
//! of this project and quirked values are reported as-is. Where possible, documentation notes are included to warn
//! where quirks are known to occur. If you encounter additional issues, please submit a PR documenting them.
//! **Guarantees are made only when explicitly stated so!**
//!
//! ## Examples
//! See [the examples directory](https://github.com/Fuzzyzilla/octotablet/tree/master/examples) for how
//! this can be integrated into `winit` or `eframe` projects and demos of the kind of hardware
//! capabilities this crate exposes.

#![warn(clippy::pedantic)]
#![warn(rustdoc::all)]
#![forbid(unsafe_op_in_unsafe_fn)]

mod platform;
use platform::{InternalID, PlatformImpl};

pub mod axis;
pub mod builder;
pub mod events;
pub mod pad;
pub mod tablet;
pub mod tool;
pub mod util;
pub use builder::Builder;
use events::Events;

/// A trait that every object is.
/// Used to cast things to `dyn Erased` which leaves us with a wholly erased type.
trait Erased {}
impl<T> Erased for T {}

#[allow(dead_code)]
enum Backing {
    // The RWH owner is arc'd and thus is valid for as long as we need.
    // We don't need to remember what type the pointer came from, but `dyn WhateverTrait` remembers
    // enough to be able to destruct it cleanly.
    Arc(std::sync::Arc<dyn Erased>),
    // The RWH is some kind of raw pointer without lifetimes, and the user has guaranteed
    // that it is valid as long as this object lives.
    Raw,
}
#[derive(Clone, Copy, Debug)]

/// List of supported backends. This is not affected by enabled features.
// Some are never constructed due to disabled features/target platform.
#[allow(dead_code)]
pub enum Backend {
    /// [`tablet_unstable_v2`](https://wayland.app/protocols/tablet-unstable-v2)
    ///
    /// **Note**: "unstable" here refers to the protocol itself, not to the stability of its integration into this crate!
    WaylandTabletUnstableV2,
    /// [`XInput2`](https://www.x.org/releases/X11R7.7/doc/inputproto/XI2proto.txt)
    XorgXInput2,
    /// [`RealTimeStylus`](https://learn.microsoft.com/en-us/windows/win32/tablet/realtimestylus-reference)
    ///
    /// The use of this interface avoids some common problems with the use of Windows Ink in drawing applications,
    /// such as stippling motions resulting in lost clicks or some motions being interpreted as scrolling or flicking gestures.
    /// Through use of this interface, this gesture recognition is bypassed to the greatest extent possible.
    WindowsInkRealTimeStylus,
}
/// Errors that may occur during even pumping.
#[derive(thiserror::Error, Debug)]
pub enum PumpError {
    #[cfg(wl_tablet)]
    #[error(transparent)]
    WaylandDispatch(#[from] wayland_client::DispatchError),
}

/// Maintains a connection to the OS's tablet server. This is the main
/// entry point for enumerating hardware and listening for events.
pub struct Manager {
    pub(crate) internal: platform::PlatformManager,
    // `_backing` MUST BE LAST IN DECLARATION ORDER!
    // the other fields may rely on the lifetime guarantees granted by the contents
    // of this `Backing`, and it's guaranteed that drop order == declaration order.
    // Never thought I'd end up in a situation like this in Rust :P
    pub(crate) _backing: Backing,
}
impl Manager {
    /// Dispatch pending events, updating hardware reports and returning an [`IntoIterator`] containing the events.
    ///
    /// This will not wait for new events, and will return immediately with empty events if there is nothing to do.
    #[allow(clippy::missing_errors_doc)]
    pub fn pump(&mut self) -> Result<Events<'_>, PumpError> {
        self.internal.pump()?;
        Ok(Events { manager: &*self })
    }
    /// Query the precision of [timestamps](events::FrameTimestamp) provided along with axis events, if any.
    /// This does *not* represent the polling rate. `None` if timestamps are not collected.
    ///
    /// This is useful for understanding velocities even if events aren't consumed immediately.
    #[must_use]
    pub fn timestamp_granularity(&self) -> Option<std::time::Duration> {
        self.internal.timestamp_granularity()
    }
    /// Query the API currently in use. May give some hints as to the capabilities and limitations.
    #[must_use]
    pub fn backed(&self) -> Backend {
        match self.internal {
            #[cfg(wl_tablet)]
            platform::PlatformManager::Wayland(_) => Backend::WaylandTabletUnstableV2,
            #[cfg(xinput2)]
            platform::PlatformManager::XInput2(_) => Backend::XorgXInput2,
            #[cfg(ink_rts)]
            platform::PlatformManager::Ink(_) => Backend::WindowsInkRealTimeStylus,
        }
    }
    /// Access pad information. Pads are the physical object that you draw on,
    /// and may have touch support, an inbuilt display, lights, buttons, rings, and/or sliders.
    /// Hardware reports are updated on each call to [`Manager::pump`].
    ///
    /// Pads are ordered arbitrarily.
    ///
    /// # Platform support
    /// * Wayland only.
    #[must_use]
    pub fn pads(&self) -> &[pad::Pad] {
        self.internal.pads()
    }
    /// Access tool information. Tools are styluses or other hardware that
    /// communicate with one or more pads, and are responsible for reporting movements, pressure, etc.,
    /// and may have multiple buttons. Hardware reports are updated on each call to [`Manager::pump`].
    ///
    /// Tools are ordered arbitrarily.
    #[must_use]
    pub fn tools(&self) -> &[tool::Tool] {
        self.internal.tools()
    }
    /// A tablet is the entry point for interactive devices, and the top level of the hierarchy
    /// which may expose several pads or tools. Hardware reports are updated on each call to [`Manager::pump`].
    ///
    /// Tablets are ordered arbitrarily.
    #[must_use]
    pub fn tablets(&self) -> &[tablet::Tablet] {
        self.internal.tablets()
    }
}
