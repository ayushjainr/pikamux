fn main() {
    let code = match pikamux::run(std::env::args_os()) {
        Ok(code) => code,
        Err(error) if error.downcast_ref::<clap::Error>().is_some() => {
            let clap_error = error.downcast_ref::<clap::Error>().expect("checked above");
            let code = clap_error.exit_code();
            let _ = clap_error.print();
            code
        }
        Err(error) => {
            eprintln!("pika: {error:#}");
            1
        }
    };
    std::process::exit(code);
}
