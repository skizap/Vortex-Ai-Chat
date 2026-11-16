fn main() {
    // Diagnostics must be initialized before the first UI boot request runs
    // (settings/health fetches happen right after mount). No-op when the
    // browser diagnostics gate is off.
    vortex_ui::diagnostics::init();
    leptos::mount::mount_to_body(vortex_ui::App);
}
