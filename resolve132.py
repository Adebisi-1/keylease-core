import re, pathlib, subprocess

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

# ---------------- test.rs: rebuild from HEAD + upstream early-cancel section ----
head_test = subprocess.run(["git","show","HEAD:contracts/registry/src/test.rs"],cwd=ROOT,capture_output=True,text=True,check=True).stdout
up_test = subprocess.run(["git","show","upstream/main:contracts/registry/src/test.rs"],cwd=ROOT,capture_output=True,text=True,check=True).stdout

head_test = head_test.replace(
"""use crate::events::{
    ContractUpgraded, EscrowTokenSet, LeaseChallengeCancelled, LeaseChallengeResolved,
    LeaseChallenged, LeaseCreated, LeaseRevoked, LeaseSettled, LeaseSettlementProposed,
    RegistryInitialized, ServiceRateUpdated, ServiceRegistered, ServiceStatusChanged,
    UpgradeCancelled, UpgradeProposed, EVENT_SCHEMA_VERSION,
};""",
"""use crate::events::{
    ContractUpgraded, EscrowTokenSet, LeaseCancellationRejected, LeaseCancellationRequested,
    LeaseCancellationWithdrawn, LeaseCancelled, LeaseChallengeCancelled, LeaseChallengeResolved,
    LeaseChallenged, LeaseCreated, LeaseRevoked, LeaseSettled, LeaseSettlementProposed,
    RegistryInitialized, ServiceRateUpdated, ServiceRegistered, ServiceStatusChanged,
    UpgradeCancelled, UpgradeProposed, EVENT_SCHEMA_VERSION,
};""", 1)

marker = "// ---------------------------------------------------------------------------\n// Early consumer cancellation\n// ---------------------------------------------------------------------------\n"
idx = up_test.index(marker)
up_section = up_test[idx:].rstrip("\n")
(ROOT/"contracts/registry/src/test.rs").write_text(head_test.rstrip("\n") + "\n\n" + up_section + "\n")
print("resolved contracts/registry/src/test.rs (rebuilt)")

# ---------------- events.rs ----------------------------------------------------
b = blocks("contracts/registry/src/events.rs")[0]
h = b[0]
h = h[:h.rindex("/// Emitted when a settlement is finalized")].rstrip("\n")
t = b[1]
t = t[:t.rindex("/// Emitted when a provider settles a lease")].rstrip("\n")
resolve("contracts/registry/src/events.rs", [
    h + "\n\n" + t + "\n\n/// Emitted when a settlement is finalized and funds leave escrow."
])

# ---------------- errors.rs ----------------------------------------------------
resolve("contracts/registry/src/errors.rs", ["""    /// No early-cancellation request exists for the lease.
    CancellationNotRequested = 25,
    /// The lease already has a pending early-cancellation request.
    CancellationAlreadyRequested = 26,
    /// Early cancellation is only available before the lease expires.
    LeaseExpired = 27,
    /// No settlement has been proposed for the given `lease_id`.
    SettlementNotFound = 28,
    /// The settlement has already been finalized; funds have been released.
    SettlementFinalized = 29,
    /// The consumer challenge window has closed (or was never open).
    ChallengeWindowClosed = 30,
    /// The settlement is already under an active consumer challenge.
    SettlementChallenged = 31,
    /// There is no active challenge to cancel or resolve.
    NoActiveChallenge = 32,
    /// The challenge window has not closed yet, so the payout stays locked.
    ChallengeWindowStillOpen = 33,
    /// The challenge evidence hash is all zeroes.
    InvalidEvidence = 34,"""])

# ---------------- storage.rs ---------------------------------------------------
b = blocks("contracts/registry/src/storage.rs")
resolve("contracts/registry/src/storage.rs", [
    b[0][0] + "\n" + b[0][1],
    b[1][0] + "\n" + b[1][1],
    b[2][0] + """
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_BUMP_THRESHOLD, LEDGER_BUMP_EXTEND);
}

""" + b[2][1],
    b[3][0] + "\n}\n\n" + b[3][1],
])

# ---------------- types.rs -----------------------------------------------------
b = blocks("contracts/registry/src/types.rs")
resolve("contracts/registry/src/types.rs", [b[0][0] + "\n}\n\n" + b[0][1]])

# ---------------- lib.rs -------------------------------------------------------
b = blocks("contracts/registry/src/lib.rs")
lib_ascii = b[0][0] + "\n" + "\n".join(b[0][1].split("\n")[1:])
lib_imports = b[1][0] + "\n" + b[1][1]
t = b[2][1]
t = t[:t.rindex("    /// Settle a lease, paying")].rstrip("\n")
lib_funcs = t + "\n\n" + b[2][0]
lib_helpers = b[3][0].rstrip("\n") + "\n    Ok(())\n}\n\n" + b[3][1].rstrip("\n")
resolve("contracts/registry/src/lib.rs", [lib_ascii, lib_imports, lib_funcs, lib_helpers])
