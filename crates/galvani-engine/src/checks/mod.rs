//! Design-rule / physics checks that run against a parsed or solved board.
//!
//! Each check is self-contained: it takes an
//! [`ExtractedBoard`](galvani_extract::ExtractedBoard) (and, where it needs
//! physics, builds and solves its own [`Circuit`](galvani_ir::Circuit)), and
//! returns a verdict plus the numbers behind it. They are kept separate from
//! the bind-time [`stress`](crate::stress) monitor: stress watches a running
//! co-simulation against datasheet ratings, whereas a check answers a specific
//! standards question about the design.
//!
//! - [`usb_c`]: the USB Type-C CC attach classifier. It attaches a generic
//!   source + cable model to a board's CC termination and classifies the result
//!   against the USB Type-C spec windows (Sink / AudioAccessory / ...). This is
//!   what re-derives the RPi 4 shared-CC-pulldown fault cold.
//! - [`straps`]: the boot strapping-pin lint. It reads each MCU's strap table
//!   from the model db and flags a strap net that cannot hold the level the part
//!   needs at reset (a free-running clock on it, or a pull to the wrong rail).

pub mod straps;
pub mod usb_c;
