# Probe 1 -- Unprivileged FS read-confine (THE KEY UNPROVEN CLAIM)
#
# DESIGN-ACCURATE model = ALLOWLIST / DEFAULT-DENY (what nub actually does), not a deny-ACE
# on a secret that sits under a dir granting ALL APPLICATION PACKAGES. An AppContainer child is
# default-deny: it can read an object ONLY where the ACL grants its AppContainer SID, a
# capability, or ALL APPLICATION PACKAGES. So we grant the AC SID read on a WORK dir (and seed
# an allowed file there), and place the secret in a VAULT dir with inheritance broken and NO
# AppContainer/AAP grant -> the child cannot reach it by default, with NO per-file deny-ACE.
#
# Round-3 diagnostics (decisive): the child first DUMPS ITS OWN TOKEN to a file so we can PROVE
# it is really in the AppContainer (TokenIsAppContainer=1, a real TokenAppContainerSid, Low IL),
# and we print the icacls ACLs of the vault/secret so an AAP grant cannot silently explain a read.
#
# NEGATIVE CONTROLS (so a PASS cannot be vacuous):
#   NC-A: PARENT reads the secret (file exists + readable -> a child block is the confinement).
#   NC-B: the SAME AppContainer child reads an ALLOWED file in the WORK dir (child can reach +
#         read the FS where granted -> the secret block is confinement, not a blanket lockout).
#   NC-C: not elevated.
# PASS = NC-A ok, NC-B child read-allowed ok (exit 0), secret read BLOCKED (exit 5/9), unelevated.

$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'
function Section($s){ Write-Host "`n=== $s ===" }
. "$PSScriptRoot\probe-common.ps1"

$id=[System.Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin=(New-Object System.Security.Principal.WindowsPrincipal($id)).IsInRole([System.Security.Principal.WindowsBuiltinRole]::Administrator)
Write-Host "Running as: $($id.Name)  IsElevated: $isAdmin"

$child = Build-ProbeChild
Write-Host "probe child: $child"

# WORK (allowlisted, AC SID gets RX) holds the allowed file + a diag dir the child can write.
$work = New-ControlledDir 'probe1'
$allowed = Join-Path $work 'allowed.txt'
Set-Content -Path $allowed -Value 'this-is-fine' -NoNewline
$diag = Join-Path $work 'diag'
New-Item -ItemType Directory -Path $diag -Force | Out-Null
$dump = Join-Path $diag 'token.txt'

# VAULT (default-deny): break inheritance and grant ONLY the running user, so the AppContainer
# SID and ALL APPLICATION PACKAGES have NO access -> the child cannot reach the secret at all.
$vault = Join-Path $work 'vault'
New-Item -ItemType Directory -Path $vault -Force | Out-Null
& icacls $vault /inheritance:r /grant:r "${env:USERNAME}:(OI)(CI)(F)" 2>&1 | Write-Host
$secret = Join-Path $vault 'secret.env'
Set-Content -Path $secret -Value 'TOPSECRET_TOKEN=sk-do-not-leak-123' -NoNewline
Write-Host "work dir: $work"
Write-Host "vault (default-deny) secret: $secret"

Section 'NC-A: parent reads secret'
$parentRead = Get-Content -Raw $secret
if ($parentRead -notlike '*TOPSECRET_TOKEN*') { throw "NC-A FAILED: parent could not read seeded secret" }
Write-Host "NC-A PASS: parent read secret OK"

Section 'Create AppContainer profile'
$acName = 'NubProbe1_' + ([guid]::NewGuid().ToString('N').Substring(0,12))
$acSidPtr = [IntPtr]::Zero
$hr = [AC]::CreateAppContainerProfile($acName,$acName,'nub probe1 read-confine',[IntPtr]::Zero,0,[ref]$acSidPtr)
if ($hr -ne 0) { throw "CreateAppContainerProfile failed hr=0x$("{0:X8}" -f $hr)" }
$acSidStr = [AC]::SidToString($acSidPtr)
Write-Host "AppContainer SID: $acSidStr"
$acAccount = New-Object System.Security.Principal.SecurityIdentifier($acSidStr)

$inAC = $false
try {
    Section 'Grant AC SID: RX on work, Modify on diag (child writes its token dump there)'
    Grant-AcRx $work $acAccount
    Grant-AcModify $diag $acAccount
    Write-Host "grants applied (vault deliberately has NO AC grant)"

    Section 'DIAGNOSTIC: child dumps its own token (PROVE in-AppContainer)'
    $codeWhoami = [AC]::Launch($acSidPtr, "`"$child`" whoami `"$dump`"", $work)
    Write-Host "child(whoami) raw exit: $codeWhoami"
    if (Test-Path $dump) {
        $dumpText = Get-Content -Raw $dump
        Write-Host $dumpText
        if ($dumpText -match 'TokenIsAppContainer=1') { $inAC = $true }
    } else { Write-Host "DIAGNOSTIC: no token dump produced (child could not write diag)" }
    Write-Host "child-in-AppContainer (TokenIsAppContainer=1)? $inAC"

    Section 'DIAGNOSTIC: ACLs (no AAP grant should be on the vault/secret)'
    Write-Host "--- icacls vault ---";  & icacls $vault  2>&1 | Write-Host
    Write-Host "--- icacls secret ---"; & icacls $secret 2>&1 | Write-Host
    Write-Host "--- icacls work ---";   & icacls $work   2>&1 | Write-Host

    Section 'Launch child: read ALLOWED file in work (NC-B, expect exit 0)'
    $codeAllowed = [AC]::Launch($acSidPtr, "`"$child`" read `"$allowed`"", $work)
    Write-Host "child(read allowed) raw exit: $codeAllowed"

    Section 'Launch child: read SECRET in default-deny vault (KEY, expect 5/9 = denied)'
    $codeSecret = [AC]::Launch($acSidPtr, "`"$child`" read `"$secret`"", $work)
    Write-Host "child(read secret) raw exit: $codeSecret"

    Section 'VERDICT'
    Write-Host "NC-B (allowed read) exit=$codeAllowed (expect 0); secret read exit=$codeSecret (expect 5/9 denied)"
    # $mech = the SECURITY outcome of allowlist/default-deny confinement. Independent of parent
    # elevation: the child is a lowbox token whose confinement holds regardless. The "unprivileged"
    # sub-claim (setup needs no admin) is tracked separately via $isAdmin.
    if (-not $inAC) {
        $mech='INCONCLUSIVE'; $detail="child token is NOT an AppContainer (TokenIsAppContainer!=1) -> launch did not confine; any read result is meaningless"
    } elseif ($codeAllowed -ne 0) {
        $mech='INCONCLUSIVE'; $detail="AppContainer child could not read the allowed file (exit=$codeAllowed) -> NC-B broken (traversal/launch, not a confinement result)"
    } elseif ($codeSecret -eq 5 -or $codeSecret -eq 9) {
        $mech='PASS'; $detail="allowed readable, secret in default-deny vault BLOCKED (exit=$codeSecret)"
    } elseif ($codeSecret -eq 0) {
        $mech='FAIL'; $detail="SECRET LEAKED -- default-deny confinement DID NOT HOLD"
    } else {
        $mech='INCONCLUSIVE'; $detail="secret-read exit=$codeSecret (neither 0 nor 5/9)"
    }
}
finally {
    [void][AC]::DeleteAppContainerProfile($acName)
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
$unpriv = (-not $isAdmin)
if ($mech -eq 'PASS' -and $unpriv) { $probe1='PASS' }
elseif ($mech -eq 'PASS' -and -not $unpriv) { $probe1='INCONCLUSIVE'; $detail="$detail; mechanism CONFIRMED but parent ELEVATED -> unprivileged sub-claim not shown in this run" }
else { $probe1=$mech }
Write-Host "PROBE1 read-confine: ${probe1}: $detail  [mechanism=$mech inAppContainer=$inAC unprivileged=$unpriv]"
if ($probe1 -ne 'PASS') { exit 1 } else { exit 0 }
