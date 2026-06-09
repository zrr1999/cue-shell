mod companion_binary;
mod config;
mod extension_dispatch;
mod path_lookup;

pub fn run_extension(
    name: &str,
    args: &[std::ffi::OsString],
    supported_subcommands: &str,
) -> anyhow::Result<i32> {
    extension_dispatch::run(name, args, supported_subcommands)
}
