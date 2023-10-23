use std::cmp::{max, min};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_channel::oneshot;
use futures_util::stream::{FuturesUnordered, StreamExt};
use futures_util::TryFutureExt;
use tokio::spawn;
// use tokio::sync::{MutexGuard, OwnedMutexGuard};
use tokio::time::{interval_at, sleep, timeout, Interval};

// use crate::lock::Mutex;
use crate::api::{Builder, ManageConnection, PooledConnection, RunError};
use crate::internals::{Approval, ApprovalIter, Conn, SharedPool, State};

pub(crate) struct PoolInner<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    inner: Arc<SharedPool<M>>,
}

impl<M> PoolInner<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    pub(crate) fn new(builder: Builder<M>, manager: M) -> Self {
        let inner = Arc::new(SharedPool::new(builder, manager));

        if inner.statics.max_lifetime.is_some() || inner.statics.idle_timeout.is_some() {
            let s = Arc::downgrade(&inner);
            if let Some(shared) = s.upgrade() {
                let start = Instant::now() + shared.statics.reaper_rate;
                let interval = interval_at(start.into(), shared.statics.reaper_rate);
                schedule_reaping(interval, s);
            }
        }

        Self { inner }
    }

    pub(crate) async fn start_connections(&self) -> Result<(), M::Error> {
        let wanted = self.inner.internals.lock().await.wanted(&self.inner.statics);
        let mut stream = self.replenish_idle_connections(wanted);
        while let Some(result) = stream.next().await {
            result?;
        }
        Ok(())
    }

    pub(crate) async fn spawn_start_connections(&self) {
        let locked = self.inner.internals.lock_owned().await.wanted(&self.inner.statics);
        self.spawn_replenishing_approvals(locked);
    }

    fn spawn_replenishing_approvals(&self, approvals: ApprovalIter) {
        if approvals.len() == 0 {
            return;
        }

        let this = self.clone();
        spawn(async move {
            let mut stream = this.replenish_idle_connections(approvals);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(()) => {}
                    Err(e) => this.inner.forward_error(e).await,
                }
            }
        });
    }

    fn replenish_idle_connections(
        &self,
        approvals: ApprovalIter,
    ) -> FuturesUnordered<impl Future<Output = Result<(), M::Error>>> {
        let stream = FuturesUnordered::new();
        for approval in approvals {
            let this = self.clone();
            stream.push(async move { this.add_connection(approval).await });
        }
        stream
    }

    pub(crate) async fn get(&self) -> Result<PooledConnection<'_, M>, RunError<M::Error>> {
        self.make_pooled(|this, conn| PooledConnection::new(this, conn))
            .await
    }

    pub(crate) async fn get_owned(
        &self,
    ) -> Result<PooledConnection<'static, M>, RunError<M::Error>> {
        self.make_pooled(|this, conn| {
            let pool = PoolInner {
                inner: Arc::clone(&this.inner),
            };
            PooledConnection::new_owned(pool, conn)
        })
        .await
    }

    pub(crate) async fn make_pooled<'a, 'b, F>(
        &'a self,
        make_pooled_conn: F,
    ) -> Result<PooledConnection<'b, M>, RunError<M::Error>>
    where
        F: Fn(&'a Self, Conn<M::Connection>) -> PooledConnection<'b, M>,
    {
        loop {
            let mut conn = {
                let locked = self.inner.internals.lock();
                match locked.await.pop(&self.inner.statics) {
                    Some((conn, approvals)) => {
                        self.spawn_replenishing_approvals(approvals);
                        make_pooled_conn(self, conn)
                    }
                    None => break,
                }
            };

            if !self.inner.statics.test_on_check_out {
                return Ok(conn);
            }

            match self.inner.manager.is_valid(&mut conn).await {
                Ok(()) => return Ok(conn),
                Err(e) => {
                    self.inner.forward_error(e).await;
                    conn.drop_invalid();
                    continue;
                }
            }
        }

        let (tx, rx) = oneshot::channel();
        {
            let locked = self.inner.internals.lock();
            let approvals = locked.await.push_waiter(tx, &self.inner.statics);
            self.spawn_replenishing_approvals(approvals);
        };

        match timeout(self.inner.statics.connection_timeout, rx).await {
            Ok(Ok(Ok(mut guard))) => Ok(make_pooled_conn(self, guard.extract())),
            Ok(Ok(Err(e))) => Err(RunError::User(e)),
            _ => Err(RunError::TimedOut),
        }
    }

    pub(crate) async fn connect(&self) -> Result<M::Connection, M::Error> {
        let mut conn = self.inner.manager.connect().await?;
        self.on_acquire_connection(&mut conn).await?;
        Ok(conn)
    }

    /// Return connection back in to the pool
    pub(crate) async fn put_back(&self, conn: Option<Conn<M::Connection>>) {
        let conn = conn.and_then(|mut conn| {
            if !self.inner.manager.has_broken(&mut conn.conn) {
                Some(conn)
            } else {
                None
            }
        });

        let locked = self.inner.internals.lock();
        match conn {
            Some(conn) => locked.await.put(conn, None, self.inner.clone()),
            None => {
                let approvals = locked.await.dropped(1, &self.inner.statics);
                self.spawn_replenishing_approvals(approvals);
            }
        }
    }

    /// Returns information about the current state of the pool.
    pub(crate) async fn state(&self) -> State {
        self.inner.internals.lock().await.state()
    }

    async fn reap(&self) {
        let internals = self.inner.internals.lock();
        let approvals = internals.await.reap(&self.inner.statics);
        self.spawn_replenishing_approvals(approvals);
    }

    // Outside of Pool to avoid borrow splitting issues on self
    async fn add_connection(&self, approval: Approval) -> Result<(), M::Error>
    where
        M: ManageConnection,
    {
        let new_shared = Arc::downgrade(&self.inner);
        let shared = match new_shared.upgrade() {
            None => return Ok(()),
            Some(shared) => shared,
        };

        let start = Instant::now();
        let mut delay = Duration::from_secs(0);
        loop {
            let conn = shared
                .manager
                .connect()
                .and_then(|mut c| async { self.on_acquire_connection(&mut c).await.map(|_| c) })
                .await;

            match conn {
                Ok(conn) => {
                    let conn = Conn::new(conn);
                    shared
                        .internals
                        .lock()
                        .await.put(conn, Some(approval), self.inner.clone());
                    return Ok(());
                }
                Err(e) => {
                    if !self.inner.statics.retry_connection
                        || Instant::now() - start > self.inner.statics.connection_timeout
                    {
                        let locked = shared.internals.lock();
                        locked.await.connect_failed(approval);
                        return Err(e);
                    } else {
                        delay = max(Duration::from_millis(200), delay);
                        delay = min(self.inner.statics.connection_timeout / 2, delay * 2);
                        sleep(delay).await;
                    }
                }
            }
        }
    }

    async fn on_acquire_connection(&self, conn: &mut M::Connection) -> Result<(), M::Error> {
        match self.inner.statics.connection_customizer.as_ref() {
            Some(customizer) => customizer.on_acquire(conn).await,
            None => Ok(()),
        }
    }
}

impl<M> Clone for PoolInner<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    fn clone(&self) -> Self {
        PoolInner {
            inner: self.inner.clone(),
        }
    }
}

impl<M> fmt::Debug for PoolInner<M>
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!("PoolInner({:p})", self.inner))
    }
}

fn schedule_reaping<M>(mut interval: Interval, weak_shared: Weak<SharedPool<M>>)
where
    M: ManageConnection + Send + Sync,
    <M as ManageConnection>::Connection: Send + Sync
{
    spawn(async move {
        loop {
            let _ = interval.tick().await;
            if let Some(inner) = weak_shared.upgrade() {
                PoolInner { inner }.reap().await;
            } else {
                break;
            }
        }
    });
}
