use once_cell::sync::OnceCell;
use std::{
    fs::File,
    process::{Child, Command},
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc, Weak,
    },
    time::Duration,
};
use tokio::sync::Mutex;

type Result<T = (), E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

#[macro_export]
/// If InfluxDB 2.0 OSS is available in the PATH at `influxd`, set up the server
/// as requested return it to the caller.
///
/// If `influxd` is not available, skip the calling test by returning
/// early. Additionally if `TEST_INTEGRATION` is set, turn this early return
/// into a panic to force a hard fail for skipped integration tests.
macro_rules! maybe_skip_integration {
    ($server_fixture:expr) => {
        match (
            std::process::Command::new("which")
                .arg("influxd")
                .stdout(std::process::Stdio::null())
                .status()
                .expect("should be able to run `which`")
                .success(),
            std::env::var("TEST_INTEGRATION").is_ok(),
        ) {
            (true, _) => $server_fixture,
            (false, true) => {
                panic!(
                    "TEST_INTEGRATION is set which requires running integration tests, but \
                     `influxd` is not available"
                )
            }
            _ => {
                eprintln!("skipping integration test - install `influxd` to run");
                return Ok(());
            }
        }
    };
}

/// Represents a server that has been started and is available for
/// testing.
pub struct ServerFixture {
    server: Arc<TestServer>,
}

impl ServerFixture {
    /// Create a new server fixture and wait for it to be ready. This
    /// is called "create" rather than new because it is async and
    /// waits. The shared database can be used immediately.
    ///
    /// This is currently implemented as a singleton so all tests *must*
    /// use a new database and not interfere with the existing database.
    pub async fn create_shared() -> Self {
        // Try and reuse the same shared server, if there is already
        // one present
        static SHARED_SERVER: OnceCell<parking_lot::Mutex<Weak<TestServer>>> = OnceCell::new();

        let shared_server = SHARED_SERVER.get_or_init(|| parking_lot::Mutex::new(Weak::new()));

        let mut shared_server = shared_server.lock();

        // is a shared server already present?
        let server = match shared_server.upgrade() {
            Some(server) => server,
            None => {
                // if not, create one
                let mut server = TestServer::new().expect("Could start test server");
                // ensure the server is ready
                server.wait_until_ready(InitialConfig::Onboarded).await;

                let server = Arc::new(server);
                // save a reference for other threads that may want to
                // use this server, but don't prevent it from being
                // destroyed when going out of scope
                *shared_server = Arc::downgrade(&server);
                server
            }
        };
        std::mem::drop(shared_server);

        Self { server }
    }

    /// Create a new server fixture and wait for it to be ready. This
    /// is called "create" rather than new because it is async and
    /// waits. The database is left unconfigured and is not shared
    /// with any other tests.
    pub async fn create_single_use() -> Self {
        let mut server = TestServer::new().expect("Could start test server");

        // ensure the server is ready
        server.wait_until_ready(InitialConfig::None).await;

        let server = Arc::new(server);

        Self { server }
    }

    /// Return a client suitable for communicating with this server
    pub fn client(&self) -> influxdb2_client::Client {
        match self.server.admin_token.as_ref() {
            Some(token) => influxdb2_client::Client::new(self.http_base(), token),
            None => influxdb2_client::Client::new(self.http_base(), ""),
        }
    }

    /// Return the http base URL for the HTTP API
    pub fn http_base(&self) -> &str {
        &self.server.http_base
    }
}

/// Specifies whether the server should be set up initially
#[derive(Debug, Copy, Clone, PartialEq)]
enum InitialConfig {
    /// Don't set up the server, the test will (for testing onboarding)
    None,
    /// Onboard the server and set up the client with the associated token (for
    /// most tests)
    Onboarded,
}

// These port numbers are chosen to not collide with a development ioxd/influxd
// server running locally.
// TODO(786): allocate random free ports instead of hardcoding.
// TODO(785): we cannot use localhost here.
static NEXT_PORT: AtomicUsize = AtomicUsize::new(8190);

/// Represents the current known state of a TestServer
#[derive(Debug)]
enum ServerState {
    Started,
    Ready,
    Error,
}

const ADMIN_TEST_USER: &str = "admin-test-user";
const ADMIN_TEST_ORG: &str = "admin-test-org";
const ADMIN_TEST_BUCKET: &str = "admin-test-bucket";
const ADMIN_TEST_PASSWORD: &str = "admin-test-password";

struct TestServer {
    /// Is the server ready to accept connections?
    ready: Mutex<ServerState>,
    /// Handle to the server process being controlled
    server_process: Child,
    /// HTTP API base
    http_base: String,
    /// Admin token, if onboarding has happened
    admin_token: Option<String>,
}

impl TestServer {
    fn new() -> Result<Self> {
        let ready = Mutex::new(ServerState::Started);
        let http_port = NEXT_PORT.fetch_add(1, SeqCst);
        let http_base = format!("http://127.0.0.1:{}", http_port);

        let temp_dir = test_helpers::tmp_dir().unwrap();

        let mut log_path = temp_dir.path().to_path_buf();
        log_path.push(format!("influxdb_server_fixture_{}.log", http_port));

        let mut bolt_path = temp_dir.path().to_path_buf();
        bolt_path.push(format!("influxd_{}.bolt", http_port));

        let mut engine_path = temp_dir.path().to_path_buf();
        engine_path.push(format!("influxd_{}_engine", http_port));

        println!("****************");
        println!("Server Logging to {:?}", log_path);
        println!("****************");
        let log_file = File::create(log_path).expect("Opening log file");

        let stdout_log_file = log_file
            .try_clone()
            .expect("cloning file handle for stdout");
        let stderr_log_file = log_file;

        let server_process = Command::new("influxd")
            .arg("--http-bind-address")
            .arg(format!(":{}", http_port))
            .arg("--bolt-path")
            .arg(bolt_path)
            .arg("--engine-path")
            .arg(engine_path)
            // redirect output to log file
            .stdout(stdout_log_file)
            .stderr(stderr_log_file)
            .spawn()?;

        Ok(Self {
            ready,
            server_process,
            http_base,
            admin_token: None,
        })
    }

    async fn wait_until_ready(&mut self, initial_config: InitialConfig) {
        let mut ready = self.ready.lock().await;
        match *ready {
            ServerState::Started => {} // first time, need to try and start it
            ServerState::Ready => {
                return;
            }
            ServerState::Error => {
                panic!("Server was previously found to be in Error, aborting");
            }
        }

        let try_http_connect = async {
            let client = reqwest::Client::new();
            let url = format!("{}/health", self.http_base);
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                match client.get(&url).send().await {
                    Ok(resp) => {
                        println!("Successfully got a response from HTTP: {:?}", resp);
                        return;
                    }
                    Err(e) => {
                        println!("Waiting for HTTP server to be up: {}", e);
                    }
                }
                interval.tick().await;
            }
        };

        let capped_check = tokio::time::timeout(Duration::from_secs(100), try_http_connect);

        match capped_check.await {
            Ok(_) => {
                println!("Successfully started {}", self);
                *ready = ServerState::Ready;
            }
            Err(e) => {
                // tell others that this server had some problem
                *ready = ServerState::Error;
                std::mem::drop(ready);
                panic!("Server was not ready in required time: {}", e);
            }
        }

        // Onboard, if requested.
        if initial_config == InitialConfig::Onboarded {
            let client = influxdb2_client::Client::new(&self.http_base, "");
            let response = client
                .onboarding(
                    ADMIN_TEST_USER,
                    ADMIN_TEST_ORG,
                    ADMIN_TEST_BUCKET,
                    Some(ADMIN_TEST_PASSWORD.to_string()),
                    Some(0),
                    None,
                )
                .await;

            match response {
                Ok(onboarding) => {
                    let token = onboarding
                        .auth
                        .expect("Onboarding should have returned auth info")
                        .token
                        .expect("Onboarding auth should have returned a token");
                    self.admin_token = Some(token);
                }
                Err(e) => {
                    *ready = ServerState::Error;
                    std::mem::drop(ready);
                    panic!("Could not onboard: {}", e);
                }
            }
        }
    }
}

impl std::fmt::Display for TestServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "TestServer (http api: {})", self.http_base)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server_process
            .kill()
            .expect("Should have been able to kill the test server");
    }
}
