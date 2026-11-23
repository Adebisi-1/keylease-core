import pathlib, subprocess

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
            out.extend(reps[n].split("\n")); n += 1; i = k+1
        else:
            out.append(lines[i]); i += 1
    assert n == len(reps), (path, n, len(reps))
    (ROOT/path).write_text("\n".join(out))
    print("resolved", path, n)

def upstream(path):
    return subprocess.run(["git","show",f"upstream/main:{path}"],cwd=ROOT,capture_output=True,text=True,check=True).stdout

# ------------------------------- lib.rs: upstream + transfer fn ---------------
head_lib = subprocess.run(["git","show","HEAD:contracts/registry/src/lib.rs"],cwd=ROOT,capture_output=True,text=True,check=True).stdout
start = head_lib.index("    /// Transfer a lease to a new consumer")
end = head_lib.index("\n    }\n", start) + len("\n    }\n")
transfer_fn = head_lib[start:end]

lib = upstream("contracts/registry/src/lib.rs")
lib = lib.replace(
"    LeaseCancellationWithdrawn, LeaseCancelled, LeaseCreated, LeaseRevoked, LeaseSettled,",
"    LeaseCancellationWithdrawn, LeaseCancelled, LeaseCreated, LeaseRevoked, LeaseSettled,\n    LeaseTransferred,", 1)
anchor = "    /// Settle a lease, paying the provider for `verified_calls` consumed calls"
assert anchor in lib
lib = lib.replace(anchor, transfer_fn + "\n" + anchor, 1)
(ROOT/"contracts/registry/src/lib.rs").write_text(lib)
print("rebuilt lib.rs")

# ------------------------------- events.rs: upstream + LeaseTransferred ------
head_ev = subprocess.run(["git","show","HEAD:contracts/registry/src/events.rs"],cwd=ROOT,capture_output=True,text=True,check=True).stdout
s = head_ev.index("/// Emitted when a lease is transferred to a new consumer.")
e = head_ev.index("}\n", head_ev.index("pub to: Address,", s)) + 2
transfer_ev = head_ev[s:e]
ev = upstream("contracts/registry/src/events.rs")
anchor = "/// Emitted when a provider settles a lease from escrow."
assert anchor in ev
ev = ev.replace(anchor, transfer_ev + "\n" + anchor, 1)
(ROOT/"contracts/registry/src/events.rs").write_text(ev)
print("rebuilt events.rs")

# ------------------------------- errors.rs ------------------------------------
resolve("contracts/registry/src/errors.rs", ["""    /// No early-cancellation request exists for the lease.
    CancellationNotRequested = 25,
    /// The lease already has a pending early-cancellation request.
    CancellationAlreadyRequested = 26,
    /// Early cancellation is only available before the lease expires.
    LeaseExpired = 27,
    /// The lease is already settled or revoked and cannot change hands.
    LeaseNotTransferable = 28,
    /// The transfer target is the current consumer.
    InvalidConsumer = 29,"""])

# ------------------------------- test.rs: upstream + transfer section ---------
head_test = subprocess.run(["git","show","HEAD:contracts/registry/src/test.rs"],cwd=ROOT,capture_output=True,text=True,check=True).stdout
head_test = head_test.replace(
"    ContractUpgraded, EscrowTokenSet, LeaseCreated, LeaseRevoked, LeaseSettled, LeaseTransferred,",
"    ContractUpgraded, EscrowTokenSet, LeaseCancellationRejected, LeaseCancellationRequested,\n    LeaseCancellationWithdrawn, LeaseCancelled, LeaseCreated, LeaseRevoked, LeaseSettled,\n    LeaseTransferred,", 1)
marker = "// Lease transfer\n// ---------------------------------------------------------------------------\n"
transfer_tests = head_test[head_test.index(marker):].rstrip("\n")
(ROOT/"contracts/registry/src/test.rs").write_text(upstream("contracts/registry/src/test.rs").rstrip("\n") + "\n\n" + transfer_tests + "\n")
print("rebuilt test.rs")

# ------------------------------- README ---------------------------------------
b = blocks("README.md")
readme_actions = b[0][0] + "\n" + b[0][1]
readme_tests = """The 116 tests in `contracts/registry/src/test.rs` cover:

- service registration and ID monotonicity,
- the happy path (lock → full-usage settle before expiry),
- lease transfers (ownership move, refund right, settlement after transfer,
  unauthorized and settled-lease rejection),
- early cancellation with provider consent, refusal, withdrawal, authorization,
  conservation at both extremes, and rejection after settlement,"""
resolve("README.md", [readme_actions, b[1][0] + "\n" + b[1][1], readme_tests])

# ------------------------------- SUMMARY / mkdocs -----------------------------
for f in ("docs/SUMMARY.md", "mkdocs.yml"):
    bb = blocks(f)
    resolve(f, [bb[0][0] + "\n" + bb[0][1]] if not f.endswith(".md") else [bb[0][0] + "\n" + bb[0][1]])

# ------------------------------- architecture ---------------------------------
b = blocks("docs/architecture.md")
arch_ev = '        EV[["Contract events<br/>lease_created · lease_transferred · lease_cancelled · lease_settled · lease_revoked"]]'
arch_row = "| Accounting / dashboards | `stellar events`, Horizon or an indexer subscribing to `lease_created` / `lease_transferred` / `lease_cancelled` / `lease_settled` / `lease_revoked`. |"
resolve("docs/architecture.md", [arch_ev, arch_row])

# ------------------------------- consumer-guide -------------------------------
b = blocks("docs/consumer-guide.md")
cg2 = """  --send=yes -- transfer_lease \\
     --consumer $CONSUMER --lease_id 1 --new_consumer <NEW_ADDRESS>
```

This is a direct assignment — the recipient does not need to accept — and it
emits `lease_transferred { lease_id, from, to }`. A settled or revoked lease
cannot be transferred, and neither can a lease whose settlement is pending under
a challenge window. An expired, unsettled lease can still be transferred: you
are selling the refund claim, not access.

### End the lease early

```bash
stellar contract invoke \\
  --id $REGISTRY --source-account consumer --network testnet \\
  --send=yes -- request_early_cancel --consumer $CONSUMER --lease_id 1
```

The provider either approves — reporting the calls it actually served — or
rejects the request. On approval you are refunded `rate × unused_calls`
immediately and the lease ends; on rejection the lease keeps running exactly as
before. You can withdraw a pending request with `withdraw_early_cancel`. Early
cancellation is only available before `expires_at`; after expiry, use the revoke
path below."""
resolve("docs/consumer-guide.md", [b[0][0], cg2, b[2][0] + "\n" + b[2][1]])

# ------------------------------- contract-reference ---------------------------
b = blocks("docs/contract-reference.md")
cr_errors = """| 25 | `CancellationNotRequested` | no pending early-cancellation request |
| 26 | `CancellationAlreadyRequested` | a request is already pending |
| 27 | `LeaseExpired` | early cancellation requires `now < expires_at` |
| 28 | `LeaseNotTransferable` | lease already settled or revoked |
| 29 | `InvalidConsumer` | transfer target is the current consumer |"""
resolve("docs/contract-reference.md", [
    b[0][0] + "\n\n" + b[0][1],
    b[1][0] + "\n" + b[1][1],
    cr_errors,
    b[3][0] + "\n\n" + b[3][1],
])

# ------------------------------- protocol -------------------------------------
b = blocks("docs/protocol.md")
proto_inv = """5. **Consented early exit.** An early cancellation releases `rate × served_calls`
   to the provider and `rate × unused_calls` to the consumer, with
   `payout + refund == locked_deposit`, and only after the provider approves.
6. **Single-owner leases.** `Lease.consumer` is the only ownership record, and a
   transfer rewrites it in one save. Access and the refund path can never belong
   to different addresses.
7. **Checked math.** Deposit, payout and timestamp arithmetic abort on overflow
   rather than truncating."""
resolve("docs/protocol.md", [
    b[0][0] + "\n\n" + b[0][1],
    b[1][0] + "\n" + b[1][1],
    proto_inv,
])
p = ROOT/"docs/protocol.md"
t = p.read_text().replace("7. **Claimable storage.**", "8. **Claimable storage.**", 1)
p.write_text(t)

# ------------------------------- security -------------------------------------
b = blocks("docs/security.md")
sec1 = b[0][0] + "\n" + b[0][1]
sec2 = """7. **Overflow aborts.** Deposit, payout, refund and expiry arithmetic use"""
sec3 = """- `payout + refund == locked_deposit` for every settlement and every approved
  early cancellation.
- A settled lease never transitions again and cannot be transferred."""
sec4 = b[3][0] + "\n" + b[3][1]
resolve("docs/security.md", [sec1, sec2, sec3, sec4])
p = ROOT/"docs/security.md"
t = p.read_text()
for old, new in [
    ("5. **A lease releases funds once.**", "6. **A lease releases funds once.**"),
    ("6. **Overflow aborts.**", "7. **Overflow aborts.**"),
    ("7. **Lease durations are storage-safe.**", "8. **Lease durations are storage-safe.**"),
    ("8. **A single lease cannot escrow an unbounded amount.**", "9. **A single lease cannot escrow an unbounded amount.**"),
    ("9. **The admin cannot touch user funds.**", "10. **The admin cannot touch user funds.**"),
    ("10. **Every mutation is authenticated.**", "11. **Every mutation is authenticated.**"),
    ("11. **Every mutation is observable.**", "12. **Every mutation is observable.**"),
]:
    t = t.replace(old, new, 1)
p.write_text(t)
print("done")
