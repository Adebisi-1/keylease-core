use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
    token, Address, BytesN, Env, Event, IntoVal, Vec,
};

use keylease_mock_token::{MockToken, MockTokenClient};

use crate::events::{
    EscrowTokenSet, LeaseCreated, LeaseDelegated, LeaseRevoked, LeaseSettled, ServiceRegistered,
    ServiceStatusChanged,
};
use crate::{
    Error, KeyLeaseRegistry, KeyLeaseRegistryClient, Lease, LeaseStatus, ServiceStats,
    MAX_LEASE_DEPOSIT, MAX_LEASE_DURATION_SECS, MAX_RATE_PER_CALL,
};

/// Per-call rate used across the tests.
const RATE: i128 = 100;
/// Escrow funded to the consumer at setup.
const FUNDING: i128 = 1_000_000;
/// The 24h provider settlement grace period, mirrored from the contract.
const GRACE: u64 = 24 * 60 * 60;

struct Ctx<'a> {
    registry: KeyLeaseRegistryClient<'a>,
    token: MockTokenClient<'a>,
    admin: Address,
    provider: Address,
    consumer: Address,
}

impl Ctx<'_> {
    fn registry_id(&self) -> Address {
        self.registry.address.clone()
    }
}

/// Deploy a mock token and registry, wire them together, fund the consumer.
fn setup(env: &Env) -> Ctx<'_> {
    env.mock_all_auths();

    let admin = Address::generate(env);
    let provider = Address::generate(env);
    let consumer = Address::generate(env);

    let token_id = env.register(MockToken, ());
    let token = MockTokenClient::new(env, &token_id);
    token.init(&admin);
    token.mint(&consumer, &FUNDING);

    let registry_id = env.register(KeyLeaseRegistry, ());
    let registry = KeyLeaseRegistryClient::new(env, &registry_id);
    registry.init(&admin);
    registry.set_token(&admin, &token_id);

    Ctx {
        registry,
        token,
        admin,
        provider,
        consumer,
    }
}

fn endpoint_hash(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[7u8; 32])
}

fn register_default_service(env: &Env, ctx: &Ctx) -> u64 {
    ctx.registry
        .register_service(&ctx.provider, &RATE, &endpoint_hash(env))
}

// ---------------------------------------------------------------------------
// Event helpers
// ---------------------------------------------------------------------------

/// Assert that the last contract invocation emitted exactly `event`, published by
/// `contract`, with the topics and payload `#[contractevent]` generates.
fn assert_single_event<E: Event>(env: &Env, contract: &Address, event: &E) {
    let expected = Vec::from_array(
        env,
        [(contract.clone(), event.topics(env), event.data(env))],
    );
    assert_eq!(env.events().all(), expected);
}

// ---------------------------------------------------------------------------
// Initialization & configuration
// ---------------------------------------------------------------------------

#[test]
fn test_init_configures_admin_and_token() {
    let env = Env::default();
    let ctx = setup(&env);

    assert_eq!(ctx.registry.get_admin(), ctx.admin);
    assert_eq!(ctx.registry.get_token(), ctx.token.address);
    assert_eq!(ctx.registry.service_count(), 0);
    assert_eq!(ctx.registry.lease_count(), 0);
}

#[test]
fn test_double_init_is_rejected() {
    let env = Env::default();
    let ctx = setup(&env);

    assert_eq!(
        ctx.registry.try_init(&ctx.admin),
        Err(Ok(Error::AlreadyInitialized))
    );
}

#[test]
fn test_set_token_is_admin_only() {
    let env = Env::default();
    let ctx = setup(&env);
    let impostor = Address::generate(&env);

    assert_eq!(
        ctx.registry.try_set_token(&impostor, &ctx.token.address),
        Err(Ok(Error::Unauthorized))
    );
}

// ---------------------------------------------------------------------------
// Service registration
// ---------------------------------------------------------------------------

#[test]
fn test_register_service_assigns_incrementing_ids() {
    let env = Env::default();
    let ctx = setup(&env);

    let first = register_default_service(&env, &ctx);
    let second = register_default_service(&env, &ctx);

    assert_eq!(first, 1);
    assert_eq!(second, 2);
    assert_eq!(ctx.registry.service_count(), 2);

    let service = ctx.registry.get_service(&first);
    assert_eq!(service.service_id, first);
    assert_eq!(service.provider, ctx.provider);
    assert_eq!(service.rate_per_call, RATE);
    assert!(service.is_active);
    assert_eq!(service.endpoint_hash, endpoint_hash(&env));
}

#[test]
fn test_register_service_rejects_non_positive_rate() {
    let env = Env::default();
    let ctx = setup(&env);

    assert_eq!(
        ctx.registry
            .try_register_service(&ctx.provider, &0, &endpoint_hash(&env)),
        Err(Ok(Error::InvalidRate))
    );
    assert_eq!(
        ctx.registry
            .try_register_service(&ctx.provider, &-1, &endpoint_hash(&env)),
        Err(Ok(Error::InvalidRate))
    );
    assert_eq!(ctx.registry.service_count(), 0);
}

#[test]
fn test_deactivated_service_rejects_new_leases() {
    let env = Env::default();
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    ctx.registry
        .set_service_active(&ctx.provider, &service_id, &false);

    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &service_id, &10, &3_600),
        Err(Ok(Error::ServiceInactive))
    );

    // Reactivating restores leasing.
    ctx.registry
        .set_service_active(&ctx.provider, &service_id, &true);
    assert_eq!(
        ctx.registry
            .create_lease(&ctx.consumer, &service_id, &10, &3_600),
        1
    );
}

#[test]
fn test_set_service_active_is_provider_only() {
    let env = Env::default();
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let impostor = Address::generate(&env);

    assert_eq!(
        ctx.registry
            .try_set_service_active(&impostor, &service_id, &false),
        Err(Ok(Error::Unauthorized))
    );
}

// ---------------------------------------------------------------------------
// Lease creation
// ---------------------------------------------------------------------------

#[test]
fn test_create_lease_locks_deposit_and_records_terms() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    assert_eq!(lease_id, 1);
    assert_eq!(ctx.registry.lease_count(), 1);

    let lease = ctx.registry.get_lease(&lease_id);
    assert_eq!(lease.lease_id, lease_id);
    assert_eq!(lease.service_id, service_id);
    assert_eq!(lease.consumer, ctx.consumer);
    assert_eq!(lease.allocated_calls, 10);
    assert_eq!(lease.used_calls, 0);
    assert_eq!(lease.expires_at, 1_000 + 3_600);
    assert_eq!(lease.locked_deposit, RATE * 10);
    assert!(!lease.settled);

    // Deposit moved from consumer into escrow.
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING - RATE * 10);
    assert_eq!(ctx.token.balance(&ctx.registry_id()), RATE * 10);
    assert_eq!(ctx.registry.remaining_calls(&lease_id), 10);
    assert_eq!(ctx.registry.locked_balance(&lease_id), RATE * 10);
}

#[test]
fn test_create_lease_rejects_zero_calls() {
    let env = Env::default();
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &service_id, &0, &3_600),
        Err(Ok(Error::InvalidCalls))
    );
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
}

#[test]
fn test_create_lease_rejects_zero_duration() {
    let env = Env::default();
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &service_id, &10, &0),
        Err(Ok(Error::InvalidDuration))
    );
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
}

#[test]
fn test_create_lease_rejects_unknown_service() {
    let env = Env::default();
    let ctx = setup(&env);

    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &999, &10, &3_600),
        Err(Ok(Error::ServiceNotFound))
    );
}

#[test]
fn test_create_lease_requires_configured_token() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let provider = Address::generate(&env);
    let consumer = Address::generate(&env);

    let registry_id = env.register(KeyLeaseRegistry, ());
    let registry = KeyLeaseRegistryClient::new(&env, &registry_id);
    registry.init(&admin);
    // Note: no set_token call.

    let service_id = registry.register_service(&provider, &RATE, &endpoint_hash(&env));

    assert_eq!(
        registry.try_create_lease(&consumer, &service_id, &10, &3_600),
        Err(Ok(Error::TokenNotSet))
    );
}

// ---------------------------------------------------------------------------
// Delegated lease creation
// ---------------------------------------------------------------------------

#[test]
fn test_delegate_can_create_lease_for_consumer() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let delegate = Address::generate(&env);

    let lease_id =
        ctx.registry
            .create_lease_for(&delegate, &ctx.consumer, &service_id, &10, &3_600);

    // `env.events().all()` only covers the *last* invocation, so snapshot the
    // creation's events before any read call overwrites them.
    let events = env.events().all();

    // The lease is the consumer's, and the consumer alone was debited.
    let lease = ctx.registry.get_lease(&lease_id);
    assert_eq!(lease.consumer, ctx.consumer);
    assert_eq!(lease.allocated_calls, 10);
    assert!(!lease.settled);
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING - RATE * 10);
    assert_eq!(ctx.token.balance(&delegate), 0);
    assert_eq!(ctx.token.balance(&ctx.registry_id()), RATE * 10);

    // `lease_created` is unchanged; `lease_delegated` records the courier.
    let created = LeaseCreated {
        lease_id,
        service_id,
        consumer: ctx.consumer.clone(),
        allocated_calls: 10,
        expires_at: 4_600,
        locked_deposit: RATE * 10,
    };
    let delegated = LeaseDelegated {
        lease_id,
        delegate: delegate.clone(),
        consumer: ctx.consumer.clone(),
    };
    let expected = Vec::from_array(
        &env,
        [
            (ctx.registry_id(), created.topics(&env), created.data(&env)),
            (
                ctx.registry_id(),
                delegated.topics(&env),
                delegated.data(&env),
            ),
        ],
    );
    assert_eq!(events, expected);
}

#[test]
fn test_delegate_cannot_create_lease_without_consumer_authorization() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let delegate = Address::generate(&env);
    let registry_id = ctx.registry_id();

    // `setup` mocked every auth; this hands the host exactly one signature —
    // the delegate's — so anything requiring the consumer must now fail.
    env.mock_auths(&[MockAuth {
        address: &delegate,
        invoke: &MockAuthInvoke {
            contract: &registry_id,
            fn_name: "create_lease_for",
            args: (&delegate, &ctx.consumer, &service_id, &10u32, &3_600u64).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let result =
        ctx.registry
            .try_create_lease_for(&delegate, &ctx.consumer, &service_id, &10, &3_600);

    assert!(
        result.is_err(),
        "a delegate must not lock a consumer's deposit without the consumer's signature"
    );
    assert_eq!(ctx.registry.lease_count(), 0);
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
    assert_eq!(ctx.token.balance(&ctx.registry_id()), 0);
}

#[test]
fn test_consumer_alone_cannot_create_a_delegated_lease() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let delegate = Address::generate(&env);
    let registry_id = ctx.registry_id();

    // The mirror case: the consumer signed, but the named delegate never did,
    // so the transaction that pays the fee cannot be attributed to it.
    env.mock_auths(&[MockAuth {
        address: &ctx.consumer,
        invoke: &MockAuthInvoke {
            contract: &registry_id,
            fn_name: "create_lease_for",
            args: (&delegate, &ctx.consumer, &service_id, &10u32, &3_600u64).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let result =
        ctx.registry
            .try_create_lease_for(&delegate, &ctx.consumer, &service_id, &10, &3_600);

    assert!(
        result.is_err(),
        "create_lease_for must authenticate the delegate too"
    );
    assert_eq!(ctx.registry.lease_count(), 0);
}

#[test]
fn test_delegated_lease_refund_belongs_to_the_consumer() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let delegate = Address::generate(&env);

    let lease_id = ctx
        .registry
        .create_lease_for(&delegate, &ctx.consumer, &service_id, &10, &100);

    // The delegate is not on the lease and cannot reclaim it.
    env.ledger().set_timestamp(1_100 + GRACE);
    assert_eq!(
        ctx.registry.try_revoke_expired_lease(&delegate, &lease_id),
        Err(Ok(Error::Unauthorized))
    );

    ctx.registry.revoke_expired_lease(&ctx.consumer, &lease_id);
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
    assert_eq!(ctx.token.balance(&delegate), 0);
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Revoked);
}

#[test]
fn test_delegated_creation_enforces_the_same_preconditions() {
    let env = Env::default();
    let ctx = setup(&env);
    let delegate = Address::generate(&env);

    // The delegate path shares the creation checks, it does not bypass them.
    assert_eq!(
        ctx.registry
            .try_create_lease_for(&delegate, &ctx.consumer, &999, &10, &3_600),
        Err(Ok(Error::ServiceNotFound))
    );
    let service_id = register_default_service(&env, &ctx);
    assert_eq!(
        ctx.registry
            .try_create_lease_for(&delegate, &ctx.consumer, &service_id, &0, &3_600),
        Err(Ok(Error::InvalidCalls))
    );
    assert_eq!(
        ctx.registry
            .try_create_lease_for(&delegate, &ctx.consumer, &service_id, &10, &0),
        Err(Ok(Error::InvalidDuration))
    );
    assert_eq!(ctx.registry.lease_count(), 0);
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

#[test]
fn test_full_usage_can_settle_before_expiry() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    // Not expired, but fully consumed -> settlement is allowed.
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &10);

    assert_eq!(ctx.token.balance(&ctx.provider), RATE * 10);
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING - RATE * 10);
    assert_eq!(ctx.token.balance(&ctx.registry_id()), 0);

    let lease = ctx.registry.get_lease(&lease_id);
    assert_eq!(lease.used_calls, 10);
    assert!(lease.settled);
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Settled);
}

#[test]
fn test_partial_settle_after_expiry_refunds_remainder() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    // Move past expiry (4_600) with a small buffer.
    env.ledger().set_timestamp(4_601);
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &6);

    // Provider paid for 6 calls; consumer refunded the remaining 4.
    assert_eq!(ctx.token.balance(&ctx.provider), RATE * 6);
    assert_eq!(
        ctx.token.balance(&ctx.consumer),
        FUNDING - RATE * 10 + RATE * 4
    );
    assert_eq!(ctx.token.balance(&ctx.registry_id()), 0);

    let lease = ctx.registry.get_lease(&lease_id);
    assert_eq!(lease.used_calls, 6);
    assert!(lease.settled);
}

#[test]
fn test_partial_settle_before_expiry_is_rejected() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    assert_eq!(
        ctx.registry.try_settle_lease(&ctx.provider, &lease_id, &4),
        Err(Ok(Error::LeaseNotSettleable))
    );
    // Escrow is untouched.
    assert_eq!(ctx.token.balance(&ctx.registry_id()), RATE * 10);
}

#[test]
fn test_lease_cannot_be_settled_twice() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    ctx.registry.settle_lease(&ctx.provider, &lease_id, &10);

    assert_eq!(
        ctx.registry.try_settle_lease(&ctx.provider, &lease_id, &10),
        Err(Ok(Error::LeaseAlreadySettled))
    );
    // Provider was paid exactly once.
    assert_eq!(ctx.token.balance(&ctx.provider), RATE * 10);
}

#[test]
fn test_only_owning_provider_can_settle() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    let impostor = Address::generate(&env);
    assert_eq!(
        ctx.registry.try_settle_lease(&impostor, &lease_id, &10),
        Err(Ok(Error::Unauthorized))
    );
    assert_eq!(ctx.token.balance(&impostor), 0);
}

#[test]
fn test_provider_cannot_over_claim_calls() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    assert_eq!(
        ctx.registry.try_settle_lease(&ctx.provider, &lease_id, &11),
        Err(Ok(Error::InvalidCalls))
    );
}

// ---------------------------------------------------------------------------
// Expired, unsettled reclamation
// ---------------------------------------------------------------------------

#[test]
fn test_expired_unsettled_lease_is_fully_reclaimed() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    // Expiry is at 1_100; exactly at the end of the 24h grace window.
    env.ledger().set_timestamp(1_100 + GRACE);
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Expired);

    ctx.registry.revoke_expired_lease(&ctx.consumer, &lease_id);

    // Full deposit returned; provider earns nothing.
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
    assert_eq!(ctx.token.balance(&ctx.provider), 0);
    assert_eq!(ctx.token.balance(&ctx.registry_id()), 0);

    let lease = ctx.registry.get_lease(&lease_id);
    assert!(lease.settled);
    // Revocation is its own terminal status, not a generic settlement.
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Revoked);
    // The provider earned nothing, and `used_calls` reads that back.
    assert_eq!(lease.used_calls, 0);
    assert_eq!(ctx.registry.remaining_calls(&lease_id), 0);
    assert_eq!(ctx.registry.locked_balance(&lease_id), 0);
}

#[test]
fn test_revoke_before_expiry_is_rejected() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    env.ledger().set_timestamp(1_050);
    assert_eq!(
        ctx.registry
            .try_revoke_expired_lease(&ctx.consumer, &lease_id),
        Err(Ok(Error::LeaseNotExpired))
    );
    assert_eq!(ctx.token.balance(&ctx.registry_id()), RATE * 10);
}

#[test]
fn test_revoke_during_grace_window_is_rejected() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    // One second short of the grace window.
    env.ledger().set_timestamp(1_100 + GRACE - 1);
    assert_eq!(
        ctx.registry
            .try_revoke_expired_lease(&ctx.consumer, &lease_id),
        Err(Ok(Error::RevokeWindowNotElapsed))
    );
}

#[test]
fn test_only_consumer_can_revoke() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    env.ledger().set_timestamp(1_100 + GRACE);
    let impostor = Address::generate(&env);
    assert_eq!(
        ctx.registry.try_revoke_expired_lease(&impostor, &lease_id),
        Err(Ok(Error::Unauthorized))
    );
}

#[test]
fn test_provider_can_settle_within_grace_window() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    // Provider settles late-but-in-time: expiry + 12h.
    env.ledger().set_timestamp(1_100 + GRACE / 2);
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &3);

    assert_eq!(ctx.token.balance(&ctx.provider), RATE * 3);
    assert_eq!(
        ctx.token.balance(&ctx.consumer),
        FUNDING - RATE * 10 + RATE * 7
    );

    // Consumer can no longer reclaim what was already settled.
    env.ledger().set_timestamp(1_100 + GRACE + 1);
    assert_eq!(
        ctx.registry
            .try_revoke_expired_lease(&ctx.consumer, &lease_id),
        Err(Ok(Error::LeaseAlreadySettled))
    );
}

// ---------------------------------------------------------------------------
// Service settlement stats
// ---------------------------------------------------------------------------

#[test]
fn test_service_stats_start_at_zero_and_reject_unknown_service() {
    let env = Env::default();
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    // A service nobody has settled yet reads as zero, not as missing.
    assert_eq!(
        ctx.registry.service_stats(&service_id),
        ServiceStats::zero()
    );
    assert_eq!(
        ctx.registry.try_service_stats(&999),
        Err(Ok(Error::ServiceNotFound))
    );
}

#[test]
fn test_service_stats_accumulate_settled_leases_calls_and_payouts() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    // Fully consumed on the first lease...
    let first = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);
    ctx.registry.settle_lease(&ctx.provider, &first, &10);

    // ...and a partial settlement after expiry on the second, so the counter
    // must record the calls the provider was paid for, not the allotment.
    let second = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);
    env.ledger().set_timestamp(4_601);
    ctx.registry.settle_lease(&ctx.provider, &second, &4);

    assert_eq!(
        ctx.registry.service_stats(&service_id),
        ServiceStats {
            settled_leases: 2,
            settled_calls: 14,
            total_payout: RATE * 14,
        }
    );
    // The aggregate equals what actually reached the provider.
    assert_eq!(ctx.token.balance(&ctx.provider), RATE * 14);
}

#[test]
fn test_service_stats_record_a_zero_call_settlement() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    // Past expiry, so a zero-call settlement is allowed: the provider refunds
    // the whole deposit and still counts as having settled the lease.
    env.ledger().set_timestamp(4_601);
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &0);

    assert_eq!(
        ctx.registry.service_stats(&service_id),
        ServiceStats {
            settled_leases: 1,
            settled_calls: 0,
            total_payout: 0,
        }
    );
}

#[test]
fn test_revoked_lease_does_not_count_toward_service_stats() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    let settled = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);
    ctx.registry.settle_lease(&ctx.provider, &settled, &10);

    // A second lease the provider never settles: the consumer reclaims it in
    // full after expiry plus the grace window.
    let revoked = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &7, &100);
    env.ledger().set_timestamp(1_100 + GRACE);
    ctx.registry.revoke_expired_lease(&ctx.consumer, &revoked);

    assert_eq!(
        ctx.registry.service_stats(&service_id),
        ServiceStats {
            settled_leases: 1,
            settled_calls: 10,
            total_payout: RATE * 10,
        }
    );
    assert_eq!(ctx.token.balance(&ctx.provider), RATE * 10);
}

#[test]
fn test_service_stats_are_scoped_per_service() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let first_service = register_default_service(&env, &ctx);
    let second_service =
        ctx.registry
            .register_service(&ctx.provider, &(RATE * 2), &endpoint_hash(&env));

    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &first_service, &10, &3_600);
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &10);

    assert_eq!(ctx.registry.service_stats(&first_service).settled_leases, 1);
    assert_eq!(
        ctx.registry.service_stats(&second_service),
        ServiceStats::zero()
    );
}

// ---------------------------------------------------------------------------
// Rate bound and deposit arithmetic
// ---------------------------------------------------------------------------

#[test]
fn test_rate_above_max_is_rejected_at_registration() {
    let env = Env::default();
    let ctx = setup(&env);

    // One over the bound, and the old unbounded extreme, both fail here.
    assert_eq!(
        ctx.registry.try_register_service(
            &ctx.provider,
            &(MAX_RATE_PER_CALL + 1),
            &endpoint_hash(&env)
        ),
        Err(Ok(Error::RateTooHigh))
    );
    assert_eq!(
        ctx.registry
            .try_register_service(&ctx.provider, &i128::MAX, &endpoint_hash(&env)),
        Err(Ok(Error::RateTooHigh))
    );
    assert_eq!(ctx.registry.service_count(), 0);
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
}

#[test]
fn test_max_rate_is_accepted_and_leasable() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);

    let service_id =
        ctx.registry
            .register_service(&ctx.provider, &MAX_RATE_PER_CALL, &endpoint_hash(&env));
    assert_eq!(
        ctx.registry.get_service(&service_id).rate_per_call,
        MAX_RATE_PER_CALL
    );

    // The largest rate is still a service somebody can actually buy, which is
    // the property `MAX_RATE_PER_CALL = MAX_LEASE_DEPOSIT` exists to guarantee:
    // one call at the ceiling is exactly the largest fundable deposit.
    ctx.token.mint(&ctx.consumer, &MAX_RATE_PER_CALL);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &1, &3_600);

    assert_eq!(
        ctx.registry.get_lease(&lease_id).locked_deposit,
        MAX_RATE_PER_CALL
    );
    assert_eq!(ctx.token.balance(&ctx.registry_id()), MAX_RATE_PER_CALL);

    // Settling the whole allotment pays the same figure back out.
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &1);
    assert_eq!(ctx.token.balance(&ctx.provider), MAX_RATE_PER_CALL);
    assert_eq!(ctx.token.balance(&ctx.registry_id()), 0);
}

#[test]
fn test_rate_bound_keeps_deposit_math_inside_i128() {
    // The bound is defined as the deposit cap, so the worst product is the cap
    // times the largest `u32`: 10^12 * 4.3 x 10^9 < 2^72, against an `i128`
    // ceiling of 2^127. The lease-time multiplication cannot overflow.
    let worst_deposit = MAX_RATE_PER_CALL.checked_mul(u32::MAX as i128);
    assert!(
        worst_deposit.is_some(),
        "MAX_RATE_PER_CALL must keep every allowed deposit inside i128"
    );
    assert!(worst_deposit.unwrap() < i128::MAX);
}

#[test]
fn test_large_but_valid_deposit_is_accepted() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);

    // This test used `(1i128 << 62) * 2`, which fits `i128` with room to spare.
    // `MAX_LEASE_DEPOSIT` now sits far below that, so "large but valid" means
    // "large and still inside the bound": one unit under the cap, which is the
    // last amount the protocol accepts. The cap itself and `cap + 1` are covered
    // by the `MAX_LEASE_DEPOSIT` tests below.
    let rate: i128 = MAX_LEASE_DEPOSIT - 1;
    let service_id = ctx
        .registry
        .register_service(&ctx.provider, &rate, &endpoint_hash(&env));

    // Fund the consumer for the full deposit (rate * 1).
    ctx.token.mint(&ctx.consumer, &rate);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &1, &3_600);

    assert_eq!(ctx.registry.get_lease(&lease_id).locked_deposit, rate);
}

#[test]
fn test_expiry_timestamp_overflow_reverts() {
    let env = Env::default();
    env.ledger().set_timestamp(u64::MAX);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &service_id, &10, &1),
        Err(Ok(Error::Overflow))
    );
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
}

// ---------------------------------------------------------------------------
// Status transitions
// ---------------------------------------------------------------------------

#[test]
fn test_lease_status_transitions() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Active);

    env.ledger().set_timestamp(4_600);
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Expired);

    ctx.registry.settle_lease(&ctx.provider, &lease_id, &0);
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Settled);
}

#[test]
fn test_lease_status_discriminants_are_stable() {
    let env = Env::default();
    use soroban_sdk::{IntoVal, TryFromVal, Val};

    // The ABI promises Active = 0, Expired = 1, Settled = 2, and `Revoked` is
    // appended as 3 — nothing renumbered, so old clients still decode history.
    let active: Val = LeaseStatus::Active.into_val(&env);
    let expired: Val = LeaseStatus::Expired.into_val(&env);
    let settled: Val = LeaseStatus::Settled.into_val(&env);
    let revoked: Val = LeaseStatus::Revoked.into_val(&env);

    assert_eq!(u32::try_from_val(&env, &active).unwrap(), 0u32);
    assert_eq!(u32::try_from_val(&env, &expired).unwrap(), 1u32);
    assert_eq!(u32::try_from_val(&env, &settled).unwrap(), 2u32);
    assert_eq!(u32::try_from_val(&env, &revoked).unwrap(), 3u32);
}

#[test]
fn test_lease_status_of_separates_revocation_from_settlement() {
    let env = Env::default();
    let consumer = Address::generate(&env);

    // Pure view over the stored lease plus the registry's revocation marker.
    let lease = |settled: bool| Lease {
        lease_id: 1,
        service_id: 1,
        consumer: consumer.clone(),
        allocated_calls: 10,
        used_calls: 0,
        expires_at: 4_600,
        locked_deposit: RATE * 10,
        settled,
    };

    assert_eq!(
        LeaseStatus::of(&lease(false), 1_000, false),
        LeaseStatus::Active
    );
    assert_eq!(
        LeaseStatus::of(&lease(false), 4_600, false),
        LeaseStatus::Expired
    );
    assert_eq!(
        LeaseStatus::of(&lease(true), 1_000, false),
        LeaseStatus::Settled
    );
    assert_eq!(
        LeaseStatus::of(&lease(true), 4_600, false),
        LeaseStatus::Settled
    );
    // A reclaimed lease is `Revoked` regardless of the timestamp it is read at:
    // the funding path, not the clock, decides the terminal status.
    assert_eq!(
        LeaseStatus::of(&lease(true), 1_000, true),
        LeaseStatus::Revoked
    );
    assert_eq!(
        LeaseStatus::of(&lease(true), 9_999, true),
        LeaseStatus::Revoked
    );
}

#[test]
fn test_revoked_lease_is_not_reported_as_settled() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    // Same service, same terms; only who closes the lease differs.
    let settled_lease = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);
    let revoked_lease = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    ctx.registry
        .settle_lease(&ctx.provider, &settled_lease, &10);

    env.ledger().set_timestamp(4_600 + GRACE);
    ctx.registry
        .revoke_expired_lease(&ctx.consumer, &revoked_lease);

    assert_eq!(
        ctx.registry.lease_status(&settled_lease),
        LeaseStatus::Settled
    );
    assert_eq!(
        ctx.registry.lease_status(&revoked_lease),
        LeaseStatus::Revoked
    );
    assert_ne!(
        ctx.registry.lease_status(&settled_lease),
        ctx.registry.lease_status(&revoked_lease),
        "a reclaimed deposit must not read as provider revenue"
    );

    // The stored `settled` flag stays true for both: the lease struct layout is
    // frozen, so `Revoked` is carried by the marker, not by a new field.
    assert!(ctx.registry.get_lease(&settled_lease).settled);
    assert!(ctx.registry.get_lease(&revoked_lease).settled);
}

#[test]
fn test_settle_after_revoke_is_rejected() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    env.ledger().set_timestamp(1_100 + GRACE);
    ctx.registry.revoke_expired_lease(&ctx.consumer, &lease_id);

    // A provider that arrives after the consumer reclaimed the escrow gets
    // nothing, and the lease stays `Revoked`.
    assert_eq!(
        ctx.registry.try_settle_lease(&ctx.provider, &lease_id, &10),
        Err(Ok(Error::LeaseAlreadySettled))
    );
    assert_eq!(ctx.token.balance(&ctx.provider), 0);
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Revoked);
    assert_eq!(
        ctx.registry.service_stats(&service_id),
        ServiceStats::zero()
    );
}

#[test]
fn test_revoke_after_settle_is_rejected() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    // Provider settles the full usage before expiry, then the grace window
    // opens: the consumer must not be able to convert a paid lease into a
    // refund, and the status must remain `Settled`.
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &10);
    env.ledger().set_timestamp(1_100 + GRACE);

    assert_eq!(
        ctx.registry
            .try_revoke_expired_lease(&ctx.consumer, &lease_id),
        Err(Ok(Error::LeaseAlreadySettled))
    );
    assert_eq!(ctx.registry.lease_status(&lease_id), LeaseStatus::Settled);
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING - RATE * 10);
    assert_eq!(ctx.token.balance(&ctx.provider), RATE * 10);
}

#[test]
fn test_lease_status_unknown_lease() {
    let env = Env::default();
    let ctx = setup(&env);

    assert_eq!(
        ctx.registry.try_lease_status(&42),
        Err(Ok(Error::LeaseNotFound))
    );
}

// ---------------------------------------------------------------------------
// Soroban contract events
// ---------------------------------------------------------------------------

#[test]
fn test_set_token_emits_event() {
    let env = Env::default();
    let ctx = setup(&env);

    assert_single_event(
        &env,
        &ctx.registry_id(),
        &EscrowTokenSet {
            admin: ctx.admin.clone(),
            token: ctx.token.address.clone(),
        },
    );
}

#[test]
fn test_service_registration_emits_event() {
    let env = Env::default();
    let ctx = setup(&env);

    let service_id = register_default_service(&env, &ctx);

    assert_single_event(
        &env,
        &ctx.registry_id(),
        &ServiceRegistered {
            service_id,
            provider: ctx.provider.clone(),
            rate_per_call: RATE,
            endpoint_hash: endpoint_hash(&env),
        },
    );
}

#[test]
fn test_service_status_change_emits_event() {
    let env = Env::default();
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    ctx.registry
        .set_service_active(&ctx.provider, &service_id, &false);

    assert_single_event(
        &env,
        &ctx.registry_id(),
        &ServiceStatusChanged {
            service_id,
            provider: ctx.provider.clone(),
            is_active: false,
        },
    );
}

#[test]
fn test_lease_lifecycle_emits_events() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &3_600);

    assert_single_event(
        &env,
        &ctx.registry_id(),
        &LeaseCreated {
            lease_id,
            service_id,
            consumer: ctx.consumer.clone(),
            allocated_calls: 10,
            expires_at: 4_600,
            locked_deposit: RATE * 10,
        },
    );

    env.ledger().set_timestamp(4_601);
    ctx.registry.settle_lease(&ctx.provider, &lease_id, &6);

    assert_single_event(
        &env,
        &ctx.registry_id(),
        &LeaseSettled {
            lease_id,
            provider: ctx.provider.clone(),
            verified_calls: 6,
            payout: RATE * 6,
            refund: RATE * 4,
        },
    );
}

#[test]
fn test_revoke_emits_event() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);
    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &10, &100);

    env.ledger().set_timestamp(1_100 + GRACE);
    ctx.registry.revoke_expired_lease(&ctx.consumer, &lease_id);

    assert_single_event(
        &env,
        &ctx.registry_id(),
        &LeaseRevoked {
            lease_id,
            consumer: ctx.consumer.clone(),
            refund: RATE * 10,
        },
    );
}

// ---------------------------------------------------------------------------
// Soroban lifecycle: metadata, upgrades, storage bounds
// ---------------------------------------------------------------------------

#[test]
fn test_version_matches_contract_metadata() {
    let env = Env::default();
    let ctx = setup(&env);

    assert_eq!(
        ctx.registry.version(),
        soroban_sdk::String::from_str(&env, "0.3.0")
    );
}

#[test]
fn test_upgrade_is_admin_only() {
    let env = Env::default();
    let ctx = setup(&env);
    let impostor = Address::generate(&env);
    let new_wasm_hash = BytesN::from_array(&env, &[9u8; 32]);

    assert_eq!(
        ctx.registry.try_upgrade(&impostor, &new_wasm_hash),
        Err(Ok(Error::Unauthorized))
    );
}

// A successful upgrade installs a real Wasm uploaded with
// `stellar contract upload`, which the unit-test host cannot fabricate (it
// requires a valid `contractspecv0` metadata section). The success path is
// exercised end to end by `scripts/deploy-testnet.sh --upgrade` against
// Testnet; here we pin the authorization guard.

#[test]
fn test_duration_above_max_is_rejected() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    assert_eq!(
        ctx.registry.try_create_lease(
            &ctx.consumer,
            &service_id,
            &10,
            &(MAX_LEASE_DURATION_SECS + 1)
        ),
        Err(Ok(Error::DurationTooLong))
    );
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
}

#[test]
fn test_max_duration_is_accepted() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_default_service(&env, &ctx);

    let lease_id =
        ctx.registry
            .create_lease(&ctx.consumer, &service_id, &10, &MAX_LEASE_DURATION_SECS);
    assert_eq!(
        ctx.registry.get_lease(&lease_id).expires_at,
        1_000 + MAX_LEASE_DURATION_SECS
    );
}

// ---------------------------------------------------------------------------
// Maximum deposit bound (MAX_LEASE_DEPOSIT)
// ---------------------------------------------------------------------------

/// Register a service at an explicit rate and return its id. The deposit bound
/// lives in the `rate × requested_calls` product, so these tests need rates far
/// outside the `RATE` the rest of the suite uses.
fn register_service_at(env: &Env, ctx: &Ctx, rate: i128) -> u64 {
    ctx.registry
        .register_service(&ctx.provider, &rate, &endpoint_hash(env))
}

#[test]
fn test_deposit_exactly_at_max_is_accepted() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    let service_id = register_service_at(&env, &ctx, MAX_LEASE_DEPOSIT);
    ctx.token.mint(&ctx.consumer, &MAX_LEASE_DEPOSIT);

    let lease_id = ctx
        .registry
        .create_lease(&ctx.consumer, &service_id, &1, &3_600);

    assert_eq!(
        ctx.registry.get_lease(&lease_id).locked_deposit,
        MAX_LEASE_DEPOSIT
    );
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
}

#[test]
fn test_deposit_one_above_max_is_rejected() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    // A rate of `MAX_LEASE_DEPOSIT + 1` can no longer be listed at all —
    // `MAX_RATE_PER_CALL` equals the deposit cap — so the product is composed
    // instead: 1_000_000_000_001 = 10_001 x 99_990_001, one unit over the cap
    // from two prices the protocol both accepts.
    let service_id = register_service_at(&env, &ctx, 10_001);
    let calls: u32 = 99_990_001;
    // Fund the lease the bound is meant to stop: the consumer could otherwise
    // afford it, and a passing assertion would only be proving poverty.
    ctx.token.mint(&ctx.consumer, &(MAX_LEASE_DEPOSIT + 1));
    let funded = ctx.token.balance(&ctx.consumer);
    let leases_before = ctx.registry.lease_count();

    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &service_id, &calls, &3_600),
        Err(Ok(Error::DepositTooLarge))
    );
    assert_eq!(ctx.token.balance(&ctx.consumer), funded);
    assert_eq!(
        ctx.registry.lease_count(),
        leases_before,
        "the bound must reject before the lease id is allocated"
    );
}

#[test]
fn test_deposit_bound_applies_to_the_product_not_the_rate() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    // Every input is well under the bound; only their product crosses it, which
    // is the case a rate-only check would let through.
    let service_id = register_service_at(&env, &ctx, 1_000_000);
    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &service_id, &1_000_001, &3_600),
        Err(Ok(Error::DepositTooLarge))
    );
    assert_eq!(ctx.token.balance(&ctx.consumer), FUNDING);
}

#[test]
fn test_the_two_bounds_fire_at_different_entry_points() {
    let env = Env::default();
    env.ledger().set_timestamp(1_000);
    let ctx = setup(&env);
    // `MAX_RATE_PER_CALL` guards the listing and `MAX_LEASE_DEPOSIT` guards the
    // lease, so the extreme rate this test used to reach through
    // `create_lease` is now stopped one call earlier and for a different
    // reason. The two errors stay distinguishable for clients, and the
    // multiplication itself can no longer overflow: the worst legal product is
    // `MAX_RATE_PER_CALL * u32::MAX < 2^72`, inside `i128` by ~10^17.
    assert_eq!(
        ctx.registry
            .try_register_service(&ctx.provider, &(i128::MAX / 2), &endpoint_hash(&env)),
        Err(Ok(Error::RateTooHigh))
    );
    assert_eq!(ctx.registry.service_count(), 0);

    // A product that crosses the deposit cap while both inputs stay legal is
    // still `DepositTooLarge`, not an arithmetic failure.
    let service_id = register_service_at(&env, &ctx, MAX_RATE_PER_CALL);
    assert_eq!(
        ctx.registry
            .try_create_lease(&ctx.consumer, &service_id, &2, &3_600),
        Err(Ok(Error::DepositTooLarge))
    );
}

// ---------------------------------------------------------------------------
// Stellar Asset Contract (SAC) integration
// ---------------------------------------------------------------------------

#[test]
fn test_escrow_runs_on_the_stellar_asset_contract() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);
    let provider = Address::generate(&env);
    let consumer = Address::generate(&env);

    // The same contract Stellar deploys for a wrapped native asset (XLM).
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let sac_id = sac.address();
    token::StellarAssetClient::new(&env, &sac_id).mint(&consumer, &FUNDING);

    let registry_id = env.register(KeyLeaseRegistry, ());
    let registry = KeyLeaseRegistryClient::new(&env, &registry_id);
    registry.init(&admin);
    registry.set_token(&admin, &sac_id);

    let service_id = registry.register_service(&provider, &RATE, &endpoint_hash(&env));
    let lease_id = registry.create_lease(&consumer, &service_id, &10, &3_600);

    let sac_client = token::Client::new(&env, &sac_id);
    assert_eq!(sac_client.balance(&consumer), FUNDING - RATE * 10);
    assert_eq!(sac_client.balance(&registry_id), RATE * 10);

    env.ledger().set_timestamp(4_601);
    registry.settle_lease(&provider, &lease_id, &4);

    assert_eq!(sac_client.balance(&provider), RATE * 4);
    assert_eq!(
        sac_client.balance(&consumer),
        FUNDING - RATE * 10 + RATE * 6
    );
    assert_eq!(sac_client.balance(&registry_id), 0);
}

// ---------------------------------------------------------------------------
// Re-entrancy hardening (checks-effects-interactions)
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
enum ReentryKey {
    Registry,
    Provider,
    LeaseId,
    Armed,
    Blocked,
}

/// A hostile SEP-41-shaped token that re-enters `settle_lease` from `transfer`.
///
/// It never moves value; it only records whether the re-entrant settlement was
/// rejected. With checks-effects-interactions the second attempt must fail with
/// `LeaseAlreadySettled` before any payout is attempted.
#[contract]
pub struct ReentrantToken;

#[contractimpl]
impl ReentrantToken {
    pub fn arm(env: Env, registry: Address, provider: Address, lease_id: u64) {
        env.storage()
            .instance()
            .set(&ReentryKey::Registry, &registry);
        env.storage()
            .instance()
            .set(&ReentryKey::Provider, &provider);
        env.storage()
            .instance()
            .set(&ReentryKey::LeaseId, &lease_id);
        env.storage().instance().set(&ReentryKey::Armed, &true);
        env.storage().instance().set(&ReentryKey::Blocked, &false);
    }

    pub fn transfer(env: Env, _from: Address, _to: Address, _amount: i128) {
        let armed: bool = env
            .storage()
            .instance()
            .get(&ReentryKey::Armed)
            .unwrap_or(false);
        if !armed {
            return;
        }

        // Disarm so the attack is attempted exactly once per settlement.
        env.storage().instance().set(&ReentryKey::Armed, &false);

        let registry: Address = env.storage().instance().get(&ReentryKey::Registry).unwrap();
        let provider: Address = env.storage().instance().get(&ReentryKey::Provider).unwrap();
        let lease_id: u64 = env.storage().instance().get(&ReentryKey::LeaseId).unwrap();

        let client = KeyLeaseRegistryClient::new(&env, &registry);
        let blocked = client.try_settle_lease(&provider, &lease_id, &0).is_err();
        env.storage().instance().set(&ReentryKey::Blocked, &blocked);
    }

    pub fn blocked(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&ReentryKey::Blocked)
            .unwrap_or(false)
    }
}

#[test]
fn test_reentrant_token_cannot_settle_a_lease_twice() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);
    let provider = Address::generate(&env);
    let consumer = Address::generate(&env);

    let token_id = env.register(ReentrantToken, ());
    let token_client = ReentrantTokenClient::new(&env, &token_id);

    let registry_id = env.register(KeyLeaseRegistry, ());
    let registry = KeyLeaseRegistryClient::new(&env, &registry_id);
    registry.init(&admin);
    registry.set_token(&admin, &token_id);

    let service_id = registry.register_service(&provider, &RATE, &endpoint_hash(&env));
    let lease_id = registry.create_lease(&consumer, &service_id, &10, &3_600);

    token_client.arm(&registry_id, &provider, &lease_id);
    env.ledger().set_timestamp(4_601);
    registry.settle_lease(&provider, &lease_id, &10);

    assert!(
        token_client.blocked(),
        "re-entrant settle_lease should have been rejected"
    );
    assert!(registry.get_lease(&lease_id).settled);
}
