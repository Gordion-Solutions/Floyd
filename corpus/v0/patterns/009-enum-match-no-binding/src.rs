//! Floyd corpus pattern 009-enum-match-no-binding.
//!
//! Two-variant enum match without bindings — the canonical
//! state-machine dispatch shape. The decomposer synthesizes a
//! condition name from the scrutinee's type and rustc's variant
//! index (`mode == Mode::variant_0`). See `pattern.toml`.

pub enum Mode {
    On,
    Off,
}

pub fn is_active(mode: Mode) -> bool {
    match mode {
        Mode::On => true,
        Mode::Off => false,
    }
}
