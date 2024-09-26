//! Magic strings used by the XI implementation as well as certain driver implementations.
//!
//! More are defined than are actually used, mostly to remind my future self that the option
//! exists.

pub type Atom = std::num::NonZero<u32>;

/// Interned atoms. For documentation on values and use, see the other modules in [`super::strings`].
pub struct Atoms {
    pub wacom: wacom::Atoms,
    pub libinput: libinput::Atoms,
    pub xi: xi::Atoms,
    pub absolute_axes: xi::axis_label::absolute::Atoms,
}

#[derive(Debug, thiserror::Error)]
pub enum InternError {
    #[error(transparent)]
    Connection(#[from] x11rb::errors::ConnectionError),
    #[error(transparent)]
    Reply(#[from] x11rb::errors::ReplyError),
    #[error("server replied with null atom")]
    NullReply,
}

/// Intern all the needed atoms.
pub fn intern<Conn>(conn: Conn) -> Result<Atoms, InternError>
where
    Conn: x11rb::connection::RequestConnection + x11rb::protocol::xproto::ConnectionExt,
{
    use xi::axis_label::absolute;
    // Reasoning - if no device has been connected to prompt the driver to appear and register it's atoms,
    // we still want to be able to see them upon attachment without restarting the octotablet client.. right?
    // On the other hand, I'm not really sure if drivers are even lazily loaded.
    const ONLY_IF_EXISTS: bool = false;

    // Bulk request, then bulk recv. Makes the protocol latency O(1) instead of O(n). Not that it matters,
    // this is one-time setup code xP
    // xlib has a bulk-intern call, x11rb does not.

    let intern = |name: &str| -> Result<
        x11rb::cookie::Cookie<'_, Conn, x11rb::protocol::xproto::InternAtomReply>,
        x11rb::errors::ConnectionError,
    > { conn.intern_atom(ONLY_IF_EXISTS, name.as_bytes()) };

    // wacom
    let prop_tool_type = intern(wacom::PROP_TOOL_TYPE)?;
    let type_stylus = intern(wacom::TYPE_STYLUS)?;
    let type_cursor = intern(wacom::TYPE_CURSOR)?;
    let type_eraser = intern(wacom::TYPE_ERASER)?;
    let type_pad = intern(wacom::TYPE_PAD)?;
    let prop_serial_ids = intern(wacom::PROP_SERIALIDS)?;

    // libinput
    let prop_tool_serial = intern(libinput::PROP_TOOL_SERIAL)?;
    let prop_tool_id = intern(libinput::PROP_TOOL_ID)?;
    let prop_pad_group_modes_available = intern(libinput::PROP_PAD_GROUP_MODES_AVAILABLE)?;
    let prop_pad_group_current_modes = intern(libinput::PROP_PAD_GROUP_CURRENT_MODES)?;
    let prop_pad_button_groups = intern(libinput::PROP_PAD_BUTTON_GROUPS)?;
    let prop_pad_strip_groups = intern(libinput::PROP_PAD_STRIP_GROUPS)?;
    let prop_pad_ring_groups = intern(libinput::PROP_PAD_RING_GROUPS)?;
    let prop_heartbeat = intern(libinput::PROP_HEARTBEAT)?;

    // xi
    let prop_product_id = intern(xi::PROP_PRODUCT_ID)?;
    let prop_device_node = intern(xi::PROP_DEVICE_NODE)?;

    // xi standard absolute  axis labels
    let x = intern(absolute::PROP_X)?;
    let y = intern(absolute::PROP_Y)?;
    let rz = intern(absolute::PROP_RZ)?;
    let distance = intern(absolute::PROP_DISTANCE)?;
    let pressure = intern(absolute::PROP_PRESSURE)?;
    let tilt_x = intern(absolute::PROP_TILT_X)?;
    let tilt_y = intern(absolute::PROP_TILT_Y)?;
    let wheel = intern(absolute::PROP_WHEEL)?;

    let parse_reply = |atom: x11rb::cookie::Cookie<
        Conn,
        x11rb::protocol::xproto::InternAtomReply,
    >|
     -> Result<Atom, InternError> {
        atom.reply()?
            .atom
            .try_into()
            .map_err(|_| InternError::NullReply)
    };

    Ok(Atoms {
        wacom: wacom::Atoms {
            prop_tool_type: parse_reply(prop_tool_type)?,
            type_stylus: parse_reply(type_stylus)?,
            type_cursor: parse_reply(type_cursor)?,
            type_eraser: parse_reply(type_eraser)?,
            type_pad: parse_reply(type_pad)?,
            prop_serial_ids: parse_reply(prop_serial_ids)?,
        },
        libinput: libinput::Atoms {
            prop_tool_serial: parse_reply(prop_tool_serial)?,
            prop_tool_id: parse_reply(prop_tool_id)?,

            prop_pad_group_modes_available: parse_reply(prop_pad_group_modes_available)?,
            prop_pad_group_current_modes: parse_reply(prop_pad_group_current_modes)?,
            prop_pad_button_groups: parse_reply(prop_pad_button_groups)?,
            prop_pad_strip_groups: parse_reply(prop_pad_strip_groups)?,
            prop_pad_ring_groups: parse_reply(prop_pad_ring_groups)?,

            prop_heartbeat: parse_reply(prop_heartbeat)?,
        },
        xi: xi::Atoms {
            prop_product_id: parse_reply(prop_product_id)?,
            prop_device_node: parse_reply(prop_device_node)?,
        },
        absolute_axes: xi::axis_label::absolute::Atoms {
            x: parse_reply(x)?,
            y: parse_reply(y)?,
            rz: parse_reply(rz)?,
            distance: parse_reply(distance)?,
            pressure: parse_reply(pressure)?,
            tilt_x: parse_reply(tilt_x)?,
            tilt_y: parse_reply(tilt_y)?,
            wheel: parse_reply(wheel)?,
        },
    })
}

/// Definitions from the xf86-input-wacom driver:
/// <https://github.com/linuxwacom/xf86-input-wacom/blob/master/include/wacom-properties.h>
///
/// See also <https://github.com/linuxwacom/xf86-input-wacom/blob/master/src/x11/xf86Wacom.c#L374>
/// which seems to imply there's a strong, pre-determined ordering of valuators. Hmf. This matches
/// with what I have seen in the wild, but it feels like the wrong solution to rely on that...?
pub mod wacom {
    /// value is an atom, equal to one of the `TYPE_*` values.
    /// This is a replacement for the deprecated "type" atom that used to exist in XI 1
    pub const PROP_TOOL_TYPE: &str = "Wacom Tool Type";
    pub const TYPE_STYLUS: &str = "STYLUS";
    pub const TYPE_CURSOR: &str = "CURSOR";
    pub const TYPE_ERASER: &str = "ERASER";
    pub const TYPE_PAD: &str = "PAD";
    pub const TYPE_TOUCH: &str = "TOUCH";

    /// CARD32[5], tablet id, old serial, old hw id, new serial, new hw id.
    /// idk what old and new means. experimentally new is 0 when out and =old when in.
    ///
    /// "old serial" matches up exactly with the value from wayland's tablet-v2, so
    /// it seems like that's our guy! :D
    pub const PROP_SERIALIDS: &str = "Wacom Serial IDs";

    pub struct Atoms {
        pub prop_tool_type: super::Atom,
        pub type_stylus: super::Atom,
        pub type_cursor: super::Atom,
        pub type_eraser: super::Atom,
        pub type_pad: super::Atom,
        pub prop_serial_ids: super::Atom,
    }
}

/// Definitions from the xf86-input-libinput driver:
/// <https://gitlab.freedesktop.org/xorg/driver/xf86-input-libinput/-/blob/master/include/libinput-properties.h?ref_type=heads>
pub mod libinput {
    /// Hardware ID, u32. If exists and is zero, it has no ID.
    pub const PROP_TOOL_SERIAL: &str = "libinput Tablet Tool Serial";
    /// Vendor-specific fine-grain hardware type, u32. Corresponds to [`crate::tool::Tool::wacom_id`]. I can't find a listing
    /// of these!
    ///
    /// See also: <https://wayland.freedesktop.org/libinput/doc/latest/tablet-support.html#vendor-specific-tablet-tool-types>
    pub const PROP_TOOL_ID: &str = "libinput Tablet Tool ID";
    // The following have been renamed to use octotablet verbage (group instead of mode group)
    // (at this point we are just using the X server as a mediator to talk to libinput through hidden channels lmao)
    /// CARD8[num groups], number of modes per group.
    pub const PROP_PAD_GROUP_MODES_AVAILABLE: &str = "libinput Pad Mode Groups Modes Available";
    /// CARD8[num groups], current mode in `[0, MODES_AVAILABLE)`.
    pub const PROP_PAD_GROUP_CURRENT_MODES: &str = "libinput Pad Mode Groups Modes";
    /// INT8[num buttons], associated group for each button, or -1 if no association.
    pub const PROP_PAD_BUTTON_GROUPS: &str = "libinput Pad Mode Group Buttons";
    /// INT8[num strips], associated group for each strip, or -1 if no association.
    // Hm. Octotablet does not support rings/strips not owned by a group. oops?
    pub const PROP_PAD_STRIP_GROUPS: &str = "libinput Pad Mode Group Strips";
    /// INT8[num strips], associated group for each ring, or -1 if no association.
    pub const PROP_PAD_RING_GROUPS: &str = "libinput Pad Mode Group Rings";

    /// Something defined for all libinput devices, dont care about the meaning.
    pub const PROP_HEARTBEAT: &str = "libinput Send Events Mode Enabled Default";

    #[allow(clippy::struct_field_names)]
    pub struct Atoms {
        pub prop_tool_serial: super::Atom,
        pub prop_tool_id: super::Atom,
        pub prop_pad_group_modes_available: super::Atom,
        pub prop_pad_group_current_modes: super::Atom,
        pub prop_pad_button_groups: super::Atom,
        pub prop_pad_strip_groups: super::Atom,
        pub prop_pad_ring_groups: super::Atom,

        pub prop_heartbeat: super::Atom,
    }
}

/// Constants for parsing xwayland devices.
///
/// Name consists of:
/// [`NAME_PREFIX`] + [`NAME_PAD_SUFFIX`], [`NAME_ERASER_SUFFIX`], or [`NAME_STYLUS_SUFFIX`] + [`NAME_SEAT_SEPARATOR`]  + integral seat id.
pub mod xwayland {
    pub const NAME_PREFIX: &str = "xwayland-tablet";
    // Weird inconsistent separator xP
    pub const NAME_PAD_SUFFIX: &str = "-pad";
    pub const NAME_ERASER_SUFFIX: &str = " eraser";
    pub const NAME_STYLUS_SUFFIX: &str = " stylus";
    pub const NAME_MOUSE_LENS_SUFFIX: &str = " cursor";
    pub const NAME_SEAT_SEPARATOR: char = ':';
}

/// Definitions from the XI internals:
/// <https://github.com/XQuartz/xorg-server/blob/master/include/xserver-properties.h>
///
/// These are not, as far as I can tell, publically documented. However, it is necessary
/// to poke at these internals to discover the capabilities of a device.
pub mod xi {
    // Device meta

    // One of these, "Coordinate Transformation Matrix," got me really excited that we could take the
    // Abx X Y axis values to logical pixel space ourselves, avoiding the client x,y weirdness and
    // implement multicursor in client space! (as of now, multiple tablets on the same seat just make
    // the cursor vibrate wildly.)
    // alas, it is the identity matrix on all devices I've tested, so it's utterly useless...

    /// CARD32[2], [usb VID, usb PID]
    pub const PROP_PRODUCT_ID: &str = "Device Product ID";
    /// String, device path.
    pub const PROP_DEVICE_NODE: &str = "Device Node";

    pub struct Atoms {
        pub prop_product_id: super::Atom,
        pub prop_device_node: super::Atom,
    }

    pub mod axis_label {
        use super::super::Atom;
        /// Relative axes
        pub mod relative {
            #![allow(dead_code)]
            pub const PROP_X: &str = "Rel X";
            pub const PROP_Y: &str = "Rel Y";
            pub const PROP_Z: &str = "Rel Z";
            pub const PROP_RX: &str = "Rel Rotary X";
            pub const PROP_RY: &str = "Rel Rotary Y";
            pub const PROP_RZ: &str = "Rel Rotary Z";
            pub const PROP_HWHEEL: &str = "Rel Horiz Wheel";
            pub const PROP_DIAL: &str = "Rel Dial";
            pub const PROP_WHEEL: &str = "Rel Vert Wheel";
            pub const PROP_MISC: &str = "Rel Misc";
            pub const PROP_VSCROLL: &str = "Rel Vert Scroll";
            pub const PROP_HSCROLL: &str = "Rel Horiz Scroll";
        }

        /// Absolute axes
        ///
        /// Some examples on how these are used in the wild:
        /// * <https://github.com/linuxwacom/xf86-input-wacom/blob/master/src/x11/xf86Wacom.c#L490>
        /// * <https://gitlab.freedesktop.org/xorg/driver/xf86-input-libinput/-/blob/master/src/xf86libinput.c?ref_type=heads#L1386>
        ///
        /// Notably, Ring2 and Strips are unlabled in both cases. how are you supposed to detect them if the label is null?!
        pub mod absolute {
            pub const PROP_X: &str = "Abs X";
            pub const PROP_Y: &str = "Abs Y";
            pub const PROP_Z: &str = "Abs Z";
            pub const PROP_RX: &str = "Abs Rotary X";
            pub const PROP_RY: &str = "Abs Rotary Y";
            pub const PROP_RZ: &str = "Abs Rotary Z";
            /// OKAY SO both input-libinput and input-wacom drivers report... *something* important
            /// about the airbrush as ABS_THROTTLE. I have no idea what!!
            /// From photos, this seems to correspond physically with pressure on a button, which
            /// should then logically correspond with octotablet's non-button-pressure axis. Idk.
            pub const PROP_THROTTLE: &str = "Abs Throttle";
            pub const PROP_RUDDER: &str = "Abs Rudder";
            pub const PROP_WHEEL: &str = "Abs Wheel";
            pub const PROP_GAS: &str = "Abs Gas";
            pub const PROP_BRAKE: &str = "Abs Brake";
            pub const PROP_HAT0X: &str = "Abs Hat 0 X";
            pub const PROP_HAT0Y: &str = "Abs Hat 0 Y";
            pub const PROP_HAT1X: &str = "Abs Hat 1 X";
            pub const PROP_HAT1Y: &str = "Abs Hat 1 Y";
            pub const PROP_HAT2X: &str = "Abs Hat 2 X";
            pub const PROP_HAT2Y: &str = "Abs Hat 2 Y";
            pub const PROP_HAT3X: &str = "Abs Hat 3 X";
            pub const PROP_HAT3Y: &str = "Abs Hat 3 Y";
            pub const PROP_PRESSURE: &str = "Abs Pressure";
            pub const PROP_DISTANCE: &str = "Abs Distance";
            pub const PROP_TILT_X: &str = "Abs Tilt X";
            pub const PROP_TILT_Y: &str = "Abs Tilt Y";
            pub const PROP_TOOL_WIDTH: &str = "Abs Tool Width";
            pub const PROP_VOLUME: &str = "Abs Volume";
            pub const PROP_MT_TOUCH_MAJOR: &str = "Abs MT Touch Major";
            pub const PROP_MT_TOUCH_MINOR: &str = "Abs MT Touch Minor";
            pub const PROP_MT_WIDTH_MAJOR: &str = "Abs MT Width Major";
            pub const PROP_MT_WIDTH_MINOR: &str = "Abs MT Width Minor";
            pub const PROP_MT_ORIENTATION: &str = "Abs MT Orientation";
            pub const PROP_MT_POSITION_X: &str = "Abs MT Position X";
            pub const PROP_MT_POSITION_Y: &str = "Abs MT Position Y";
            pub const PROP_MT_TOOL_TYPE: &str = "Abs MT Tool Type";
            pub const PROP_MT_BLOB_ID: &str = "Abs MT Blob ID";
            pub const PROP_MT_TRACKING_ID: &str = "Abs MT Tracking ID";
            pub const PROP_MT_PRESSURE: &str = "Abs MT Pressure";
            pub const PROP_MT_DISTANCE: &str = "Abs MT Distance";
            pub const PROP_MT_TOOL_X: &str = "Abs MT Tool X";
            pub const PROP_MT_TOOL_Y: &str = "Abs MT Tool Y";
            pub const PROP_MISC: &str = "Abs Misc";

            pub struct Atoms {
                pub x: super::Atom,
                pub y: super::Atom,
                /// "Roll" in octotablet.
                pub rz: super::Atom,
                pub distance: super::Atom,
                pub pressure: super::Atom,
                pub tilt_x: super::Atom,
                pub tilt_y: super::Atom,
                pub wheel: super::Atom,
            }
        }
    }
}
