//! Durable workspace storage backed by zvec.

mod backend;
mod codec;
mod dictionary;
pub(crate) mod spi;
mod zvec;

pub(crate) use backend::ZvecStorageFactory;
