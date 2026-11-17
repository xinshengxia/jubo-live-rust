//! Explicit integration fixture, compiled separately; never used by the app.
use std::{env, fs, net::TcpListener, process::{Command, Stdio}, time::Duration};
fn main() {
    let args: Vec<_> = env::args_os().collect();
    if args[1] == "parent" {
        let mut command = Command::new(env::current_exe().unwrap());
        command.arg("child").arg(&args[2]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x0800_0000); }
        command.spawn().unwrap();
    } else {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(&args[2], format!("{}:{}", std::process::id(), listener.local_addr().unwrap().port())).unwrap();
        std::thread::sleep(Duration::from_secs(15));
    }
}
