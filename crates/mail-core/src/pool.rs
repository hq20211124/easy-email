//! Reuse IMAP connections instead of handshaking for every command.
//!
//! Every command used to open its own connection: TCP + TLS + LOGIN, hundreds of milliseconds each,
//! and `AccountConfig::max_connections` was never actually enforced. This pool keeps recently used
//! connections around so a second `list_messages` costs one round trip rather than a handshake.
//!
//! Two constraints shape the code:
//!
//! * **A pooled connection must be probed before it is reused.** Servers close idle connections
//!   without telling the client, and in IMAP that does not surface as anything until the *next*
//!   command fails — usually some unrelated one. One `NOOP` (a few milliseconds) buys certainty.
//! * **The lock is never held across an `.await`.** `ImapClient` is `Send` but not `Sync` (its
//!   transport is `Box<dyn MailStream>` and that trait has no `Sync` bound), so a future holding a
//!   `MutexGuard<…>` across an await would not be `Send` — which a Tauri command's future must be.
//!   Connections are therefore moved out from under the lock, probed, and moved back.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::imap::ImapClient;
use crate::model::AccountConfig;

/// How old an idle connection may be before it is not worth probing.
///
/// Servers commonly drop idle connections somewhere around 30 minutes; this is deliberately well
/// inside that so the probe almost always succeeds.
const MAX_IDLE_AGE: Duration = Duration::from_secs(5 * 60);

struct Idle {
    client: ImapClient,
    since: Instant,
}

/// Per-account pool of ready-to-use connections.
#[derive(Default)]
pub struct ConnectionPool {
    /// Account id → connections that are not currently in use.
    idle: Mutex<HashMap<String, Vec<Idle>>>,
}

impl ConnectionPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a connection for `cfg`, reusing an idle one when possible.
    pub async fn acquire(&self, cfg: &AccountConfig, password: &str) -> Result<PooledClient<'_>> {
        let key = cfg.id.clone();

        // Move a candidate out from under the lock first: the `NOOP` below is an await, and the
        // guard must not be alive across it.
        let candidate = {
            let mut guard = self.idle.lock().expect("connection pool mutex");
            guard.get_mut(&key).and_then(Vec::pop)
        };

        if let Some(mut idle) = candidate {
            let fresh_enough = idle.since.elapsed() <= MAX_IDLE_AGE;
            // A dead probe means the server hung up on it; fall through and connect anew rather
            // than hand back a connection that is about to fail.
            if fresh_enough && idle.client.noop().await.is_ok() {
                return Ok(PooledClient {
                    pool: self,
                    key,
                    limit: cfg.max_connections,
                    client: Some(idle.client),
                    poisoned: false,
                });
            }
        }

        let client = ImapClient::connect(cfg, password).await?;
        Ok(PooledClient {
            pool: self,
            key,
            limit: cfg.max_connections,
            client: Some(client),
            poisoned: false,
        })
    }

    /// How many idle connections are being held for an account.
    pub fn live_count(&self, account_id: &str) -> usize {
        self.idle
            .lock()
            .expect("connection pool mutex")
            .get(account_id)
            .map_or(0, Vec::len)
    }

    /// Drop everything held for an account (account removed, or the app is shutting down).
    pub fn clear(&self, account_id: &str) {
        self.idle
            .lock()
            .expect("connection pool mutex")
            .remove(account_id);
    }

    fn release(&self, key: &str, client: ImapClient, limit: u8) {
        // `max_connections` is enforced on *idle* connections, which is what can grow without bound.
        // Anything above it is dropped, closing the socket.
        let capacity = usize::from(limit.max(1));
        let mut guard = self.idle.lock().expect("connection pool mutex");
        let slot = guard.entry(key.to_string()).or_default();
        if slot.len() < capacity {
            slot.push(Idle {
                client,
                since: Instant::now(),
            });
        }
        // Over capacity: `client` is dropped here. A `LOGOUT` would be politer, but this runs in
        // `Drop`, which cannot await, and spawning a task to say goodbye is not worth it — closing
        // the socket is something every server copes with.
    }
}

/// A checked-out connection. Returns itself to the pool when dropped.
pub struct PooledClient<'a> {
    pool: &'a ConnectionPool,
    key: String,
    limit: u8,
    client: Option<ImapClient>,
    poisoned: bool,
}

impl PooledClient<'_> {
    /// Mark the connection as unusable so it is closed instead of pooled.
    ///
    /// Call this after an I/O failure: a connection that just failed is worth discarding, and
    /// returning it would make the *next* command inherit the same failure.
    pub fn poison(&mut self) {
        self.poisoned = true;
    }
}

impl Deref for PooledClient<'_> {
    type Target = ImapClient;

    fn deref(&self) -> &ImapClient {
        self.client.as_ref().expect("checked-out connection")
    }
}

impl DerefMut for PooledClient<'_> {
    fn deref_mut(&mut self) -> &mut ImapClient {
        self.client.as_mut().expect("checked-out connection")
    }
}

impl Drop for PooledClient<'_> {
    fn drop(&mut self) {
        if let (Some(client), false) = (self.client.take(), self.poisoned) {
            self.pool.release(&self.key, client, self.limit);
        }
    }
}

/// Run `op` on a pooled connection, retrying once on a fresh one if the first attempt fails.
///
/// **The `NOOP` probe in [`ConnectionPool::acquire`] is not sufficient on its own**, and this is
/// measured rather than assumed: when the peer has just closed the socket, the client's write can
/// still succeed and async-imap reads the end of the stream as an empty response, so `noop()`
/// returns `Ok`. The next real command is what fails, with a transport error. A pool for this
/// protocol therefore has to treat "the command failed" as "that connection was bad".
///
/// Retrying something that failed for a protocol reason costs one wasted round trip. Retrying a
/// credential failure is pointless, so `Auth` is passed straight through.
///
/// **Why this is a macro rather than a function taking a closure.** The block has to run twice
/// against two different borrows of a connection, so the natural bound is
/// `for<'a> Fn(&'a mut ImapClient) -> BoxFuture<'a, T>`. A closure that satisfies that bound may not
/// capture anything that does not outlive the universally quantified `'a`, so the compiler demands
/// every captured reference be `'static` — which no realistic call site can give, because they are
/// borrows of command parameters. Expanding the block twice sidesteps the bound entirely, and the
/// block keeps borrowing exactly what it needs.
///
/// The block must **evaluate to** `Result<T, MailError>`; a bare `return` inside it returns from the
/// block, not from the enclosing function.
///
/// ```ignore
/// let folders = pool_op!(pool, &cfg, &password, |client| {
///     client.list_mailboxes().await
/// })?;
/// ```
#[macro_export]
macro_rules! pool_op {
    ($pool:expr, $cfg:expr, $password:expr, |$client:ident| $body:block) => {{
        let mut attempt = 0u32;
        loop {
            attempt += 1;

            // `match` rather than `?`: the macro expands into functions whose error type is
            // `String`, and a `?` here would demand a conversion this crate cannot provide.
            let mut $client = match $pool.acquire($cfg, $password).await {
                Ok(client) => client,
                Err(error) => break Err(error),
            };

            // The annotation pins the block's error type to this crate's; without it the `?`s
            // inside the block get inferred as the *caller's* error type and stop compiling.
            let outcome: $crate::Result<_> = async { $body }.await;
            match outcome {
                Ok(value) => break Ok(value),
                Err(error) => {
                    // Never hand a connection that just failed back to the pool: the next command
                    // would inherit the same failure with no explanation.
                    $client.poison();
                    drop($client);
                    if attempt >= 2 || matches!(error, $crate::MailError::Auth { .. }) {
                        break Err(error);
                    }
                    // Loop: the poisoned connection was discarded, so `acquire` opens a fresh one.
                }
            }
        }
    }};
}
