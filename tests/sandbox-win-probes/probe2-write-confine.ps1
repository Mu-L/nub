# Probe 2 -- Unprivileged FS write-confine
#
# A child confined via AppContainer SID can write ONLY inside an allowed dir and is
# BLOCKED writing elsewhere -- no elevation, no second account.
#
# NEGATIVE CONTROLS:
#   NC-A: parent writes BOTH allowed + outside dirs (both writable absent sandbox).
#   NC-B: AppContainer child writes inside allowed dir (exit 0) -> child can write at all.
#   NC-C: not elevated.
# PASS = NC-A both ok, child allowed-write exit 0, child outside-write BLOCKED (exit 5),
#        unelevated. Hardened: controlled dir chain + tiny native child + raw codes.

$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'
function Section($s){ Write-Host "`n=== $s ===" }
. "$PSScriptRoot\probe-common.ps1"

$id=[System.Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin=(New-Object System.Security.Principal.WindowsPrincipal($id)).IsInRole([System.Security.Principal.WindowsBuiltinRole]::Administrator)
Write-Host "Running as: $($id.Name)  IsElevated: $isAdmin"

$child = Build-ProbeChild
Write-Host "probe child: $child"

$root = New-ControlledDir 'probe2'
$allowedDir = Join-Path $root 'allowed'
$outsideDir = Join-Path $root 'outside'
New-Item -ItemType Directory -Path $allowedDir -Force | Out-Null
New-Item -ItemType Directory -Path $outsideDir -Force | Out-Null

Section 'NC-A: parent writes both dirs'
Set-Content -Path (Join-Path $allowedDir 'p.txt') -Value 'parent' -NoNewline
Set-Content -Path (Join-Path $outsideDir 'p.txt') -Value 'parent' -NoNewline
Write-Host "NC-A PASS: parent wrote both"

Section 'Create AppContainer profile'
$acName='NubProbe2_'+([guid]::NewGuid().ToString('N').Substring(0,12))
$sidPtr=[IntPtr]::Zero
$hr=[AC]::CreateAppContainerProfile($acName,$acName,'nub probe2 write-confine',[IntPtr]::Zero,0,[ref]$sidPtr)
if($hr -ne 0){ throw "CreateAppContainerProfile hr=0x$("{0:X8}" -f $hr)" }
$sidStr=[AC]::SidToString($sidPtr); Write-Host "AppContainer SID: $sidStr"
$acAcct=New-Object System.Security.Principal.SecurityIdentifier($sidStr)

$probe2='INCONCLUSIVE'
try {
    Section 'ACEs: RX on root (traverse+read child), Modify on allowedDir; outsideDir gets NO AC write grant'
    Grant-AcRx $root $acAcct
    Grant-AcModify $allowedDir $acAcct
    # outsideDir: deliberately NO allow-write ACE for the AC SID. AppContainer is deny-by-default
    # for SIDs absent from the ACL, so absence of an allow = blocked write.
    Write-Host "ACEs applied (allowedDir=Modify, outsideDir=no AC write grant)"

    $a = Join-Path $allowedDir 'child.txt'
    $o = Join-Path $outsideDir 'child.txt'

    Section 'Launch child: write ALLOWED dir (NC-B, expect exit 0)'
    $codeAllowed=[AC]::Launch($sidPtr, "`"$child`" write `"$a`"", $allowedDir)
    Write-Host "child(write allowed) raw exit: $codeAllowed"

    Section 'Launch child: write OUTSIDE dir (expect exit 5 = ACCESS_DENIED)'
    $codeOutside=[AC]::Launch($sidPtr, "`"$child`" write `"$o`"", $allowedDir)
    Write-Host "child(write outside) raw exit: $codeOutside"

    Section 'VERDICT'
    Write-Host "allowed-write exit=$codeAllowed (expect 0); outside-write exit=$codeOutside (expect 5)"
    # $mech = security outcome (independent of parent elevation; the child is a lowbox token).
    if($codeAllowed -ne 0){ $mech='INCONCLUSIVE'; $detail="child could not write allowed dir (exit=$codeAllowed) -> NC-B broken" }
    elseif($codeOutside -eq 5){ $mech='PASS'; $detail="allowed-dir write OK, outside-dir write blocked (ACCESS_DENIED)" }
    elseif($codeOutside -eq 0){ $mech='FAIL'; $detail="WROTE OUTSIDE -- write-confine DID NOT HOLD" }
    else { $mech='INCONCLUSIVE'; $detail="outside-write exit=$codeOutside (neither 0 nor 5)" }
}
finally {
    [void][AC]::DeleteAppContainerProfile($acName)
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
$unpriv = (-not $isAdmin)
if ($mech -eq 'PASS' -and $unpriv) { $probe2='PASS' }
elseif ($mech -eq 'PASS' -and -not $unpriv) { $probe2='INCONCLUSIVE'; $detail="$detail; mechanism CONFIRMED but parent ELEVATED -> unprivileged sub-claim not shown in this run" }
else { $probe2=$mech }
Write-Host "PROBE2 write-confine: ${probe2}: $detail  [mechanism=$mech unprivileged=$unpriv]"
if($probe2 -ne 'PASS'){ exit 1 } else { exit 0 }
