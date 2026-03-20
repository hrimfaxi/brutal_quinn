#[cfg(any(
    all(feature = "jls", feature = "iroh"),
    all(feature = "jls", feature = "quinn"),
    all(feature = "iroh", feature = "quinn"),
))]
compile_error!("features 'jls', 'iroh', and 'quinn' are mutually exclusive");

#[cfg(not(any(feature = "jls", feature = "iroh", feature = "quinn")))]
compile_error!("enable one of: 'jls', 'iroh', or 'quinn'");

mod brutal_core;

#[cfg(feature = "jls")]
mod jls;

#[cfg(feature = "iroh")]
mod iroh;

pub use brutal_core::BrutalConfig;

#[cfg(feature = "jls")]
pub use jls::*;

#[cfg(feature = "iroh")]
pub use iroh::*;
