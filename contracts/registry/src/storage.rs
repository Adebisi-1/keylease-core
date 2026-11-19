use soroban_sdk::{contracttype, Address, Env, Map, Symbol, TryFromVal, Val};

use crate::errors::Error;
use crate::types::{Lease, Service};
use crate::SETTLE_GRACE_PERIOD_SECS;

/// Expected Stellar ledger close time, used to translate lease durations into
/// ledger counts for TTL extension.
pub const LEDGER_CLOSE_SECS: u64 = 5;

/// Ledger count below which a persistent entry is extended (~1 day @ 5s ledgers).
pub const LEDGER_BUMP_THRESHOLD: u32 = 17_280;
/// Ledger count that persistent entries are extended to (~30 days @ 5s ledgers).
pub const LEDGER_BUMP_EXTEND: u32 = 518_400;

/// Storage keys for the registry.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Protocol administrator (instance storage).
    Admin,
    /// SEP-41 escrow token used for all deposits and payouts (instance storage).
    Token,
    /// Monotonic service id counter (instance storage).
    ServiceCount,
    /// Monotonic lease id counter (instance storage).
    LeaseCount,
    /// `Service` by id (persistent storage).
    Service(u64),
    /// `Lease` by id (persistent storage).
    Lease(u64),
}

// ---------------------------------------------------------------------------
// Initialization / configuration
// ---------------------------------------------------------------------------

pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Admin)
}

pub fn set_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&DataKey::Admin, admin);
}

pub fn get_admin(env: &Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::Admin)
}

pub fn set_token(env: &Env, token: &Address) {
    env.storage().instance().set(&DataKey::Token, token);
}

pub fn get_token(env: &Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::Token)
}

/// Bump the TTL of the instance (configuration) storage entry.
pub fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(LEDGER_BUMP_THRESHOLD, LEDGER_BUMP_EXTEND);
}

// ---------------------------------------------------------------------------
// Monotonic identifiers
// ---------------------------------------------------------------------------

pub fn next_service_id(env: &Env) -> Result<u64, Error> {
    let next = service_count(env).checked_add(1).ok_or(Error::Overflow)?;
    env.storage().instance().set(&DataKey::ServiceCount, &next);
    Ok(next)
}

pub fn next_lease_id(env: &Env) -> Result<u64, Error> {
    let next = lease_count(env).checked_add(1).ok_or(Error::Overflow)?;
    env.storage().instance().set(&DataKey::LeaseCount, &next);
    Ok(next)
}

pub fn service_count(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::ServiceCount)
        .unwrap_or(0)
}

pub fn lease_count(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::LeaseCount)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

pub fn save_service(env: &Env, service: &Service) {
    let key = DataKey::Service(service.service_id);
    env.storage().persistent().set(&key, service);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_BUMP_THRESHOLD, LEDGER_BUMP_EXTEND);
}

pub fn get_service(env: &Env, service_id: u64) -> Result<Service, Error> {
    let key = DataKey::Service(service_id);
    let service = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::ServiceNotFound)?;
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_BUMP_THRESHOLD, LEDGER_BUMP_EXTEND);
    Ok(service)
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------
//
// `Lease` gained a `created_at` field, and a `#[contracttype]` struct decodes by
// unpacking the exact field-name keys it knows: a value written before that field
// existed cannot be decoded into the current `Lease`, and the host raises (it does
// not return an error) when the key count differs. `get_lease` therefore inspects
// the stored map first and reads the pre-`created_at` shape through `LegacyLease`,
// reporting `created_at = 0` (unknown) instead of trapping. That keeps pre-upgrade
// leases settleable and revocable, i.e. escrow stays releasable across an upgrade.
//
// The change is append-only in every other sense: no `DataKey` variant was added,
// removed or renumbered, and the new field is last in the struct. Nothing has to
// be migrated up front, and an entry is rewritten in the current shape the next
// time it is saved (settlement or revocation).

/// Number of fields in the pre-`created_at` lease encoding.
const LEGACY_LEASE_FIELDS: u32 = 8;

/// The `Lease` shape stored by releases before `created_at` existed. Used only to
/// decode existing entries; never written except by tests.
#[contracttype]
struct LegacyLease {
    lease_id: u64,
    service_id: u64,
    consumer: Address,
    allocated_calls: u32,
    used_calls: u32,
    expires_at: u64,
    locked_deposit: i128,
    settled: bool,
}

pub fn save_lease(env: &Env, lease: &Lease) {
    let key = DataKey::Lease(lease.lease_id);
    env.storage().persistent().set(&key, lease);
    env.storage().persistent().extend_ttl(
        &key,
        LEDGER_BUMP_THRESHOLD,
        lease_ttl_extend(env, lease),
    );
}

/// Ledgers a lease entry must survive: its remaining term, the provider's
/// settlement grace window, and a one-day read buffer.
///
/// Only the time the lease can actually be acted on is paid for. Extending every
/// lease to the generic thirty-day horizon made Soroban rent dominate short
/// leases on Testnet; the buffer here keeps the entry alive through expiry, the
/// 24-hour grace window, and a day of slack for the consumer's revoke call.
///
/// `create_lease` caps `duration_secs` at `MAX_LEASE_DURATION_SECS`, so this is
/// always bounded and a locked deposit can never outlive its lease entry.
fn lease_ttl_extend(env: &Env, lease: &Lease) -> u32 {
    let now = env.ledger().timestamp();
    let remaining_secs = lease
        .expires_at
        .saturating_sub(now)
        .saturating_add(SETTLE_GRACE_PERIOD_SECS);
    let ledgers = remaining_secs / LEDGER_CLOSE_SECS;
    let extend = ledgers.saturating_add(LEDGER_BUMP_THRESHOLD as u64);
    extend.min(u32::MAX as u64) as u32
}

pub fn get_lease(env: &Env, lease_id: u64) -> Result<Lease, Error> {
    let key = DataKey::Lease(lease_id);
    let val: Val = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::LeaseNotFound)?;

    // Decide which shape is stored *before* decoding: the derive unpacks an exact
    // field-name key set and the host escalates a size mismatch instead of
    // returning a recoverable error.
    let map = Map::<Symbol, Val>::try_from_val(env, &val).map_err(|_| Error::LeaseNotFound)?;
    let fields = map.len();
    let has_created_at = map.contains_key(Symbol::new(env, "created_at"));

    let lease = if has_created_at {
        Lease::try_from_val(env, &val).map_err(|_| Error::LeaseNotFound)?
    } else if fields == LEGACY_LEASE_FIELDS {
        // A lease written before `created_at` existed. Its creation time is not
        // recoverable from storage, so report `0` (unknown) rather than failing a
        // settlement or refund that is still owed.
        let legacy = LegacyLease::try_from_val(env, &val).map_err(|_| Error::LeaseNotFound)?;
        Lease {
            lease_id: legacy.lease_id,
            service_id: legacy.service_id,
            consumer: legacy.consumer,
            allocated_calls: legacy.allocated_calls,
            used_calls: legacy.used_calls,
            created_at: 0,
            expires_at: legacy.expires_at,
            locked_deposit: legacy.locked_deposit,
            settled: legacy.settled,
        }
    } else {
        // Neither a current nor a pre-`created_at` lease entry.
        return Err(Error::LeaseNotFound);
    };

    env.storage().persistent().extend_ttl(
        &key,
        LEDGER_BUMP_THRESHOLD,
        lease_ttl_extend(env, &lease),
    );
    Ok(lease)
}

/// Test-only: rewrite a lease entry in the pre-`created_at` shape so the legacy
/// decode path in [`get_lease`] can be exercised.
#[cfg(test)]
pub(crate) fn save_legacy_lease_for_test(env: &Env, lease: &Lease) {
    let legacy = LegacyLease {
        lease_id: lease.lease_id,
        service_id: lease.service_id,
        consumer: lease.consumer.clone(),
        allocated_calls: lease.allocated_calls,
        used_calls: lease.used_calls,
        expires_at: lease.expires_at,
        locked_deposit: lease.locked_deposit,
        settled: lease.settled,
    };
    env.storage()
        .persistent()
        .set(&DataKey::Lease(lease.lease_id), &legacy);
}
