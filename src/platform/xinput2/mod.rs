use core::str;

use crate::events::raw;
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        xinput::{self, ConnectionExt},
        xproto::{ConnectionExt as _, Timestamp},
    },
};

mod strings;

/// If this many milliseconds since last ring interaction, emit an Out event.
const RING_TIMEOUT_MS: Timestamp = 200;
const XI_ANY_PROPERTY_TYPE: u32 = 0;

/// Maximum number of aspects to try and query from libinput devices.
/// I don't think there's a downside to making these almost arbitrarily large..?
const LIBINPUT_MAX_GROUPS: u32 = 4;
const LIBINPUT_MAX_STRIPS: u32 = 4;
const LIBINPUT_MAX_RINGS: u32 = 4;
const LIBINPUT_MAX_BUTTONS: u32 = 32;

/// If this many milliseconds since last tool interaction, emit an Out event.
const TOOL_TIMEOUT_MS: Timestamp = 500;
// Some necessary constants not defined by x11rb:
const XI_ALL_DEVICES: u16 = 0;
const XI_ALL_MASTER_DEVICES: u16 = 1;
/// Magic timestamp signalling to the server "now".
const NOW_MAGIC: x11rb::protocol::xproto::Timestamp = 0;

const EMULATED_TABLET_NAME: &str = "octotablet emulated";

/// Comes from datasize of "button count" field of `ButtonInfo` - button names in xinput are indices,
/// with the zeroth index referring to the tool "down" state.
pub type ButtonID = std::num::NonZero<u16>;

#[derive(Debug, Clone, Copy)]
enum ValuatorAxis {
    // Absolute position, in a normalized device space.
    AbsX,
    AbsY,
    AbsDistance,
    AbsPressure,
    // Degrees, -,- left and away from user.
    AbsTiltX,
    AbsTiltY,
    AbsRz,
    // This pad ring, degrees, and maybe also stylus scrollwheel? I have none to test,
    // but under Xwayland this capability is listed for both pad and stylus.
    AbsWheel,
}
impl TryFrom<ValuatorAxis> for crate::axis::Axis {
    type Error = ();
    fn try_from(value: ValuatorAxis) -> Result<Self, Self::Error> {
        Ok(match value {
            ValuatorAxis::AbsX | ValuatorAxis::AbsY => return Err(()),
            ValuatorAxis::AbsPressure => Self::Pressure,
            ValuatorAxis::AbsTiltX | ValuatorAxis::AbsTiltY => Self::Tilt,
            ValuatorAxis::AbsWheel => Self::Wheel,
            ValuatorAxis::AbsDistance => Self::Distance,
            ValuatorAxis::AbsRz => Self::Roll,
        })
    }
}
fn match_valuator_label(
    label: u32,
    atoms: &strings::xi::axis_label::absolute::Atoms,
) -> Option<ValuatorAxis> {
    let label = std::num::NonZero::new(label)?;
    if label == atoms.x {
        Some(ValuatorAxis::AbsX)
    } else if label == atoms.y {
        Some(ValuatorAxis::AbsY)
    } else if label == atoms.distance {
        Some(ValuatorAxis::AbsDistance)
    } else if label == atoms.pressure {
        Some(ValuatorAxis::AbsPressure)
    } else if label == atoms.tilt_x {
        Some(ValuatorAxis::AbsTiltX)
    } else if label == atoms.tilt_y {
        Some(ValuatorAxis::AbsTiltY)
    } else if label == atoms.rz {
        Some(ValuatorAxis::AbsRz)
    } else if label == atoms.wheel {
        Some(ValuatorAxis::AbsWheel)
    } else {
        None
    }
}

#[derive(Copy, Clone, Debug)]
enum DeviceType {
    Tool(Option<crate::tool::Type>),
    Pad,
}

#[derive(Debug)]
struct XWaylandDeviceInfo {
    device_type: DeviceType,
    // opaque seat ident. given that wayland identifies seats by string name, the exact
    // interpretation of an integer id is unknown to me and i gave up reading the xwayland
    // implementation lol.
    seat: u32,
}

/// Parse the device name of an xwayland device, where the type is stored.
fn parse_xwayland_from_name(device_name: &str) -> Option<XWaylandDeviceInfo> {
    use crate::tool::Type;
    use strings::xwayland;
    let class = device_name.strip_prefix(xwayland::NAME_PREFIX)?;
    // there is a numeric field at the end, which seems to be an opaque
    // representation of the wayland seat the cursor belongs to.
    // weirdly, they behave as several children to one master instead of many masters.
    let colon = class.rfind(xwayland::NAME_SEAT_SEPARATOR)?;
    let class = &class[..colon];
    let seat: u32 = class
        .get((std::ops::Bound::Excluded(colon), std::ops::Bound::Unbounded))
        .and_then(|seat| seat.parse().ok())?;

    let class = match class {
        xwayland::NAME_PAD_SUFFIX => DeviceType::Pad,
        xwayland::NAME_STYLUS_SUFFIX => DeviceType::Tool(Some(Type::Pen)),
        xwayland::NAME_ERASER_SUFFIX => DeviceType::Tool(Some(Type::Eraser)),
        // Lenses and mice get coerced to this same xwayland ident.. darn.
        xwayland::NAME_MOUSE_LENS_SUFFIX => DeviceType::Tool(Some(Type::Mouse)),
        _ => return None,
    };

    Some(XWaylandDeviceInfo {
        device_type: class,
        seat,
    })
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub enum ID {
    /// Special value for the emulated tablet. This is an invalid ID for tools and pads.
    /// A bit of an API design whoopsie!
    EmulatedTablet,
    ID {
        /// Xinput re-uses the IDs of removed devices.
        /// Since we need to keep around devices for an extra frame to report added/removed,
        /// it means a conflict can occur.
        generation: u16,
        device_id: std::num::NonZero<u16>,
    },
}

#[derive(Debug)]
enum ToolNameFields<'a> {
    Xwayland(XWaylandDeviceInfo),
    Generic {
        /// The tablet name we expect to own this tool or pad
        maybe_associated_tablet: Option<std::borrow::Cow<'a, str>>,
        /// The hardware serial of the tool.
        id: Option<crate::tool::HardwareID>,
        /// The expected type of the device
        device_type: Option<DeviceType>,
    },
}
impl ToolNameFields<'_> {
    fn device_type(&self) -> Option<DeviceType> {
        match self {
            Self::Xwayland(XWaylandDeviceInfo { device_type, .. }) => Some(*device_type),
            Self::Generic { device_type, .. } => *device_type,
        }
    }
}
/// From the user-facing Device name, try to parse several tool fields.
fn guess_from_name(mut name: &str) -> ToolNameFields {
    // some drivers place tool hardware IDs within the human-readable Name of the device, and this is
    // the only place it is exposed. Predictably, as with all things X, this is not documented as far
    // as I can tell.
    // https://gitlab.freedesktop.org/xorg/driver/xf86-input-libinput/-/blob/master/src/xf86libinput.c?ref_type=heads#2429

    // Some drivers, input-wacom and input-libinput, also expose this through device properties.

    // From experiments, it tends to consist of the [tablet name]<space>[tool type string]<space>[hex number (or zero)
    // in parentheses] - This is a hueristic and non-exhaustive, for example it does not apply to xwayland.

    // xwayland has a fixed format, check for that before we get all hueristic-y.
    if let Some(xwayland) = parse_xwayland_from_name(name) {
        return ToolNameFields::Xwayland(xwayland);
    }

    // Get the numeric ID, along with the string minus that id.
    // This seems to go back to prehistoric versions of xf86-input-libinput.
    let mut try_take_id = || -> Option<crate::tool::HardwareID> {
        // Detect the range of characters within the last set of parens.
        let open_paren = name.rfind('(')?;
        let after_open_paren = open_paren + 1;
        // Find the close paren after the last open paren (weird change-of-base-address thing)
        let close_paren = after_open_paren + name.get(after_open_paren..)?.find(')')?;

        // Update the name to this if we determine that it had an ID field.
        let name_minus_id = name[..open_paren].trim_ascii_end();

        // Find the id field.
        // id_text is literal '0', or a hexadecimal number prefixed by literal '0x'
        let id_text = &name[after_open_paren..close_paren];

        if id_text == "0" {
            // Should this be considered "None"? The XP-PEN DECO-01 reports this value, despite (afaik)
            // lacking a genuine hardware ID capability.
            // Answer: Yes! https://gitlab.freedesktop.org/xorg/driver/xf86-input-libinput/-/blob/master/include/libinput-properties.h?ref_type=heads#L251
            // This is only for libinput-backed devices, though, hmst.
            // The fact that this is "0" and not "0x0" comes from POSIX [sn]printf.
            name = name_minus_id;
            None
        } else if let Some(id_text) = id_text.strip_prefix("0x") {
            u64::from_str_radix(id_text, 16).ok().map(|id| {
                // map with side effects? i will be ostrisized for my actions.
                name = name_minus_id;
                crate::tool::HardwareID(id)
            })
        } else {
            // Nothing found, don't trim.
            None
        }
    };

    // May be none. this is not a failure.
    let id = try_take_id();

    // .. and try to parse remaining two properties.
    // Two forms we need to worry about, visible on input-wacom and input-libinput
    // [tablet name] + [tool string]
    // [tablet name] - " Pen" + " Pad [Pp]ad"
    // :V oof
    let try_parse_tablet_name_and_ty = || -> Option<(std::borrow::Cow<'_, str>, DeviceType)> {
        use crate::tool::Type;
        // Funny special case, from libinput
        if let Some(tablet) = name.strip_suffix(" unknown tool") {
            return Some((tablet.into(), DeviceType::Tool(None)));
        }
        // Tend to be named "tablet name" + "tool type"
        let last_space = name.rfind(' ')?;
        let last_word = name.get(last_space.checked_add(1)?..)?;
        let mut tablet_name = std::borrow::Cow::Borrowed(&name[..last_space]);

        let ty = match last_word {
            // this can totally be a false positive! Eg, my intuos is called
            // "Intuos S Pen" and the stylus is called "Intuos S Pen Pen (0xblahblah)".
            "pen" | "Pen" | "stylus" | "Stylus" => DeviceType::Tool(Some(Type::Pen)),
            "brush" | "Brush" => DeviceType::Tool(Some(Type::Brush)),
            "pencil" | "Pencil" => DeviceType::Tool(Some(Type::Pencil)),
            "airbrush" | "Airbrush" => DeviceType::Tool(Some(Type::Airbrush)),
            "eraser" | "Eraser" => DeviceType::Tool(Some(Type::Eraser)),
            // Sometimes Mouse amd Lens devices get coerced to the "Cursor" label.
            // "Mouse" obviously falsly identifies random mice as tools, so we filter for
            // Mouse devices with RZ axis (libinput source code says all tablet pointers have it!)
            "cursor" | "Cursor" | "mouse" | "Mouse" => DeviceType::Tool(Some(Type::Mouse)),
            "lens" | "Lens" => DeviceType::Tool(Some(Type::Lens)),
            "pad" | "Pad" => {
                // Pads break the pattern of suffix removal, weirdly.
                // Try to convert it to "<Model Name> Pen" which is used everywhere else.
                // There's no real fallback here, it's hueristic anywayyy
                if let Some(name_minus_pad) = tablet_name
                    .strip_suffix("Pad")
                    .or_else(|| tablet_name.strip_suffix("pad"))
                {
                    tablet_name = std::borrow::Cow::Owned(name_minus_pad.to_owned() + "Pen");
                }
                DeviceType::Pad
            }
            // "Finger" | "finger" => todo!(),
            // "Touch" | "touch" => todo!(),
            _ => return None,
        };

        Some((tablet_name, ty))
    };

    let (tablet_name, ty) = try_parse_tablet_name_and_ty().unzip();

    ToolNameFields::Generic {
        maybe_associated_tablet: tablet_name,
        device_type: ty,
        id,
    }
}
/// Turn an xinput fixed-point number into a float, rounded.
// I could probably keep them fixed for more maths, but this is easy for right now.
fn fixed32_to_f32(fixed: xinput::Fp3232) -> f32 {
    // Could bit-twiddle these into place instead, likely with more precision.
    let integral = fixed.integral as f32;
    // Funny thing. the spec says that frac is the 'decimal fraction'.
    // that's a mighty weird way to spell that -- is this actually in base10?
    let fractional = fixed.frac as f32 / (u64::from(u32::MAX) + 1) as f32;

    if fixed.integral.is_positive() {
        integral + fractional
    } else {
        integral - fractional
    }
}
/// Turn an xinput fixed-point number into a float, rounded.
// I could probably keep them fixed for more maths, but this is easy for right now.
fn fixed16_to_f32(fixed: xinput::Fp1616) -> f32 {
    // Could bit-twiddle these into place instead, likely with more precision.
    (fixed as f32) / 65536.0
}

#[derive(Copy, Clone, Debug)]
struct WacomIDs {
    hardware_serial: u32,
    hardware_id: u32,
    tablet_id: u32,
    // This info is fetched, but unfortunately it's subject to TOCTOU bugs.
    // is_in: bool,
}
#[derive(Copy, Clone, Debug)]
enum WacomInfo {
    Tool {
        ty: crate::tool::Type,
        ids: Option<WacomIDs>,
    },
    Pad {
        ids: Option<WacomIDs>,
    },
    // Does not report tablets. Does report opaque tablet IDs for the other two hw types,
    // which can kinda be used to hallucinate what the tablet might've been lol.
}
impl WacomInfo {
    /// Get the corresponding device type.
    fn device_type(self) -> DeviceType {
        match self {
            Self::Tool { ty, .. } => DeviceType::Tool(Some(ty)),
            Self::Pad { .. } => DeviceType::Pad,
        }
    }
    fn ids(self) -> Option<WacomIDs> {
        match self {
            Self::Pad { ids } | Self::Tool { ids, .. } => ids,
        }
    }
    fn hardware_serial(self) -> Option<u32> {
        self.ids().map(|ids| ids.hardware_serial)
    }
    fn hardware_id(self) -> Option<u32> {
        self.ids().map(|ids| ids.hardware_id)
    }
}
#[derive(Copy, Clone, Debug)]
struct LibinputToolInfo {
    hardware_serial: Option<std::num::NonZero<u32>>,
    hardware_id: Option<u32>,
}
#[derive(Clone, Debug)]
struct LibinputGroupfulPadInfo {
    /// Len < 128. As if, lol.
    groups: Vec<LibinputGroupInfo>,
    strip_associations: Vec<Option<u8>>,
    ring_associations: Vec<Option<u8>>,
    /// Okay sooooo...  we have no mapping from xinput button idx to
    /// hardware idx, so these cannot be used. Unfortunate.
    /// They do not line up. xinput lists 11 buttons with all seven spanning
    /// the range seemingly randomly, whereas this lists seven.
    button_associations: Vec<Option<u8>>,
}
#[derive(Copy, Clone, Debug)]
struct LibinputGroupInfo {
    num_modes: u8,
    /// Beware, subject to TOCTOU bugs.
    current_mode: u8,
}
#[derive(Clone, Debug)]
enum LibinputInfo {
    Tool(LibinputToolInfo),
    GroupfulPad(LibinputGroupfulPadInfo),
    /// *Something* libinput, Mouse or keyboard or groupless pad or tablet or...
    /// Libinput provides no concrete way to distinguish :<
    SomethingElse,
}
impl LibinputInfo {
    /// Get the corresponding device type, or None if not known.
    fn device_type(&self) -> Option<DeviceType> {
        match self {
            Self::Tool(_) => Some(DeviceType::Tool(None)),
            Self::GroupfulPad(_) => Some(DeviceType::Pad),
            Self::SomethingElse => None,
        }
    }
}

#[derive(Copy, Clone)]
enum Transform {
    BiasScale { bias: f32, scale: f32 },
}
impl Transform {
    fn transform(self, value: f32) -> f32 {
        match self {
            Self::BiasScale { bias, scale } => (value + bias) * scale,
        }
    }
    fn transform_fixed(self, value: xinput::Fp3232) -> f32 {
        self.transform(fixed32_to_f32(value))
    }
    /// Create a transform that projects `[min, max]` onto `[0.0, new_max]`
    fn normalized(min: f32, max: f32, new_max: f32) -> Self {
        Self::BiasScale {
            bias: -min,
            scale: new_max / (max - min),
        }
    }
}

#[derive(Copy, Clone)]
struct AxisInfo {
    // Where in the valuator array is this?
    index: u16,
    // How to adapt the numeric value to octotablet's needs?
    transform: Transform,
}

#[derive(Eq, PartialEq, Clone, Copy)]
enum Phase {
    In,
    Down,
    Out,
}

/// Contains the metadata for translating a device's events to octotablet events,
/// as well as the x11 specific state required to emulate certain events.
struct ToolInfo {
    distance: Option<AxisInfo>,
    pressure: Option<AxisInfo>,
    roll: Option<AxisInfo>,
    tilt: [Option<AxisInfo>; 2],
    wheel: Option<AxisInfo>,
    /// The tablet this tool belongs to, based on heuristics.
    /// When "In" is fired, this is the device to reference, because X doesn't provide
    /// such info.
    /// (tool -> tablet relationship is one-to-one-or-less in xinput instead of one-to-one-or-more as we expect)
    tablet: ID,
    phase: Phase,
    /// The master cursor. Grab this device when this cursor Enters, release it when it
    /// leaves.
    master_pointer: u16,
    /// The master keyboard associated with the master pointer.
    master_keyboard: u16,
    // A change has occured on this pump that requires a frame event at this time.
    // (pose, button, enter, ect)
    frame_pending: Option<Timestamp>,
    last_interaction: Option<Timestamp>,
}
impl ToolInfo {
    fn take_timeout(&mut self, now: Timestamp) -> bool {
        let Some(interaction) = self.last_interaction else {
            return false;
        };

        if interaction > now {
            return false;
        }

        let diff = now - interaction;

        if diff >= TOOL_TIMEOUT_MS {
            self.last_interaction = None;
            true
        } else {
            false
        }
    }
    /// Set the current phase of interaction, emitting any needed events to get to that state.
    fn set_phase(&mut self, self_id: ID, phase: Phase, events: &mut Vec<raw::Event<ID>>) {
        enum Transition {
            In,
            Down,
            Out,
            Up,
        }
        // Find the transitions that need to occur, in order.
        #[allow(clippy::match_same_arms)]
        let changes: &[_] = match (self.phase, phase) {
            (Phase::Down, Phase::Out) => &[Transition::Up, Transition::Out],
            (Phase::Down, Phase::In) => &[Transition::Up],
            (Phase::Down, Phase::Down) => &[],
            (Phase::In, Phase::Out) => &[Transition::Out],
            (Phase::In, Phase::In) => &[],
            (Phase::In, Phase::Down) => &[Transition::Down],
            (Phase::Out, Phase::Out) => &[],
            (Phase::Out, Phase::In) => &[Transition::In],
            (Phase::Out, Phase::Down) => &[Transition::In, Transition::Down],
        };
        self.phase = phase;

        for change in changes {
            events.push(raw::Event::Tool {
                tool: self_id,
                event: match change {
                    Transition::In => raw::ToolEvent::In {
                        tablet: self.tablet,
                    },
                    Transition::Out => raw::ToolEvent::Out,
                    Transition::Down => raw::ToolEvent::Down,
                    Transition::Up => raw::ToolEvent::Up,
                },
            });
        }
    }
    /// If the tool is Out, move it In. no effect if down or in already.
    fn ensure_in(&mut self, self_id: ID, events: &mut Vec<raw::Event<ID>>) {
        if self.phase == Phase::Out {
            self.phase = Phase::In;

            events.push(raw::Event::Tool {
                tool: self_id,
                event: raw::ToolEvent::In {
                    tablet: self.tablet,
                },
            });
        }
    }
}

struct RingInfo {
    axis: AxisInfo,
    last_interaction: Option<Timestamp>,
}
impl RingInfo {
    /// Returns true if the ring was interacted but the interaction timed out.
    /// When true, emit an Out event.
    fn take_timeout(&mut self, now: Timestamp) -> bool {
        let Some(interaction) = self.last_interaction else {
            return false;
        };

        if interaction > now {
            return false;
        }

        let diff = now - interaction;

        if diff >= RING_TIMEOUT_MS {
            self.last_interaction = None;
            true
        } else {
            false
        }
    }
}
struct PadInfo {
    ring: Option<RingInfo>,
    /// The tablet this tool belongs to, based on heuristics, or Dummy.
    tablet: ID,
}

fn tool_info_mut_from_device_id(
    id: u16,
    infos: &mut std::collections::BTreeMap<ID, ToolInfo>,
    now_generation: u16,
) -> Option<(ID, &mut ToolInfo)> {
    let non_zero_id = std::num::NonZero::<u16>::new(id)?;
    let id = ID::ID {
        generation: now_generation,
        device_id: non_zero_id,
    };

    infos.get_mut(&id).map(|info| (id, info))
}
fn pad_info_mut_from_device_id(
    id: u16,
    infos: &mut std::collections::BTreeMap<ID, PadInfo>,
    now_generation: u16,
) -> Option<(ID, &mut PadInfo)> {
    let non_zero_id = std::num::NonZero::<u16>::new(id)?;
    let id = ID::ID {
        generation: now_generation,
        device_id: non_zero_id,
    };

    infos.get_mut(&id).map(|info| (id, info))
}
fn pad_mut_from_device_id(
    id: u16,
    infos: &mut [crate::pad::Pad],
    now_generation: u16,
) -> Option<(ID, &mut crate::pad::Pad)> {
    let non_zero_id = std::num::NonZero::<u16>::new(id)?;
    let id = ID::ID {
        generation: now_generation,
        device_id: non_zero_id,
    };

    infos
        .iter_mut()
        .find(|pad| *pad.internal_id.unwrap_xinput2() == id)
        .map(|info| (id, info))
}

pub struct Manager {
    conn: x11rb::rust_connection::RustConnection,
    tool_infos: std::collections::BTreeMap<ID, ToolInfo>,
    tools: Vec<crate::tool::Tool>,
    pad_infos: std::collections::BTreeMap<ID, PadInfo>,
    pads: Vec<crate::pad::Pad>,
    tablets: Vec<crate::tablet::Tablet>,
    events: Vec<crate::events::raw::Event<ID>>,
    window: x11rb::protocol::xproto::Window,
    atoms: strings::Atoms,
    // What is the most recent event timecode?
    server_time: Timestamp,
    /// Device ID generation. Increment when one or more devices is removed in a frame.
    device_generation: u16,
}

impl Manager {
    pub fn build_window(_opts: crate::Builder, window: std::num::NonZeroU32) -> Self {
        let window = window.get();

        let (conn, _screen) = x11rb::connect(None).unwrap();
        // Check we have XInput2 and get it's version.
        conn.extension_information(xinput::X11_EXTENSION_NAME)
            .unwrap()
            .unwrap();

        let version = conn.xinput_xi_query_version(2, 4).unwrap().reply().unwrap();

        println!(
            "Server supports v{}.{}",
            version.major_version, version.minor_version
        );

        assert!(version.major_version >= 2);

        let hierarchy_interest = xinput::EventMask {
            deviceid: XI_ALL_DEVICES,
            mask: [
                // device add/remove/enable/disable.
                xinput::XIEventMask::HIERARCHY,
            ]
            .into(),
        };

        // Ask for notification of device added/removed/reassigned. This is done before
        // enumeration to avoid TOCTOU bug, but now the bug is in the opposite direction-
        // We could enumerate a device *and* recieve an added message for it, or get a removal
        // message for devices we never met. Beware!
        conn.xinput_xi_select_events(window, std::slice::from_ref(&hierarchy_interest))
            .unwrap()
            .check()
            .unwrap();

        // Future note for how to access core events, if needed.
        // "XSelectInput" is just a wrapper over this, funny!
        // https://github.com/mirror/libX11/blob/ff8706a5eae25b8bafce300527079f68a201d27f/src/SelInput.c#L33
        /*conn.change_window_attributes(
            window,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux {
                event_mask: Some(x11rb::protocol::xproto::EventMask::NO_EVENT),
                ..Default::default()
            },
        )
        .unwrap()
        .check()
        .unwrap();*/

        let mut this = Self {
            atoms: strings::intern(&conn).unwrap(),
            conn,
            tool_infos: std::collections::BTreeMap::new(),
            pad_infos: std::collections::BTreeMap::new(),
            tools: vec![],
            pads: vec![],
            events: vec![],
            tablets: vec![],
            window,
            server_time: 0,
            device_generation: 0,
        };

        // Poll for devices.
        this.repopulate();
        this
    }
    /// Close bound devices and enumerate server devices. Generates user-facing info structs and emits
    /// change events accordingly.
    #[allow(clippy::too_many_lines)]
    fn repopulate(&mut self) {
        // Fixme, hehe. We need to a) keep these alive for the next pump, and b) appropriately
        // report adds/removes.
        self.tools.clear();
        self.tablets.clear();
        self.pads.clear();
        self.tool_infos.clear();
        self.pad_infos.clear();

        // Tools ids to bulk-enable events on.
        let mut tool_listen_events = vec![];
        // Pad ids to bulk-enable events on.
        let mut pad_listen_events = vec![];

        // Device detection strategy:
        // * Look for wacom-specific type field.
        //   * If found, gather up all the wacom-specific properties.
        //   * Not so "device agnostic" anymore, are ya, octotablet? :pensive:
        // * Look for xwayland name
        // * Look for generic tablet-like names ("foobar Stylus/Eraser/Pad" and the matching "foobar" device)
        //   * If found, try to also parse the hardware ID, which is often found as hexx in parenthesis at the end.
        //   * Will likely ident tablets as tools, rely on the fact that tablets don't have valuators to filter.
        // * Look for stylus-like capabilities
        //   * Abs X, Y OR Abs <non-wheel axis>
        //   * This *will* falsely detect other devices, like pads. perhaps we will have to wait for an
        //     event to determine whether it's a pad or tool.
        let device_infos = self
            .conn
            .xinput_xi_query_device(XI_ALL_DEVICES)
            .unwrap()
            .reply()
            .unwrap()
            .infos;

        for device in &device_infos {
            // Only look at enabled, non-master devices.
            if !device.enabled
                || matches!(
                    device.type_,
                    xinput::DeviceType::MASTER_KEYBOARD | xinput::DeviceType::MASTER_POINTER
                )
            {
                continue;
            }

            // Zero is a special value (ALL_DEVICES), and can't be used by a device.
            let Some(nonzero_id) = std::num::NonZero::new(device.deviceid) else {
                continue;
            };
            let octotablet_id = ID::ID {
                generation: self.device_generation,
                device_id: nonzero_id,
            };

            // Wacom driver provides a very definitive yes or no to whether that driver is in use.
            let wacom = self.try_query_wacom(device.deviceid);
            // Libinput is much more fuzzy. Don't bother if wacom was found.
            // Technically this is better expressed through Option<Either<wacom, libinput>> but ehhhh
            let libinput = if wacom.is_none() {
                self.try_query_libinput(device.deviceid)
            } else {
                None
            };

            // Otherwise, we don't immediately fail. other versions of the drivers or other drivers
            // entirely (digimend, udev, old libinput) might still be available.
            // Fields from above are authoritative, but the information may be gathered by less clean
            // means uwu

            // UTF8 human-readable device name, which encodes some additional info sometimes.
            let raw_name = String::from_utf8_lossy(&device.name);
            // Apply heaps of heuristics to figure out what the heck this device is all about.
            let name_fields = guess_from_name(&raw_name);

            println!("===={raw_name}====\n Wacom - {wacom:#?}\n Libinput - {libinput:#?}\n Heuristic - {name_fields:#?}");

            // Combine our knowledge. Trusting drivers over guessed.
            // If we couldn't determine, then we can't use the device.
            // todo: determine from Classes, which is 10x more unreliable....
            let Some(device_type) = wacom
                .map(WacomInfo::device_type)
                // None from libinput could still be a groupless pad, fixme.
                .or(libinput.as_ref().and_then(LibinputInfo::device_type))
                .or(name_fields.device_type())
            else {
                continue;
            };

            // At this point, we're pretty sure this is a tool, pad, or tablet!

            match device_type {
                DeviceType::Tool(ty) => {
                    // It's a tool! Parse all relevant infos.

                    // There are many false posiives, namely mice being detected as tablet pointers and
                    // tablets being detected as pens. These are filtered based on valuator hueristics.

                    // We can only handle tools which have a parent.
                    // (and obviously they shouldn't be a keyboard.)
                    // Technically, a floating pointer can work for our needs,
                    // but we aren't provided with enough info to project Abs X/Abs Y to client logical pixels,
                    // so rely on the Master's x/y.
                    if device.type_ != xinput::DeviceType::SLAVE_POINTER {
                        continue;
                    }

                    // These both need a rename at the crate level.
                    let hardware_id = wacom
                        .and_then(WacomInfo::hardware_serial)
                        .or(libinput.as_ref().and_then(|libinput| match libinput {
                            LibinputInfo::Tool(t) => Some(t.hardware_serial?.get()),
                            _ => None,
                        }))
                        .map(|val| crate::tool::HardwareID(val.into()));
                    let wacom_id = wacom.and_then(WacomInfo::hardware_id).or(libinput
                        .as_ref()
                        .and_then(|libinput| match libinput {
                            LibinputInfo::Tool(t) => t.hardware_id,
                            _ => None,
                        }));

                    /*let tablet_id = name_fields.maybe_associated_tablet().and_then(|expected| {
                        // Find the device with the expected name, and return it's ID if found.
                        let tablet_info = device_infos
                            .iter()
                            .find(|info| info.name == expected.as_bytes())?;

                        Some(ID::ID {
                            generation: self.device_generation,
                            // 0 is a special value, this is infallible.
                            device_id: tablet_info.deviceid.try_into().unwrap(),
                        })
                    });*/

                    let mut octotablet_info = crate::tool::Tool {
                        internal_id: super::InternalID::XInput2(octotablet_id),
                        name: Some(raw_name.clone().into_owned()),
                        hardware_id,
                        wacom_id: wacom_id.map(Into::into),
                        tool_type: ty,
                        axes: crate::axis::FullInfo::default(),
                    };

                    let mut x11_info = ToolInfo {
                        pressure: None,
                        distance: None,
                        roll: None,
                        tilt: [None, None],
                        wheel: None,
                        tablet: ID::EmulatedTablet, //tablet_id.unwrap_or(ID::EmulatedTablet),
                        phase: Phase::Out,
                        master_pointer: device.attachment,
                        master_keyboard: device_infos
                            .iter()
                            .find_map(|q| {
                                // Find the info for the master pointer
                                if q.deviceid == device.attachment {
                                    // Look at the master pointer's attachment,
                                    // which is the associated master keyboard's ID.
                                    Some(q.attachment)
                                } else {
                                    None
                                }
                            })
                            // Above search should be infallible but I trust nothing at this point.
                            .unwrap_or_default(),
                        frame_pending: None,
                        last_interaction: None,
                    };

                    // If it is definitively a tool, start as true.
                    // Otherwise, look for tool-ish aspects. If false at the end, reject.
                    let mut looks_toolish = wacom
                        .is_some_and(|wacom| matches!(wacom, WacomInfo::Tool { .. }))
                        || libinput
                            .is_some_and(|libinput| matches!(libinput, LibinputInfo::Tool(_)));

                    // Look for axes!
                    for class in &device.classes {
                        if let Some(v) = class.data.as_valuator() {
                            if v.mode != xinput::ValuatorMode::ABSOLUTE {
                                continue;
                            };
                            // Weird case, that does happen in practice. :V
                            if v.min == v.max {
                                continue;
                            }
                            let Some(label) =
                                match_valuator_label(v.label, &self.atoms.absolute_axes)
                            else {
                                continue;
                            };

                            let min = fixed32_to_f32(v.min);
                            let max = fixed32_to_f32(v.max);

                            // Any absolute valuators.
                            // This excludes relative styluses with no additional features...
                            // ..pen-shaped-mice? are those a thing? hm.
                            looks_toolish = true;

                            match label {
                                ValuatorAxis::AbsX | ValuatorAxis::AbsY => (),
                                ValuatorAxis::AbsPressure => {
                                    // Scale and bias to [0,1].
                                    x11_info.pressure = Some(AxisInfo {
                                        index: v.number,
                                        transform: Transform::normalized(min, max, 1.0),
                                    });
                                    octotablet_info.axes.pressure =
                                        Some(crate::axis::NormalizedInfo { granularity: None });
                                }
                                ValuatorAxis::AbsDistance => {
                                    // Scale and bias to [0,1].
                                    x11_info.distance = Some(AxisInfo {
                                        index: v.number,
                                        transform: Transform::normalized(min, max, 1.0),
                                    });
                                    octotablet_info.axes.distance =
                                        Some(crate::axis::LengthInfo::Normalized(
                                            crate::axis::NormalizedInfo { granularity: None },
                                        ));
                                }
                                ValuatorAxis::AbsRz => {
                                    // Scale and bias to [0,TAU).
                                    // This may be biased to the wrong 0 angle, but octotablet
                                    // doesn't make hard guarantees about it anyway.
                                    x11_info.roll = Some(AxisInfo {
                                        index: v.number,
                                        transform: Transform::normalized(
                                            min,
                                            max,
                                            std::f32::consts::TAU,
                                        ),
                                    });
                                    octotablet_info.axes.roll =
                                        Some(crate::axis::CircularInfo { granularity: None });
                                }
                                ValuatorAxis::AbsTiltX => {
                                    // Seemingly always in degrees.
                                    let deg_to_rad = 1.0f32.to_radians();
                                    x11_info.tilt[0] = Some(AxisInfo {
                                        index: v.number,
                                        transform: Transform::BiasScale {
                                            bias: 0.0,
                                            scale: deg_to_rad,
                                        },
                                    });

                                    let min = min.to_radians();
                                    let max = max.to_radians();

                                    let new_info = crate::axis::Info {
                                        limits: Some(crate::axis::Limits {
                                            min: min.to_radians(),
                                            max: max.to_radians(),
                                        }),
                                        granularity: None,
                                    };

                                    // Set the limits, or if already set take the union of the limits.
                                    match &mut octotablet_info.axes.tilt {
                                        slot @ None => *slot = Some(new_info),
                                        Some(v) => match &mut v.limits {
                                            slot @ None => *slot = new_info.limits,
                                            Some(v) => {
                                                v.max = v.max.max(max);
                                                v.min = v.min.min(min);
                                            }
                                        },
                                    }
                                }
                                ValuatorAxis::AbsTiltY => {
                                    // Seemingly always in degrees.
                                    let deg_to_rad = 1.0f32.to_radians();
                                    x11_info.tilt[1] = Some(AxisInfo {
                                        index: v.number,
                                        transform: Transform::BiasScale {
                                            bias: 0.0,
                                            scale: deg_to_rad,
                                        },
                                    });

                                    let min = min.to_radians();
                                    let max = max.to_radians();

                                    let new_info = crate::axis::Info {
                                        limits: Some(crate::axis::Limits {
                                            min: min.to_radians(),
                                            max: max.to_radians(),
                                        }),
                                        granularity: None,
                                    };

                                    // Set the limits, or if already set take the union of the limits.
                                    match &mut octotablet_info.axes.tilt {
                                        slot @ None => *slot = Some(new_info),
                                        Some(v) => match &mut v.limits {
                                            slot @ None => *slot = new_info.limits,
                                            Some(v) => {
                                                v.max = v.max.max(max);
                                                v.min = v.min.min(min);
                                            }
                                        },
                                    }
                                }
                                ValuatorAxis::AbsWheel => {
                                    // uhh, i don't know. I have no hardware to test with.
                                }
                            }

                            // Resolution field is.. meaningless, I think. xwayland is the only server I have
                            // seen that even bothers to fill it out, and even there it's weird.
                        }
                    }
                    // Picked up on name heuristics, but doesn't have anything that looks like a tool.
                    if !looks_toolish {
                        continue;
                    }

                    // Tablet mice always have rotation axis. This filters out standard mice which
                    // got wrongly picked up by our name hueristics.
                    if matches!(octotablet_info.tool_type, Some(crate::tool::Type::Mouse))
                        && x11_info.roll.is_none()
                    {
                        continue;
                    }

                    tool_listen_events.push(device.deviceid);
                    self.tools.push(octotablet_info);
                    self.tool_infos.insert(octotablet_id, x11_info);
                }
                DeviceType::Pad => {
                    let libinput = libinput.as_ref().and_then(|libinput| match libinput {
                        LibinputInfo::GroupfulPad(groupful) => Some(groupful),
                        _ => None,
                    });

                    // Second ring has no label. really stupid. So, the only way to know if there is two is
                    // if libinput says there is through nonstandard means. :V
                    let is_dual_ring =
                        libinput.is_some_and(|info| info.ring_associations.len() >= 2);

                    let mut buttons = 0;
                    let mut ring_axis = None;
                    for class in &device.classes {
                        match &class.data {
                            xinput::DeviceClassData::Button(b) => {
                                buttons = b.num_buttons();
                            }
                            xinput::DeviceClassData::Valuator(v) => {
                                // Look for and bind an "Abs Wheel" which is our ring.
                                if v.mode != xinput::ValuatorMode::ABSOLUTE {
                                    continue;
                                }
                                // This fails to detect xwayland's Ring axis, since it is present but not labeled.
                                // However, in my testing, it's borked anyways and always returns position 71.
                                let Some(label) =
                                    match_valuator_label(v.label, &self.atoms.absolute_axes)
                                else {
                                    continue;
                                };
                                if matches!(label, ValuatorAxis::AbsWheel) {
                                    // Remap to [0, TAU], clockwise from logical north.
                                    if v.min == v.max {
                                        continue;
                                    }
                                    let min = fixed32_to_f32(v.min);
                                    let max = fixed32_to_f32(v.max);
                                    ring_axis = Some(AxisInfo {
                                        index: v.number,
                                        transform: Transform::normalized(
                                            min,
                                            max,
                                            std::f32::consts::TAU,
                                        ),
                                    });
                                }
                            }
                            _ => (),
                        }
                    }
                    if buttons == 0 && ring_axis.is_none() {
                        // This pad has no functionality for us.
                        continue;
                    }

                    let mut rings = vec![];
                    if ring_axis.is_some() {
                        rings.push(crate::pad::Ring {
                            granularity: None,
                            internal_id: crate::platform::InternalID::XInput2(octotablet_id),
                        });
                    };
                    // X11 has no concept of groups (i don't .. think?)
                    // So make a single group that owns everything.
                    let group = crate::pad::Group {
                        buttons: (0..buttons).map(Into::into).collect::<Vec<_>>(),
                        feedback: None,
                        internal_id: crate::platform::InternalID::XInput2(octotablet_id),
                        mode_count: libinput
                            .and_then(|info| Some(info.groups.first()?.num_modes))
                            .map(u32::from)
                            .and_then(std::num::NonZero::new),
                        rings,
                        strips: vec![],
                    };
                    self.pads.push(crate::pad::Pad {
                        internal_id: crate::platform::InternalID::XInput2(octotablet_id),
                        total_buttons: buttons.into(),
                        groups: vec![group],
                    });

                    pad_listen_events.push(device.deviceid);

                    // Find the tablet this belongs to.
                    /*let tablet = name_fields.maybe_associated_tablet().and_then(|expected| {
                        // Find the device with the expected name, and return it's ID if found.
                        let tablet_info = device_infos
                            .iter()
                            .find(|info| info.name == expected.as_bytes())?;

                        Some(ID::ID {
                            generation: self.device_generation,
                            // 0 is ALL_DEVICES, this is infallible.
                            device_id: tablet_info.deviceid.try_into().unwrap(),
                        })
                    });*/

                    self.pad_infos.insert(
                        octotablet_id,
                        PadInfo {
                            ring: ring_axis.map(|ring_axis| RingInfo {
                                axis: ring_axis,
                                last_interaction: None,
                            }),
                            tablet: ID::EmulatedTablet, //tablet.unwrap_or(ID::EmulatedTablet),
                        },
                    );
                }
            }
        }

        // True if any tablet refers to a non-existant device.
        let mut wants_dummy_tablet = false;
        for tool in self.tool_infos.values_mut() {
            // Look through associated tablet ids. If any refers to a non-existant device, refer
            // instead to a dummy device.
            if let ID::ID {
                device_id: desired_tablet,
                ..
            } = tool.tablet
            {
                if !self
                    .tablets
                    .iter()
                    .any(|tablet| match *tablet.internal_id.unwrap_xinput2() {
                        ID::ID { device_id, .. } => device_id == desired_tablet,
                        ID::EmulatedTablet => false,
                    })
                {
                    tool.tablet = ID::EmulatedTablet;
                }
            }

            if tool.tablet == ID::EmulatedTablet {
                wants_dummy_tablet = true;
            }
        }
        for (id, pad) in &mut self.pad_infos {
            // Look through associated tablet ids. If any refers to a non-existant device, refer
            // instead to a dummy device.
            if let ID::ID {
                device_id: desired_tablet,
                ..
            } = pad.tablet
            {
                if !self
                    .tablets
                    .iter()
                    .any(|tablet| match *tablet.internal_id.unwrap_xinput2() {
                        ID::ID { device_id, .. } => device_id == desired_tablet,
                        ID::EmulatedTablet => false,
                    })
                {
                    wants_dummy_tablet = true;
                }
            }

            if pad.tablet == ID::EmulatedTablet {
                wants_dummy_tablet = true;
            }

            // In x11, pads cannot roam between tablets. Eagerly announce their attachment just once.
            // FIXME: on initial device enumeration these are lost due to `events.clear()` in `pump`.
            self.events.push(raw::Event::Pad {
                pad: *id,
                event: raw::PadEvent::Enter { tablet: pad.tablet },
            });
        }

        if wants_dummy_tablet {
            // So.... xinput doesn't have the same "Tablet owns pads and tools"
            // hierarchy as we do. When we associate tools with tablets, we need a tablet
            // to bind it to, but xinput does not necessarily provide one.

            // Wacom tablets and the DECO-01 use a consistent naming scheme, where tools are called
            // <Tablet name> {Pen, Eraser} (hardware id), which we can use to extract such information.
            self.tablets.push(crate::tablet::Tablet {
                internal_id: super::InternalID::XInput2(ID::EmulatedTablet),
                name: Some(EMULATED_TABLET_NAME.to_owned()),
                usb_id: None,
            });
        }

        // Skip if nothing to enable. (Avoids server error)
        if tool_listen_events.is_empty() {
            return;
        }

        // Tool events:
        let mut interest = tool_listen_events
            .into_iter()
            .map(|deviceid| {
                xinput::EventMask {
                    deviceid,
                    mask: [
                        // Barrel and tip buttons
                        xinput::XIEventMask::BUTTON_PRESS
                        | xinput::XIEventMask::BUTTON_RELEASE
                        // Cursor entering and leaving client area. Doesn't work,
                        // perhaps it's for master pointers only.
                        // | xinput::XIEventMask::ENTER
                        // | xinput::XIEventMask::LEAVE
                        // Touches user-defined pointer barrier
                        // | xinput::XIEventMask::BARRIER_HIT
                        // | xinput::XIEventMask::BARRIER_LEAVE
                        // Axis movement
                        // since XI2.4, RAW_MOTION should work here, and give us events regardless
                        // of grab state. it does not work. COol i love this API
                        | xinput::XIEventMask::MOTION

                        // property change. The only properties we look at are static.
                        // | xinput::XIEventMask::PROPERTY
                        // Sent when a different device is controlling a master (dont care)
                        // or when a physical device changes it's properties (do care)
                        | xinput::XIEventMask::DEVICE_CHANGED,
                    ]
                    .into(),
                }
            })
            .collect::<Vec<_>>();

        // Pad events:
        interest.extend(pad_listen_events.into_iter().map(|deviceid| {
            xinput::EventMask {
                deviceid,
                mask: [
                    // Barrel and tip buttons
                    xinput::XIEventMask::BUTTON_PRESS
                | xinput::XIEventMask::BUTTON_RELEASE
                // Axis movement, for pads this is the Ring (plural?)
                | xinput::XIEventMask::MOTION
                // property change. The only properties we look at are static.
                // | xinput::XIEventMask::PROPERTY
                // Sent when a different device is controlling a master (dont care)
                // or when a physical device changes it's properties (do care)
                | xinput::XIEventMask::DEVICE_CHANGED,
                ]
                .into(),
            }
        }));

        // Pointer events:
        interest.push(xinput::EventMask {
            deviceid: XI_ALL_MASTER_DEVICES,
            mask: [
                // Pointer coming into and out of our client
                xinput::XIEventMask::ENTER
                    | xinput::XIEventMask::LEAVE
                    // Keyboard focus coming into and out of our client.
                    | xinput::XIEventMask::FOCUS_IN
                    | xinput::XIEventMask::FOCUS_OUT,
            ]
            .into(),
        });

        self.conn
            .xinput_xi_select_events(self.window, &interest)
            .unwrap()
            .check()
            .unwrap();
    }
    fn try_query_wacom(&self, deviceid: u16) -> Option<WacomInfo> {
        use crate::tool::Type;
        let atoms = &self.atoms.wacom;
        // We assume that if this property is missing or malformed then it's not wacom.
        let ty = {
            let type_atom = self
                .conn
                .xinput_xi_get_property(
                    deviceid,
                    false,
                    atoms.prop_tool_type.get(),
                    XI_ANY_PROPERTY_TYPE,
                    0,
                    1,
                )
                .unwrap()
                .reply()
                .ok()?;
            let type_atom = *type_atom.items.as_data32()?.first()?;
            let ty = std::num::NonZero::new(type_atom)?;

            if ty == atoms.type_stylus {
                DeviceType::Tool(Some(Type::Pen))
            } else if ty == atoms.type_eraser {
                DeviceType::Tool(Some(Type::Eraser))
            } else if ty == atoms.type_pad {
                DeviceType::Pad
            } else if ty == atoms.type_cursor {
                DeviceType::Tool(Some(Type::Mouse))
            } else {
                return None;
            }
        };

        // This one however, we leave optional.
        let try_fetch_ids = || -> Option<WacomIDs> {
            let &[
                tablet_id,
                old_serial,
                old_hardware_id,
                /*_new_serial, _new_hardware_id*/
                ..
            ] = self
                .conn
                .xinput_xi_get_property(
                    deviceid,
                    false,
                    atoms.prop_serial_ids.get(),
                    XI_ANY_PROPERTY_TYPE,
                    0,
                    3,
                )
                .unwrap()
                .reply()
                .ok()?
                .items
                .as_data32()?
                .as_slice()
            else {
                return None;
            };

            Some(WacomIDs {
                tablet_id,
                hardware_id: old_hardware_id,
                hardware_serial: old_serial,
            })
        };

        let ids = try_fetch_ids();

        Some(match ty {
            DeviceType::Pad => WacomInfo::Pad { ids },
            DeviceType::Tool(Some(ty)) => WacomInfo::Tool { ty, ids },
            // None tool type is never generated.
            _ => unreachable!(),
        })
    }
    fn try_query_libinput(&self, deviceid: u16) -> Option<LibinputInfo> {
        let atoms = &self.atoms.libinput;

        Some(if let Some(tool) = self.try_query_libinput_tool(deviceid) {
            LibinputInfo::Tool(tool)
        } else if let Some(groupful_pad) = self.try_query_libinput_groupful_pad(deviceid) {
            LibinputInfo::GroupfulPad(groupful_pad)
        } else {
            // Check an always-present property to see if this is even a libinput device.
            let is_libinput = self
                .conn
                .xinput_xi_get_property(
                    deviceid,
                    false,
                    atoms.prop_heartbeat.get(),
                    XI_ANY_PROPERTY_TYPE,
                    0,
                    0,
                )
                .unwrap()
                .reply()
                .ok()?
                .type_
                // if Type == None atom, property doesn't exist.
                != 0;

            if is_libinput {
                LibinputInfo::SomethingElse
            } else {
                return None;
            }
        })
    }
    fn try_query_libinput_tool(&self, deviceid: u16) -> Option<LibinputToolInfo> {
        let atoms = &self.atoms.libinput;

        // ALWAYS present when a libinput tablet tool, thank goodness! Peter Hutterer you have saved me.
        // This requires a fairly recent version of the driver, but hopefully fallback device detection will
        // make it work still. (namely, the stringified field as part of the name is much much older)
        // https://gitlab.freedesktop.org/xorg/driver/xf86-input-libinput/-/blob/master/src/xf86libinput.c?ref_type=heads#L6640
        let hardware_serial = *self
            .conn
            .xinput_xi_get_property(
                deviceid,
                false,
                atoms.prop_tool_serial.get(),
                XI_ANY_PROPERTY_TYPE,
                0,
                1,
            )
            .unwrap()
            .reply()
            .ok()?
            .items
            .as_data32()?
            .first()?;

        // This one is optional, however.
        let hardware_id = self
            .conn
            .xinput_xi_get_property(
                deviceid,
                false,
                atoms.prop_tool_id.get(),
                XI_ANY_PROPERTY_TYPE,
                0,
                1,
            )
            .unwrap()
            .reply()
            .ok()
            .and_then(|repl| repl.items.as_data32()?.first().copied());

        Some(LibinputToolInfo {
            // Zero is special None value.
            hardware_serial: hardware_serial.try_into().ok(),
            hardware_id,
        })
    }
    fn try_query_libinput_groupful_pad(&self, deviceid: u16) -> Option<LibinputGroupfulPadInfo> {
        let atoms = &self.atoms.libinput;

        let groups = {
            let group_modes_available_reply = self
                .conn
                .xinput_xi_get_property(
                    deviceid,
                    false,
                    atoms.prop_pad_group_modes_available.get(),
                    XI_ANY_PROPERTY_TYPE,
                    0,
                    // Len, in 4-byte units.
                    LIBINPUT_MAX_GROUPS.div_ceil(4),
                )
                .unwrap()
                .reply()
                .ok()?;
            let num_groups = group_modes_available_reply.num_items;
            let group_modes_available = group_modes_available_reply.items.as_data8()?;
            let group_current_mode_reply = self
                .conn
                .xinput_xi_get_property(
                    deviceid,
                    false,
                    atoms.prop_pad_group_current_modes.get(),
                    XI_ANY_PROPERTY_TYPE,
                    0,
                    // Len, in 4-byte units.
                    num_groups.div_ceil(4),
                )
                .unwrap()
                .reply()
                .ok()?;
            let group_current_mode = group_current_mode_reply.items.as_data8()?;

            group_modes_available
                .iter()
                // poor behavior if lengths mismatched. That's invalid anyway.
                .zip(group_current_mode)
                .map(|(&avail, &cur)| LibinputGroupInfo {
                    current_mode: cur,
                    num_modes: avail,
                })
                .collect::<Vec<_>>()
        };

        // Fetch associations from the given property name and max item count.
        let fetch_associations = |prop: strings::Atom, max: u32| -> Vec<Option<u8>> {
            self.conn
                .xinput_xi_get_property(
                    deviceid,
                    false,
                    prop.get(),
                    XI_ANY_PROPERTY_TYPE,
                    0,
                    // Len, in 4-byte units.
                    max.div_ceil(4),
                )
                .unwrap()
                .reply()
                .ok()
                .as_ref()
                .and_then(|repl| repl.items.as_data8().map(Vec::as_slice))
                // If not found, empty slice.
                .unwrap_or_default()
                .iter()
                .map(|&association| {
                    // Signedness is not reported by the reply type system.
                    // note that association is actually i8, but negatives are None.
                    #[allow(clippy::cast_possible_wrap)]
                    if (association as i8).is_negative() || usize::from(association) > groups.len()
                    {
                        None
                    } else {
                        Some(association)
                    }
                })
                .collect()
        };

        let ring_associations = fetch_associations(atoms.prop_pad_ring_groups, LIBINPUT_MAX_RINGS);
        let strip_associations =
            fetch_associations(atoms.prop_pad_strip_groups, LIBINPUT_MAX_STRIPS);
        let button_associations =
            fetch_associations(atoms.prop_pad_button_groups, LIBINPUT_MAX_BUTTONS);

        Some(LibinputGroupfulPadInfo {
            groups,
            strip_associations,
            ring_associations,
            button_associations,
        })
    }
    fn parent_left(&mut self, master: u16, time: Timestamp) {
        // Release tools.
        for (&id, tool) in &mut self.tool_infos {
            let is_child = tool.master_pointer == master || tool.master_keyboard == master;
            if !is_child {
                continue;
            }

            let was_in = matches!(tool.phase, Phase::In | Phase::Down);

            if was_in {
                // Emit frame for previous events before sending more
                if let Some(last_time) = tool.frame_pending.replace(time) {
                    if last_time != time {
                        self.events.push(raw::Event::Tool {
                            tool: id,
                            event: raw::ToolEvent::Frame(Some(crate::events::FrameTimestamp(
                                std::time::Duration::from_millis(last_time.into()),
                            ))),
                        });
                    }
                }
            }

            tool.set_phase(id, Phase::Out, &mut self.events);
        }
    }
    fn pre_frame_cleanup(&mut self) {
        self.events.clear();
    }
    fn post_frame_cleanup(&mut self) {
        // Emit emulated ring outs.
        for (&id, pad) in &mut self.pad_infos {
            if let Some(ring) = &mut pad.ring {
                if ring.take_timeout(self.server_time) {
                    self.events.push(raw::Event::Pad {
                        pad: id,
                        event: raw::PadEvent::Group {
                            group: id,
                            event: raw::PadGroupEvent::Ring {
                                ring: id,
                                event: crate::events::TouchStripEvent::Up,
                            },
                        },
                    });
                }
            }
        }
        // Emit pending tool frames and emulate Out from timeout.
        for (&id, tool) in &mut self.tool_infos {
            if let Some(time) = tool.frame_pending.take() {
                self.events.push(raw::Event::Tool {
                    tool: id,
                    event: raw::ToolEvent::Frame(Some(crate::events::FrameTimestamp(
                        std::time::Duration::from_millis(time.into()),
                    ))),
                });
            }
            if tool.take_timeout(self.server_time) {
                tool.set_phase(id, Phase::Out, &mut self.events);
            }
        }
    }
}

impl super::PlatformImpl for Manager {
    #[allow(clippy::too_many_lines)]
    fn pump(&mut self) -> Result<(), crate::PumpError> {
        self.pre_frame_cleanup();
        let mut has_repopulated = false;

        while let Ok(Some(event)) = self.conn.poll_for_event() {
            use x11rb::protocol::Event;
            match event {
                // Devices added, removed, reassigned, etc.
                Event::XinputHierarchy(h) => {
                    self.server_time = h.time;
                    // for device in h.infos {
                    // let Ok(device_id) = u8::try_from(device.deviceid) else {
                    //    continue;
                    //};
                    // if let Some((id, info)) = tool_info_mut_from_device_id(
                    //     device_id,
                    //     &mut self.tool_infos,
                    //     self.device_generation,
                    // ) {}
                    // if let Some((id, info)) = pad_info_mut_from_device_id(
                    //     device_id,
                    //     &mut self.tool_infos,
                    //     self.device_generation,
                    // ) {}
                    // }
                    // The event does not necessarily reflect *all* changes, the spec specifically says
                    // that the client should probably just rescan. lol
                    if !has_repopulated {
                        has_repopulated = true;
                        self.repopulate();
                    }
                }
                Event::XinputDeviceChanged(c) => {
                    // We only care if a physical device's capabilities changed.
                    if c.reason != xinput::ChangeReason::DEVICE_CHANGE {
                        continue;
                    }
                }
                Event::XinputProperty(wawa) => {
                    // Listen to this to determine when an input-wacom tool goes in and out of prox
                    // (through "new serial/hw id" becoming zeroed)
                    // and for input-libinput pad mode switch.
                }
                // xwayland fails to emit Leave/Enter when the cursor is warped to/from another window
                // by a proximity in event. However, it emits a FocusOut/FocusIn for the associated
                // master keyboard in that case, which we can use to emulate.
                // On a genuine X11 server this causes the device release logic to happen twice.
                // Could we just always rely on FocusOut, or would that add more edge cases?
                Event::XinputLeave(leave) | Event::XinputFocusOut(leave) => {
                    self.server_time = leave.time;
                    // MASTER POINTER ONLY. Cursor has left the client bounds.
                    self.parent_left(leave.deviceid, leave.time);
                }
                Event::XinputEnter(enter) | Event::XinputFocusIn(enter) => {
                    self.server_time = enter.time;
                    // MASTER POINTER ONLY. Cursor has entered client bounds.
                }
                Event::XinputButtonPress(e) | Event::XinputButtonRelease(e) => {
                    // Tool buttons.
                    self.server_time = e.time;
                    if e.flags
                        .intersects(xinput::PointerEventFlags::POINTER_EMULATED)
                    {
                        // Key press emulation from scroll wheel.
                        continue;
                    }

                    let pressed = e.event_type == xinput::BUTTON_PRESS_EVENT;

                    if let Some((id, tool)) = tool_info_mut_from_device_id(
                        e.deviceid,
                        &mut self.tool_infos,
                        self.device_generation,
                    ) {
                        let Ok(button_idx) = u16::try_from(e.detail) else {
                            continue;
                        };
                        // Emulate In event if currently out.
                        tool.ensure_in(id, &mut self.events);

                        // Detail gives the "button index".
                        match button_idx {
                            // Doesn't occur, I don't think.
                            0 => (),
                            // Tip button
                            1 => {
                                tool.set_phase(
                                    id,
                                    if pressed { Phase::Down } else { Phase::In },
                                    &mut self.events,
                                );
                            }
                            // Other (barrel) button.
                            _ => {
                                self.events.push(raw::Event::Tool {
                                    tool: id,
                                    event: raw::ToolEvent::Button {
                                        button_id: crate::platform::ButtonID::XInput2(
                                            // Already checked != 0
                                            button_idx.try_into().unwrap(),
                                        ),
                                        pressed,
                                    },
                                });
                            }
                        }
                    } else if let Some((id, pad)) =
                        pad_mut_from_device_id(e.deviceid, &mut self.pads, self.device_generation)
                    {
                        let button_idx = e.detail;
                        if button_idx == 0 || button_idx > pad.total_buttons {
                            // Okay, there's a weird off-by-one here, that even throws off the `xinput` debug
                            // utility. My Intuos Pro S reports 11 buttons, but the maximum button index is.... 11,
                            // which is clearly invalid. Silly.
                            // I interpret this as it actually being [1, max_button] instead of [0, max_button)
                            continue;
                        }

                        self.events.push(raw::Event::Pad {
                            pad: id,
                            event: raw::PadEvent::Button {
                                // Shift 1-based to 0-based indexing.
                                button_idx: button_idx - 1,
                                // "Pressed" event code.
                                pressed,
                            },
                        });
                    };
                }
                Event::XinputMotion(m) => {
                    // Tool valuators and pad rings.
                    self.server_time = m.time;

                    let valuator_fetch = |idx: u16| -> Option<xinput::Fp3232> {
                        // Check that it's not masked out-
                        let word_idx = idx / u32::BITS as u16;
                        let bit_idx = idx % u32::BITS as u16;
                        let word = m.valuator_mask.get(usize::from(word_idx))?;

                        // This valuator did not report, value is undefined.
                        if word & (1 << u32::from(bit_idx)) == 0 {
                            return None;
                        }

                        // Quirk (why can't we have nice things)
                        // Pad rings report a mask that the valuator is in the 5th position,
                        // but then only report a single valuator at idx 0, which contains the value.
                        // The spec states that this is supposed to be a non-sparse array. oh well.
                        if m.axisvalues.len() == 1 {
                            return m.axisvalues.first().copied();
                        }

                        // Fetch it!
                        m.axisvalues.get(usize::from(idx)).copied()
                    };

                    let mut try_tool = || -> Option<()> {
                        let (id, tool) = tool_info_mut_from_device_id(
                            m.deviceid,
                            &mut self.tool_infos,
                            self.device_generation,
                        )?;

                        // About to emit events. Emit frame if the time differs.
                        if let Some(last_time) = tool.frame_pending.replace(m.time) {
                            if last_time != m.time {
                                self.events.push(raw::Event::Tool {
                                    tool: id,
                                    event: raw::ToolEvent::Frame(Some(
                                        crate::events::FrameTimestamp(
                                            std::time::Duration::from_millis(last_time.into()),
                                        ),
                                    )),
                                });
                            }
                        }

                        tool.ensure_in(id, &mut self.events);

                        // Access valuators, and map them to our range for the associated axis.
                        let pressure = tool
                            .pressure
                            .and_then(|axis| {
                                Some(axis.transform.transform_fixed(valuator_fetch(axis.index)?))
                            })
                            .and_then(crate::util::NicheF32::new_some)
                            .unwrap_or(crate::util::NicheF32::NONE);
                        let tilt_x = tool.tilt[0].and_then(|axis| {
                            Some(axis.transform.transform_fixed(valuator_fetch(axis.index)?))
                        });
                        let tilt_y = tool.tilt[1].and_then(|axis| {
                            Some(axis.transform.transform_fixed(valuator_fetch(axis.index)?))
                        });

                        self.events.push(raw::Event::Tool {
                            tool: id,
                            event: raw::ToolEvent::Pose(crate::axis::Pose {
                                // Seems to already be in logical space.
                                // Using this seems to be the "wrong" solution. It's the master's position,
                                // which gets funky when two tools are active under the same master.
                                position: [fixed16_to_f32(m.event_x), fixed16_to_f32(m.event_y)],
                                distance: crate::util::NicheF32::NONE,
                                pressure,
                                button_pressure: crate::util::NicheF32::NONE,
                                tilt: match (tilt_x, tilt_y) {
                                    (Some(x), Some(y)) => Some([x, y]),
                                    (Some(x), None) => Some([x, 0.0]),
                                    (None, Some(y)) => Some([0.0, y]),
                                    (None, None) => None,
                                },
                                roll: crate::util::NicheF32::NONE,
                                wheel: None,
                                slider: crate::util::NicheF32::NONE,
                                contact_size: None,
                            }),
                        });
                        Some(())
                    };
                    if try_tool().is_some() {
                        continue;
                    }
                    let mut try_pad = || -> Option<()> {
                        let (id, pad) = pad_info_mut_from_device_id(
                            m.deviceid,
                            &mut self.pad_infos,
                            self.device_generation,
                        )?;
                        let ring_info = pad.ring.as_mut()?;
                        let raw_valuator_value = valuator_fetch(ring_info.axis.index)?;
                        let transformed_valuator_value =
                            ring_info.axis.transform.transform_fixed(raw_valuator_value);

                        if ring_info.take_timeout(self.server_time) {
                            self.events.push(raw::Event::Pad {
                                pad: id,
                                event: raw::PadEvent::Group {
                                    group: id,
                                    event: raw::PadGroupEvent::Ring {
                                        ring: id,
                                        event: crate::events::TouchStripEvent::Up,
                                    },
                                },
                            });
                        }

                        if raw_valuator_value
                            == (xinput::Fp3232 {
                                integral: 0,
                                frac: 0,
                            })
                        {
                            // On release, this is snapped back to zero, but zero is also a valid value.

                            // Snapping back to zero makes this entirely useless for knob control (which is the primary
                            // purpose of the ring) so we take this little loss.
                            return None;
                        }

                        self.events.push(raw::Event::Pad {
                            pad: id,
                            event: raw::PadEvent::Group {
                                group: id,
                                event: raw::PadGroupEvent::Ring {
                                    ring: id,
                                    event: crate::events::TouchStripEvent::Pose(
                                        transformed_valuator_value,
                                    ),
                                },
                            },
                        });

                        self.events.push(raw::Event::Pad {
                            pad: id,
                            event: raw::PadEvent::Group {
                                group: id,
                                event: raw::PadGroupEvent::Ring {
                                    ring: id,
                                    event: crate::events::TouchStripEvent::Frame(Some(
                                        crate::events::FrameTimestamp(
                                            std::time::Duration::from_millis(
                                                self.server_time.into(),
                                            ),
                                        ),
                                    )),
                                },
                            },
                        });

                        ring_info.last_interaction = Some(self.server_time);

                        Some(())
                    };
                    let _ = try_pad();
                }
                // DeviceValuator, DeviceButton{Pressed, Released}, Proximity{In, Out} are red herrings
                // left over from XI 1.x and are never recieved. Don't fall for it!
                // It is strange, but XI 2 has no concept of proximity, even though XI 1 does.
                _ => (),
            }
        }

        self.post_frame_cleanup();

        Ok(())
    }
    fn raw_events(&self) -> super::RawEventsIter<'_> {
        super::RawEventsIter::XInput2(self.events.iter())
    }
    fn tablets(&self) -> &[crate::tablet::Tablet] {
        &self.tablets
    }
    fn pads(&self) -> &[crate::pad::Pad] {
        &self.pads
    }
    fn timestamp_granularity(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(1))
    }
    fn tools(&self) -> &[crate::tool::Tool] {
        &self.tools
    }
}
