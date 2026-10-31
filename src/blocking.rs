//! A server on its own background runtime, for tests that are not async.

use std::io;
use std::net::SocketAddr;
use std::ops::Deref;

use tokio::runtime::Runtime;

use crate::{Config, Server};

/// A [`Server`] together with the Tokio runtime it runs on, started by [`Server::start_blocking`].
///
/// Everything [`Server`] offers through `&self` is available here by deref. Dropping the handle
/// stops the server. Outside async code the listener is closed by the time `drop` returns; inside
/// async code (where blocking is not allowed) the drop does not wait, and the port closes shortly
/// after.
pub struct BlockingServer {
    // Declared first so the listener closes before the runtime goes away.
    server: Option<Server>,
    runtime: Option<Runtime>,
}

impl Server {
    /// Starts a server on a new background runtime and returns once it is listening.
    ///
    /// For synchronous tests, or tests that already have a runtime of their own and do not want
    /// the server to share it. Must not be called from inside an async context; use
    /// [`Server::start`] there. The runtime has at most 4 worker threads.
    ///
    /// ```no_run
    /// let server = fakellm::Server::start_blocking(fakellm::Config::default()).unwrap();
    /// println!("{}", server.endpoint());
    /// // The server stops when `server` goes out of scope.
    /// ```
    pub fn start_blocking(config: Config) -> io::Result<BlockingServer> {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("fakellm")
            .enable_all()
            .build()?;
        let server = runtime.block_on(Server::start(config))?;
        Ok(BlockingServer {
            server: Some(server),
            runtime: Some(runtime),
        })
    }
}

impl BlockingServer {
    pub fn addr(&self) -> SocketAddr {
        self.server().addr()
    }

    /// `http://host:port`, ready to use as an base endpoint (add `/v1` for OpenAI clients).
    pub fn endpoint(&self) -> String {
        self.server().endpoint()
    }

    /// Stops the server and waits for the runtime to wind down. Same as dropping it, except that
    /// this waits for the listener to close. Must not be called from inside an async context.
    pub fn shutdown(mut self) {
        if let (Some(server), Some(runtime)) = (self.server.take(), self.runtime.take()) {
            runtime.block_on(server.shutdown());
        }
    }

    fn server(&self) -> &Server {
        self.server.as_ref().expect("server is only taken by shutdown and drop")
    }
}

impl Deref for BlockingServer {
    type Target = Server;

    fn deref(&self) -> &Server {
        self.server()
    }
}

impl Drop for BlockingServer {
    fn drop(&mut self) {
        let (Some(server), Some(runtime)) = (self.server.take(), self.runtime.take()) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            // Wait for the accept loop to exit, which drops the listener.
            runtime.block_on(server.shutdown());
        } else {
            // Blocking is not allowed here; signal the accept loop and let it close on its own.
            drop(server);
        }
        // Abandons remaining connection tasks without blocking, which `Runtime::drop` would refuse
        // to do in async code.
        runtime.shutdown_background();
    }
}
