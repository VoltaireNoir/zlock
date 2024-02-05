use std::{
    error::Error,
    ffi::{CStr, CString},
    str::Utf8Error,
    sync::OnceLock,
    time::Duration,
};
use xcb::{
    x::{self, EventMask},
    Connection,
};
use xkbcommon::xkb;

const MAX_BUF_SIZE: usize = 500;
const MIN_BUF_CAP: usize = 15;

// TODO: Add proper error handling
// TODO: Add simple tty lock as well

fn main() {
    Lock::lock_screen()
        .expect("failed to lock the screen")
        .authenticate()
        .expect("failure occured while trying to authenticate password");
}

#[derive(Debug, Clone, Copy)]
enum Auth {
    Correct,
    Incorrect,
}

fn pass_check(pass: &str) -> Auth {
    let hash = get_hash();
    if pwhash::unix::verify(pass, hash) {
        return Auth::Correct;
    }
    Auth::Incorrect
}

fn get_hash() -> &'static str {
    // TODO: Add support for retrieving hash from passwd file if present
    static HASH: OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        let name = CString::new(std::env::var("USER").unwrap()).unwrap();
        let info = unsafe { libc::getspnam(name.as_ptr()) };
        if info.is_null() {
            panic!("Failed to acquire password hash. Make sure the executible is running as root");
        }
        let pass = unsafe { CStr::from_ptr((*info).sp_pwdp) };
        pass.to_str()
            .expect("Failed to acquire password hash: cannot convert to String")
            .to_owned()
    })
}

// TODO: Implement graceful shutdown/unlock (use Drop trait to: destroy win and cursor, ungrab keyboard and mouse)
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
                x::Cw::EventMask(x::EventMask::KEY_PRESS),
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
        let mut handler = InputHandler::new();
        loop {
            handler.get_input(&self.conn);
            let Ok(pass) = handler.build_str() else {
                handler.clear();
                continue;
            };
            if !pass.is_empty() {
                if matches!(pass_check(pass), Auth::Correct) {
                    break;
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
    buf: Vec<u8>,
    len: usize,
    keyb: Keyb,
}

impl InputHandler {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(MIN_BUF_CAP),
            len: 0,
            keyb: Keyb::new().expect("failed to acquire keyboard state"),
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.len = 0;
    }

    fn push_char(&mut self, c: char) {
        if self.len == MAX_BUF_SIZE {
            self.clear();
        }
        self.buf.push(c as _);
        self.len += 1;
    }

    fn pop_char(&mut self) {
        self.buf.pop();
        self.len = self.len.saturating_sub(1);
    }

    fn build_str(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.buf[..self.len])
    }

    fn get_input(&mut self, conn: &Connection) {
        loop {
            let xcb::Event::X(x::Event::KeyPress(key_press)) =
                conn.wait_for_event().expect("failed to poll for event")
            else {
                continue;
            };
            let code = key_press.detail();
            match self.keyb.keycode_to_keysym(code) {
                xkb::Keysym::Return => {
                    break;
                }
                xkb::Keysym::Escape => {
                    self.clear();
                }
                xkb::Keysym::BackSpace => {
                    self.pop_char();
                }
                other => {
                    let Some(ch) = Keyb::keysym_to_char(other) else {
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
    fn new() -> Option<Self> {
        let context = xkb::Context::new(0);
        xkb::Keymap::new_from_names(&context, "", "", "", "", None, 0)
            .map(|kmap| Keyb(xkb::State::new(&kmap)))
    }

    fn keycode_to_keysym(&self, code: x::Keycode) -> xkb::Keysym {
        self.0.key_get_one_sym(xkb::Keycode::new(code as u32))
    }

    fn keysym_to_char(key: xkb::Keysym) -> Option<char> {
        char::from_u32(key.raw())
    }
}
