use std::error::Error;
use xcb::{
    x::{self, EventMask},
    Connection,
};
use xkbcommon::xkb::{self, x11, KeyDirection};

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
}

impl Lock {
    #[inline]
    fn new() -> Result<Self, Box<dyn Error>> {
        let (conn, scr_no) = Connection::connect(None)?;
        let (cursor, lock) = (conn.generate_id(), conn.generate_id());
        Ok(Self {
            lock,
            cursor,
            conn,
            scr_no,
        })
    }

    #[inline]
    fn draw_win(&self) -> Result<(), Box<dyn Error>> {
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
                x::Cw::BackPixel(screen.black_pixel()),
                x::Cw::OverrideRedirect(true),
                x::Cw::EventMask(
                    x::EventMask::KEY_PRESS
                        | x::EventMask::KEYMAP_STATE
                        | x::EventMask::KEY_RELEASE,
                ),
            ],
        })?;
        self.conn
            .send_and_check_request(&x::MapWindow { window: self.lock })?;
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

    #[inline]
    fn flush(&self) -> Result<(), Box<dyn Error>> {
        self.conn.flush()?;
        Ok(())
    }

    #[inline]
    fn lock_screen() -> Result<Lock, Box<dyn Error>> {
        let lock = Lock::new()?;
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
            handler.get_input(&self.conn);
            let pass = handler.get_str();
            if !pass.is_empty() {
                pam_client.conversation_mut().set_credentials(&user, pass);
                if pam_client.authenticate().is_ok() {
                    return Ok(());
                }
                handler.clear();
            }
        }
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

    fn get_input(&mut self, conn: &Connection) {
        loop {
            let event = match conn.wait_for_event() {
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
                    break;
                }
                xkb::Keysym::Escape => {
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
        Self(x11::state_new_from_device(&keymap, conn, device_id))
    }

    fn update(&mut self, code: u8, dir: KeyDirection) -> u32 {
        self.0.update_key(xkb::Keycode::from(code), dir)
    }

    fn keycode_to_keysym(&self, code: x::Keycode) -> xkb::Keysym {
        self.0.key_get_one_sym(xkb::Keycode::new(code as u32))
    }
}
