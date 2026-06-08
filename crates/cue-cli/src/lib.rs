#[cfg(feature = "extensions")]
mod companion_binary;
#[cfg(feature = "extensions")]
mod config;
#[cfg(feature = "extensions")]
mod extension_dispatch;
#[cfg(feature = "extensions")]
mod path_lookup;

#[cfg(feature = "extensions")]
pub fn run_extension(
    name: &str,
    args: &[std::ffi::OsString],
    supported_subcommands: &str,
) -> anyhow::Result<i32> {
    extension_dispatch::run(name, args, supported_subcommands)
}
