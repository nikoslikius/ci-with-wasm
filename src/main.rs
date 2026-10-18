mod container;
mod git;
mod image;
mod logging;
mod runner;
mod server;
mod storage;
mod wasm;

use std::process::ExitCode;

fn main() -> anyhow::Result<ExitCode> {
    logging::init();

    if image::handle_internal_unpack()? {
        return Ok(ExitCode::SUCCESS);
    }
    if runner::handle_internal_run()? {
        return Ok(ExitCode::SUCCESS);
    }

    wasm::run_from_process_args()
}
