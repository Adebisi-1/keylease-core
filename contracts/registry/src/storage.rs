use soroban_sdk::{contracttype, Address, Env};

use crate::errors::Error;
use crate::types::{Lease, Service, ServiceStats};
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
    /// Cumulative settlement totals by service id (persistent storage).
    ServiceStats(u64),
    /// Presence marker for a lease reclaimed by its consumer (persistent storage).
    LeaseRevoked(u64),
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
    let lease = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::LeaseNotFound)?;
    env.storage().persistent().extend_ttl(
        &key,
        LEDGER_BUMP_THRESHOLD,
        lease_ttl_extend(env, &lease),
    );
    Ok(lease)
}

// ---------------------------------------------------------------------------
// Derived per-entity state: settlement stats and revocation markers
// ---------------------------------------------------------------------------
//
// Both keys below describe an entity that already has its own entry, and both
// are additive: neither changes the layout of `Service` or `Lease`, so entries
// written by an earlier Wasm keep decoding. Their TTL policy is therefore
// "stay readable at least as long as the entity they describe":
//
// * `ServiceStats` uses the generic persistent policy
//   (`LEDGER_BUMP_THRESHOLD` / `LEDGER_BUMP_EXTEND`, ~30 days) and is rewritten
//   by every settlement, so a service that is still being leased cannot age out
//   mid-life. If an entry does age out — a service idle for thirty days —
//   `get_service_stats` reads [`ServiceStats::zero`]. The counters are a
//   convenience over the ledger; `lease_settled` stays the authoritative record.
// * `LeaseRevoked` is extended with `lease_ttl_extend`, the horizon the lease
//   entry itself is given in the same transaction, so any lease that is still
//   readable always has its marker readable.
//
// Neither key has a user-controlled lifetime: the marker is written once per
// lease by `revoke_expired_lease`, and the counters only move inside
// `settle_lease`, so no caller can grow storage without bound.

pub fn get_service_stats(env: &Env, service_id: u64) -> ServiceStats {
    let key = DataKey::ServiceStats(service_id);
    match env.storage().persistent().get(&key) {
        Some(stats) => {
            env.storage()
                .persistent()
                .extend_ttl(&key, LEDGER_BUMP_THRESHOLD, LEDGER_BUMP_EXTEND);
            stats
        }
        // A service with no settlement has no entry yet, and reads as zero.
        None => ServiceStats::zero(),
    }
}

/// Add one provider settlement to its service's cumulative totals.
///
/// Totals are checked, never saturating: a silently clamped revenue counter is
/// worse for a reporter than a reverted settlement. The bound is arithmetic,
/// not economic — with `MAX_RATE_PER_CALL` capping one settlement's payout
/// below 2^72, overflowing `total_payout` would take ~4 × 10^16 such
/// settlements, i.e. far more tokens than exist.
pub fn record_settlement(
    env: &Env,
    service_id: u64,
    settled_calls: u32,
    payout: i128,
) -> Result<(), Error> {
    let key = DataKey::ServiceStats(service_id);
    let mut stats: ServiceStats = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or(ServiceStats::zero());

    stats.settled_leases = stats.settled_leases.checked_add(1).ok_or(Error::Overflow)?;
    stats.settled_calls = stats
        .settled_calls
        .checked_add(settled_calls as u64)
        .ok_or(Error::Overflow)?;
    stats.total_payout = stats
        .total_payout
        .checked_add(payout)
        .ok_or(Error::Overflow)?;

    env.storage().persistent().set(&key, &stats);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_BUMP_THRESHOLD, LEDGER_BUMP_EXTEND);
    Ok(())
}

/// Record that `lease` was reclaimed by its consumer instead of settled.
pub fn mark_lease_revoked(env: &Env, lease: &Lease) {
    let key = DataKey::LeaseRevoked(lease.lease_id);
    env.storage().persistent().set(&key, &true);
    env.storage().persistent().extend_ttl(
        &key,
        LEDGER_BUMP_THRESHOLD,
        lease_ttl_extend(env, lease),
    );
}

/// Whether `lease` was reclaimed by its consumer rather than settled by its provider.
pub fn is_lease_revoked(env: &Env, lease: &Lease) -> bool {
    let key = DataKey::LeaseRevoked(lease.lease_id);
    if !env.storage().persistent().has(&key) {
        return false;
    }
    env.storage().persistent().extend_ttl(
        &key,
        LEDGER_BUMP_THRESHOLD,
        lease_ttl_extend(env, lease),
    );
    true
}
