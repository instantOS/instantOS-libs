fn main() {
    std::process::exit(instantos_terminal::run_cli(std::env::args_os().skip(1)));
}
