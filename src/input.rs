use crate::plumbing::CycleDetected;
use crate::plumbing::InputQueryStorageOps;
use crate::plumbing::QueryStorageMassOps;
use crate::plumbing::QueryStorageOps;
use crate::plumbing::UncheckedMutQueryStorageOps;
use crate::runtime::ChangedAt;
use crate::runtime::Revision;
use crate::runtime::StampedValue;
use crate::Database;
use crate::Event;
use crate::EventKind;
use crate::Query;
use crate::SweepStrategy;
use log::debug;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;

/// Input queries store the result plus a list of the other queries
/// that they invoked. This means we can avoid recomputing them when
/// none of those inputs have changed.
pub struct InputStorage<DB, Q>
where
    Q: Query<DB>,
    DB: Database,
{
    map: RwLock<FxHashMap<Q::Key, StampedValue<Q::Value>>>,
}

impl<DB, Q> Default for InputStorage<DB, Q>
where
    Q: Query<DB>,
    DB: Database,
{
    fn default() -> Self {
        InputStorage {
            map: RwLock::new(FxHashMap::default()),
        }
    }
}

struct IsConstant(bool);

impl<DB, Q> InputStorage<DB, Q>
where
    Q: Query<DB>,
    DB: Database,
{
    fn read<'q>(
        &self,
        _db: &'q DB,
        key: &Q::Key,
        _descriptor: &DB::QueryDescriptor,
    ) -> Result<StampedValue<Q::Value>, CycleDetected> {
        {
            let map_read = self.map.read();
            if let Some(value) = map_read.get(key) {
                return Ok(value.clone());
            }
        }

        panic!("no value set for {:?}({:?})", Q::default(), key)
    }

    fn set_common(
        &self,
        db: &DB,
        key: &Q::Key,
        descriptor: &DB::QueryDescriptor,
        value: Q::Value,
        is_constant: IsConstant,
    ) {
        let key = key.clone();

        // The value is changing, so even if we are setting this to a
        // constant, we still need a new revision.
        //
        // CAREFUL: This will block until the global revision lock can
        // be acquired. If there are still queries executing, they may
        // need to read from this input. Therefore, we wait to acquire
        // the lock on `map` until we also hold the global query write
        // lock.
        db.salsa_runtime().with_incremented_revision(|next_revision| { // 1
            let mut map = self.map.write();

            db.salsa_event(|| Event {
                runtime_id: db.salsa_runtime().id(),
                kind: EventKind::WillChangeInputValue {
                    descriptor: descriptor.clone(),
                },
            });

            // Do this *after* we acquire the lock, so that we are not
            // racing with somebody else to modify this same cell.
            // (Otherwise, someone else might write a *newer* revision
            // into the same cell while we block on the lock.)
            let changed_at = ChangedAt {
                is_constant: is_constant.0,
                revision: next_revision,
            };

            let stamped_value = StampedValue { value, changed_at };

            match map.entry(key) {
                Entry::Occupied(mut entry) => {
                    assert!(
                        !entry.get().changed_at.is_constant,
                        "modifying `{:?}({:?})`, which was previously marked as constant (old value `{:?}`, new value `{:?}`)",
                        Q::default(),
                        entry.key(),
                        entry.get().value,
                        stamped_value.value,
                    );

                    entry.insert(stamped_value);
                }

                Entry::Vacant(entry) => {
                    entry.insert(stamped_value);
                }
            }
        });
    }
}

impl<DB, Q> QueryStorageOps<DB, Q> for InputStorage<DB, Q>
where
    Q: Query<DB>,
    DB: Database,
{
    fn try_fetch(
        &self,
        db: &DB,
        key: &Q::Key,
        descriptor: &DB::QueryDescriptor,
    ) -> Result<Q::Value, CycleDetected> {
        let StampedValue { value, changed_at } = self.read(db, key, &descriptor)?;

        db.salsa_runtime().report_query_read(descriptor, changed_at);

        Ok(value)
    }

    fn maybe_changed_since(
        &self,
        _db: &DB,
        revision: Revision,
        key: &Q::Key,
        _descriptor: &DB::QueryDescriptor,
    ) -> bool {
        debug!(
            "{:?}({:?})::maybe_changed_since(revision={:?})",
            Q::default(),
            key,
            revision,
        );

        let changed_at = {
            let map_read = self.map.read();
            map_read
                .get(key)
                .map(|v| v.changed_at)
                .unwrap_or(ChangedAt {
                    is_constant: false,
                    revision: Revision::ZERO,
                })
        };

        debug!(
            "{:?}({:?}): changed_at = {:?}",
            Q::default(),
            key,
            changed_at,
        );

        changed_at.changed_since(revision)
    }

    fn is_constant(&self, _db: &DB, key: &Q::Key) -> bool {
        let map_read = self.map.read();
        map_read
            .get(key)
            .map(|v| v.changed_at.is_constant)
            .unwrap_or(false)
    }

    fn keys<C>(&self, _db: &DB) -> C
    where
        C: std::iter::FromIterator<Q::Key>,
    {
        let map = self.map.read();
        map.keys().cloned().collect()
    }
}

impl<DB, Q> QueryStorageMassOps<DB> for InputStorage<DB, Q>
where
    Q: Query<DB>,
    DB: Database,
{
    fn sweep(&self, _db: &DB, _strategy: SweepStrategy) {}
}

impl<DB, Q> InputQueryStorageOps<DB, Q> for InputStorage<DB, Q>
where
    Q: Query<DB>,
    DB: Database,
{
    fn set(&self, db: &DB, key: &Q::Key, descriptor: &DB::QueryDescriptor, value: Q::Value) {
        log::debug!("{:?}({:?}) = {:?}", Q::default(), key, value);

        self.set_common(db, key, descriptor, value, IsConstant(false))
    }

    fn set_constant(
        &self,
        db: &DB,
        key: &Q::Key,
        descriptor: &DB::QueryDescriptor,
        value: Q::Value,
    ) {
        log::debug!("{:?}({:?}) = {:?}", Q::default(), key, value);

        self.set_common(db, key, descriptor, value, IsConstant(true))
    }
}

impl<DB, Q> UncheckedMutQueryStorageOps<DB, Q> for InputStorage<DB, Q>
where
    Q: Query<DB>,
    DB: Database,
{
    fn set_unchecked(&self, db: &DB, key: &Q::Key, value: Q::Value) {
        let key = key.clone();

        let mut map_write = self.map.write();

        // Unlike with `set`, here we use the **current revision** and
        // do not create a new one.
        let changed_at = ChangedAt {
            is_constant: false,
            revision: db.salsa_runtime().current_revision(),
        };

        map_write.insert(key, StampedValue { value, changed_at });
    }
}
