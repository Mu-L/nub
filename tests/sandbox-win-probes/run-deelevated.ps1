# Runs a target probe script under a genuinely NON-ELEVATED (medium-IL, admin-filtered)
# token, so the probes' "no elevation required" claim is really tested even on a runner
# whose default job token is elevated (GitHub Actions windows-latest runs jobs with a FULL
# admin token, IsElevated=True, but the account has NO fetchable UAC linked split-token).
#
# De-elevation strategy (first that works wins; all logged):
#   1. TokenLinkedToken  -- the UAC-filtered token, present only on split-token accounts.
#                           GitHub's runneradmin has none, so this is best-effort.
#   2. CreateRestrictedToken(LUA_TOKEN) -- SYNTHESIZES the same filtered/medium-IL token from
#                           the current token (Administrators -> deny-only, IL -> Medium),
#                           needing no pre-existing linked token. This is the reliable path
#                           on the GH admin runner.
#   The chosen token is launched with CreateProcessWithTokenW, which needs SeImpersonate-
#   Privilege (Enabled on the runner) and does NOT depend on the Secondary Logon service.
#   3. Direct fallback   -- if neither token can be produced/launched, run the target directly
#                           (still elevated) so a MECHANISM verdict is always produced; the
#                           probe reports unprivileged=False and the harness stays red, which
#                           is the honest signal that the unprivileged sub-claim was not shown.
#
# The relaunched child gets a fresh console, so its stdout would not reach the CI log -- the
# inner probe redirects its whole output to a log file which we print here. The probe's REAL
# exit code is preserved.
#
# Usage: run-deelevated.ps1 <path-to-probe.ps1>   (exit code mirrors the target probe)

param([Parameter(Mandatory=$true)][string]$Target)
$ErrorActionPreference='Stop'
try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}

$id=[System.Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin=(New-Object System.Security.Principal.WindowsPrincipal($id)).IsInRole([System.Security.Principal.WindowsBuiltinRole]::Administrator)
Write-Host "[run-deelevated] parent IsElevated=$isAdmin target=$Target"

# Prepare the controlled root C:\probework while we (may) still hold the elevated token --
# a non-elevated child cannot create dirs at C:\ or re-ACL the root. Grant AppContainer
# groups RX (inherited) + the interactive user Modify. The LUA/linked token is the SAME user
# SID (only the Administrators group is filtered off), so the user's Modify grant still
# applies under the de-elevated token. Idempotent.
. "$PSScriptRoot\probe-common.ps1"
Ensure-ProbeRoot
Write-Host "[run-deelevated] prepared C:\probework (AC groups RX + user Modify)"

if (-not $isAdmin) {
    Write-Host "[run-deelevated] already non-elevated; running target directly"
    & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $Target
    exit $LASTEXITCODE
}

Add-Type -Language CSharp -TypeDefinition @"
using System; using System.Runtime.InteropServices; using System.ComponentModel; using System.Text;
public static class DeElev {
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr h, uint access, out IntPtr tok);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool GetTokenInformation(IntPtr tok, int cls, IntPtr buf, int len, out int ret);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool CreateRestrictedToken(IntPtr ExistingToken, uint Flags, uint DisableSidCount, IntPtr SidsToDisable, uint DeletePrivCount, IntPtr PrivsToDelete, uint RestrictedSidCount, IntPtr SidsToRestrict, out IntPtr NewToken);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    static extern bool CreateProcessWithTokenW(IntPtr hToken, uint dwLogonFlags, string app, string cmd, uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr h, out uint c);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);

    const uint TOKEN_QUERY=0x0008, TOKEN_DUPLICATE=0x0002, TOKEN_ASSIGN_PRIMARY=0x0001, TOKEN_ADJUST_DEFAULT=0x0080;
    const int TokenLinkedToken=19;
    const uint LUA_TOKEN=0x4;
    const uint CREATE_UNICODE_ENVIRONMENT=0x400;
    [StructLayout(LayoutKind.Sequential)] struct STARTUPINFO { public int cb; public string r1,desk,title; public int dwX,dwY,dwXS,dwYS,dwXC,dwYC,dwFill,dwFlags; public short wShow,cbR2; public IntPtr r2,hIn,hOut,hErr; }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr hProcess,hThread; public int pid,tid; }

    static IntPtr OpenSelf(){
        IntPtr cur;
        if(!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY|TOKEN_DUPLICATE|TOKEN_ASSIGN_PRIMARY|TOKEN_ADJUST_DEFAULT, out cur))
            throw new Win32Exception(Marshal.GetLastWin32Error(),"OpenProcessToken");
        return cur;
    }

    // Best-effort: the UAC linked (filtered) token, present only on split-token accounts.
    // Returns IntPtr.Zero (no throw) when the account has none -- the GH runneradmin case.
    public static IntPtr TryLinkedToken(){
        try {
            IntPtr cur=OpenSelf();
            int len; GetTokenInformation(cur, TokenLinkedToken, IntPtr.Zero, 0, out len);
            if(len<=0) return IntPtr.Zero;
            IntPtr buf=Marshal.AllocHGlobal(len);
            try {
                if(!GetTokenInformation(cur, TokenLinkedToken, buf, len, out len)) return IntPtr.Zero;
                return Marshal.ReadIntPtr(buf); // TOKEN_LINKED_TOKEN.LinkedToken
            } finally { Marshal.FreeHGlobal(buf); CloseHandle(cur); }
        } catch { return IntPtr.Zero; }
    }

    // Reliable: synthesize a LUA (filtered admin -> deny-only Administrators, medium IL) token
    // from the current token. Equivalent to the UAC-consented user's token; needs no linked
    // token to exist. Throws on failure so the caller can fall back.
    public static IntPtr CreateLuaToken(){
        IntPtr cur=OpenSelf(); IntPtr lua;
        bool ok=CreateRestrictedToken(cur, LUA_TOKEN, 0, IntPtr.Zero, 0, IntPtr.Zero, 0, IntPtr.Zero, out lua);
        CloseHandle(cur);
        if(!ok) throw new Win32Exception(Marshal.GetLastWin32Error(),"CreateRestrictedToken(LUA_TOKEN)");
        return lua;
    }

    // Launch cmd under the given primary token via CreateProcessWithTokenW (needs SeImpersonate).
    // Returns the child's exit code. Throws on launch failure.
    public static uint RunUnderToken(IntPtr token, string cmd, string cwd){
        var si=new STARTUPINFO(); si.cb=Marshal.SizeOf(typeof(STARTUPINFO));
        PROCESS_INFORMATION pi;
        if(!CreateProcessWithTokenW(token, 0, null, cmd, CREATE_UNICODE_ENVIRONMENT, IntPtr.Zero, cwd, ref si, out pi))
            throw new Win32Exception(Marshal.GetLastWin32Error(),"CreateProcessWithTokenW");
        WaitForSingleObject(pi.hProcess, 300000);
        uint code; GetExitCodeProcess(pi.hProcess, out code);
        CloseHandle(pi.hProcess); CloseHandle(pi.hThread);
        return code;
    }
}
"@

$psExe=(Get-Command powershell.exe).Source
$log = Join-Path $env:TEMP ("deelev-" + [guid]::NewGuid().ToString('N') + ".log")
# -EncodedCommand so we can redirect the whole invocation and propagate the file's exit code.
$inner = "& '$Target' *> '$log'; exit `$LASTEXITCODE"
$encInner = [Convert]::ToBase64String([System.Text.Encoding]::Unicode.GetBytes($inner))
$cmd = "`"$psExe`" -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encInner"
$cwd = (Split-Path $Target -Parent)

# Acquire a de-elevated token: linked (best-effort) -> LUA (reliable).
$token=[IntPtr]::Zero; $tokenKind='none'
$linked=[DeElev]::TryLinkedToken()
if ($linked -ne [IntPtr]::Zero) { $token=$linked; $tokenKind='linked' }
else {
    Write-Host "[run-deelevated] no UAC linked token on this account; synthesizing a LUA token"
    try { $token=[DeElev]::CreateLuaToken(); $tokenKind='lua' }
    catch { Write-Host "[run-deelevated] CreateLuaToken FAILED: $($_.Exception.Message)" }
}

if ($token -ne [IntPtr]::Zero) {
    Write-Host "[run-deelevated] relaunching target under $tokenKind (non-elevated) token; log=$log"
    try {
        $code = [DeElev]::RunUnderToken($token, $cmd, $cwd)
    } catch {
        Write-Host "[run-deelevated] CreateProcessWithTokenW FAILED ($tokenKind): $($_.Exception.Message)"
        Write-Host "[run-deelevated] falling back to DIRECT (elevated) execution -- probe will report unprivileged=False"
        $env:NUB_PROBE_DEELEV='direct-elevated'
        & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $Target
        exit $LASTEXITCODE
    }
    Write-Host "[run-deelevated] ----- begin target output ($tokenKind, non-elevated) -----"
    if (Test-Path $log) { Get-Content -Raw $log | Write-Host; Remove-Item -Force $log -ErrorAction SilentlyContinue }
    else { Write-Host "[run-deelevated] WARNING: no log produced (child may have failed to start)" }
    Write-Host "[run-deelevated] ----- end target output -----"
    Write-Host "[run-deelevated] target exit code (under $tokenKind non-elevated token): $code"
    exit $code
}

# Last resort: no de-elevated token could be produced -- run directly (elevated) so a
# mechanism verdict is still produced. The probe reports unprivileged=False and exits non-zero.
Write-Host "[run-deelevated] could not produce a de-elevated token; running target DIRECTLY (elevated)"
$env:NUB_PROBE_DEELEV='direct-elevated'
& powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $Target
exit $LASTEXITCODE
