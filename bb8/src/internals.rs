use std::cmp::min;
use std::sync::Arc;
use std::time::Instant;

use crate::{api::QueueStrategy, lock::Mutex};
use futures_channel::oneshot;

use crate::api::{Builder, ManageConnection};
use std::collections::VecDeque;

/// The guts of a `Pool`.
#[allow(missing_debug_implementations)]
pub(crate) struct SharedPool<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    pub(crate) statics: Builder<M>,
    pub(crate) manager: M,
    pub(crate) internals: Mutex<PoolInternals<M>>,
}

impl<M> SharedPool<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    pub(crate) fn new(statics: Builder<M>, manager: M) -> Self {
        Self {
            statics,
            manager,
            internals: Mutex::new(PoolInternals::default()),
        }
    }

    pub(crate) async fn forward_error(&self, mut err: M::Error) {
        let mut locked = self.internals.lock().await;
        while let Some(waiter) = locked.waiters.pop_front() {
            match waiter.send(Err(err)) {
                Ok(_) => return,
                Err(Err(e)) => err = e,
                Err(Ok(_)) => unreachable!(),
            }
        }
        self.statics.error_sink.sink(err);
    }
}

/// The pool data that must be protected by a lock.
#[allow(missing_debug_implementations)]
pub(crate) struct PoolInternals<M>
where
    M: ManageConnection + Send + Sync,
<M as ManageConnection>::Connection: Send + Sync
{
    waiters: VecDeque<oneshot::Sender<Result<InternalsGuard<M>, M::Error>>>,
    conns: VecDeque<IdleConn<M::Connection>>,
    num_conns: u32,
    pending_conns: u32,
}

impl<M> PoolInternals<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    pub(crate) fn pop(
        &mut self,
        config: &Builder<M>,
    ) -> Option<(Conn<M::Connection>, ApprovalIter)>
    {
        self.conns
            .pop_front()
            .map(|idle| (idle.conn, self.wanted(config)))
    }

    pub(crate) fn put(
        &mut self,
        conn: Conn<M::Connection>,
        approval: Option<Approval>,
        pool: Arc<SharedPool<M>>,
    ) {
        if approval.is_some() {
            self.pending_conns -= 1;
            self.num_conns += 1;
        }

        let queue_strategy = pool.statics.queue_strategy;

        let mut guard = InternalsGuard::new(conn, pool);
        while let Some(waiter) = self.waiters.pop_front() {
            // This connection is no longer idle, send it back out
            match waiter.send(Ok(guard)) {
                Ok(()) => return,
                Err(Ok(g)) => {
                    guard = g;
                }
                Err(Err(_)) => unreachable!(),
            }
        }

        // Queue it in the idle queue
        let conn = IdleConn::from(guard.conn.take().unwrap());
        match queue_strategy {
            QueueStrategy::Fifo => self.conns.push_back(conn),
            QueueStrategy::Lifo => self.conns.push_front(conn),
        }
    }

    pub(crate) fn connect_failed(&mut self, _: Approval) {
        self.pending_conns -= 1;
    }

    pub(crate) fn dropped(&mut self, num: u32, config: &Builder<M>) -> ApprovalIter {
        self.num_conns -= num;
        self.wanted(config)
    }

    pub(crate) fn wanted(&mut self, config: &Builder<M>) -> ApprovalIter {
        let available = self.conns.len() as u32 + self.pending_conns;
        let min_idle = config.min_idle.unwrap_or(0);
        let wanted = if available < min_idle {
            min_idle - available
        } else {
            0
        };

        self.approvals(config, wanted)
    }

    pub(crate) fn push_waiter(
        &mut self,
        waiter: oneshot::Sender<Result<InternalsGuard<M>, M::Error>>,
        config: &Builder<M>,
    ) -> ApprovalIter {
        self.waiters.push_back(waiter);
        self.approvals(config, 1)
    }

    fn approvals(&mut self, config: &Builder<M>, num: u32) -> ApprovalIter {
        let current = self.num_conns + self.pending_conns;
        let allowed = if current < config.max_size {
            config.max_size - current
        } else {
            0
        };

        let num = min(num, allowed);
        self.pending_conns += num;
        ApprovalIter { num: num as usize }
    }

    pub(crate) fn reap(&mut self, config: &Builder<M>) -> ApprovalIter {
        let now = Instant::now();
        let before = self.conns.len();

        self.conns.retain(|conn| {
            let mut keep = true;
            if let Some(timeout) = config.idle_timeout {
                keep &= now - conn.idle_start < timeout;
            }
            if let Some(lifetime) = config.max_lifetime {
                keep &= now - conn.conn.birth < lifetime;
            }
            keep
        });

        self.dropped((before - self.conns.len()) as u32, config)
    }

    pub(crate) fn state(&self) -> State {
        State {
            connections: self.num_conns,
            idle_connections: self.conns.len() as u32,
        }
    }
}

impl<M> Default for PoolInternals<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    fn default() -> Self {
        Self {
            waiters: VecDeque::new(),
            conns: VecDeque::new(),
            num_conns: 0,
            pending_conns: 0,
        }
    }
}

pub(crate) struct InternalsGuard<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    conn: Option<Conn<M::Connection>>,
    pool: Arc<SharedPool<M>>,
}

impl<M> InternalsGuard<M>
where
M: ManageConnection + Send + Sync,
<M as ManageConnection>::Connection: Send + Sync
{
    fn new(conn: Conn<M::Connection>, pool: Arc<SharedPool<M>>) -> Self {
        Self {
            conn: Some(conn),
            pool,
        }
    }

    pub(crate) fn extract(&mut self) -> Conn<M::Connection> {
        self.conn.take().unwrap() // safe: can only be `None` after `Drop`
    }
}

impl<M> Drop for InternalsGuard<M>
where
M: ManageConnection + Send + Sync,
<M as ManageConnection>::Connection: Send + Sync
{
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let pool = self.pool.clone();
            tokio::spawn(async move{
                let locked = pool.internals.lock_owned();
                locked.await.put(conn, None, pool.clone()) });
        }
    }
}

#[must_use]
pub(crate) struct ApprovalIter
{
    num: usize,
}

impl Iterator for ApprovalIter {
    type Item = Approval;

    fn next(&mut self) -> Option<Self::Item> {
        match self.num {
            0 => None,
            _ => {
                self.num -= 1;
                Some(Approval { _priv: () })
            }
        }
    }
}

impl ExactSizeIterator for ApprovalIter {
    fn len(&self) -> usize {
        self.num
    }
}

#[must_use]
pub(crate) struct Approval {
    _priv: (),
}

#[derive(Debug)]
pub(crate) struct Conn<C>
where
    C: Send + Sync,
{
    pub(crate) conn: C,
    birth: Instant,
}

impl<C: Send + Sync> Conn<C> {
    pub(crate) fn new(conn: C) -> Self {
        Self {
            conn,
            birth: Instant::now(),
        }
    }
}

impl<C: Send + Sync> From<IdleConn<C>> for Conn<C> {
    fn from(conn: IdleConn<C>) -> Self {
        conn.conn
    }
}

struct IdleConn<C>
where
    C: Send + Sync,
{
    conn: Conn<C>,
    idle_start: Instant,
}

impl<C: Send + Sync> From<Conn<C>> for IdleConn<C> {
    fn from(conn: Conn<C>) -> Self {
        IdleConn {
            conn,
            idle_start: Instant::now(),
        }
    }
}

/// Information about the state of a `Pool`.
#[derive(Debug)]
#[non_exhaustive]
pub struct State {
    /// The number of connections currently being managed by the pool.
    pub connections: u32,
    /// The number of idle connections.
    pub idle_connections: u32,
}
