use crate::events::raw;
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        xinput::{self, ConnectionExt},
        xproto::{ConnectionExt as _, Timestamp},
    },
};

/// If this many milliseconds since last ring interaction, emit an Out event.
const RING_TIMEOUT_MS: Timestamp = 200;
/// If this many milliseconds since last tool interaction, emit an Out event.
const TOOL_TIMEOUT_MS: Timestamp = 500;
// Some necessary constants not defined by x11rb:
const XI_ALL_DEVICES: u16 = 0;
const XI_ALL_MASTER_DEVICES: u16 = 1;
/// Magic timestamp signalling to the server "now".
const NOW_MAGIC: x11rb::protocol::xproto::Timestamp = 0;
// Strings are used to communicate the class of device, so we need a hueristic to
// find devices we are interested in and a transformation to a more well-documented enum.
// Strings used here are defined in X11/extensions/XI.h
/// X "device_type" atom for [`crate::tablet`]s...
const TYPE_TABLET: &str = "TABLET";
/// .. [`crate::pad`]s...
const TYPE_PAD: &str = "PAD";
/// .. [`crate::pad`]s also ?!?!?...
const TYPE_TOUCHPAD: &str = "TOUCHPAD";
/// ..stylus tips..
const TYPE_STYLUS: &str = "STYLUS";
/// and erasers!
const TYPE_ERASER: &str = "ERASER";
// Type "xwayland-pointer" is used for xwayland mice, styluses, erasers and... *squint* ...keyboards?
// The role could instead be parsed from it's user-facing device name:
// "xwayland-tablet-pad:<some value of mystery importance>"
// "xwayland-tablet eraser:<some value>" (note the hyphen becomes a space)
// "xwayland-tablet stylus:<some value>"
// Which is unfortunately a collapsed stream of all devices (similar to X's concept of a Master device)
// and thus all per-device info (names, hardware IDs, capabilities) is lost in abstraction.
const TYPE_XWAYLAND_POINTER: &str = "xwayland-pointer";

const EMULATED_TABLET_NAME: &str = "octotablet emulated";

const TYPE_MOUSE: &str = "MOUSE";
const TYPE_TOUCHSCREEN: &str = "TOUCHSCREEN";

/// Comes from datasize of "button count" field of `ButtonInfo` - button names in xinput are indices,
/// with the zeroth index referring to the tool "down" state.
pub type ButtonID = std::num::NonZero<u16>;

#[derive(Debug, Clone, Copy)]
enum ValuatorAxis {
    // Absolute position, in a normalized device space.
    // AbsX,
    // AbsY,
    AbsPressure,
    // Degrees, -,- left and away from user.
    AbsTiltX,
    AbsTiltY,
    // This pad ring, degrees, and maybe also stylus scrollwheel? I have none to test,
    // but under Xwayland this capability is listed for both pad and stylus.
    AbsWheel,
}
impl std::str::FromStr for ValuatorAxis {
    type Err = ();
    fn from_str(axis_label: &str) -> Result<Self, Self::Err> {
        Ok(match axis_label {
            // "Abs X" => Self::AbsX,
            // "Abs Y" => Self::AbsY,
            "Abs Pressure" => Self::AbsPressure,
            "Abs Tilt X" => Self::AbsTiltX,
            "Abs Tilt Y" => Self::AbsTiltY,
            "Abs Wheel" => Self::AbsWheel,
            // My guess is the next one is roll axis, but I do
            // not have a any devices that report this axis.
            _ => return Err(()),
        })
    }
}
impl From<ValuatorAxis> for crate::axis::Axis {
    fn from(value: ValuatorAxis) -> Self {
        match value {
            ValuatorAxis::AbsPressure => Self::Pressure,
            ValuatorAxis::AbsTiltX | ValuatorAxis::AbsTiltY => Self::Tilt,
            ValuatorAxis::AbsWheel => Self::Wheel,
            //Self::AbsX | Self::AbsY => return None,
        }
    }
}
enum DeviceType {
    Tool(crate::tool::Type),
    Tablet,
    Pad,
}
enum DeviceTypeOrXwayland {
    Type(DeviceType),
    /// Device type of xwayland-pointer doesn't tell us much, we must
    /// also inspect the user-facing device name.
    Xwayland,
}
impl std::str::FromStr for DeviceTypeOrXwayland {
    type Err = ();
    fn from_str(device_type: &str) -> Result<Self, Self::Err> {
        use crate::tool::Type;
        Ok(match device_type {
            TYPE_STYLUS => Self::Type(DeviceType::Tool(Type::Pen)),
            TYPE_ERASER => Self::Type(DeviceType::Tool(Type::Eraser)),
            TYPE_PAD => Self::Type(DeviceType::Pad),
            TYPE_TABLET => Self::Type(DeviceType::Tablet),
            // TYPE_MOUSE => Self::Tool(Type::Mouse),
            TYPE_XWAYLAND_POINTER => Self::Xwayland,
            _ => return Err(()),
        })
    }
}

/// Parse the device name of an xwayland device, where the type is stored.
/// Use if [`DeviceType`] parsing came back as `DeviceType::Xwayland`.
fn xwayland_type_from_name(device_name: &str) -> Option<DeviceType> {
    use crate::tool::Type;
    let class = device_name.strip_prefix("xwayland-tablet")?;
    // there is a numeric field at the end, unclear what it means.
    // For me, it's *always* `:43`, /shrug!
    let colon = class.rfind(':')?;
    let class = &class[..colon];

    Some(match class {
        // Weird inconsistent prefix xP
        "-pad" => DeviceType::Pad,
        " stylus" => DeviceType::Tool(Type::Pen),
        " eraser" => DeviceType::Tool(Type::Eraser),
        _ => return None,
    })
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub(super) enum ID {
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

#[derive(Copy, Clone)]
struct ToolName<'a> {
    /// The friendly form of the name, minus ID code.
    human_readable: &'a str,
    /// The tablet name we expect to own this tool
    maybe_associated_tablet: Option<&'a str>,
    /// The hardware serial of the tool.
    id: Option<crate::tool::HardwareID>,
}
impl<'a> ToolName<'a> {
    fn human_readable(self) -> &'a str {
        self.human_readable
    }
    fn id(self) -> Option<crate::tool::HardwareID> {
        self.id
    }
    fn maybe_associated_tablet(self) -> Option<&'a str> {
        self.maybe_associated_tablet
    }
}

/// From the user-facing Device name, try to parse several tool fields.
fn parse_tool_name(name: &str) -> ToolName {
    // X11 seems to place tool hardware IDs within the human-readable Name of the device, and this is
    // the only place it is exposed. Predictably, as with all things X, this is not documented as far
    // as I can tell.

    // From experiments, it consists of the [tablet name]<space>[tool type string]<space>[hex number (or zero)
    // in parentheses] - This is a hueristic and likely non-exhaustive, for example it does not apply to xwayland.

    let try_parse_id = || -> Option<(&str, crate::tool::HardwareID)> {
        // Detect the range of characters within the last set of parens.
        let open_paren = name.rfind('(')?;
        let after_open_paren = open_paren + 1;
        // Find the close paren after the last open paren (weird change-of-base-address thing)
        let close_paren = after_open_paren + name.get(after_open_paren..)?.find(')')?;

        // Find the human-readable name content, minus the id field.
        let name_text = name[..open_paren].trim_ascii_end();

        // Find the id field.
        // id_text is literal '0', or a hexadecimal number prefixed by literal '0x'
        let id_text = &name[after_open_paren..close_paren];

        let id_num = if id_text == "0" {
            // Should this be considered "None"? The XP-PEN DECO-01 reports this value, despite (afaik)
            // lacking a genuine hardware ID capability.
            0
        } else if let Some(id_text) = id_text.strip_prefix("0x") {
            u64::from_str_radix(id_text, 16).ok()?
        } else {
            return None;
        };

        Some((name_text, crate::tool::HardwareID(id_num)))
    };

    let id_parse_result = try_parse_id();

    let (human_readable, id) = match id_parse_result {
        Some((name, id)) => (name, Some(id)),
        None => (name, None),
    };

    let try_parse_maybe_associated_tablet = || -> Option<&str> {
        // Hueristic, of course. These are the only two kinds of hardware I have to test with,
        // unsure how e.g. an airbrush would register.
        if let Some(tablet_name) = human_readable.strip_suffix(" Pen") {
            return Some(tablet_name);
        }
        if let Some(tablet_name) = human_readable.strip_suffix(" Eraser") {
            return Some(tablet_name);
        }
        None
    };

    ToolName {
        human_readable,
        maybe_associated_tablet: try_parse_maybe_associated_tablet(),
        id,
    }
}
fn pad_maybe_associated_tablet(name: &str) -> Option<String> {
    // Hueristic, of course.
    name.strip_suffix(" Pad")
        .map(|prefix| prefix.to_owned() + " Pen")
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
fn fixed16_to_f32(fixed: i32) -> f32 {
    // Could bit-twiddle these into place instead, likely with more precision.
    (fixed as f32) / 65536.0
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
    pressure: Option<AxisInfo>,
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
    grabbed: bool,
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
    master_pointer: u16,
    master_keyboard: u16,
    grabbed: bool,
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
    atom_usb_id: Option<std::num::NonZero<x11rb::protocol::xproto::Atom>>,
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
        /*let version = conn
        // What the heck is "name"? it is totally undocumented and is not part of the XLib interface.
        // I was unable to reverse engineer it, it seems to work regardless of what data is given to it.
        .xinput_get_extension_version(b"Fixme!")
        .unwrap()
        .reply()
        .unwrap();*/
        let version = conn.xinput_xi_query_version(2, 4).unwrap().reply().unwrap();
        println!(
            "Server supports v{}.{}",
            version.major_version, version.minor_version
        );

        assert!(version.major_version >= 2);

        // conn.xinput_select_extension_event(
        //     window,
        //     // Some crazy logic involving the output of OpenDevice.
        //     // /usr/include/X11/extensions/XInput.h has the macros that do the crime, however it seems nonportable to X11rb.
        //     // https://www.x.org/archive/X11R6.8.2/doc/XGetSelectedExtensionEvents.3.html
        //     &[u32::from(xinput::CHANGE_DEVICE_NOTIFY_EVENT)],
        // )
        // .unwrap()
        // .check()
        // .unwrap();
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

        let atom_usb_id = conn
            .intern_atom(false, b"Device Product ID")
            .ok()
            .and_then(|resp| resp.reply().ok())
            .and_then(|reply| reply.atom.try_into().ok());

        let mut this = Self {
            conn,
            tool_infos: std::collections::BTreeMap::new(),
            pad_infos: std::collections::BTreeMap::new(),
            tools: vec![],
            pads: vec![],
            events: vec![],
            tablets: vec![],
            window,
            atom_usb_id,
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

        // Okay, this is weird. There are two very similar functions, xi_query_device and list_input_devices.
        // The venne diagram of the data contained within their responses is nearly a circle, however each
        // has subtle differences such that we need to query both and join the data. >~<;

        // "Clients are requested to avoid mixing XI1.x and XI2 code as much as possible" well then maybe
        // you shoulda made query_device actually return all the necessary data ya silly goober.
        let device_queries = self
            .conn
            .xinput_xi_query_device(XI_ALL_DEVICES)
            .unwrap()
            .reply()
            .unwrap();

        let device_list = self
            .conn
            .xinput_list_input_devices()
            .unwrap()
            .reply()
            .unwrap();

        // We recieve axis infos in a flat list, into which the individual devices refer.
        // (mutable slice as we'll trim it as we consume)
        let mut flat_infos = &device_list.infos[..];
        // We also recieve name strings in a parallel list.
        for (name, device) in device_list
            .names
            .into_iter()
            .zip(device_list.devices.into_iter())
        {
            // Zero is a special value (ALL_DEVICES), and can't be used by a device.
            let nonzero_id = std::num::NonZero::new(u16::from(device.device_id)).unwrap();
            let octotablet_id = ID::ID {
                generation: self.device_generation,
                device_id: nonzero_id,
            };

            let _infos = {
                // Split off however many axes this device claims.
                let (infos, tail_infos) = flat_infos.split_at(device.num_class_info.into());
                flat_infos = tail_infos;
                infos
            };
            // Find the query that represents this device.
            // Query and list contain very similar info, but both have tiny extra nuggets that we
            // need.
            let Some(query) = device_queries
                .infos
                .iter()
                .find(|qdevice| qdevice.deviceid == u16::from(device.device_id))
            else {
                continue;
            };

            // Query the "type" atom, which will describe what this device actually is through some heuristics.
            // We can't use the capabilities it advertises as our detection method, since a lot of them are
            // nonsensical (pad reporting absolute x,y, pressure, etc - but it doesn't do anything!)
            if device.device_type == 0 {
                // None.
                continue;
            }
            let Some(device_type) = self
                .conn
                // This is *not* cached. Should we? We expect a small set of valid values,
                // but on the other hand this isn't exactly a hot path.
                .get_atom_name(device.device_type)
                .ok()
                // Whew!
                .and_then(|response| response.reply().ok())
                .and_then(|atom| String::from_utf8(atom.name).ok())
                .and_then(|type_stirng| type_stirng.parse::<DeviceTypeOrXwayland>().ok())
            else {
                continue;
            };

            // UTF8 human-readable device name, which encodes some additional info sometimes.
            let raw_name = String::from_utf8(name.name).ok();
            let device_type = match device_type {
                DeviceTypeOrXwayland::Type(t) => t,
                // Generic xwayland type, parse the device name to find type instead.
                DeviceTypeOrXwayland::Xwayland => {
                    let Some(ty) = raw_name.as_deref().and_then(xwayland_type_from_name) else {
                        // Couldn't figure out what the device is..
                        continue;
                    };
                    ty
                }
            };

            // At this point, we're pretty sure this is a tool, pad, or tablet!

            match device_type {
                DeviceType::Tool(ty) => {
                    // It's a tool! Parse all relevant infos.

                    // We can only handle tools which have a parent.
                    // (and obviously they shouldn't be a keyboard.)
                    // Technically, a floating pointer can work for our needs,
                    // but it behaves weird when not grabbed and it's not easy to know
                    // when to grab/release a floating device.
                    // (We could manually implement a hit test? yikes)
                    if query.type_ != xinput::DeviceType::SLAVE_POINTER {
                        continue;
                    }

                    // Try to parse the hardware ID from the name field.
                    let name_fields = raw_name.as_deref().map(parse_tool_name);

                    let tablet_id = name_fields
                        .and_then(ToolName::maybe_associated_tablet)
                        .and_then(|expected| {
                            // Find the device with the expected name, and return it's ID if found.
                            let tablet_info = device_queries
                                .infos
                                .iter()
                                .find(|info| info.name == expected.as_bytes())?;

                            Some(ID::ID {
                                generation: self.device_generation,
                                // 0 is a special value, this is infallible.
                                device_id: tablet_info.deviceid.try_into().unwrap(),
                            })
                        });

                    let mut octotablet_info = crate::tool::Tool {
                        internal_id: super::InternalID::XInput2(octotablet_id),
                        name: name_fields
                            .map(ToolName::human_readable)
                            .map(ToOwned::to_owned),
                        hardware_id: name_fields.and_then(ToolName::id),
                        wacom_id: None,
                        tool_type: Some(ty),
                        axes: crate::axis::FullInfo::default(),
                    };

                    let mut x11_info = ToolInfo {
                        pressure: None,
                        tilt: [None, None],
                        wheel: None,
                        tablet: tablet_id.unwrap_or(ID::EmulatedTablet),
                        phase: Phase::Out,
                        master_pointer: query.attachment,
                        master_keyboard: device_queries
                            .infos
                            .iter()
                            .find_map(|q| {
                                // Find the info for the master pointer
                                if q.deviceid == query.attachment {
                                    // Look at the master pointer's attachment,
                                    // which is the associated master keyboard's ID.
                                    Some(q.attachment)
                                } else {
                                    None
                                }
                            })
                            // Above search should be infallible but I trust nothing at this point.
                            .unwrap_or_default(),
                        grabbed: false,
                        frame_pending: None,
                        last_interaction: None,
                    };

                    // Look for axes!
                    for class in &query.classes {
                        if let Some(v) = class.data.as_valuator() {
                            if v.mode != xinput::ValuatorMode::ABSOLUTE {
                                continue;
                            };
                            // Weird case, that does happen in practice. :V
                            if v.min == v.max {
                                continue;
                            }
                            let Some(label) = self
                                .conn
                                .get_atom_name(v.label)
                                .ok()
                                .and_then(|response| response.reply().ok())
                                .and_then(|atom| String::from_utf8(atom.name).ok())
                                .and_then(|label| label.parse::<ValuatorAxis>().ok())
                            else {
                                continue;
                            };

                            let min = fixed32_to_f32(v.min);
                            let max = fixed32_to_f32(v.max);

                            match label {
                                ValuatorAxis::AbsPressure => {
                                    // Scale and bias to [0,1].
                                    x11_info.pressure = Some(AxisInfo {
                                        index: v.number,
                                        transform: Transform::BiasScale {
                                            bias: -min,
                                            scale: 1.0 / (max - min),
                                        },
                                    });
                                    octotablet_info.axes.pressure =
                                        Some(crate::axis::NormalizedInfo { granularity: None });
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

                            // Resolution is.. meaningless, I think. xwayland is the only server I have
                            // seen that even bothers to fill it out, and even there it's weird.
                        }
                    }

                    tool_listen_events.push(u16::from(device.device_id));
                    self.tools.push(octotablet_info);
                    self.tool_infos.insert(octotablet_id, x11_info);
                }
                DeviceType::Pad => {
                    if query.type_ == xinput::DeviceType::FLOATING_SLAVE {
                        // We need master attachments in order to grab.
                        continue;
                    }
                    let mut buttons = 0;
                    let mut ring_axis = None;
                    for class in &query.classes {
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
                                let Some(label) = self
                                    .conn
                                    .get_atom_name(v.label)
                                    .ok()
                                    .and_then(|response| response.reply().ok())
                                    .and_then(|atom| String::from_utf8(atom.name).ok())
                                    .and_then(|label| label.parse::<ValuatorAxis>().ok())
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
                                        transform: Transform::BiasScale {
                                            bias: -min,
                                            scale: std::f32::consts::TAU / (max - min),
                                        },
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
                        mode_count: None,
                        rings,
                        strips: vec![],
                    };
                    self.pads.push(crate::pad::Pad {
                        internal_id: crate::platform::InternalID::XInput2(octotablet_id),
                        total_buttons: buttons.into(),
                        groups: vec![group],
                    });

                    pad_listen_events.push(u16::from(device.device_id));

                    // Find the tablet this belongs to.
                    let tablet = raw_name
                        .as_deref()
                        .and_then(pad_maybe_associated_tablet)
                        .and_then(|expected| {
                            // Find the device with the expected name, and return it's ID if found.
                            let tablet_info = device_queries
                                .infos
                                .iter()
                                .find(|info| info.name == expected.as_bytes())?;

                            Some(ID::ID {
                                generation: self.device_generation,
                                // 0 is ALL_DEVICES, this is infallible.
                                device_id: tablet_info.deviceid.try_into().unwrap(),
                            })
                        });
                    let primary_master = query.attachment;
                    let other_master = device_queries
                        .infos
                        .iter()
                        .find_map(|q| {
                            // Find the info for the master pointer
                            if q.deviceid == primary_master {
                                // Look at the master pointer's attachment,
                                // which is the associated master keyboard's ID.
                                Some(q.attachment)
                            } else {
                                None
                            }
                        })
                        // Above search should be infallible but I trust nothing at this point.
                        .unwrap_or_default();

                    // Depending on what flavor of device we are, the master is pointer or keyboard.
                    let (master_pointer, master_keyboard) = if matches!(
                        query.type_,
                        xinput::DeviceType::MASTER_KEYBOARD | xinput::DeviceType::SLAVE_KEYBOARD
                    ) {
                        (primary_master, other_master)
                    } else {
                        (other_master, primary_master)
                    };

                    self.pad_infos.insert(
                        octotablet_id,
                        PadInfo {
                            ring: ring_axis.map(|ring_axis| RingInfo {
                                axis: ring_axis,
                                last_interaction: None,
                            }),
                            master_pointer,
                            master_keyboard,
                            tablet: tablet.unwrap_or(ID::EmulatedTablet),
                            grabbed: false,
                        },
                    );
                }
                DeviceType::Tablet => {
                    // Tablets are of... dubious usefulness in xinput?
                    // They do not follow the paradigms needed by octotablet.
                    // Alas, we can still fetch some useful information!
                    let usb_id = self
                        .conn
                        // USBID consists of two 16 bit integers, [vid, pid].
                        .xinput_xi_get_property(
                            device.device_id,
                            false,
                            self.atom_usb_id.map_or(0, std::num::NonZero::get),
                            0,
                            0,
                            2,
                        )
                        .ok()
                        .and_then(|resp| resp.reply().ok())
                        .and_then(|property| {
                            #[allow(clippy::get_first)]
                            // Try to accept any type.
                            Some(match property.items {
                                xinput::XIGetPropertyItems::Data16(d) => crate::tablet::UsbId {
                                    vid: *d.get(0)?,
                                    pid: *d.get(1)?,
                                },
                                xinput::XIGetPropertyItems::Data8(d) => crate::tablet::UsbId {
                                    vid: (*d.get(0)?).into(),
                                    pid: (*d.get(1)?).into(),
                                },
                                xinput::XIGetPropertyItems::Data32(d) => crate::tablet::UsbId {
                                    vid: (*d.get(0)?).try_into().ok()?,
                                    pid: (*d.get(1)?).try_into().ok()?,
                                },
                                xinput::XIGetPropertyItems::InvalidValue(_) => return None,
                            })
                        });

                    // We can also fetch device path here.

                    let tablet = crate::tablet::Tablet {
                        internal_id: super::InternalID::XInput2(octotablet_id),
                        name: raw_name,
                        usb_id,
                    };

                    self.tablets.push(tablet);
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
                        // Cursor entering and leaving client area. Doesn't work.
                        | xinput::XIEventMask::ENTER
                        | xinput::XIEventMask::LEAVE
                        // Also enter and leave? Doesn't work.
                        | xinput::XIEventMask::FOCUS_IN
                        | xinput::XIEventMask::FOCUS_OUT
                        // Touches user-defined pointer barrier
                        // | xinput::XIEventMask::BARRIER_HIT
                        // | xinput::XIEventMask::BARRIER_LEAVE
                        // Axis movement
                        // since XI2.4, RAW_MOTION should work here, and give us events regardless
                        // of grab state. it does not work. COol i love this API
                        | xinput::XIEventMask::MOTION
                        // Proximity is implicit, i guess. I'm losing my mind.

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
    fn parent_entered(&mut self, master: u16, time: Timestamp) {
        // Grab tools.
        for (&id, tool) in &mut self.tool_infos {
            let is_child = tool.master_pointer == master || tool.master_keyboard == master;
            if tool.grabbed || !is_child {
                continue;
            }

            println!("Grabbed Uwu");
            // Don't care if it succeeded or failed.
            let status = self
                .conn
                .xinput_xi_grab_device(
                    self.window,
                    time,
                    // don't override visual cursor.
                    0,
                    match id {
                        ID::ID { device_id, .. } => device_id.get(),
                        ID::EmulatedTablet => unreachable!(),
                    },
                    // Allow the device to continue sending events
                    x11rb::protocol::xproto::GrabMode::ASYNC,
                    // Allow other devices to continue sending events.
                    // There's some reason to use SYNC here, meaning the master won't update,
                    // as then Winit won't send pointer events in addition to octotablet's events.
                    x11rb::protocol::xproto::GrabMode::ASYNC,
                    // Grab the events we already asked for.
                    xinput::GrabOwner::OWNER,
                    &[],
                )
                .unwrap()
                .reply()
                .unwrap()
                .status;
            if status == x11rb::protocol::xproto::GrabStatus::SUCCESS {
                tool.grabbed = true;
            }
        }
        // Grab pads.
        for (id, pad) in &mut self.pad_infos {
            let is_child = pad.master_pointer == master || pad.master_keyboard == master;
            if pad.grabbed || !is_child {
                continue;
            }
            let status = self
                .conn
                .xinput_xi_grab_device(
                    self.window,
                    time,
                    // don't override visual cursor.
                    0,
                    match id {
                        ID::ID { device_id, .. } => device_id.get(),
                        ID::EmulatedTablet => unreachable!(),
                    },
                    // Allow the device to continue sending events
                    x11rb::protocol::xproto::GrabMode::ASYNC,
                    // Allow other devices to continue sending events.
                    // There's some reason to use SYNC here, meaning the master won't update,
                    // as then Winit won't send pointer events in addition to octotablet's events.
                    x11rb::protocol::xproto::GrabMode::ASYNC,
                    // Grab the events we already asked for.
                    xinput::GrabOwner::OWNER,
                    &[],
                )
                .unwrap()
                .reply()
                .unwrap()
                .status;

            if status == x11rb::protocol::xproto::GrabStatus::SUCCESS {
                pad.grabbed = true;
            }
        }
    }
    fn parent_left(&mut self, master: u16, time: Timestamp) {
        // Release tools.
        for (&id, tool) in &mut self.tool_infos {
            let is_child = tool.master_pointer == master || tool.master_keyboard == master;
            if !tool.grabbed || !is_child {
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

            println!("Released uwU");
            tool.set_phase(id, Phase::Out, &mut self.events);

            let succeeded = self
                .conn
                .xinput_xi_ungrab_device(
                    time,
                    match id {
                        ID::ID { device_id, .. } => device_id.get(),
                        ID::EmulatedTablet => unreachable!(),
                    },
                )
                .unwrap()
                .check()
                .is_ok();
            if succeeded {
                tool.grabbed = false;
            }
        }
        // Release pads.
        for (&id, pad) in &mut self.pad_infos {
            let is_child = pad.master_pointer == master || pad.master_keyboard == master;
            if !pad.grabbed || !is_child {
                continue;
            }

            // Don't care if it succeeded or failed.
            let succeeded = self
                .conn
                .xinput_xi_ungrab_device(
                    time,
                    match id {
                        ID::ID { device_id, .. } => device_id.get(),
                        ID::EmulatedTablet => unreachable!(),
                    },
                )
                .unwrap()
                .check()
                .is_ok();
            if succeeded {
                pad.grabbed = false;
            }
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
                    self.parent_entered(enter.deviceid, enter.time);
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
                        println!("Looking for {}", ring_info.axis.index);
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