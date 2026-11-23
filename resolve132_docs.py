import pathlib

ROOT = pathlib.Path("/workspace/chats/265ace985ae3eeea3e99cab2")

def blocks(path):
    lines = (ROOT/path).read_text().split("\n")
    out, i = [], 0
    while i < len(lines):
        if lines[i].startswith("<<<<<<< "):
            j = i+1
            while lines[j] != "=======": j += 1
            k = j+1
            while not lines[k].startswith(">>>>>>> "): k += 1
            out.append(("\n".join(lines[i+1:j]), "\n".join(lines[j+1:k])))
            i = k+1
        else:
            i += 1
    return out

def resolve(path, reps):
    lines = (ROOT/path).read_text().split("\n")
    out, i, n = [], 0, 0
    while i < len(lines):
        if lines[i].startswith("<<<<<<< "):
            j = i+1
            while lines[j] != "=======": j += 1
            k = j+1
            while not lines[k].startswith(">>>>>>> "): k += 1
            out.extend(reps[n].split("\n"))
            n += 1
            i = k+1
        else:
            out.append(lines[i]); i += 1
    assert n == len(reps), (path, n, len(reps))
    (ROOT/path).write_text("\n".join(out))
    print("resolved", path, n)

# ------------------------------- README ---------------------------------------
b = blocks("README.md")
readme_actions = b[0][0] + "\n" + "\n".join(b[0][1].split("\n")[:2])
readme_flow = """allocated_calls`. Settlement is two-phase: `settle_lease` proposes the payout and
refund but moves nothing, the consumer has 24 hours to challenge the reported
call count, and `finalize_settlement` releases the funds afterwards. A challenged
settlement is resolved by the admin (uphold → full consumer refund, reject →
proposal stands). Before expiry, the consumer can also ask the provider to end
the lease early (`request_early_cancel`); if the provider approves, served calls
are paid and unused calls are refunded immediately. The consumer's revoke path
stays available only when no settlement was ever proposed and the 24-hour
provider grace window has elapsed."""
readme_tests = """The 120 tests in `contracts/registry/src/test.rs` cover:

- service registration and ID monotonicity,
- the happy path (lock → full-usage settle before expiry → finalize),
- early cancellation with provider consent, refusal, withdrawal, authorization,
  conservation at both extremes, and rejection after settlement,"""
resolve("README.md", [
    readme_actions,
    b[1][0] + "\n" + b[1][1],
    readme_flow,
    readme_tests,
])

# ------------------------------- SUMMARY / mkdocs -----------------------------
resolve("docs/SUMMARY.md", [blocks("docs/SUMMARY.md")[0][0] + "\n" + blocks("docs/SUMMARY.md")[0][1]])
resolve("mkdocs.yml", [blocks("mkdocs.yml")[0][0] + "\n" + blocks("mkdocs.yml")[0][1]])

# ------------------------------- architecture ---------------------------------
b = blocks("docs/architecture.md")
resolve("docs/architecture.md", [b[0][0] + "\n" + b[0][1], b[1][0] + "\n" + b[1][1], b[2][0] + "\n" + b[2][1]])

# ------------------------------- developer-guide ------------------------------
b = blocks("docs/developer-guide.md")
resolve("docs/developer-guide.md", [b[0][0] + "\n" + b[0][1]])

# ------------------------------- integration ----------------------------------
b = blocks("docs/integration.md")
int_flow = """   fully consumed, then **finalize** with `finalize_settlement` after the
   24-hour challenge window. Settlement only proposes the split; funds move at
   finalization. If the consumer asks to end the lease early
   (`request_early_cancel`), decide with `approve_early_cancel` (report served
   calls and release the unused balance) or `reject_early_cancel` (leave the
   lease running)."""
resolve("docs/integration.md", [int_flow, b[1][0] + "\n" + b[1][1]])

# ------------------------------- provider-guide -------------------------------
b = blocks("docs/provider-guide.md")
prov = """  published in the `lease_settled` event at finalization.
- **Unused consumer balance:** refunded to the consumer at finalization, on an
  approved early cancellation, or by the consumer via `revoke_expired_lease`
  after the 24h grace window if you never settled.
- **Early cancellation:** `lease_cancelled` publishes `verified_calls`, `payout`
  and `refund`. It is a consented early settlement; the same conservation rule
  holds (`payout + refund == locked_deposit`).
- **Disputes:** the consumer can challenge your reported count inside the
  24-hour window with an evidence hash. The registry admin resolves it; an upheld
  challenge refunds the whole deposit and pays you nothing. Report honestly and
  keep the evidence that supports your count."""
resolve("docs/provider-guide.md", [prov])

# ------------------------------- security -------------------------------------
b = blocks("docs/security.md")
sec1 = """4. **Early cancellation needs the provider's consent.** Only the consumer can
   request or withdraw, only the owning provider can approve or reject, and only
   approval moves funds (`Unauthorized`, `CancellationNotRequested`). A request
   never blocks use, settlement or revocation and expires with the lease.
5. **A lease releases funds once.** A settlement is proposed once (`settled` is
   persisted at proposal time), its state is persisted as `Finalized` before any
   token call, and every payout path rejects an already-finalized settlement
   (`LeaseAlreadySettled`, `SettlementFinalized`). Checks-effects-interactions is
   enforced by `test_reentrant_token_cannot_settle_a_lease_twice`.
6. **The payout is locked during a dispute.** A pending settlement cannot be
   finalized before `challengeable_until`, and a challenged one cannot be
   finalized at all until the admin resolves it (`ChallengeWindowStillOpen`,
   `SettlementChallenged`).
7. **Overflow aborts.** Deposit, payout and expiry arithmetic use"""
sec2 = """10. **The admin cannot redirect user funds.** It can call `set_token`, `upgrade`,
    and `resolve_challenge`. Resolution only chooses between the provider's
    proposal and a full consumer refund; there is no path that pays an arbitrary
    recipient.
11. **Every mutation is authenticated.** No state-changing entry point trusts its
    caller without `require_auth()`. `finalize_settlement` is permissionless by
    design: it can only release the exact split already recorded on chain.
12. **Every mutation is observable.** Each transition emits a typed Soroban event
    with no secret material in it."""
sec3 = """- `payout + refund == locked_deposit` for every settlement and every approved
  early cancellation."""
sec4 = b[3][0] + "\n" + b[3][1]
resolve("docs/security.md", [sec1, sec2, sec3, sec4])
# renumber the items that followed the first inserted property
p = ROOT/"docs/security.md"
text = p.read_text()
text = text.replace("7. **Lease durations are storage-safe.**", "8. **Lease durations are storage-safe.**", 1)
text = text.replace("8. **A single lease cannot escrow an unbounded amount.**", "9. **A single lease cannot escrow an unbounded amount.**", 1)
p.write_text(text)

# ------------------------------- protocol -------------------------------------
b = blocks("docs/protocol.md")
proto_inv = """4. **Locked while disputed.** No payout leaves escrow while a challenge is
   active; only the admin can resolve it.
5. **Consented early exit.** An early cancellation releases `rate × served_calls`
   to the provider and `rate × unused_calls` to the consumer, with
   `payout + refund == locked_deposit`, and only after the provider approves.
6. **Guaranteed exit.** Every lease has a consumer path to a full refund, through
   the revoke path, the challenge path or an approved early cancellation.
7. **Checked math.** Deposit, payout and timestamp arithmetic abort on overflow
   rather than truncating.
8. **Claimable storage.** A lease's persistent entry outlives its term plus the
   grace window, and settlement entries are kept for the standard persistent
   horizon (`MAX_LEASE_DURATION_SECS = 30 days`)."""
# NOTE: the following lines in the file are numbered; ensure no duplicate "2."
resolve("docs/protocol.md", [
    b[0][0] + "\n\n" + b[0][1],
    b[1][0] + "\n" + b[1][1],
    proto_inv,
    b[3][0] + "\n" + b[3][1],
])

# ------------------------------- contract-reference ---------------------------
b = blocks("docs/contract-reference.md")
cr_types = b[1][0] + "\n}\n\n" + b[1][1]
cr_errors = """| 25 | `CancellationNotRequested` | no pending early-cancellation request |
| 26 | `CancellationAlreadyRequested` | a request is already pending |
| 27 | `LeaseExpired` | early cancellation requires `now < expires_at` |
| 28 | `SettlementNotFound` | no settlement proposed for that lease |
| 29 | `SettlementFinalized` | settlement already released funds |
| 30 | `ChallengeWindowClosed` | challenge filed at or after `challengeable_until` |
| 31 | `SettlementChallenged` | settlement is already under an active challenge |
| 32 | `NoActiveChallenge` | nothing to cancel or resolve |
| 33 | `ChallengeWindowStillOpen` | finalize attempted before the window closes |
| 34 | `InvalidEvidence` | challenge evidence hash is all zeroes |"""
cr_cli = b[4][1][:b[4][1].rindex("# settle")] + "# propose a settlement (opens the 24h challenge window)"
resolve("docs/contract-reference.md", [
    b[0][0] + "\n" + b[0][1],
    cr_types,
    b[2][0] + "\n" + b[2][1],
    cr_errors,
    cr_cli,
])
