use std::error::Error;
use std::ffi::{CStr, CString};
use std::str::Utf8Error;
use std::sync::OnceLock;
use xcb::x::EventMask;
use xcb::{x, Connection};
use xkbcommon::xkb;

// TODO: Add simple tty lock as well

fn main() {
    let lock = Lock::lock_screen().unwrap();
    lock.handle_events().unwrap();
}

#[derive(Debug, Clone, Copy)]
enum Auth {
    Correct,
    Incorrect,
}

fn pass_check(pass: &str) -> Auth {
    let hash = get_hash();
    if pwhash::unix::verify(pass.trim(), hash) {
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

    fn handle_events(&self) -> Result<(), Box<dyn Error>> {
        let keyb = Keyb::new().expect("failed to acquire keyboard state");
        let mut acc = Accumulator::new();
        loop {
            acc.get_input(&self.conn, &keyb);
            let pass = acc.build_str().unwrap();
            if !pass.is_empty() {
                if matches!(pass_check(pass), Auth::Correct) {
                    break;
                }
                acc.clear();
                continue;
            } else {
                // escape hatch if pass fails (for dev only)
                break;
            }
        }
        Ok(())
    }
}

struct Accumulator {
    buf: Vec<u8>,
    len: usize,
}

impl Accumulator {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(10),
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.len = 0;
    }

    fn push_char(&mut self, c: char) {
        if self.len > 255 {
            self.clear();
        }
        self.buf.push(c as _);
        self.len += 1;
    }

    fn build_str(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.buf[..self.len])
    }

    fn get_input(&mut self, conn: &Connection, keyb: &Keyb) {
        loop {
            let xcb::Event::X(x::Event::KeyPress(key_press)) =
                conn.wait_for_event().expect("failed to poll for event")
            else {
                continue;
            };
            let code = key_press.detail();
            if keyb.is_enter(code) {
                break;
            }
            let Some(ch) = keyb.keycode_to_char(code) else {
                self.clear();
                break;
            };
            self.push_char(ch);
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

    fn keycode_to_char(&self, code: x::Keycode) -> Option<char> {
        char::from_u32(self.keycode_to_keysym(code).raw())
    }

    fn is_enter(&self, code: x::Keycode) -> bool {
        let sym = self.keycode_to_keysym(code);
        xkb::Keysym::Return == sym
    }
}
