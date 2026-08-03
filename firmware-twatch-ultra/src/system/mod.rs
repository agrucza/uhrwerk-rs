//! Board-specific system glue for the T-Watch Ultra: the
//! `system_core::board::Board` impl, PMU/expander bring-up, and the
//! haptics and GPS dispatch tasks.

pub mod gps;
pub mod haptics;
pub mod power;
