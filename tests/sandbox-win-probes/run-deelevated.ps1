# Runs a target probe script under a NON-ELEVATED token, so the probes' "no elevation
# required" claim is genuinely tested even on a runner whose default job token is elevated
# (GitHub Actions windows-latest runs jobs with a FULL admin token, IsElevated=True).
#
# Technique: an elevated process holds a LINKED (filtered, medium-IL, admin-stripped)
# token -- the same token a normal UAC-consented user runs with. We fetch it via
# GetTokenInformation(TokenLinkedToken) and relaunch the target with CreateProcessWithTokenW.
# Inside the relaunched child, IsInRole(Administrator) == False, so the probe measures the
# UNPRIVILEGED path. If we're already non-elevated, we just run the target directly.
#
# Usage: run-deelevated.ps1 <path-to-probe.ps1>
# Exit code mirrors the target probe's exit code.

param([Parameter(Mandatory=$true)][string]$Target)
$ErrorActionPreference='Stop'

$id=[System.Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin=(New-Object System.Security.Principal.WindowsPrincipal($id)).IsInRole([System.Security.Principal.WindowsBuiltinRole]::Administrator)
Write-Host "[run-deelevated] parent IsElevated=$isAdmin target=$Target"

# Prepare the controlled root C:\probework while we (may) still hold the elevated token --
# a non-elevated child cannot create dirs at C:\ or re-ACL the root. Grant AppContainer
# groups RX (inherited) + the interactive user Modify, so the de-elevated probe can seed +
# ACL its per-run subdirs and the AC child can traverse/read. Idempotent.
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
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    static extern bool CreateProcessWithTokenW(IntPtr hToken, uint dwLogonFlags, string app, string cmd, uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr h, out uint c);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);

    const uint TOKEN_QUERY=0x0008;
    const int TokenLinkedToken=19;
    [StructLayout(LayoutKind.Sequential)] struct STARTUPINFO { public int cb; public string r1,desk,title; public int dwX,dwY,dwXS,dwYS,dwXC,dwYC,dwFill,dwFlags; public short wShow,cbR2; public IntPtr r2,hIn,hOut,hErr; }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr hProcess,hThread; public int pid,tid; }
    const uint CREATE_UNICODE_ENVIRONMENT=0x400;

    // Returns the child's exit code after running cmd under the linked (filtered) token.
    public static uint RunUnderLinkedToken(string cmd, string cwd){
        IntPtr cur; if(!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, out cur)) throw new Win32Exception(Marshal.GetLastWin32Error(),"OpenProcessToken");
        int len; GetTokenInformation(cur, TokenLinkedToken, IntPtr.Zero, 0, out len);
        IntPtr buf=Marshal.AllocHGlobal(len);
        if(!GetTokenInformation(cur, TokenLinkedToken, buf, len, out len)) throw new Win32Exception(Marshal.GetLastWin32Error(),"GetTokenInformation(LinkedToken)");
        IntPtr linked=Marshal.ReadIntPtr(buf); // TOKEN_LINKED_TOKEN.LinkedToken
        var si=new STARTUPINFO(); si.cb=Marshal.SizeOf(typeof(STARTUPINFO));
        PROCESS_INFORMATION pi;
        // dwLogonFlags=0; passing IntPtr.Zero env => child inherits this process's env (fine).
        if(!CreateProcessWithTokenW(linked, 0, null, cmd, CREATE_UNICODE_ENVIRONMENT, IntPtr.Zero, cwd, ref si, out pi))
            throw new Win32Exception(Marshal.GetLastWin32Error(),"CreateProcessWithTokenW");
        WaitForSingleObject(pi.hProcess, 180000);
        uint code; GetExitCodeProcess(pi.hProcess, out code);
        CloseHandle(pi.hProcess); CloseHandle(pi.hThread); Marshal.FreeHGlobal(buf);
        return code;
    }
}
"@

# CreateProcessWithTokenW gives the child a fresh console, so its stdout would NOT appear in
# the CI log. Redirect the relaunched probe's streams to a log file (via PowerShell's *> ),
# then print that log here so the probe output is visible in CI. The probe's REAL exit code
# is preserved by having the inner powershell exit with it.
$psExe=(Get-Command powershell.exe).Source
$log = Join-Path $env:TEMP ("deelev-" + [guid]::NewGuid().ToString('N') + ".log")
# -Command so we can redirect the whole invocation and propagate the file's exit code.
$inner = "& '$Target' *> '$log'; exit `$LASTEXITCODE"
$encInner = [Convert]::ToBase64String([System.Text.Encoding]::Unicode.GetBytes($inner))
$cmd = "`"$psExe`" -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encInner"
Write-Host "[run-deelevated] relaunching target under linked (non-elevated) token; log=$log"
$code = [DeElev]::RunUnderLinkedToken($cmd, (Split-Path $Target -Parent))
Write-Host "[run-deelevated] ----- begin target output (non-elevated) -----"
if (Test-Path $log) { Get-Content -Raw $log | Write-Host; Remove-Item -Force $log -ErrorAction SilentlyContinue }
else { Write-Host "[run-deelevated] WARNING: no log produced (child may have failed to start)" }
Write-Host "[run-deelevated] ----- end target output -----"
Write-Host "[run-deelevated] target exit code (under non-elevated token): $code"
exit $code
