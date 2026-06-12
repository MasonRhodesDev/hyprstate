//! Impure snapshot/read helpers shared by the CLI, daemon, and powerd.
//! (Named `sysio` to stay unambiguous next to `std::io`.)

pub mod gpu_state;
pub mod hypr_instance;
pub mod power_conf;
pub mod profiles;
pub mod sysfs;
