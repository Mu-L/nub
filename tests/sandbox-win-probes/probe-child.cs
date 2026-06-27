// Minimal AppContainer child for the nub Windows sandbox validation probes.
// Tiny load surface (no PowerShell, no heavy CLR module probing) so an AppContainer
// child starts cleanly and the probe measures the SECURITY outcome, not a host-init crash.
//
// Usage + exit-code contract (read by the parent probe):
//   probe-child.exe whoami         -> 0 ; dumps its own token (IsAppContainer, AC SID,
//                                        integrity level, package/capability groups) so the
//                                        parent can PROVE the child is really in the AppContainer
//   probe-child.exe read   <path>  -> 0 read OK | 5 ACCESS_DENIED | 9 other error
//   probe-child.exe write  <path>  -> 0 write OK | 5 ACCESS_DENIED | 9 other error
//   probe-child.exe connect <ip> <port>
//                                  -> 0 connect OK | 5 access-denied(WSAEACCES/10013)
//                                     | 6 timeout | 9 other error
//   probe-child.exe getenv <NAME>  -> 0 present (prints value) | 4 absent
//
// Anything that prints "CHILD ..." goes to stdout so the parent's captured log shows it.
using System;
using System.IO;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security.Principal;

static class ProbeChild {
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr h, uint access, out IntPtr tok);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool GetTokenInformation(IntPtr tok, int cls, IntPtr buf, int len, out int ret);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool ConvertSidToStringSid(IntPtr sid, out IntPtr str);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr h);
    const uint TOKEN_QUERY = 0x0008;
    const int TokenIntegrityLevel = 25, TokenIsAppContainer = 29, TokenAppContainerSid = 31;

    static int GetDword(IntPtr tok, int cls) {
        IntPtr buf = Marshal.AllocHGlobal(4);
        try { int ret; return GetTokenInformation(tok, cls, buf, 4, out ret) ? Marshal.ReadInt32(buf) : -1; }
        finally { Marshal.FreeHGlobal(buf); }
    }
    static string SidStr(IntPtr sid) {
        if (sid == IntPtr.Zero) return "<null>";
        IntPtr s; if (!ConvertSidToStringSid(sid, out s)) return "<convfail " + Marshal.GetLastWin32Error() + ">";
        string r = Marshal.PtrToStringUni(s); LocalFree(s); return r;
    }
    // The first pointer-sized field of TOKEN_APPCONTAINER_INFORMATION and TOKEN_MANDATORY_LABEL.Label
    // is a PSID, so one helper reads both the AppContainer SID and the integrity-level SID.
    static string GetLeadingSid(IntPtr tok, int cls) {
        int len; GetTokenInformation(tok, cls, IntPtr.Zero, 0, out len);
        if (len <= 0) return "<none/err " + Marshal.GetLastWin32Error() + ">";
        IntPtr buf = Marshal.AllocHGlobal(len);
        try {
            if (!GetTokenInformation(tok, cls, buf, len, out len)) return "<err " + Marshal.GetLastWin32Error() + ">";
            return SidStr(Marshal.ReadIntPtr(buf));
        } finally { Marshal.FreeHGlobal(buf); }
    }
    // An AppContainer child's stdout is NOT captured by the parent's redirected log, so the
    // token dump is TEE'd to outPath (a file in a dir the AC SID was granted write) -- that
    // file is the reliable diagnostic channel the parent reads back.
    static void DumpToken(string outPath) {
        var sb = new System.Text.StringBuilder();
        Action<string> emit = line => { Console.WriteLine(line); sb.AppendLine(line); };
        try {
            emit("CHILD whoami user=" + WindowsIdentity.GetCurrent().Name);
            IntPtr tok;
            if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, out tok)) {
                emit("CHILD whoami OpenProcessToken failed " + Marshal.GetLastWin32Error());
            } else {
                emit("CHILD whoami TokenIsAppContainer=" + GetDword(tok, TokenIsAppContainer));
                emit("CHILD whoami TokenAppContainerSid=" + GetLeadingSid(tok, TokenAppContainerSid));
                emit("CHILD whoami IntegrityLevelSid=" + GetLeadingSid(tok, TokenIntegrityLevel));
                // Enumerate group SIDs and surface the package/capability/integrity families.
                foreach (IdentityReference g in WindowsIdentity.GetCurrent().Groups) {
                    string s = g.Value;
                    if (s.StartsWith("S-1-15-2") || s.StartsWith("S-1-15-3") || s.StartsWith("S-1-16"))
                        emit("CHILD whoami group=" + s);
                }
            }
        } catch (Exception e) { emit("CHILD whoami ERR: " + e.Message); }
        if (outPath != null) {
            try { File.WriteAllText(outPath, sb.ToString()); }
            catch (Exception e) { Console.WriteLine("CHILD whoami could not write dump to " + outPath + ": " + e.Message); }
        }
    }

    static int Main(string[] a) {
        try {
            if (a.Length < 1) { Console.WriteLine("CHILD bad-args"); return 2; }
            if (a[0] == "whoami") { DumpToken(a.Length >= 2 ? a[1] : null); return 0; }
            if (a.Length < 2) { Console.WriteLine("CHILD bad-args"); return 2; }
            switch (a[0]) {
                case "read": {
                    try {
                        string s = File.ReadAllText(a[1]);
                        Console.WriteLine("CHILD read OK len=" + s.Length);
                        return 0;
                    } catch (UnauthorizedAccessException e) {
                        Console.WriteLine("CHILD read DENIED: " + e.Message); return 5;
                    } catch (Exception e) {
                        Console.WriteLine("CHILD read ERR: " + e.GetType().Name + " " + e.Message); return 9;
                    }
                }
                case "write": {
                    try {
                        File.WriteAllText(a[1], "from-appcontainer-child");
                        Console.WriteLine("CHILD write OK");
                        return 0;
                    } catch (UnauthorizedAccessException e) {
                        Console.WriteLine("CHILD write DENIED: " + e.Message); return 5;
                    } catch (Exception e) {
                        Console.WriteLine("CHILD write ERR: " + e.GetType().Name + " " + e.Message); return 9;
                    }
                }
                case "connect": {
                    if (a.Length < 3) { Console.WriteLine("CHILD connect bad-args"); return 2; }
                    int port = int.Parse(a[2]);
                    try {
                        using (var c = new TcpClient()) {
                            var iar = c.BeginConnect(a[1], port, null, null);
                            if (!iar.AsyncWaitHandle.WaitOne(8000, false)) {
                                Console.WriteLine("CHILD connect TIMEOUT"); return 6;
                            }
                            c.EndConnect(iar);
                            Console.WriteLine("CHILD connect OK");
                            return 0;
                        }
                    } catch (SocketException se) {
                        // 10013 = WSAEACCES (AppContainer egress block surfaces here)
                        Console.WriteLine("CHILD connect FAILED SocketErrorCode=" + (int)se.SocketErrorCode
                            + " (" + se.SocketErrorCode + ") msg=" + se.Message);
                        return (se.SocketErrorCode == SocketError.AccessDenied) ? 5 : 9;
                    } catch (Exception e) {
                        Console.WriteLine("CHILD connect ERR: " + e.GetType().Name + " " + e.Message);
                        var se = e.InnerException as SocketException;
                        if (se != null) {
                            Console.WriteLine("  inner SocketErrorCode=" + (int)se.SocketErrorCode);
                            return (se.SocketErrorCode == SocketError.AccessDenied) ? 5 : 9;
                        }
                        return 9;
                    }
                }
                case "getenv": {
                    string v = Environment.GetEnvironmentVariable(a[1]);
                    if (v == null) { Console.WriteLine("CHILD env ABSENT"); return 4; }
                    Console.WriteLine("CHILD env PRESENT: " + v); return 0;
                }
                default:
                    Console.WriteLine("CHILD unknown-cmd"); return 2;
            }
        } catch (Exception e) {
            Console.WriteLine("CHILD FATAL: " + e); return 3;
        }
    }
}
