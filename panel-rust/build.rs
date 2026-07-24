fn main() {
    // Slint's compiler recurses through ui/app.slint's full component tree
    // (app/sidebar/chat_area/message_card/palette/types, deeply nested
    // since the Phase 1 modular-.slint migration); Windows' default
    // 1MB main-thread stack isn't enough for that recursion depth and
    // crashes the build script with STATUS_STACK_OVERFLOW, confirmed on a
    // real windows-latest CI run (Linux's 8MB default stack masks the
    // same recursion depth). Run the actual compile on a thread with an
    // explicit, generous stack size instead of relying on the platform
    // default -- portable across all targets, no linker/platform-specific
    // stack-size flags needed.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            slint_build::compile_with_config(
                "ui/app.slint",
                slint_build::CompilerConfiguration::new().with_debug_info(true),
            )
            .expect("panel-rust: ui/app.slint failed to compile");
        })
        .expect("panel-rust: failed to spawn build-script compile thread")
        .join()
        .expect("panel-rust: build-script compile thread panicked");
}
