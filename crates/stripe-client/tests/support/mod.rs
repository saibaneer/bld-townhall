use std::{
    io::BufRead as _,
    process::{Child, Command, Stdio},
    sync::OnceLock,
};

pub struct MockStripeProcess {
    child: Child,
    pub base_url: String,
}

impl MockStripeProcess {
    pub fn spawn() -> Self {
        build_mock_once();
        let binary =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-stripe");
        let mut child = Command::new(binary)
            .args(["--port", "0"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn mock-stripe");
        let stdout = child.stdout.take().expect("piped stdout");
        let ready = std::io::BufReader::new(stdout)
            .lines()
            .next()
            .expect("READY line")
            .expect("readable READY line");
        let port: u16 = ready
            .strip_prefix("READY ")
            .unwrap_or_else(|| panic!("expected READY, got {ready:?}"))
            .parse()
            .expect("READY port");
        Self {
            child,
            base_url: format!("http://127.0.0.1:{port}"),
        }
    }
}

impl Drop for MockStripeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn build_mock_once() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "mock-stripe", "--features", "test-faults"])
            .status()
            .expect("cargo build mock-stripe");
        assert!(status.success(), "mock-stripe must build");
    });
}
