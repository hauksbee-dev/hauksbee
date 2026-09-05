<# PowerShell 5.1-compatible, race-free Windows Job Object process runner. #>

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not ("Hauksbee.WindowsOwnedProcess" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace Hauksbee {
    public sealed class OwnedProcessResult {
        public int ExitCode { get; private set; }
        public bool TimedOut { get; private set; }

        internal OwnedProcessResult(int exitCode, bool timedOut) {
            ExitCode = exitCode;
            TimedOut = timedOut;
        }
    }

    public static class WindowsOwnedProcess {
        private const uint CREATE_SUSPENDED = 0x00000004;
        private const uint CREATE_UNICODE_ENVIRONMENT = 0x00000400;
        private const uint STARTF_USESTDHANDLES = 0x00000100;
        private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
        private const int JobObjectExtendedLimitInformation = 9;
        private const uint WAIT_OBJECT_0 = 0;
        private const uint WAIT_TIMEOUT = 258;
        private const uint INFINITE = 0xffffffff;
        private const uint GENERIC_WRITE = 0x40000000;
        private const uint FILE_SHARE_READ = 1;
        private const uint FILE_SHARE_WRITE = 2;
        private const uint CREATE_ALWAYS = 2;
        private const uint FILE_ATTRIBUTE_NORMAL = 0x00000080;
        private const int STD_INPUT_HANDLE = -10;
        private static readonly IntPtr InvalidHandle = new IntPtr(-1);

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct STARTUPINFO {
            public int cb;
            public string lpReserved;
            public string lpDesktop;
            public string lpTitle;
            public int dwX;
            public int dwY;
            public int dwXSize;
            public int dwYSize;
            public int dwXCountChars;
            public int dwYCountChars;
            public int dwFillAttribute;
            public uint dwFlags;
            public short wShowWindow;
            public short cbReserved2;
            public IntPtr lpReserved2;
            public IntPtr hStdInput;
            public IntPtr hStdOutput;
            public IntPtr hStdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct PROCESS_INFORMATION {
            public IntPtr hProcess;
            public IntPtr hThread;
            public uint dwProcessId;
            public uint dwThreadId;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct SECURITY_ATTRIBUTES {
            public int nLength;
            public IntPtr lpSecurityDescriptor;
            [MarshalAs(UnmanagedType.Bool)] public bool bInheritHandle;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize;
            public UIntPtr MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct IO_COUNTERS {
            public ulong ReadOperationCount;
            public ulong WriteOperationCount;
            public ulong OtherOperationCount;
            public ulong ReadTransferCount;
            public ulong WriteTransferCount;
            public ulong OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            public IO_COUNTERS IoInfo;
            public UIntPtr ProcessMemoryLimit;
            public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed;
            public UIntPtr PeakJobMemoryUsed;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern bool CreateProcessW(
            string applicationName, StringBuilder commandLine,
            IntPtr processAttributes, IntPtr threadAttributes,
            bool inheritHandles, uint creationFlags, IntPtr environment,
            string currentDirectory, ref STARTUPINFO startupInfo,
            out PROCESS_INFORMATION processInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern IntPtr CreateJobObject(IntPtr attributes, string name);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(
            IntPtr job, int informationClass,
            ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION information,
            uint informationLength);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateJobObject(IntPtr job, uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateProcess(IntPtr process, uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint ResumeThread(IntPtr thread);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CloseHandle(IntPtr handle);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateFileW(
            string path, uint access, uint share, ref SECURITY_ATTRIBUTES security,
            uint creation, uint flags, IntPtr template);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern IntPtr GetStdHandle(int standardHandle);

        private static Win32Exception Error(string operation) {
            return new Win32Exception(Marshal.GetLastWin32Error(), operation);
        }

        private static string Quote(string argument) {
            if (argument.Length > 0 && argument.IndexOfAny(new char[] { ' ', '\t', '\n', '\v', '"' }) < 0) {
                return argument;
            }
            StringBuilder quoted = new StringBuilder("\"");
            int backslashes = 0;
            foreach (char current in argument) {
                if (current == '\\') {
                    backslashes++;
                } else if (current == '"') {
                    quoted.Append('\\', backslashes * 2 + 1);
                    quoted.Append('"');
                    backslashes = 0;
                } else {
                    quoted.Append('\\', backslashes);
                    quoted.Append(current);
                    backslashes = 0;
                }
            }
            quoted.Append('\\', backslashes * 2);
            quoted.Append('"');
            return quoted.ToString();
        }

        private static StringBuilder CommandLine(string executable, string[] arguments) {
            StringBuilder line = new StringBuilder(Quote(executable));
            foreach (string argument in arguments) {
                line.Append(' ');
                line.Append(Quote(argument));
            }
            return line;
        }

        public static OwnedProcessResult Run(
            string executable, string[] arguments, string currentDirectory,
            string standardOutput, string standardError, int timeoutMilliseconds) {
            IntPtr stdout = IntPtr.Zero;
            IntPtr stderr = IntPtr.Zero;
            IntPtr job = IntPtr.Zero;
            PROCESS_INFORMATION process = new PROCESS_INFORMATION();
            bool processCreated = false;
            bool assigned = false;
            try {
                SECURITY_ATTRIBUTES security = new SECURITY_ATTRIBUTES();
                security.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
                security.bInheritHandle = true;
                stdout = CreateFileW(standardOutput, GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE, ref security, CREATE_ALWAYS,
                    FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
                if (stdout == InvalidHandle) throw Error("opening child stdout");
                stderr = CreateFileW(standardError, GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE, ref security, CREATE_ALWAYS,
                    FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
                if (stderr == InvalidHandle) throw Error("opening child stderr");

                STARTUPINFO startup = new STARTUPINFO();
                startup.cb = Marshal.SizeOf(typeof(STARTUPINFO));
                startup.dwFlags = STARTF_USESTDHANDLES;
                startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
                startup.hStdOutput = stdout;
                startup.hStdError = stderr;
                if (!CreateProcessW(executable, CommandLine(executable, arguments),
                    IntPtr.Zero, IntPtr.Zero, true,
                    CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT, IntPtr.Zero,
                    currentDirectory, ref startup, out process)) {
                    throw Error("creating suspended child process");
                }
                processCreated = true;

                job = CreateJobObject(IntPtr.Zero, null);
                if (job == IntPtr.Zero) throw Error("creating Job Object");
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits =
                    new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation,
                    ref limits, (uint)Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)))) {
                    throw Error("setting Job Object limits");
                }
                if (!AssignProcessToJobObject(job, process.hProcess)) {
                    throw Error("assigning suspended child to Job Object");
                }
                assigned = true;
                if (ResumeThread(process.hThread) == UInt32.MaxValue) {
                    throw Error("resuming Job-owned child");
                }

                uint wait = WaitForSingleObject(process.hProcess, (uint)timeoutMilliseconds);
                bool timedOut = wait == WAIT_TIMEOUT;
                if (timedOut) {
                    if (!TerminateJobObject(job, 124)) throw Error("terminating timed-out Job Object");
                    if (WaitForSingleObject(process.hProcess, INFINITE) != WAIT_OBJECT_0) {
                        throw Error("waiting for terminated Job-owned child");
                    }
                } else if (wait != WAIT_OBJECT_0) {
                    throw Error("waiting for Job-owned child");
                }
                uint exitCode;
                if (!GetExitCodeProcess(process.hProcess, out exitCode)) {
                    throw Error("reading Job-owned child exit code");
                }
                return new OwnedProcessResult(timedOut ? 124 : unchecked((int)exitCode), timedOut);
            } finally {
                if (processCreated && !assigned) TerminateProcess(process.hProcess, 125);
                if (job != IntPtr.Zero) CloseHandle(job);
                if (process.hThread != IntPtr.Zero) CloseHandle(process.hThread);
                if (process.hProcess != IntPtr.Zero) CloseHandle(process.hProcess);
                if (stderr != IntPtr.Zero && stderr != InvalidHandle) CloseHandle(stderr);
                if (stdout != IntPtr.Zero && stdout != InvalidHandle) CloseHandle(stdout);
            }
        }
    }
}
'@
}

function Invoke-HauksbeeJobProcess {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][string]$StandardOutput,
        [Parameter(Mandatory = $true)][string]$StandardError,
        [Parameter(Mandatory = $true)][int]$TimeoutMilliseconds
    )
    $command = Get-Command $FilePath -CommandType Application -ErrorAction Stop
    return [Hauksbee.WindowsOwnedProcess]::Run(
        $command.Path,
        $ArgumentList,
        $WorkingDirectory,
        [IO.Path]::GetFullPath($StandardOutput),
        [IO.Path]::GetFullPath($StandardError),
        $TimeoutMilliseconds
    )
}
