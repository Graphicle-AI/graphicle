//! The `graphicle` program of this crate: the shared command line
//! ([`graphicle_runtime::cli`]) with no extensions installed. An edition with
//! extensions builds its own program on the same command line.

// Projection creates many short-lived metadata records. Use the same allocator
// as the standalone graph engine; library callers retain control of their own.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let code = graphicle_runtime::cli::main_with(graphicle_runtime::extension::Extensions::none());
    if code != 0 {
        std::process::exit(code);
    }
}
