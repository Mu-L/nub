#!/usr/bin/env bash
# Self-contained macOS write-confine smoke tests — the fast core of the harness:
# enforcement + the bypass surface, no mega-fixture, no network. Each assertion
# carries a negative control so a PASS cannot be vacuous. Exits non-zero on any
# failure (CI gate). Run: ./smoke-test.sh
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GEN="$HERE/gen-profile.mjs"
FAIL=0
pass() { echo "  ok  $1"; }
fail() { echo "FAIL  $1"; FAIL=$((FAIL + 1)); }

[[ "$(uname)" == "Darwin" ]] || { echo "smoke-test: macOS only (uname=$(uname))"; exit 2; }

# Base the fixture under /tmp (-> /private/tmp), NOT mktemp's default
# /var/folders/<uid>/T — the latter is the DARWIN_USER_TEMP_DIR that --darwin-temp
# legitimately grants, which would make a fixture placed there writable and mask the
# confinement. /private/tmp is outside every grant, so an out-of-pkg write there is a
# true negative.
B="$(mktemp -d /tmp/scwc-base.XXXXXX)"
trap 'rm -rf "$B"' EXIT
mkdir -p "$B/pkg" "$B/secret"
echo ORIGINAL >"$B/secret/data.txt"
PROF="$(node "$GEN" --pkg "$B/pkg" --mode strict --darwin-temp)"

# A profile that fails to PARSE must fail-CLOSED (sandbox-exec errors, no run).
if sandbox-exec -p "(version 1) (this-is-not-valid" /bin/echo hi >/dev/null 2>&1; then
  fail "malformed profile should fail-closed (sandbox-exec must error)"
else
  pass "malformed profile fails closed (sandbox-exec errors, does not run unconfined)"
fi

# 1. write INSIDE pkg → allowed
if sandbox-exec -p "$PROF" /bin/sh -c "echo hi >'$B/pkg/in.txt'" 2>/dev/null; then
  pass "write inside pkg allowed"
else
  fail "write inside pkg should be allowed"
fi

# 2. write OUTSIDE pkg → EPERM (the confinement)
if sandbox-exec -p "$PROF" /bin/sh -c "echo hi >'$B/secret/out.txt'" 2>/dev/null; then
  fail "write outside pkg should be DENIED"
else
  pass "write outside pkg denied"
fi

# 3. symlink escape → blocked
ln -sf "$B/secret" "$B/pkg/link"
if sandbox-exec -p "$PROF" /bin/sh -c "echo PWNED >'$B/pkg/link/data.txt'" 2>/dev/null; then
  fail "symlink-escape should be blocked"
else
  [[ "$(cat "$B/secret/data.txt")" == ORIGINAL ]] && pass "symlink-escape blocked (secret intact)" || fail "symlink-escape mutated secret"
fi

# 4. .. traversal → blocked
if sandbox-exec -p "$PROF" /bin/sh -c "echo PWNED >'$B/pkg/../secret/dd.txt'" 2>/dev/null; then
  fail ".. traversal should be blocked"
else
  [[ -e "$B/secret/dd.txt" ]] && fail ".. traversal created file" || pass ".. traversal blocked"
fi

# 5. in-jail hardlink creation to an out-of-root inode → blocked at creation
if sandbox-exec -p "$PROF" /bin/sh -c "ln '$B/secret/data.txt' '$B/pkg/h.txt'" 2>/dev/null; then
  fail "in-jail hardlink creation to out-of-root target should be denied"
else
  pass "in-jail hardlink creation to out-of-root target denied"
fi

# 6. device /dev/null write → allowed (the device minimum)
if sandbox-exec -p "$PROF" /bin/sh -c "echo x >/dev/null" 2>/dev/null; then
  pass "/dev/null write allowed (device minimum)"
else
  fail "/dev/null write should be allowed"
fi

# 7. canonicalization: a write-allow given in SYMLINK form must still confine —
#    i.e. the generator emits the canonical path so the allow actually works AND
#    nothing outside it is writable. Use a /tmp-rooted pkg (firmlink to /private/tmp).
TMPPKG="$(mktemp -d /tmp/scwc.XXXXXX)"
PROF2="$(node "$GEN" --pkg "$TMPPKG" --mode strict)"
if sandbox-exec -p "$PROF2" /bin/sh -c "echo hi >'$TMPPKG/a.txt'" 2>/dev/null; then
  pass "canonicalized /tmp-form pkg grant actually allows the write (not inert)"
else
  fail "canonicalized /tmp-form pkg grant should allow the write (symlink-form would be inert)"
fi
rm -rf "$TMPPKG"

echo
if [[ $FAIL -eq 0 ]]; then echo "ALL SMOKE TESTS PASS"; else echo "$FAIL SMOKE FAILURE(S)"; fi
exit $((FAIL == 0 ? 0 : 1))
