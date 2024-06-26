// Copyright (c) 2022 MASSA LABS <info@massa.net>
//! Hash management crate

#![warn(missing_docs)]
#![warn(unused_crate_dependencies)]
pub use error::MassaHashError;
pub use settings::HASH_SIZE_BYTES;
pub use settings::HASH_XOF_SIZE_BYTES;

mod error;
mod hash;
mod hash_xof;
pub use hash::*;
pub use hash_xof::*;
mod settings;
