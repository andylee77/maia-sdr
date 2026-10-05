//! The player's WebAssembly interface: plain exported functions, no bindings generator. The page
//! makes a player and holds it by its address; it writes the stream's bytes into the player's
//! input buffer and reads the sound from its output buffer, both in the module's memory.
//!
//! - `player_new(program)` (0: the first programme with sound) gives the player;
//! - `player_input(p, len)` gives where to write `len` bytes, then `player_push(p)` takes them;
//! - `player_sound(p)` moves to the next syncframe's sound and gives its sample pairs (0: none
//!   yet): `player_sound_ptr(p)` (f32, left and right), `player_sound_pts(p)` (90 kHz ticks,
//!   -1 when unknown) and `player_sound_rate(p)`;
//! - `player_describe(p)` gives the length of a JSON description at `player_text(p)`;
//! - `player_free(p)`.

use crate::player::{describe, Player};

pub struct Handle {
    player: Player,
    input: Vec<u8>,
    sound: Vec<f32>,
    pts: f64,
    rate: u32,
    text: String,
}

#[no_mangle]
pub extern "C" fn player_new(program: u32) -> *mut Handle {
    let program = u16::try_from(program).ok().filter(|&n| n != 0);
    Box::into_raw(Box::new(Handle { player: Player::new(program), input: Vec::new(), sound: Vec::new(), pts: -1.0, rate: 0, text: String::new() }))
}

/// # Safety
///
/// `p` came from `player_new` and is not used again.
#[no_mangle]
pub unsafe extern "C" fn player_free(p: *mut Handle) {
    drop(Box::from_raw(p));
}

#[no_mangle]
pub extern "C" fn player_input(h: &mut Handle, len: usize) -> *mut u8 {
    h.input.resize(len, 0);
    h.input.as_mut_ptr()
}

#[no_mangle]
pub extern "C" fn player_push(h: &mut Handle) {
    h.player.push(&h.input);
}

#[no_mangle]
pub extern "C" fn player_sound(h: &mut Handle) -> u32 {
    match h.player.sound() {
        Some(s) => {
            h.pts = s.pts.map_or(-1.0, |t| t as f64);
            h.rate = s.rate;
            h.sound = s.samples;
            (h.sound.len() / 2) as u32
        }
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn player_sound_ptr(h: &Handle) -> *const f32 {
    h.sound.as_ptr()
}

#[no_mangle]
pub extern "C" fn player_sound_pts(h: &Handle) -> f64 {
    h.pts
}

#[no_mangle]
pub extern "C" fn player_sound_rate(h: &Handle) -> u32 {
    h.rate
}

#[no_mangle]
pub extern "C" fn player_describe(h: &mut Handle) -> u32 {
    h.text = describe(&h.player);
    h.text.len() as u32
}

#[no_mangle]
pub extern "C" fn player_text(h: &Handle) -> *const u8 {
    h.text.as_ptr()
}
