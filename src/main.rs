use std::{error::Error, time::Duration};
use xcb::{
    x::{self, EventMask, Gcontext, Rectangle},
    Connection,
};
use xkbcommon::xkb::{self, x11, KeyDirection, State};

const MAX_BUF_SIZE: usize = 15;
const MIN_BUF_CAP: usize = 15;

// TODO: Add proper error handling
// TODO: Add simple tty lock as well

fn main() {
    println!("Locking screen");
    Lock::lock_screen()
        .expect("failed to lock the screen")
        .authenticate()
        .expect("failure occured while trying to authenticate password");
    println!("Unlocking screen");
}

// TODO: Handle multiple screens
struct Lock {
    cursor: x::Cursor,
    lock: x::Window,
    conn: Connection,
    scr_no: i32,
    gc: Gcontext,
    width: u16,
    height: u16,
}

impl Lock {
    #[inline]
    fn new() -> Result<Self, Box<dyn Error>> {
        let (conn, scr_no) = Connection::connect(None)?;
        let (cursor, lock, gc) = (conn.generate_id(), conn.generate_id(), conn.generate_id());
        Ok(Self {
            lock,
            cursor,
            conn,
            scr_no,
            gc,
            width: 0,
            height: 0,
        })
    }

    #[inline]
    fn draw_win(&mut self) -> Result<(), Box<dyn Error>> {
        let screen = self
            .conn
            .get_setup()
            .roots()
            .nth(self.scr_no as usize)
            .expect("unexpected failure while getting screen");
        self.conn.send_and_check_request(&x::CreateWindow {
            depth: screen.root_depth(),
            wid: self.lock,
            parent: screen.root(),
            x: 0,
            y: 0,
            width: screen.width_in_pixels(),
            height: screen.height_in_pixels(),
            border_width: 0,
            class: x::WindowClass::CopyFromParent,
            visual: screen.root_visual(),
            value_list: &[
                x::Cw::OverrideRedirect(true),
                x::Cw::EventMask(
                    x::EventMask::KEY_PRESS | x::EventMask::KEY_RELEASE | x::EventMask::EXPOSURE,
                ),
            ],
        })?;
        self.width = screen.width_in_pixels();
        self.height = screen.height_in_pixels();
        // Create GC and set foreground color
        self.conn.send_and_check_request(&x::CreateGc {
            cid: self.gc,
            drawable: x::Drawable::Window(self.lock),
            value_list: &[x::Gc::Foreground(rgb2u32(BLACK))],
        })?;

        self.conn
            .send_and_check_request(&x::MapWindow { window: self.lock })?;
        self.flush()?;

        // wait until window is drawn
        while !matches!(
            self.conn.wait_for_event()?,
            xcb::Event::X(x::Event::Expose(_))
        ) {}

        self.conn.send_and_check_request(&x::PolyFillRectangle {
            drawable: x::Drawable::Window(self.lock),
            gc: self.gc,
            rectangles: &[Rectangle {
                x: 0,
                y: 0,
                width: screen.width_in_pixels(),
                height: screen.height_in_pixels(),
            }],
        })?;
        Ok(())
    }

    #[inline]
    fn init_cursor(&self) -> Result<(), Box<dyn Error>> {
        let font: x::Font = self.conn.generate_id();
        self.conn.send_and_check_request(&x::OpenFont {
            fid: font,
            name: "cursor".as_bytes(),
        })?;
        self.conn.send_and_check_request(&x::CreateGlyphCursor {
            cid: self.cursor,
            source_font: font,
            mask_font: font,
            source_char: ' ' as u16,
            mask_char: ' ' as u16,
            fore_red: 0,
            fore_green: 0,
            fore_blue: 0,
            back_red: 0,
            back_green: 0,
            back_blue: 0,
        })?;
        Ok(())
    }

    #[inline]
    fn grab_cursor(&self) {
        self.conn.send_request(&x::GrabPointer {
            owner_events: false,
            grab_window: self.lock,
            event_mask: EventMask::empty(),
            pointer_mode: x::GrabMode::Async,
            keyboard_mode: x::GrabMode::Async,
            confine_to: self.lock,
            cursor: self.cursor,
            time: x::CURRENT_TIME,
        });
    }

    #[inline]
    fn grab_keyboard(&self) {
        self.conn.send_request(&x::GrabKeyboard {
            owner_events: true,
            grab_window: self.lock,
            time: x::CURRENT_TIME,
            pointer_mode: x::GrabMode::Async,
            keyboard_mode: x::GrabMode::Async,
        });
    }

    fn set_win_color(&self, rgb: RGB) -> Result<(), Box<dyn Error>> {
        self.conn.send_and_check_request(&x::ChangeGc {
            gc: self.gc,
            value_list: &[x::Gc::Foreground(rgb2u32(rgb))],
        })?;
        self.conn.send_and_check_request(&x::PolyFillRectangle {
            drawable: x::Drawable::Window(self.lock),
            gc: self.gc,
            rectangles: &[Rectangle {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            }],
        })?;
        self.flush()
    }

    #[inline]
    fn flush(&self) -> Result<(), Box<dyn Error>> {
        self.conn.flush()?;
        Ok(())
    }

    #[inline]
    fn lock_screen() -> Result<Lock, Box<dyn Error>> {
        let mut lock = Lock::new()?;
        lock.draw_win()?;
        lock.init_cursor()?;
        lock.grab_cursor();
        lock.grab_keyboard();
        lock.flush()?;
        Ok(lock)
    }

    fn authenticate(&self) -> Result<(), Box<dyn Error>> {
        let mut pam_client = pam::Client::with_password("system-auth")?;
        let user = std::env::var("USER").unwrap();
        let mut handler = InputHandler::new(&self.conn);
        loop {
            handler.get_input(self);
            let pass = handler.get_str();
            if !pass.is_empty() {
                pam_client.conversation_mut().set_credentials(&user, pass);
                if pam_client.authenticate().is_ok() {
                    self.set_win_color(GREEN)?;
                    std::thread::sleep(Duration::from_millis(500));
                    break;
                } else {
                    self.set_win_color(RED)?;
                    std::thread::sleep(Duration::from_millis(500));
                }
                handler.clear();
            }
        }
        Ok(())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.conn.send_request(&x::FreeCursor {
            cursor: self.cursor,
        });
        self.conn.send_request(&x::UngrabKeyboard {
            time: x::CURRENT_TIME,
        });
        self.conn.send_request(&x::UngrabPointer {
            time: x::CURRENT_TIME,
        });
        self.conn
            .send_request(&x::DestroyWindow { window: self.lock });
        let _ = self.conn.flush();
    }
}

struct InputHandler {
    buf: String,
    keyb: Keyb,
}

impl InputHandler {
    fn new(conn: &Connection) -> Self {
        Self {
            buf: String::with_capacity(MIN_BUF_CAP),
            keyb: Keyb::new(conn),
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
    }

    fn push_char(&mut self, c: char) {
        if self.buf.len() == MAX_BUF_SIZE {
            println!("Reached max input buffer size");
            self.clear();
        }
        self.buf.push(c);
    }

    fn pop_char(&mut self) {
        self.buf.pop();
    }

    fn get_str(&self) -> &str {
        &self.buf
    }

    fn get_input(&mut self, lock: &Lock) {
        lock.set_win_color(BLACK).unwrap();
        loop {
            let event = match lock.conn.wait_for_event() {
                Ok(xcb::Event::X(x::Event::KeyPress(event))) => {
                    self.keyb.update(event.detail(), KeyDirection::Down);
                    event
                }
                Ok(xcb::Event::X(x::Event::KeyRelease(event))) => {
                    self.keyb.update(event.detail(), KeyDirection::Up);
                    continue;
                }
                _ => continue,
            };

            match self.keyb.keycode_to_keysym(event.detail()) {
                xkb::Keysym::Return => {
                    lock.set_win_color(WHITE).unwrap();
                    break;
                }
                xkb::Keysym::Escape => {
                    lock.set_win_color(BLACK).unwrap();
                    self.clear();
                }
                xkb::Keysym::BackSpace => {
                    self.pop_char();
                }
                other if other.is_modifier_key() => (),
                other => {
                    let Some(ch) = other.key_char() else {
                        // password will be invalid anyway if it's not a valid char
                        // clearing it will fail auth correctly
                        self.clear();
                        break;
                    };

                    if self.buf.is_empty() {
                        lock.set_win_color(CYAN).unwrap();
                    }
                    self.push_char(ch);
                }
            }
        }
    }
}

struct Keyb(xkb::State);

impl Keyb {
    fn new(conn: &xcb::Connection) -> Self {
        let has_xkb = x11::setup_xkb_extension(
            conn,
            xkb::x11::MIN_MAJOR_XKB_VERSION,
            xkb::x11::MIN_MINOR_XKB_VERSION,
            xkb::x11::SetupXkbExtensionFlags::NoFlags,
            &mut 0,
            &mut 0,
            &mut 0,
            &mut 0,
        );
        if !has_xkb {
            panic!("XKB extension is not supported");
        }
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let device_id = x11::get_core_keyboard_device_id(conn);
        let keymap =
            x11::keymap_new_from_device(&context, conn, device_id, xkb::KEYMAP_COMPILE_NO_FLAGS);

        Self(Self::clear_mod_state(x11::state_new_from_device(
            &keymap, conn, device_id,
        )))
    }

    fn update(&mut self, code: u8, dir: KeyDirection) -> u32 {
        self.0.update_key(xkb::Keycode::from(code), dir)
    }

    fn keycode_to_keysym(&self, code: x::Keycode) -> xkb::Keysym {
        self.0.key_get_one_sym(xkb::Keycode::new(code as u32))
    }

    /// Clear active depressed or latched modifier state while preserving locked modifiers
    fn clear_mod_state(mut state: State) -> State {
        // Get the current locked modifiers (like CapsLock/NumLock)
        let locked_mods = state.serialize_mods(xkb::STATE_MODS_LOCKED);
        let locked_layout = state.serialize_layout(xkb::STATE_LAYOUT_LOCKED);

        // Apply the reset:
        // Clear Depressed and Latched, but keep Locked.
        state.update_mask(
            0,             // depressed (physically held keys - clears your Mod+Shift)
            0,             // latched (sticky keys)
            locked_mods,   // locked (keeps CapsLock/NumLock)
            0,             // depressed layout
            0,             // latched layout
            locked_layout, // locked layout
        );
        state
    }
}

type RGB = (u8, u8, u8);

const CYAN: RGB = (0, 255, 255);

const RED: RGB = (255, 0, 0);

const GREEN: RGB = (0, 255, 160);

const BLACK: RGB = (0, 0, 0);

const WHITE: RGB = (255, 255, 255);

#[inline]
fn rgb2u32((r, g, b): RGB) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}
