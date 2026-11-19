# Integration

KeyLease has no privileged off-chain service and no bespoke credential format.
A provider integrates with the Stellar ledger directly: it verifies the lease on
chain, binds the caller to the lease's consumer using Stellar account
authentication, and settles through the contract.

This page describes the supported pattern. Any implementation that follows it is
compatible with the protocol.

## What the credential is

The **lease is the credential**. `get_lease(lease_id)` returns the authoritative
terms: `allocated_calls`, `used_calls`, `expires_at`, `settled`, and the
`consumer` who owns the refund path. A provider admits a request only when that
on-chain record says it is allowed.

There is nothing else to issue: no API key, no session JWT, no signing secret.
The consumer's identity is its Stellar address (`G…` or `C…`).

## The verification pipeline

A provider should implement these steps, in order, and fail closed at every one:

1. **Authenticate the caller with Stellar.** Use a challenge–response flow with
   the consumer's Stellar key; [SEP-10 Stellar Web Authentication][sep10] is the
   standard way to prove account ownership to an off-chain service. The result is
   a trusted `Address` for the request — not a password.
2. **Resolve the lease on chain.** Call `get_lease(lease_id)` over Soroban RPC
   and cache the result briefly (a few seconds is usually enough for a busy
   provider). An unreachable RPC is a **503**, never an allow.
3. **Check the lease.** The lease must exist, must not be `settled`, and
   `ledger.timestamp() < expires_at`. Reject otherwise (`403`).
4. **Check ownership.** The authenticated address must equal `lease.consumer`.
   Otherwise reject (`403`). This is what makes a stolen lease id worthless.
5. **Enforce the quota.** Use `max(lease.used_calls, local_counter)` against
   `lease.allocated_calls`; reject with `429` when exhausted, and increment the
   counter only for admitted requests.
6. **Serve the request**, adding the remaining-calls header if you want the
   client to see its budget.
7. **Settle** with `settle_lease` when the lease expires or when the allotment is
   fully consumed, then **finalize** with `finalize_settlement` after the
   24-hour challenge window. Settlement only proposes the split; funds move at
   finalization.

Step 5's local counter is an optimisation, not the source of truth. On-chain
`allocated_calls × rate_per_call` bounds what the provider can ever be paid, so a
tampered counter can only make the provider charge **less**. It can never
over-charge the consumer.

[sep10]: https://developers.stellar.org/docs/learn/encyclopedia/security/authentication

## Reading a lease

With the Stellar CLI:

```bash
stellar contract invoke --id $REGISTRY --source-account keylease-admin \
  --network testnet -- get_lease --lease_id 1
```

With `@stellar/stellar-sdk`, simulate a `get_lease` invocation and decode the
result:

```ts
import {
  Contract, TransactionBuilder, Networks, Account,
  nativeToScVal, scValToNative, rpc,
} from '@stellar/stellar-sdk';

const server = new rpc.Server(process.env.KEYLEASE_RPC_URL!);
const contract = new Contract(process.env.KEYLEASE_CONTRACT_ID!);

export async function readLease(leaseId: bigint) {
  const source = new Account(
    'GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF',
    '0',
  );
  const tx = new TransactionBuilder(source, {
    fee: '100',
    networkPassphrase: Networks.TESTNET,
  })
    .addOperation(contract.call('get_lease', nativeToScVal(leaseId, { type: 'u64' })))
    .setTimeout(30)
    .build();

  const sim = await server.simulateTransaction(tx);
  if (!rpc.Api.isSimulationSuccess(sim) || !sim.result) {
    throw new Error('get_lease failed');
  }
  return scValToNative(sim.result.retval) as {
    lease_id: bigint;
    service_id: bigint;
    consumer: string;
    allocated_calls: number;
    used_calls: number;
    expires_at: bigint;
    locked_deposit: bigint;
    settled: boolean;
  };
}
```

The read views require no authorization, so simulation is enough — a provider
never needs a Stellar secret key to verify a lease. That is deliberate:
compromising an enforcement tier does not compromise escrowed funds.

## Following the event stream

For accounting, dashboards and settlement automation, subscribe to the contract's
events instead of polling every lease:

```bash
stellar events --id $REGISTRY --network testnet --start-ledger <ledger>
```

| Event | Use |
| --- | --- |
| `lease_created` | Build a work queue of leases to settle; alert on new demand. |
| `lease_settlement_proposed` | A settlement is waiting out the challenge window; schedule a finalize at `challengeable_until`. |
| `lease_challenged` / `lease_challenge_cancelled` | The consumer disputed (or withdrew a dispute of) the reported calls; the payout is frozen until resolved. |
| `lease_challenge_resolved` | The admin decided a dispute (`upheld`), immediately followed by the payout event. |
| `lease_settled` | Funds actually left escrow: reconcile payouts and refunds; close the lease in your ledger. |
| `lease_revoked` | A consumer reclaimed an expired lease; the provider earned nothing. |
| `service_registered` / `service_status_changed` | Keep a service catalogue in sync. |
| `contract_upgraded` | Detect a new Wasm version (`version()` also changes). |

Every event carries the ids as topics, so a subscriber can filter by `lease_id`
or `service_id` without decoding the payload. Event names and fields are part of
the ABI; see the [contract reference](contract-reference.md#events).

## Settling

Settlement is two transactions. The provider proposes a split at or after expiry,
or as soon as every allocated call is consumed:

```bash
stellar contract invoke --id $REGISTRY --source-account provider --network testnet \
  --send=yes -- settle_lease \
    --provider $PROVIDER --lease_id 1 --verified_calls 320
```

That transaction moves no funds. It emits `lease_settlement_proposed` and opens a
24-hour consumer challenge window. Once the window closes, anyone can release the
funds:

```bash
stellar contract invoke --id $REGISTRY --source-account provider --network testnet \
  --send=yes -- finalize_settlement --lease_id 1
```

Reporting `verified_calls = 0` proposes a full refund for a lease you could not
serve. Choose a settlement cadence that fits your traffic: one settlement per
lease, not per request.

## The consumer challenge window

The consumer can dispute the provider's reported call count while the window is
open:

```bash
stellar contract invoke --id $REGISTRY --source-account consumer --network testnet \
  --send=yes -- challenge_settlement \
    --consumer $CONSUMER --lease_id 1 \
    --evidence_hash <sha256-of-your-evidence>
```

Only a 32-byte commitment goes on chain; the evidence itself is presented to the
admin off chain. A challenge freezes the payout. The consumer may withdraw it
(`cancel_challenge`) and the settlement returns to pending, with the original
deadline unchanged. The registry admin resolves an active challenge on chain:

```bash
stellar contract invoke --id $REGISTRY --source-account admin --network testnet \
  --send=yes -- resolve_challenge --admin $ADMIN --lease_id 1 --upheld true
```

`upheld = true` refunds the entire deposit to the consumer and pays the provider
nothing. `upheld = false` releases the proposal unchanged. Index `upheld` from
the `lease_challenge_resolved` event and reconcile the following `lease_settled`
event.

## Hardening checklist

- [ ] Verify leases server-side; never trust a `lease_id` supplied by the client
      without checking `consumer` against an authenticated Stellar address.
- [ ] Fail closed on RPC errors, malformed lease data and unknown leases.
- [ ] Cache lease reads briefly (≈5–30 s) but never past the request you are
      authorising; re-read on a cache miss or when close to `expires_at`.
- [ ] Share the per-lease counter if you run more than one instance, or accept
      that each instance can grant the full allotment (the provider only loses
      revenue; the consumer is still bounded on chain).
- [ ] Keep provider signing keys out of the request path where possible: reads
      need no key, only settlement does.
- [ ] Watch `lease_created` so no lease expires unsettled and becomes revocable.
- [ ] Never log Stellar secret seeds.

## Read next

- [Networks](networks.md) — RPC endpoints and passphrases.
- [Provider guide](provider-guide.md) — the operational view.
- [Contract reference](contract-reference.md) — the ABI you are calling.
