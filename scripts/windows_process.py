"""Run one measured process and contain its descendants in a Windows Job Object.

The primary thread stays suspended until the job owns the process. Only the two
standard handles are inherited. Closing the job kills any remaining descendants.
Counters describe the direct process, not the job or its compiler descendants.
The job limits the aggregate committed memory of its processes to eight GiB by
default. Allocation requests above this limit fail in the measured process.
Compiler calls can explicitly terminate a pinned VCTIP telemetry process after
the leader exits. Runtime calls still require every descendant to exit naturally.

API contracts:
https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw
https://learn.microsoft.com/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject
https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute
https://learn.microsoft.com/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject
https://learn.microsoft.com/windows/win32/api/jobapi/nf-jobapi-isprocessinjob
https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-queryfullprocessimagenamew
"""

from contextlib import ExitStack
import ctypes
import json
import math
import ntpath
import os
import subprocess
import sys
import time


DWORD = ctypes.c_uint32
HANDLE = ctypes.c_void_p
BOOL = ctypes.c_int32
SIZE_T = ctypes.c_size_t
DEFAULT_MEMORY_LIMIT = 8 * 1024**3


class JobBasicLimits(ctypes.Structure):
    """Match JOBOBJECT_BASIC_LIMIT_INFORMATION on the native Windows ABI."""

    _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64),
                ("PerJobUserTimeLimit", ctypes.c_int64), ("LimitFlags", DWORD),
                ("MinimumWorkingSetSize", SIZE_T), ("MaximumWorkingSetSize", SIZE_T),
                ("ActiveProcessLimit", DWORD), ("Affinity", SIZE_T),
                ("PriorityClass", DWORD), ("SchedulingClass", DWORD)]


class JobExtendedLimits(ctypes.Structure):
    """Keep the reserved IO_COUNTERS fields at their documented width."""

    _fields_ = [("BasicLimitInformation", JobBasicLimits),
                ("IoInfo", ctypes.c_uint64 * 6),
                ("ProcessMemoryLimit", SIZE_T), ("JobMemoryLimit", SIZE_T),
                ("PeakProcessMemoryUsed", SIZE_T), ("PeakJobMemoryUsed", SIZE_T)]


class JobBasicAccounting(ctypes.Structure):
    """Read the active process count without looking up recyclable PIDs."""

    _fields_ = [(name, ctypes.c_int64) for name in (
        "TotalUserTime", "TotalKernelTime", "ThisPeriodTotalUserTime", "ThisPeriodTotalKernelTime"
    )] + [(name, DWORD) for name in (
        "TotalPageFaultCount", "TotalProcesses", "ActiveProcesses", "TotalTerminatedProcesses"
    )]


class JobProcessIds(ctypes.Structure):
    """Describe the header and aligned variable array of owned process IDs."""

    _fields_ = [("assigned", DWORD), ("returned", DWORD), ("process_ids", SIZE_T * 1)]


class StartupInfo(ctypes.Structure):
    """Represent STARTUPINFOW with native pointers and fixed DWORD fields."""

    _fields_ = [("cb", DWORD), ("lpReserved", ctypes.c_wchar_p),
                ("lpDesktop", ctypes.c_wchar_p), ("lpTitle", ctypes.c_wchar_p)] + [
        (name, DWORD) for name in (
            "dwX", "dwY", "dwXSize", "dwYSize", "dwXCountChars", "dwYCountChars",
            "dwFillAttribute", "dwFlags"
        )
    ] + [("wShowWindow", ctypes.c_uint16), ("cbReserved2", ctypes.c_uint16),
         ("lpReserved2", ctypes.c_void_p), ("hStdInput", HANDLE),
         ("hStdOutput", HANDLE), ("hStdError", HANDLE)]


class StartupInfoEx(ctypes.Structure):
    """Carry the explicit handle inheritance list."""

    _fields_ = [("StartupInfo", StartupInfo), ("lpAttributeList", ctypes.c_void_p)]


class ProcessInformation(ctypes.Structure):
    """Retain the process and primary-thread handles returned by CreateProcessW."""

    _fields_ = [("process", HANDLE), ("thread", HANDLE), ("pid", DWORD), ("tid", DWORD)]


class WindowsProcessApi:
    """Bind only the Win32 operations needed by the qualification runner."""

    def __init__(self, kernel32=None, get_osfhandle=None):
        if kernel32 is None:
            if sys.platform != "win32":
                raise OSError("owned Windows processes require Windows")
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            import msvcrt
            get_osfhandle = msvcrt.get_osfhandle
        self.kernel = kernel32
        self.get_osfhandle = get_osfhandle
        signatures = {
            "CreateJobObjectW": ([ctypes.c_void_p, ctypes.c_wchar_p], HANDLE),
            "SetInformationJobObject": ([HANDLE, ctypes.c_int, ctypes.c_void_p, DWORD], BOOL),
            "QueryInformationJobObject": ([HANDLE, ctypes.c_int, ctypes.c_void_p, DWORD,
                                           ctypes.POINTER(DWORD)], BOOL),
            "AssignProcessToJobObject": ([HANDLE, HANDLE], BOOL),
            "TerminateJobObject": ([HANDLE, DWORD], BOOL),
            "TerminateProcess": ([HANDLE, DWORD], BOOL),
            "CloseHandle": ([HANDLE], BOOL),
            "GetCurrentProcess": ([], HANDLE),
            "DuplicateHandle": ([HANDLE, HANDLE, HANDLE, ctypes.POINTER(HANDLE),
                                 DWORD, BOOL, DWORD], BOOL),
            "InitializeProcThreadAttributeList": ([ctypes.c_void_p, DWORD, DWORD,
                                                   ctypes.POINTER(SIZE_T)], BOOL),
            "UpdateProcThreadAttribute": ([ctypes.c_void_p, DWORD, SIZE_T, ctypes.c_void_p,
                                           SIZE_T, ctypes.c_void_p, ctypes.c_void_p], BOOL),
            "DeleteProcThreadAttributeList": ([ctypes.c_void_p], None),
            "CreateProcessW": ([ctypes.c_wchar_p, ctypes.c_wchar_p, ctypes.c_void_p,
                                ctypes.c_void_p, BOOL, DWORD, ctypes.c_void_p,
                                ctypes.c_wchar_p, ctypes.POINTER(StartupInfoEx),
                                ctypes.POINTER(ProcessInformation)], BOOL),
            "ResumeThread": ([HANDLE], DWORD),
            "WaitForSingleObject": ([HANDLE, DWORD], DWORD),
            "GetExitCodeProcess": ([HANDLE, ctypes.POINTER(DWORD)], BOOL),
            "OpenProcess": ([DWORD, BOOL, DWORD], HANDLE),
            "IsProcessInJob": ([HANDLE, HANDLE, ctypes.POINTER(BOOL)], BOOL),
            "QueryFullProcessImageNameW": ([HANDLE, DWORD, ctypes.c_wchar_p,
                                             ctypes.POINTER(DWORD)], BOOL),
        }
        for name, (arguments, result) in signatures.items():
            function = getattr(kernel32, name)
            function.argtypes, function.restype = arguments, result

    def checked(self, name, *arguments):
        """Keep native failures visible instead of proceeding without containment."""
        result = getattr(self.kernel, name)(*arguments)
        if not result:
            raise OSError(ctypes.get_last_error(), f"{name} failed")
        return result

    def close(self, handle):
        """Close only a handle returned by an owned operation."""
        self.checked("CloseHandle", handle)

    def create_job(self, memory_limit_bytes=DEFAULT_MEMORY_LIMIT):
        """Bound aggregate commit, disallow breakaway and kill on the last close."""
        validate_memory_limit(memory_limit_bytes)
        job = self.checked("CreateJobObjectW", None, None)
        try:
            limits = JobExtendedLimits()
            # JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            limits.BasicLimitInformation.LimitFlags = 0x00002200
            limits.JobMemoryLimit = memory_limit_bytes
            self.checked("SetInformationJobObject", job, 9,
                         ctypes.byref(limits), ctypes.sizeof(limits))
        except BaseException:
            self.close(job)
            raise
        return job

    def duplicate(self, file):
        """Create an inheritable copy without changing the caller's handle flags."""
        current = self.kernel.GetCurrentProcess()
        duplicate = HANDLE()
        self.checked("DuplicateHandle", current, self.get_osfhandle(file.fileno()),
                     current, ctypes.byref(duplicate), 0, True, 2)
        return duplicate.value

    def create_process(self, command, transcript, cwd, env):
        """Start suspended with an explicit executable, environment and handle list."""
        process = ProcessInformation()

        def abort_creation(error_type, error, traceback):
            # Handle cleanup can fail after CreateProcessW succeeds. The caller
            # then never receives ownership of the suspended process.
            if error_type is not None and process.process:
                with ExitStack() as handles:
                    handles.callback(self.close, process.process)
                    handles.callback(self.close, process.thread)
                    self.terminate_process(process.process)
                    if not self.wait(process.process, 10):
                        raise OSError("the suspended process did not exit after termination")

        with ExitStack() as cleanup:
            cleanup.push(abort_creation)
            transcript.flush()
            stdin = cleanup.enter_context(open(os.devnull, "rb"))
            handles = []
            for file in (stdin, transcript):
                handle = self.duplicate(file)
                cleanup.callback(self.close, handle)
                handles.append(handle)
            inherited = (HANDLE * len(handles))(*handles)

            size = SIZE_T()
            result = self.kernel.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(size))
            if result or ctypes.get_last_error() != 122 or not size.value:
                raise OSError(ctypes.get_last_error(), "attribute list size query failed")
            attributes = ctypes.create_string_buffer(size.value)
            self.checked("InitializeProcThreadAttributeList", attributes, 1, 0, ctypes.byref(size))
            cleanup.callback(self.kernel.DeleteProcThreadAttributeList, attributes)
            self.checked("UpdateProcThreadAttribute", attributes, 0, 0x00020002,
                         inherited, ctypes.sizeof(inherited), None, None)

            startup = StartupInfoEx()
            startup.StartupInfo.cb = ctypes.sizeof(startup)
            startup.StartupInfo.dwFlags = 0x00000100  # STARTF_USESTDHANDLES
            startup.StartupInfo.hStdInput = handles[0]
            startup.StartupInfo.hStdOutput = startup.StartupInfo.hStdError = handles[1]
            startup.lpAttributeList = ctypes.cast(attributes, ctypes.c_void_p)
            line = ctypes.create_unicode_buffer(subprocess.list2cmdline(command))
            environment = ctypes.create_unicode_buffer(
                "\0".join(f"{key}={env[key]}" for key in sorted(env, key=str.upper)) + "\0"
            )
            # CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT
            self.checked("CreateProcessW", command[0], line, None, None, True,
                         0x00080404, environment, cwd, ctypes.byref(startup), ctypes.byref(process))
            return process

    def assign(self, job, process):
        """Attach the suspended process before it can create a child."""
        self.checked("AssignProcessToJobObject", job, process)

    def resume(self, thread):
        """Require exactly one suspension to end."""
        if self.kernel.ResumeThread(thread) != 1:
            raise OSError(ctypes.get_last_error(), "ResumeThread did not resume the primary thread")

    def wait(self, process, timeout):
        """Wait on the retained handle, using a finite millisecond deadline."""
        milliseconds = min(0xFFFFFFFE, max(0, math.ceil(timeout * 1000)))
        result = self.kernel.WaitForSingleObject(process, milliseconds)
        if result not in (0, 258):
            raise OSError(ctypes.get_last_error(), "WaitForSingleObject failed")
        return result == 0

    def active_processes(self, job):
        """Count all live descendants through the job handle."""
        accounting = JobBasicAccounting()
        self.checked("QueryInformationJobObject", job, 1, ctypes.byref(accounting),
                     ctypes.sizeof(accounting), None)
        return accounting.ActiveProcesses

    def exit_code(self, process):
        """Read the leader's exact exit status after its handle becomes signalled."""
        code = DWORD()
        self.checked("GetExitCodeProcess", process, ctypes.byref(code))
        return code.value

    def process_ids(self, job):
        """Enumerate only this job, with bounded retries if its process list grows."""
        capacity = 16
        offset = JobProcessIds.process_ids.offset
        for _ in range(8):
            buffer = ctypes.create_string_buffer(offset + capacity * ctypes.sizeof(SIZE_T))
            result = self.kernel.QueryInformationJobObject(job, 3, buffer, len(buffer), None)
            header = JobProcessIds.from_buffer(buffer)
            error = 0 if result else ctypes.get_last_error()
            if result and header.returned == header.assigned and header.returned <= capacity:
                identifiers = list((SIZE_T * header.returned).from_buffer(buffer, offset))
                if not all(identifiers) or len(set(identifiers)) != len(identifiers):
                    raise OSError("the owned job returned invalid process IDs")
                return identifiers
            if error not in (0, 234):  # ERROR_MORE_DATA
                raise OSError(error, "QueryInformationJobObject process list failed")
            capacity = max(capacity * 2, header.assigned)
            if capacity > 65536:
                break
        raise OSError("the owned job process list did not stabilize")

    def open_process(self, pid):
        """Retain identity before checking membership or terminating a process."""
        # PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE | PROCESS_TERMINATE
        return self.checked("OpenProcess", 0x00101001, False, pid)

    def in_job(self, process, job):
        """Reject a recycled PID that now names a process outside the owned job."""
        member = BOOL()
        self.checked("IsProcessInJob", process, job, ctypes.byref(member))
        return bool(member.value)

    def image_path(self, process):
        """Read the full executable path from the retained process handle."""
        path = ctypes.create_unicode_buffer(32768)
        length = DWORD(len(path))
        self.checked("QueryFullProcessImageNameW", process, 0, path, ctypes.byref(length))
        return path.value

    def terminate_job(self, job):
        """Stop only the process tree owned by this invocation."""
        self.checked("TerminateJobObject", job, 1)

    def terminate_process(self, process):
        """Stop an owned suspended leader or an explicitly verified telemetry child."""
        self.checked("TerminateProcess", process, 1)


def validate_memory_limit(memory_limit_bytes):
    """Reject absent, truncated and non-positive native memory bounds."""
    if (type(memory_limit_bytes) is not int or memory_limit_bytes <= 0
            or memory_limit_bytes >= 2 ** (8 * ctypes.sizeof(SIZE_T))):
        raise ValueError("the job memory limit must be a positive native-size byte count")


def validate_arguments(command, cwd, env, timeout):
    """Reject shell scripts, implicit executable search and malformed inputs."""
    if (not isinstance(command, (list, tuple)) or not command
            or any(not isinstance(value, str) or "\0" in value for value in command)
            or not ntpath.isabs(command[0]) or not command[0].lower().endswith(".exe")):
        raise ValueError("an absolute .exe path and explicit argument list are required")
    if not isinstance(cwd, str) or not ntpath.isabs(cwd) or "\0" in cwd:
        raise ValueError("an absolute Windows working directory is required")
    if type(timeout) not in (float, int) or not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("the process timeout must be finite and positive")
    if (not isinstance(env, dict) or any(
            not isinstance(key, str) or not key or "=" in key[1:] or "\0" in key
            or (key.startswith("=") and not (
                len(key) == 3 and key[1] in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz" and key[2] == ":"))
            or not isinstance(value, str) or "\0" in value for key, value in env.items())
            or len({key.upper() for key in env}) != len(env)):
        raise ValueError("the Windows environment contains invalid or duplicate names")


def wait_for_empty_job(api, job, deadline, clock, sleep, command, timeout):
    """Keep descendants inside the same deadline even after the leader exits."""
    while api.active_processes(job):
        remaining = deadline - clock()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(command, timeout)
        sleep(min(0.01, remaining))


def terminate_compiler_telemetry(api, job, expected_path, deadline, clock, command, timeout, terminated):
    """Stop only retained VCTIP handles after the compiler leader succeeds.

    Full image paths and job membership must both match. A new child can appear
    after enumeration, so this function never terminates the job on success.
    The caller still requires the job to become empty within the original limit.
    """
    with ExitStack() as handles:
        verified = []
        for pid in api.process_ids(job):
            process = api.open_process(pid)
            handles.callback(api.close, process)
            if not api.in_job(process, job):
                raise OSError("a compiler descendant no longer belongs to the owned job")
            if api.wait(process, 0):
                continue
            image_path = api.image_path(process)
            if ntpath.normcase(ntpath.normpath(image_path)) != ntpath.normcase(ntpath.normpath(expected_path)):
                raise OSError(f"unexpected compiler descendant: {image_path}")
            verified.append((process, {"pid": pid, "image_path": image_path}))

        for process, record in verified:
            if clock() >= deadline:
                raise subprocess.TimeoutExpired(command, timeout)
            try:
                api.terminate_process(process)
            except OSError as error:
                # TerminateProcess returns access denied if this retained process
                # already exited. The handle wait must prove that exact race.
                if error.errno != 5 or not api.wait(process, 0):
                    raise
            else:
                terminated.append(record)
                remaining = deadline - clock()
                if remaining <= 0 or not api.wait(process, remaining):
                    raise subprocess.TimeoutExpired(command, timeout)


def run_process(command, transcript, cwd, env, timeout, provider, *, api=None,
                memory_limit_bytes=DEFAULT_MEMORY_LIMIT, compiler_telemetry_path=None,
                clock=time.monotonic, sleep=time.sleep):
    """Return direct-process counters only after the entire owned tree exits.

    The caller owns the open transcript. Nonzero exits and timeouts raise the
    corresponding subprocess exception. Every failure preserves the transcript.
    Compiler callers can supply the pinned full VCTIP.EXE path. Only that image
    can be terminated after a successful leader exit. The receipt distinguishes
    requested telemetry termination from natural exit. `leader_elapsed_seconds`
    stops at leader exit. `elapsed_seconds` also includes descendant cleanup.
    Execution and cleanup failures carry a `windows_process_receipt` attribute
    and an exception note. Input validation errors do not carry a receipt.
    These retain any telemetry termination that preceded the failure.
    """
    validate_arguments(command, cwd, env, timeout)
    validate_memory_limit(memory_limit_bytes)
    if compiler_telemetry_path is not None and (
            not isinstance(compiler_telemetry_path, str) or "\0" in compiler_telemetry_path
            or not ntpath.isabs(compiler_telemetry_path)
            or ntpath.basename(compiler_telemetry_path).lower() != "vctip.exe"):
        raise ValueError("compiler telemetry requires the pinned absolute VCTIP.EXE path")
    api = api or WindowsProcessApi()
    started = None
    leader_elapsed = None
    returncode = None
    terminated_telemetry = []

    def record_failure(error, cleanup_completed):
        receipt = {"returncode": returncode,
                   "elapsed_seconds": None if started is None else clock() - started,
                   "leader_elapsed_seconds": leader_elapsed,
                   "compiler_telemetry_path": compiler_telemetry_path,
                   "terminated_telemetry": list(terminated_telemetry),
                   "error_cleanup_completed": cleanup_completed,
                   "job_memory_limit_bytes": memory_limit_bytes}
        error.windows_process_receipt = receipt
        error.add_note("Windows process receipt: " + json.dumps(receipt, sort_keys=True))

    def record_handle_failure(error_type, error, traceback):
        # This callback runs last, so failed CloseHandle calls cannot discard
        # telemetry termination evidence from an otherwise successful result.
        if error is not None and not hasattr(error, "windows_process_receipt"):
            record_failure(error, False)

    with ExitStack() as cleanup:
        cleanup.push(record_handle_failure)
        job = api.create_job(memory_limit_bytes)
        cleanup.callback(api.close, job)
        started = clock()
        process = api.create_process(command, transcript, cwd, env)
        cleanup.callback(api.close, process.process)
        cleanup.callback(api.close, process.thread)
        assigned = False
        try:
            api.assign(job, process.process)
            assigned = True
            api.resume(process.thread)
            remaining = started + timeout - clock()
            if remaining <= 0 or not api.wait(process.process, remaining):
                raise subprocess.TimeoutExpired(command, timeout)
            leader_elapsed = clock() - started
            returncode = api.exit_code(process.process)
            if returncode:
                raise subprocess.CalledProcessError(returncode, command)
            if compiler_telemetry_path is not None:
                terminate_compiler_telemetry(
                    api, job, compiler_telemetry_path, started + timeout, clock, command, timeout,
                    terminated_telemetry
                )
            wait_for_empty_job(api, job, started + timeout, clock, sleep, command, timeout)
            elapsed = clock() - started
            if elapsed > timeout:
                raise subprocess.TimeoutExpired(command, timeout)
            counters = provider.read(process.process)
            return {"returncode": returncode, "elapsed_seconds": elapsed,
                    "leader_elapsed_seconds": leader_elapsed,
                    "counters": counters, "descendants_exited": True,
                    "descendants_exited_naturally": not terminated_telemetry,
                    "compiler_telemetry_path": compiler_telemetry_path,
                    "terminated_telemetry": terminated_telemetry,
                    "job_memory_limit_bytes": memory_limit_bytes}
        except BaseException as error:
            try:
                if assigned:
                    api.terminate_job(job)
                else:
                    api.terminate_process(process.process)
                deadline = clock() + 10
                if not api.wait(process.process, 10):
                    raise OSError("the owned process did not exit after termination")
                if assigned:
                    wait_for_empty_job(api, job, deadline, clock, sleep, command, 10)
            except BaseException as cleanup_error:
                record_failure(cleanup_error, False)
                raise
            record_failure(error, True)
            raise
