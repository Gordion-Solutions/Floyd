//! Guarded command validation.
//!
//! A command is accepted only when (a) a request is present (the
//! `Option` holds a value), (b) the request's payload validates
//! against the protocol's structural checks, and (c) the system is
//! armed for command execution. The `if let` gates the structural
//! checks behind the presence of a request; the `&&` then gates
//! execution behind the armed signal. Same boolean function as a
//! three-condition AND, but realised in the shape Rust code
//! actually uses for guarded payload handling.

pub fn accept_command(req: Option<bool>, armed: bool) -> bool {
    if let Some(payload_ok) = req {
        payload_ok && armed
    } else {
        false
    }
}
