//! flavite xtask binary — thin wrapper over the `xtask` library.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    xtask::run(&args)
}
