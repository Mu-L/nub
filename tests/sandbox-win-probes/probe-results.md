# Windows sandbox primitive probes — results

Ground-truth validation of the unprivileged Windows sandbox primitives nub's sandbox backend relies on, run on a real `windows-latest` GitHub Actions runner (Windows Server 2025, build 10.0.26100). Each probe is relaunched under a genuine non-elevated token and carries a negative control so a PASS cannot be vacuous. The harness lives alongside this file; `sandbox-win-probes.yml` drives it.

## Verdicts

| Axis | Verdict | Unprivileged | Evidence |
|---|---|---|---|
| FS read-confine (KEY) | PASS | yes | allowed read exit 0, vault secret read exit 5 (ACCESS_DENIED) |
| FS write-confine | PASS | yes | allowed-dir write exit 0, outside-dir write exit 5 |
| Network egress block | PASS | yes | connect without `internetClient` exit 5 (WSAEACCES), with it exit 0 |
| Job-object reap | PASS | yes | grandchild alive before job-handle close, gone after |
| Env-scrub at spawn | PASS | yes | inherit-env child sees token; scrubbed-env child does not |

"Unprivileged" means the whole setup ran under a non-elevated token (`IsElevated: False`) — no admin rights, no UAC consent.

## CI runs

- `28275788051` — first real per-axis verdicts (de-elevation via a standard user working end to end).
- `28276071205` — FS read-confine confirmed PASS with the token-dump and ACL evidence below.
- Env-scrub reached a real PASS/FAIL only after the `.ArgumentList` fix (that property does not exist in .NET Framework / Windows PowerShell 5.1); landed for confirmation on the run following `28276071205`.

## KEY result — unprivileged FS read-confine

The load-bearing question was whether a child can be confined to read only an allowlisted directory, with no elevation and no dedicated account, on native Windows. It can, via an AppContainer launched default-deny.

Model: grant the AppContainer SID read-and-execute on a work directory (seed an allowed file there); place the secret in a vault directory with inheritance broken and no AppContainer or ALL APPLICATION PACKAGES grant. An AppContainer (lowbox) token can reach an object only where the ACL grants its AppContainer SID, a capability, or ALL APPLICATION PACKAGES — so the vault is unreachable by default, with no per-file deny-ACE.

Token-dump evidence (the child reports its own token; run `28276071205`, parent `IsElevated: False`, child user `nubprobe37343`):

```
CHILD whoami TokenIsAppContainer=1
CHILD whoami TokenAppContainerSid=S-1-15-2-2631071299-3134207189-3800372815-1829338890-2948641700-...
CHILD whoami IntegrityLevelSid=S-1-16-4096
```

`TokenIsAppContainer=1` with a real AppContainer SID and Low integrity (`S-1-16-4096`) proves the child is genuinely in the AppContainer, not a plain process.

ACL evidence (the vault grants only the running user; no AppContainer or ALL APPLICATION PACKAGES ACE):

```
--- icacls vault ---
...\vault runnervmo3n6x\nubprobe37343:(OI)(CI)(F)
--- icacls work ---
...  APPLICATION PACKAGES:(I)(OI)(CI)(RX)
```

The work directory carries ALL APPLICATION PACKAGES read-and-execute (the allowlist); the vault carries none. So the read outcome is the confinement, not an inherited grant:

```
child(read allowed) raw exit: 0
child(read secret)  raw exit: 5
PROBE1 read-confine: PASS: allowed readable, secret in default-deny vault BLOCKED (exit=5)
```

## Conclusion

Unprivileged Windows filesystem read-confine works via an AppContainer launched default-deny plus a grant on only the work directory — no elevation, no dedicated account. Write-confine, coarse network-egress block, job-object descendant reap, and spawn-time env-scrub all hold under the same non-elevated conditions.

## Production mechanism vs the harness

In production, nub launches its lifecycle child directly into an AppContainer with `CreateProcess` + `STARTUPINFOEX` carrying a `SECURITY_CAPABILITIES` (the AppContainer SID, and capability SIDs such as `internetClient` when egress is allowed). That is the whole mechanism.

The harness adds one thing nub does not need: it relaunches each probe under a throwaway standard user (`New-LocalUser` + `CreateProcessWithLogonW`) before creating the AppContainer. That exists only to PROVE the unprivileged sub-claim on a runner whose default job token is elevated and has no UAC linked split-token. The confinement itself does not depend on it — an AppContainer child is restricted by its AppContainer SID regardless of the parent's elevation.

## Running it

Push to the `sandbox-windows-probes` branch (or dispatch the workflow) to run on `windows-latest`. The job fails loudly unless every probe reports PASS; each probe prints a `PROBE<N> <name>: <verdict>: <detail>` line with its mechanism and unprivileged sub-results. These probes are standalone and do not touch `crates/nub-sandbox`.
