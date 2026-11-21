use super::InputSink;
use crate::video::linux_wlr::wire::{MsgBuilder, WaylandConn, create_memfd};
use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
#[cfg(test)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;
use usher_proto::input::{HeldInputTracker, InputEvent, MouseButton};

const WLR_PTR_MGR_OP_CREATE_POINTER: u16 = 0;
const WLR_PTR_MGR_OP_DESTROY: u16 = 2;

const WLR_PTR_OP_MOTION: u16 = 0;
const WLR_PTR_OP_MOTION_ABSOLUTE: u16 = 1;
const WLR_PTR_OP_BUTTON: u16 = 2;
const WLR_PTR_OP_AXIS: u16 = 3;
const WLR_PTR_OP_FRAME: u16 = 4;
const WLR_PTR_OP_DESTROY: u16 = 8;

const WLR_AXIS_VERTICAL: u32 = 0;
const WLR_AXIS_HORIZONTAL: u32 = 1;

const ZWP_KBD_MGR_OP_CREATE_KEYBOARD: u16 = 0;
const ZWP_KBD_OP_KEYMAP: u16 = 0;
const ZWP_KBD_OP_KEY: u16 = 1;
const ZWP_KBD_OP_MODIFIERS: u16 = 2;
const ZWP_KBD_OP_DESTROY: u16 = 3;

const WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1: u32 = 1;

const MOD_SHIFT: u32 = 1 << 0;
const MOD_LOCK: u32 = 1 << 1;
const MOD_CTRL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_SUPER: u32 = 1 << 6;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// Standard US QWERTY evdev XKB v1 keymap (null-terminated) uploaded over `memfd`.
const XKB_KEYMAP_US: &[u8] = concat!(
r#"xkb_keymap {
xkb_keycodes "evdev+aliases(qwerty)" {
    minimum = 8;
    maximum = 255;
    <ESC> = 9;
    <AE01> = 10; <AE02> = 11; <AE03> = 12; <AE04> = 13; <AE05> = 14;
    <AE06> = 15; <AE07> = 16; <AE08> = 17; <AE09> = 18; <AE10> = 19;
    <AE11> = 20; <AE12> = 21; <BKSP> = 22;
    <TAB>  = 23; <AD01> = 24; <AD02> = 25; <AD03> = 26; <AD04> = 27;
    <AD05> = 28; <AD06> = 29; <AD07> = 30; <AD08> = 31; <AD09> = 32;
    <AD10> = 33; <AD11> = 34; <AD12> = 35; <RTRN> = 36; <LCTL> = 37;
    <AC01> = 38; <AC02> = 39; <AC03> = 40; <AC04> = 41; <AC05> = 42;
    <AC06> = 43; <AC07> = 44; <AC08> = 45; <AC09> = 46; <AC10> = 47;
    <AC11> = 48; <TLDE> = 49; <LFSH> = 50; <BKSL> = 51;
    <AB01> = 52; <AB02> = 53; <AB03> = 54; <AB04> = 55; <AB05> = 56;
    <AB06> = 57; <AB07> = 58; <AB08> = 59; <AB09> = 60; <AB10> = 61;
    <RTSH> = 62; <KPMU> = 63; <LALT> = 64; <SPCE> = 65; <CAPS> = 66;
    <FK01> = 67; <FK02> = 68; <FK03> = 69; <FK04> = 70; <FK05> = 71;
    <FK06> = 72; <FK07> = 73; <FK08> = 74; <FK09> = 75; <FK10> = 76;
    <NMLK> = 77; <SCLK> = 78;
    <KP7>  = 79; <KP8>  = 80; <KP9>  = 81; <KPSU> = 82;
    <KP4>  = 83; <KP5>  = 84; <KP6>  = 85; <KPAD> = 86;
    <KP1>  = 87; <KP2>  = 88; <KP3>  = 89; <KP0>  = 90; <KPDL> = 91;
    <FK11> = 95; <FK12> = 96;
    <KPEN> = 104; <RCTL> = 105; <KPDV> = 106; <PRSC> = 107; <RALT> = 108;
    <HOME> = 110; <UP> = 111; <PGUP> = 112; <LEFT> = 113; <RGHT> = 114;
    <END> = 115; <DOWN> = 116; <PGDN> = 117; <INS> = 118; <DELE> = 119;
    <PAUS> = 127; <LWIN> = 133; <RWIN> = 134; <COMP> = 135;
};
xkb_types "complete" {
    virtual_modifiers NumLock,Alt,LevelThree,Super;
    type "ONE_LEVEL" { modifiers= none; map[none]= Level1; level_name[Level1]= "Any"; };
    type "TWO_LEVEL" { modifiers= Shift; map[Shift]= Level2; level_name[Level1]= "Base"; level_name[Level2]= "Shift"; };
    type "ALPHABETIC" {
        modifiers= Shift+Lock;
        map[Shift]= Level2; map[Lock]= Level2;
        level_name[Level1]= "Base"; level_name[Level2]= "Caps";
    };
    type "KEYPAD" {
        modifiers= Shift+NumLock;
        map[Shift]= Level2; map[NumLock]= Level2;
        level_name[Level1]= "Base"; level_name[Level2]= "Number";
    };
};
xkb_compatibility "complete" {
    virtual_modifiers NumLock,Alt,LevelThree,Super;
    interpret Shift_L+AnyOf(all) { action= SetMods(modifiers=Shift); };
    interpret Shift_R+AnyOf(all) { action= SetMods(modifiers=Shift); };
    interpret Control_L+AnyOf(all) { action= SetMods(modifiers=Control); };
    interpret Control_R+AnyOf(all) { action= SetMods(modifiers=Control); };
    interpret Alt_L+AnyOf(all) { action= SetMods(modifiers=Mod1); };
    interpret Alt_R+AnyOf(all) { action= SetMods(modifiers=Mod1); };
    interpret Super_L+AnyOf(all) { action= SetMods(modifiers=Mod4); };
    interpret Super_R+AnyOf(all) { action= SetMods(modifiers=Mod4); };
    interpret Caps_Lock { action= LockMods(modifiers=Lock); };
    interpret Num_Lock { action= LockMods(modifiers=NumLock); };
};
xkb_symbols "pc+us" {
    name[group1]="English (US)";
    key <ESC>  { [ Escape ] };
    key <AE01> { [ 1, exclam ] };
    key <AE02> { [ 2, at ] };
    key <AE03> { [ 3, numbersign ] };
    key <AE04> { [ 4, dollar ] };
    key <AE05> { [ 5, percent ] };
    key <AE06> { [ 6, asciicircum ] };
    key <AE07> { [ 7, ampersand ] };
    key <AE08> { [ 8, asterisk ] };
    key <AE09> { [ 9, parenleft ] };
    key <AE10> { [ 0, parenright ] };
    key <AE11> { [ minus, underscore ] };
    key <AE12> { [ equal, plus ] };
    key <BKSP> { [ BackSpace, BackSpace ] };
    key <TAB>  { [ Tab, ISO_Left_Tab ] };
    key <AD01> { [ q, Q ] };
    key <AD02> { [ w, W ] };
    key <AD03> { [ e, E ] };
    key <AD04> { [ r, R ] };
    key <AD05> { [ t, T ] };
    key <AD06> { [ y, Y ] };
    key <AD07> { [ u, U ] };
    key <AD08> { [ i, I ] };
    key <AD09> { [ o, O ] };
    key <AD10> { [ p, P ] };
    key <AD11> { [ bracketleft, braceleft ] };
    key <AD12> { [ bracketright, braceright ] };
    key <RTRN> { [ Return ] };
    key <LCTL> { [ Control_L ] };
    key <AC01> { [ a, A ] };
    key <AC02> { [ s, S ] };
    key <AC03> { [ d, D ] };
    key <AC04> { [ f, F ] };
    key <AC05> { [ g, G ] };
    key <AC06> { [ h, H ] };
    key <AC07> { [ j, J ] };
    key <AC08> { [ k, K ] };
    key <AC09> { [ l, L ] };
    key <AC10> { [ semicolon, colon ] };
    key <AC11> { [ apostrophe, quotedbl ] };
    key <TLDE> { [ grave, asciitilde ] };
    key <LFSH> { [ Shift_L ] };
    key <BKSL> { [ backslash, bar ] };
    key <AB01> { [ z, Z ] };
    key <AB02> { [ x, X ] };
    key <AB03> { [ c, C ] };
    key <AB04> { [ v, V ] };
    key <AB05> { [ b, B ] };
    key <AB06> { [ n, N ] };
    key <AB07> { [ m, M ] };
    key <AB08> { [ comma, less ] };
    key <AB09> { [ period, greater ] };
    key <AB10> { [ slash, question ] };
    key <RTSH> { [ Shift_R ] };
    key <KPMU> { [ KP_Multiply ] };
    key <LALT> { [ Alt_L, Meta_L ] };
    key <SPCE> { [ space ] };
    key <CAPS> { [ Caps_Lock ] };
    key <FK01> { [ F1 ] }; key <FK02> { [ F2 ] }; key <FK03> { [ F3 ] };
    key <FK04> { [ F4 ] }; key <FK05> { [ F5 ] }; key <FK06> { [ F6 ] };
    key <FK07> { [ F7 ] }; key <FK08> { [ F8 ] }; key <FK09> { [ F9 ] };
    key <FK10> { [ F10 ] }; key <FK11> { [ F11 ] }; key <FK12> { [ F12 ] };
    key <NMLK> { [ Num_Lock ] };
    key <SCLK> { [ Scroll_Lock ] };
    key <KP7>  { [ KP_Home, KP_7 ] }; key <KP8>  { [ KP_Up, KP_8 ] };
    key <KP9>  { [ KP_Prior, KP_9 ] }; key <KPSU> { [ KP_Subtract ] };
    key <KP4>  { [ KP_Left, KP_4 ] }; key <KP5>  { [ KP_Begin, KP_5 ] };
    key <KP6>  { [ KP_Right, KP_6 ] }; key <KPAD> { [ KP_Add ] };
    key <KP1>  { [ KP_End, KP_1 ] }; key <KP2>  { [ KP_Down, KP_2 ] };
    key <KP3>  { [ KP_Next, KP_3 ] }; key <KP0>  { [ KP_Insert, KP_0 ] };
    key <KPDL> { [ KP_Delete, KP_Decimal ] };
    key <KPEN> { [ KP_Enter ] }; key <RCTL> { [ Control_R ] };
    key <KPDV> { [ KP_Divide ] }; key <PRSC> { [ Print, Sys_Req ] };
    key <RALT> { [ Alt_R, Meta_R ] };
    key <HOME> { [ Home ] }; key <UP>   { [ Up ] }; key <PGUP> { [ Prior ] };
    key <LEFT> { [ Left ] }; key <RGHT> { [ Right ] }; key <END>  { [ End ] };
    key <DOWN> { [ Down ] }; key <PGDN> { [ Next ] }; key <INS>  { [ Insert ] };
    key <DELE> { [ Delete ] };
    key <PAUS> { [ Pause, Break ] };
    key <LWIN> { [ Super_L ] }; key <RWIN> { [ Super_R ] }; key <COMP> { [ Menu ] };
    modifier_map Shift { <LFSH>, <RTSH> };
    modifier_map Lock { <CAPS> };
    modifier_map Control { <LCTL>, <RCTL> };
    modifier_map Mod1 { <LALT>, <RALT> };
    modifier_map Mod2 { <NMLK> };
    modifier_map Mod4 { <LWIN>, <RWIN> };
};
};
"#,
"\0"
).as_bytes();

fn evdev_modifier_bit(evdev_code: u16) -> Option<u32> {
    match evdev_code {
        42 | 54 => Some(MOD_SHIFT),
        29 | 97 => Some(MOD_CTRL),
        56 | 100 => Some(MOD_ALT),
        125 | 126 => Some(MOD_SUPER),
        _ => None,
    }
}

fn mouse_button_code(btn: MouseButton) -> u32 {
    match btn {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Side => BTN_SIDE,
        MouseButton::Extra => BTN_EXTRA,
    }
}

pub struct WaylandVirtualInputSink {
    conn: WaylandConn,
    ptr_mgr_id: u32,
    ptr_id: u32,
    kbd_id: u32,
    start: Instant,
    held: HeldInputTracker,
    mods_depressed: u32,
    mods_locked: u32,
}

impl WaylandVirtualInputSink {
    pub fn connect(socket_path: &Path) -> Result<Self> {
        let conn = WaylandConn::connect(socket_path)?;
        Self::from_conn(conn)
    }

    #[cfg(test)]
    pub fn from_stream(stream: UnixStream) -> Result<Self> {
        let conn = WaylandConn::from_stream(stream)?;
        Self::from_conn(conn)
    }

    fn from_conn(mut conn: WaylandConn) -> Result<Self> {
        let seat_global = conn
            .globals
            .iter()
            .find(|g| g.interface == "wl_seat")
            .cloned()
            .context("compositor lacks wl_seat")?;
        let ptr_mgr_global = conn
            .globals
            .iter()
            .find(|g| g.interface == "zwlr_virtual_pointer_manager_v1")
            .cloned()
            .context("compositor lacks zwlr_virtual_pointer_manager_v1")?;
        let kbd_mgr_global = conn
            .globals
            .iter()
            .find(|g| g.interface == "zwp_virtual_keyboard_manager_v1")
            .cloned()
            .context("compositor lacks zwp_virtual_keyboard_manager_v1")?;

        let seat_id = conn.bind_global(&seat_global, seat_global.version.min(7))?;
        let ptr_mgr_id = conn.bind_global(&ptr_mgr_global, ptr_mgr_global.version.min(2))?;
        let kbd_mgr_id = conn.bind_global(&kbd_mgr_global, 1)?;

        let ptr_id = conn.alloc_id();
        let mut create_ptr = MsgBuilder::new(ptr_mgr_id, WLR_PTR_MGR_OP_CREATE_POINTER);
        create_ptr.push_u32(0); // optional seat = null
        create_ptr.push_u32(ptr_id);
        conn.send(create_ptr)?;

        let kbd_id = conn.alloc_id();
        let mut create_kbd = MsgBuilder::new(kbd_mgr_id, ZWP_KBD_MGR_OP_CREATE_KEYBOARD);
        create_kbd.push_u32(seat_id);
        create_kbd.push_u32(kbd_id);
        conn.send(create_kbd)?;

        // Upload US XKB keymap over memfd
        let fd = create_memfd("usher-xkb-keymap", XKB_KEYMAP_US.len())?;
        let mut file = File::from(fd);
        file.write_all(XKB_KEYMAP_US)
            .context("writing XKB keymap to memfd")?;
        file.flush().ok();

        let mut km = MsgBuilder::new(kbd_id, ZWP_KBD_OP_KEYMAP);
        km.push_u32(WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1);
        km.push_fd(file.as_raw_fd());
        km.push_u32(XKB_KEYMAP_US.len() as u32);
        conn.send(km)?;
        conn.roundtrip()?;

        Ok(Self {
            conn,
            ptr_mgr_id,
            ptr_id,
            kbd_id,
            start: Instant::now(),
            held: HeldInputTracker::default(),
            mods_depressed: 0,
            mods_locked: 0,
        })
    }

    fn time_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn send_modifiers(&mut self) {
        let mut m = MsgBuilder::new(self.kbd_id, ZWP_KBD_OP_MODIFIERS);
        m.push_u32(self.mods_depressed);
        m.push_u32(0); // mods_latched
        m.push_u32(self.mods_locked);
        m.push_u32(0); // group
        let _ = self.conn.send(m);
    }

    fn send_pointer_frame(&mut self) {
        let _ = self.conn.send(MsgBuilder::new(self.ptr_id, WLR_PTR_OP_FRAME));
    }

    fn handle_inner(&mut self, event: InputEvent) {
        let t = self.time_ms();
        match event {
            InputEvent::MouseMoveAbs { x, y } => {
                let mut m = MsgBuilder::new(self.ptr_id, WLR_PTR_OP_MOTION_ABSOLUTE);
                m.push_u32(t);
                m.push_u32(x as u32);
                m.push_u32(y as u32);
                m.push_u32(65535);
                m.push_u32(65535);
                if self.conn.send(m).is_ok() {
                    self.send_pointer_frame();
                }
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                let mut m = MsgBuilder::new(self.ptr_id, WLR_PTR_OP_MOTION);
                m.push_u32(t);
                m.push_i32(dx.saturating_mul(256));
                m.push_i32(dy.saturating_mul(256));
                if self.conn.send(m).is_ok() {
                    self.send_pointer_frame();
                }
            }
            InputEvent::MouseButton { button, pressed } => {
                let mut m = MsgBuilder::new(self.ptr_id, WLR_PTR_OP_BUTTON);
                m.push_u32(t);
                m.push_u32(mouse_button_code(button));
                m.push_u32(u32::from(pressed));
                if self.conn.send(m).is_ok() {
                    self.send_pointer_frame();
                }
            }
            InputEvent::MouseWheel { dx, dy } => {
                let mut any = false;
                if dy != 0 {
                    let mut m = MsgBuilder::new(self.ptr_id, WLR_PTR_OP_AXIS);
                    m.push_u32(t);
                    m.push_u32(WLR_AXIS_VERTICAL);
                    m.push_i32((-dy as i32) * 15 * 256);
                    any |= self.conn.send(m).is_ok();
                }
                if dx != 0 {
                    let mut m = MsgBuilder::new(self.ptr_id, WLR_PTR_OP_AXIS);
                    m.push_u32(t);
                    m.push_u32(WLR_AXIS_HORIZONTAL);
                    m.push_i32((dx as i32) * 15 * 256);
                    any |= self.conn.send(m).is_ok();
                }
                if any {
                    self.send_pointer_frame();
                }
            }
            InputEvent::MouseWheelHiRes { dx, dy } => {
                let mut any = false;
                if dy != 0 {
                    let mut m = MsgBuilder::new(self.ptr_id, WLR_PTR_OP_AXIS);
                    m.push_u32(t);
                    m.push_u32(WLR_AXIS_VERTICAL);
                    m.push_i32(((-dy as i32) * 15 * 256) / 120);
                    any |= self.conn.send(m).is_ok();
                }
                if dx != 0 {
                    let mut m = MsgBuilder::new(self.ptr_id, WLR_PTR_OP_AXIS);
                    m.push_u32(t);
                    m.push_u32(WLR_AXIS_HORIZONTAL);
                    m.push_i32(((dx as i32) * 15 * 256) / 120);
                    any |= self.conn.send(m).is_ok();
                }
                if any {
                    self.send_pointer_frame();
                }
            }
            InputEvent::Key {
                evdev_code,
                pressed,
            } => {
                let mut k = MsgBuilder::new(self.kbd_id, ZWP_KBD_OP_KEY);
                k.push_u32(t);
                k.push_u32(evdev_code as u32);
                k.push_u32(u32::from(pressed));
                let _ = self.conn.send(k);

                let mut mod_changed = false;
                if let Some(bit) = evdev_modifier_bit(evdev_code) {
                    let prev = self.mods_depressed;
                    if pressed {
                        self.mods_depressed |= bit;
                    } else {
                        self.mods_depressed &= !bit;
                    }
                    mod_changed = self.mods_depressed != prev;
                } else if evdev_code == 58 && pressed {
                    self.mods_locked ^= MOD_LOCK;
                    mod_changed = true;
                }
                if mod_changed {
                    self.send_modifiers();
                }
            }
            InputEvent::GamepadState { .. } | InputEvent::ReleaseAll => {}
        }
    }
}

impl InputSink for WaylandVirtualInputSink {
    fn handle(&mut self, event: InputEvent) {
        if matches!(event, InputEvent::ReleaseAll) {
            for rel in self.held.drain_release_events() {
                self.handle_inner(rel);
            }
            if self.mods_depressed != 0 || self.mods_locked != 0 {
                self.mods_depressed = 0;
                self.mods_locked = 0;
                self.send_modifiers();
            }
            return;
        }
        self.held.record(&event);
        self.handle_inner(event);
    }
}

impl Drop for WaylandVirtualInputSink {
    fn drop(&mut self) {
        let _ = self.conn.send(MsgBuilder::new(self.ptr_id, WLR_PTR_OP_DESTROY));
        let _ = self.conn.send(MsgBuilder::new(self.kbd_id, ZWP_KBD_OP_DESTROY));
        let _ = self.conn.send(MsgBuilder::new(self.ptr_mgr_id, WLR_PTR_MGR_OP_DESTROY));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};

    fn read_msg(stream: &mut UnixStream) -> (u32, u16, Vec<u8>, Option<OwnedFd>) {
        let mut hdr_buf = [0u8; 8];
        let mut cbuf = [0u64; 8];
        let mut iov = libc::iovec {
            iov_base: hdr_buf.as_mut_ptr().cast::<libc::c_void>(),
            iov_len: 8,
        };
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &mut iov;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = cbuf.as_mut_ptr().cast::<libc::c_void>();
        msghdr.msg_controllen = std::mem::size_of_val(&cbuf) as _;

        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msghdr, 0) };
        assert_eq!(n, 8);
        let mut fd = None;
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msghdr);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                {
                    let mut raw: RawFd = -1;
                    std::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(cmsg),
                        (&mut raw as *mut RawFd).cast::<u8>(),
                        std::mem::size_of::<RawFd>(),
                    );
                    if raw >= 0 {
                        fd = Some(OwnedFd::from_raw_fd(raw));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msghdr, cmsg);
            }
        }
        let obj = u32::from_ne_bytes(hdr_buf[0..4].try_into().unwrap());
        let word1 = u32::from_ne_bytes(hdr_buf[4..8].try_into().unwrap());
        let size = (word1 >> 16) as usize;
        let op = (word1 & 0xffff) as u16;
        let mut pay = vec![0u8; size - 8];
        if !pay.is_empty() {
            use std::io::Read;
            stream.read_exact(&mut pay).unwrap();
        }
        (obj, op, pay, fd)
    }

    fn send_ev(stream: &mut UnixStream, msg: MsgBuilder) {
        let (b, _) = msg.finish();
        stream.write_all(&b).unwrap();
    }

    #[test]
    fn wayland_virtual_input_sends_pointer_and_keyboard_events() {
        let (client_sock, mut server_sock) = UnixStream::pair().unwrap();

        let server = std::thread::spawn(move || {
            let (_, _, pay1, _) = read_msg(&mut server_sock); // get_registry
            let reg_id = u32::from_ne_bytes(pay1[0..4].try_into().unwrap());
            let (_, _, pay2, _) = read_msg(&mut server_sock); // sync
            let cb1 = u32::from_ne_bytes(pay2[0..4].try_into().unwrap());

            for (n, i, v) in [
                (1u32, "wl_seat", 7u32),
                (2u32, "zwlr_virtual_pointer_manager_v1", 2u32),
                (3u32, "zwp_virtual_keyboard_manager_v1", 1u32),
            ] {
                let mut g = MsgBuilder::new(reg_id, 0);
                g.push_u32(n);
                g.push_str(i);
                g.push_u32(v);
                send_ev(&mut server_sock, g);
            }
            let mut done1 = MsgBuilder::new(cb1, 0);
            done1.push_u32(1);
            send_ev(&mut server_sock, done1);

            // Expect 3x bind, create_pointer, create_keyboard, keymap(with memfd), sync
            for _ in 0..3 {
                let _ = read_msg(&mut server_sock);
            }
            let (_, op_cp, pay_cp, _) = read_msg(&mut server_sock);
            assert_eq!(op_cp, WLR_PTR_MGR_OP_CREATE_POINTER);
            let ptr_id = u32::from_ne_bytes(pay_cp[4..8].try_into().unwrap());

            let (_, op_ck, pay_ck, _) = read_msg(&mut server_sock);
            assert_eq!(op_ck, ZWP_KBD_MGR_OP_CREATE_KEYBOARD);
            let kbd_id = u32::from_ne_bytes(pay_ck[4..8].try_into().unwrap());

            let (obj_km, op_km, pay_km, km_fd) = read_msg(&mut server_sock);
            assert_eq!((obj_km, op_km), (kbd_id, ZWP_KBD_OP_KEYMAP));
            let fmt = u32::from_ne_bytes(pay_km[0..4].try_into().unwrap());
            let sz = u32::from_ne_bytes(pay_km[4..8].try_into().unwrap());
            assert_eq!(fmt, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1);
            assert_eq!(sz as usize, XKB_KEYMAP_US.len());
            assert!(km_fd.is_some());

            let (_, _, sync2, _) = read_msg(&mut server_sock);
            let cb2 = u32::from_ne_bytes(sync2[0..4].try_into().unwrap());
            let mut done2 = MsgBuilder::new(cb2, 0);
            done2.push_u32(2);
            send_ev(&mut server_sock, done2);

            // Expect MouseMoveAbs -> motion_absolute + frame
            let (o, op, p, _) = read_msg(&mut server_sock);
            assert_eq!((o, op), (ptr_id, WLR_PTR_OP_MOTION_ABSOLUTE));
            let x = u32::from_ne_bytes(p[4..8].try_into().unwrap());
            let y = u32::from_ne_bytes(p[8..12].try_into().unwrap());
            assert_eq!((x, y), (1000, 2000));
            let (o, op, _, _) = read_msg(&mut server_sock);
            assert_eq!((o, op), (ptr_id, WLR_PTR_OP_FRAME));

            // Expect Key Shift down -> key + modifiers(MOD_SHIFT)
            let (o, op, p, _) = read_msg(&mut server_sock);
            assert_eq!((o, op), (kbd_id, ZWP_KBD_OP_KEY));
            let code = u32::from_ne_bytes(p[4..8].try_into().unwrap());
            let state = u32::from_ne_bytes(p[8..12].try_into().unwrap());
            assert_eq!((code, state), (42, 1));
            let (o, op, p, _) = read_msg(&mut server_sock);
            assert_eq!((o, op), (kbd_id, ZWP_KBD_OP_MODIFIERS));
            let dep = u32::from_ne_bytes(p[0..4].try_into().unwrap());
            assert_eq!(dep, MOD_SHIFT);

            // Expect ReleaseAll -> Shift up + modifiers(0)
            let (o, op, p, _) = read_msg(&mut server_sock);
            assert_eq!((o, op), (kbd_id, ZWP_KBD_OP_KEY));
            assert_eq!(u32::from_ne_bytes(p[8..12].try_into().unwrap()), 0);
            let (o, op, p, _) = read_msg(&mut server_sock);
            assert_eq!((o, op), (kbd_id, ZWP_KBD_OP_MODIFIERS));
            assert_eq!(u32::from_ne_bytes(p[0..4].try_into().unwrap()), 0);
        });

        let mut sink = WaylandVirtualInputSink::from_stream(client_sock).unwrap();
        sink.handle(InputEvent::MouseMoveAbs { x: 1000, y: 2000 });
        sink.handle(InputEvent::Key {
            evdev_code: 42,
            pressed: true,
        });
        sink.handle(InputEvent::ReleaseAll);
        drop(sink);
        server.join().unwrap();
    }
}
